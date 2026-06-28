#![deny(warnings)]
#![deny(missing_docs)]
//! Read-only on-chain access and backend mint proposal signing.

use std::sync::Arc;

use alloy::network::EthereumWallet;
use alloy::primitives::{Address, Bytes, B256, TxHash, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder, RootProvider};
use alloy::rpc::types::BlockNumberOrTag;
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::SignerSync;
use alloy::transports::TransportError;
use common::{Commodity, MintProposal};

#[allow(missing_docs, dead_code, non_snake_case, clippy::all)]
mod oracle_abi {
    use alloy::sol;

    sol! {
        #[sol(rpc)]
        interface IOracleAggregator {
            function submitPrice(uint8 commodity, uint256 price) external;
            function getPriceUnchecked(uint8 commodity) external view returns (uint256 price, uint256 updatedAt, bool initialized);
        }
    }
}

use oracle_abi::IOracleAggregator;

#[allow(missing_docs, dead_code, non_snake_case, clippy::all)]
mod mining_abi {
    use alloy::sol;

    sol! {
        #[sol(rpc)]
        interface IMiningContract {
            function proposeMint(bytes32 proposalId, bytes32 sessionId, address recipient, uint256 amount) external;
            function approveMint(bytes32 proposalId) external;
            function executeMint(bytes32 proposalId) external;
            function cancelMint(bytes32 proposalId) external;
            function proposals(bytes32 proposalId) external view returns (bytes32 sessionId, address recipient, uint256 amount, uint256 proposedAt, uint8 approvals, bool executed, bool cancelled);
        }
    }
}

use mining_abi::IMiningContract;

#[allow(missing_docs, dead_code, non_snake_case, clippy::all)]
mod staking_abi {
    use alloy::sol;

    sol! {
        #[sol(rpc)]
        interface IStakingContract {
            function stakes(address user) external view returns (uint256 amount, uint8 lockPeriod, uint256 stakedAt, uint256 unlockAt, bool active);
        }
    }
}

use staking_abi::IStakingContract;

/// Errors returned by the on-chain client.
#[derive(Debug)]
pub enum OnchainClientError {
    /// RPC transport error.
    Transport(TransportError),
    /// The RPC URL could not be parsed.
    InvalidUrl(String),
    /// The proposal could not be serialized.
    Serialization(serde_json::Error),
    /// The proposal could not be signed.
    Signing(alloy::signers::Error),
    /// A contract call reverted or otherwise failed on-chain.
    Contract(String),
}

impl std::fmt::Display for OnchainClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "transport error: {e}"),
            Self::InvalidUrl(url) => write!(f, "invalid rpc url: {url}"),
            Self::Serialization(e) => write!(f, "serialization error: {e}"),
            Self::Signing(e) => write!(f, "signing error: {e}"),
            Self::Contract(msg) => write!(f, "contract error: {msg}"),
        }
    }
}

impl std::error::Error for OnchainClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(e) => Some(e),
            Self::InvalidUrl(_) => None,
            Self::Serialization(e) => Some(e),
            Self::Signing(e) => Some(e),
            Self::Contract(_) => None,
        }
    }
}

impl From<TransportError> for OnchainClientError {
    fn from(e: TransportError) -> Self {
        Self::Transport(e)
    }
}

impl From<serde_json::Error> for OnchainClientError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serialization(e)
    }
}

impl From<alloy::signers::Error> for OnchainClientError {
    fn from(e: alloy::signers::Error) -> Self {
        Self::Signing(e)
    }
}

/// Read-only Ethereum client plus backend proposal signing. Never writes
/// on-chain; it produces signed proposals for the admin multi-sig flow.
pub struct OnchainClient {
    provider: Arc<RootProvider>,
    chain_id: u64,
}

impl OnchainClient {
    /// Builds an HTTP provider for `rpc_url` bound to `chain_id`.
    pub async fn new(rpc_url: &str, chain_id: u64) -> Result<Self, OnchainClientError> {
        let url = rpc_url
            .parse()
            .map_err(|_| OnchainClientError::InvalidUrl(rpc_url.to_string()))?;
        let provider = ProviderBuilder::new().connect_http(url);
        let root = provider.root().clone();
        Ok(Self {
            provider: Arc::new(root),
            chain_id,
        })
    }

    /// Returns the chain id this client is bound to.
    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Returns the latest block number.
    pub async fn get_block_number(&self) -> Result<u64, OnchainClientError> {
        Ok(self.provider.get_block_number().await?)
    }

