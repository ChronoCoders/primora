#![deny(warnings)]
#![deny(missing_docs)]
//! Read-only on-chain access and backend mint proposal signing.

use std::sync::Arc;

use alloy::primitives::Bytes;
use alloy::providers::{Provider, ProviderBuilder, RootProvider};
use alloy::rpc::types::BlockNumberOrTag;
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::SignerSync;
use alloy::transports::http::{reqwest, Http};
use alloy::transports::TransportError;
use common::MintProposal;

type HttpProvider = RootProvider<Http<reqwest::Client>>;

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
}

impl std::fmt::Display for OnchainClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "transport error: {e}"),
            Self::InvalidUrl(url) => write!(f, "invalid rpc url: {url}"),
            Self::Serialization(e) => write!(f, "serialization error: {e}"),
            Self::Signing(e) => write!(f, "signing error: {e}"),
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
    provider: Arc<HttpProvider>,
    chain_id: u64,
}

impl OnchainClient {
    /// Builds an HTTP provider for `rpc_url` bound to `chain_id`.
    pub async fn new(rpc_url: &str, chain_id: u64) -> Result<Self, OnchainClientError> {
        let url: reqwest::Url = rpc_url
            .parse()
            .map_err(|_| OnchainClientError::InvalidUrl(rpc_url.to_string()))?;
        let provider = ProviderBuilder::new().on_http(url);
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
            .get_block_by_number(BlockNumberOrTag::Number(block_number), false)
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

#[cfg(test)]
mod tests {
    // Run with: cargo test -p onchain-client -- --ignored
    use super::*;
    use alloy_primitives::{Address, Signature, U256};
    use chrono::{DateTime, Utc};
    use common::{AttestationResult, Commodity, ProposalStatus, SessionId};

    const TEST_RPC: &str = "https://ethereum-rpc.publicnode.com";

    fn epoch() -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(0, 0).unwrap()
    }

    fn dummy_proposal() -> MintProposal {
        MintProposal {
            session_id: SessionId("0".to_string()),
            wallet: Address::ZERO,
            gross_prm: 0,
            commodity: Commodity::Gold,
            attestation: AttestationResult {
                session_id: SessionId("0".to_string()),
                signatures: Vec::new(),
                node_ids: Vec::new(),
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
}