    /// Returns the 32-byte hash of `block_number`, or `None` if the block is
    /// absent. Used to derive the session seed from `start_block - 3`.
    pub async fn get_block_hash(
        &self,
        block_number: u64,
    ) -> Result<Option<[u8; 32]>, OnchainClientError> {
        let block = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Number(block_number))
            .await?;
        Ok(block.map(|b| b.header.hash.0))
    }

    /// Signs the JSON encoding of `proposal` with `signing_key` and returns the
    /// signature bytes. The proposal is signed but never submitted on-chain.
    pub fn sign_proposal(
        proposal: &MintProposal,
        signing_key: &PrivateKeySigner,
    ) -> Result<Bytes, OnchainClientError> {
        let json = serde_json::to_vec(proposal)?;
        let signature = signing_key.sign_message_sync(&json)?;
        Ok(Bytes::from(signature.as_bytes().to_vec()))
    }
}

/// Write-capable client scoped to a single on-chain authority: submitting
/// backend-computed TWAP prices to the OracleAggregator via `submitPrice`. It
/// holds a signer-backed provider and has no minting capability.
pub struct OracleSubmitter {
    provider: DynProvider,
    aggregator_address: Address,
}

impl OracleSubmitter {
    /// Builds a signer-backed HTTP client bound to the OracleAggregator at
    /// `aggregator_address`. The wallet derived from `signing_key` is used only
    /// to sign `submitPrice` transactions.
    pub async fn new(
        rpc_url: &str,
        signing_key: PrivateKeySigner,
        aggregator_address: Address,
    ) -> Result<Self, OnchainClientError> {
        let url = rpc_url
            .parse()
            .map_err(|_| OnchainClientError::InvalidUrl(rpc_url.to_string()))?;
        let wallet = EthereumWallet::from(signing_key);
        let provider = ProviderBuilder::new().wallet(wallet).connect_http(url).erased();
        Ok(Self {
            provider,
            aggregator_address,
        })
    }

    /// Submits a normalized 8-decimal `price` for `commodity` (the contract enum
    /// ordinal) to the OracleAggregator and returns the transaction hash. A
    /// divergence rejection or any other revert maps to
    /// [`OnchainClientError::Contract`].
    pub async fn submit_price(
        &self,
        commodity: u8,
        price: u128,
    ) -> Result<TxHash, OnchainClientError> {
        let contract = IOracleAggregator::new(self.aggregator_address, &self.provider);
        let receipt = contract
            .submitPrice(commodity, U256::from(price))
            .send()
            .await
            .map_err(|e| OnchainClientError::Contract(e.to_string()))?
            .get_receipt()
            .await
            .map_err(|e| OnchainClientError::Contract(e.to_string()))?;
        if !receipt.status() {
            return Err(OnchainClientError::Contract("transaction reverted".into()));
        }
        let tx_hash = receipt.transaction_hash;
        tracing::info!(commodity = %commodity, price = %price, tx_hash = %tx_hash, "submitted price to oracle aggregator");
        Ok(tx_hash)
    }
}

/// Maps a [`Commodity`] to the OracleAggregator Solidity enum ordinal
/// (Gold=0, Platinum=1, Silver=2, CrudeOil=3).
pub fn commodity_to_u8(commodity: &Commodity) -> u8 {
    match commodity {
        Commodity::Gold => 0,
        Commodity::Platinum => 1,
        Commodity::Silver => 2,
        Commodity::CrudeOil => 3,
    }
}

/// On-chain mint proposal state read from the MiningContract.
#[derive(Debug, Clone)]
pub struct OnchainProposal {
    /// Session this mint settles (bytes32).
    pub session_id: [u8; 32],
    /// Recipient of the minted PRM.
    pub recipient: Address,
    /// Amount of PRM to mint, in wei.
    pub amount: u128,
    /// Unix timestamp at which the proposal was created.
    pub proposed_at: u64,
    /// Number of signer approvals collected.
    pub approvals: u8,
    /// Whether the proposal has executed.
    pub executed: bool,
    /// Whether the proposal has been cancelled.
    pub cancelled: bool,
}

/// Write-capable client for driving the MiningContract multi-sig mint lifecycle
/// (propose, approve, execute, cancel). Holds a signer-backed provider; this is
/// the operator's authority and must be guarded accordingly.
pub struct MiningWriter {
    provider: DynProvider,
    mining_address: Address,
}

impl MiningWriter {
    /// Builds a signer-backed HTTP client bound to the MiningContract at
    /// `mining_address`. The wallet derived from `signing_key` signs the
    /// lifecycle transactions.
    pub async fn new(
        rpc_url: &str,
        signing_key: PrivateKeySigner,
        mining_address: Address,
    ) -> Result<Self, OnchainClientError> {
        let url = rpc_url
            .parse()
            .map_err(|_| OnchainClientError::InvalidUrl(rpc_url.to_string()))?;
        let wallet = EthereumWallet::from(signing_key);
        let provider = ProviderBuilder::new().wallet(wallet).connect_http(url).erased();
        Ok(Self {
            provider,
            mining_address,
        })
    }

    /// Proposes a mint settling `session_id` to `recipient` for `amount`.
    pub async fn propose_mint(
        &self,
        proposal_id: [u8; 32],
        session_id: [u8; 32],
        recipient: Address,
        amount: u128,
    ) -> Result<TxHash, OnchainClientError> {
        let contract = IMiningContract::new(self.mining_address, &self.provider);
        let receipt = contract
            .proposeMint(B256::from(proposal_id), B256::from(session_id), recipient, U256::from(amount))
            .send()
            .await
            .map_err(|e| OnchainClientError::Contract(e.to_string()))?
            .get_receipt()
            .await
            .map_err(|e| OnchainClientError::Contract(e.to_string()))?;
        if !receipt.status() {
            return Err(OnchainClientError::Contract("transaction reverted".into()));
        }
        Ok(receipt.transaction_hash)
    }

    /// Adds a signer approval to a pending proposal.
    pub async fn approve_mint(&self, proposal_id: [u8; 32]) -> Result<TxHash, OnchainClientError> {
        let contract = IMiningContract::new(self.mining_address, &self.provider);
        let receipt = contract
            .approveMint(B256::from(proposal_id))
            .send()
            .await
            .map_err(|e| OnchainClientError::Contract(e.to_string()))?
            .get_receipt()
            .await
            .map_err(|e| OnchainClientError::Contract(e.to_string()))?;
        if !receipt.status() {
            return Err(OnchainClientError::Contract("transaction reverted".into()));
        }
        Ok(receipt.transaction_hash)
    }

    /// Executes a fully-approved proposal after its timelock.
    pub async fn execute_mint(&self, proposal_id: [u8; 32]) -> Result<TxHash, OnchainClientError> {
        let contract = IMiningContract::new(self.mining_address, &self.provider);
        let receipt = contract
            .executeMint(B256::from(proposal_id))
            .send()
            .await
            .map_err(|e| OnchainClientError::Contract(e.to_string()))?
            .get_receipt()
            .await
            .map_err(|e| OnchainClientError::Contract(e.to_string()))?;
        if !receipt.status() {
            return Err(OnchainClientError::Contract("transaction reverted".into()));
        }
        Ok(receipt.transaction_hash)
    }

    /// Cancels a pending proposal.
    pub async fn cancel_mint(&self, proposal_id: [u8; 32]) -> Result<TxHash, OnchainClientError> {
        let contract = IMiningContract::new(self.mining_address, &self.provider);
        let receipt = contract
            .cancelMint(B256::from(proposal_id))
            .send()
            .await
            .map_err(|e| OnchainClientError::Contract(e.to_string()))?
            .get_receipt()
            .await
            .map_err(|e| OnchainClientError::Contract(e.to_string()))?;
        if !receipt.status() {
            return Err(OnchainClientError::Contract("transaction reverted".into()));
        }
        Ok(receipt.transaction_hash)
    }

    /// Reads the on-chain state of a proposal.
    pub async fn get_proposal(
        &self,
        proposal_id: [u8; 32],
    ) -> Result<OnchainProposal, OnchainClientError> {
        let contract = IMiningContract::new(self.mining_address, &self.provider);
        let ret = contract
            .proposals(B256::from(proposal_id))
            .call()
            .await
            .map_err(|e| OnchainClientError::Contract(e.to_string()))?;
        Ok(OnchainProposal {
            session_id: ret.sessionId.0,
            recipient: ret.recipient,
            amount: u128::try_from(ret.amount)
                .map_err(|_| OnchainClientError::Contract("amount overflows u128".into()))?,
            proposed_at: u64::try_from(ret.proposedAt)
                .map_err(|_| OnchainClientError::Contract("proposedAt overflows u64".into()))?,
            approvals: ret.approvals,
            executed: ret.executed,
            cancelled: ret.cancelled,
        })
    }
}

/// A wallet's active stake on one chain's StakingContract.
#[derive(Debug, Clone)]
pub struct StakeInfo {
    /// PRM staked, in wei (18 decimals). Assumed to fit in `u128`.
    pub amount: u128,
    /// Lock-period enum ordinal: 0 = 30d, 1 = 90d, 2 = 180d.
    pub lock_period: u8,
    /// Whether the stake is currently active. When false, the caller treats the
    /// stake as zero for boost purposes.
    pub active: bool,
}

/// Read-only client for a single chain's StakingContract. The combined
/// cross-chain boost is computed off-chain by the caller (Decision 4d); this
/// reader only exposes one chain's raw stake.
pub struct StakingReader {
    provider: DynProvider,
    staking_address: Address,
}

impl StakingReader {
    /// Builds a read-only HTTP client bound to the StakingContract at
    /// `staking_address`.
    pub async fn new(
        rpc_url: &str,
        staking_address: Address,
    ) -> Result<Self, OnchainClientError> {
        let url = rpc_url
            .parse()
            .map_err(|_| OnchainClientError::InvalidUrl(rpc_url.to_string()))?;
        let provider = ProviderBuilder::new().connect_http(url).erased();
        Ok(Self {
            provider,
            staking_address,
        })
    }

    /// Reads `wallet`'s stake from the StakingContract `stakes` mapping. An
    /// inactive stake is returned as-is with `active = false`.
    pub async fn read_stake(&self, wallet: Address) -> Result<StakeInfo, OnchainClientError> {
        let contract = IStakingContract::new(self.staking_address, &self.provider);
        let ret = contract
            .stakes(wallet)
            .call()
            .await
            .map_err(|e| OnchainClientError::Contract(e.to_string()))?;
        Ok(StakeInfo {
            amount: u128::try_from(ret.amount)
                .map_err(|_| OnchainClientError::Contract("stake amount overflows u128".into()))?,
            lock_period: ret.lockPeriod,
            active: ret.active,
        })
    }
}

#[cfg(test)]
mod tests {
    // Run with: cargo test -p onchain-client -- --ignored
    use super::*;
    use alloy_primitives::{Address, Signature, U256};
    use chrono::{DateTime, Utc};
    use common::{AttestationResult, Chain, Commodity, ProposalStatus, SessionId};

    const TEST_RPC: &str = "https://ethereum-rpc.publicnode.com";

    fn epoch() -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(0, 0).unwrap()
    }

    fn dummy_proposal() -> MintProposal {
        MintProposal {
            session_id: SessionId("0".to_string()),
            wallet: Address::ZERO,
            gross_prm: 0,
            net_usd_cents: None,
            commodity: Commodity::Gold,
            chain: Chain::Ethereum,
            attestation: AttestationResult {
                session_id: SessionId("0".to_string()),
                signatures: Vec::new(),
                node_ids: Vec::new(),
                signers: Vec::new(),
                proof_hash: [0u8; 32],
                timestamp: epoch(),
            },
            backend_sig: Signature::new(U256::ZERO, U256::ZERO, false),
            created_at: epoch(),
            status: ProposalStatus::Pending,
        }
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_block_number() {
        let client = OnchainClient::new(TEST_RPC, 1).await.unwrap();
        let number = client.get_block_number().await.unwrap();
        assert!(number > 0);
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_block_hash() {
        let client = OnchainClient::new(TEST_RPC, 1).await.unwrap();
        let number = client.get_block_number().await.unwrap();
        let hash = client.get_block_hash(number - 3).await.unwrap();
        assert!(hash.is_some());
    }

    #[test]
    fn test_sign_proposal() {
        let proposal = dummy_proposal();
        let signer = PrivateKeySigner::random();
        let signature = OnchainClient::sign_proposal(&proposal, &signer).unwrap();
        assert!(!signature.is_empty());
    }

    #[test]
    fn test_commodity_to_u8() {
        assert_eq!(commodity_to_u8(&Commodity::Gold), 0);
        assert_eq!(commodity_to_u8(&Commodity::Platinum), 1);
        assert_eq!(commodity_to_u8(&Commodity::Silver), 2);
        assert_eq!(commodity_to_u8(&Commodity::CrudeOil), 3);
    }

    #[tokio::test]
    #[ignore]
    async fn test_submit_price_live() {
        // Manual Anvil path: run `anvil`, deploy via contracts/script/Deploy.s.sol,
        // then set `aggregator` to the printed OracleAggregator address. Anvil
        // account 0 (key below) is the authorized submitter configured by the
        // deploy script.
        let key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let signer: PrivateKeySigner = key.parse().unwrap();
        let aggregator: Address = "0xa513E6E4b8f2a923D98304ec87F64353C4D5C853"
            .parse()
            .unwrap();
        let submitter = OracleSubmitter::new("http://localhost:8545", signer, aggregator)
            .await
            .unwrap();
        let tx_hash = submitter.submit_price(0, 320_400_000_000).await.unwrap();
        assert_ne!(tx_hash, TxHash::ZERO);
    }
}
