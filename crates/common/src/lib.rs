#![deny(warnings)]
#![deny(missing_docs)]
//! Common types shared across all Primora backend crates.

use std::net::IpAddr;

use alloy_primitives::{Address, Signature};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Validation depth requested for a proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValidationMode {
    /// Backend fast non-crypto checks.
    PreFilter,
    /// Node RandomX validate and sign.
    Full,
}

/// Outcome of validating a partial proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValidationResult {
    /// Proof accepted.
    Valid,
    /// Proof rejected for the given reason.
    Invalid(InvalidReason),
    /// Proof accepted but flagged at the given suspicion level.
    Suspicious(SuspicionLevel),
}

/// Reason a proof was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InvalidReason {
    /// Proof structure could not be parsed.
    MalformedProof,
    /// Commit-reveal mismatch at session end.
    HashMismatch,
    /// Proof interval < 2s or > 5min.
    TimingAnomaly,
    /// Reported rate exceeds physical max for client type.
    HashrateImpossible,
    /// Same wallet active in concurrent session.
    DuplicateSession,
    /// Total proofs < 50% of expected count.
    ProofDeficit,
    /// Timestamp outside acceptable window.
    StaleProof,
    /// Full mode only: RandomX hash verify failed.
    InvalidSignature,
    /// Escape hatch for future trigger types.
    Other(String),
}

/// Severity of a flagged-but-not-rejected proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SuspicionLevel {
    /// Low severity.
    Low,
    /// Medium severity.
    Medium,
    /// High severity.
    High,
}

/// Client software submitting proofs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientType {
    /// Browser-based client.
    Browser,
    /// Desktop application client.
    Desktop,
    /// Command-line client.
    Cli,
}

/// Phase 1 commodity backing a mint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Commodity {
    /// Gold (XAU).
    Gold,
    /// Platinum (XPT).
    Platinum,
    /// Silver (XAG).
    Silver,
    /// Crude oil (WTI).
    CrudeOil,
}

/// The blockchains Primora deploys to. The same contract suite runs
/// independently on each; see Spec Decision #4 (dual-chain).
///
/// The enum is mainnet-semantic. For local Anvil testing, chain id 31337 maps to
/// whichever chain the local deployment stands in for (tests configure this
/// explicitly via config); there is deliberately no Anvil variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Chain {
    /// Ethereum mainnet -- long-term investors, canonical chain for oracle
    /// reads and node-selection seed.
    Ethereum,
    /// Polygon mainnet -- daily traders, low gas.
    Polygon,
}

impl Chain {
    /// The EVM chain id for this chain.
    pub fn chain_id(&self) -> u64 {
        match self {
            Chain::Ethereum => 1,
            Chain::Polygon => 137,
        }
    }

    /// Parses from an EVM chain id, returning `None` for unknown ids.
    pub fn from_chain_id(id: u64) -> Option<Chain> {
        match id {
            1 => Some(Chain::Ethereum),
            137 => Some(Chain::Polygon),
            _ => None,
        }
    }

    /// Lowercase string identifier used in API requests and storage.
    pub fn as_str(&self) -> &'static str {
        match self {
            Chain::Ethereum => "ethereum",
            Chain::Polygon => "polygon",
        }
    }

    /// Parses from the lowercase string identifier.
    pub fn from_str_id(s: &str) -> Option<Chain> {
        match s {
            "ethereum" => Some(Chain::Ethereum),
            "polygon" => Some(Chain::Polygon),
            _ => None,
        }
    }

    /// True if this is the canonical chain (Ethereum) used for oracle reads and
    /// the node-selection seed per Decisions #15 and #16.
    pub fn is_canonical(&self) -> bool {
        matches!(self, Chain::Ethereum)
    }

    /// All supported chains.
    pub fn all() -> [Chain; 2] {
        [Chain::Ethereum, Chain::Polygon]
    }
}

impl std::fmt::Display for Chain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Geographic site of a node, resolved from backend config (`NODE_SITES`).
/// Purely geographic: code, city, country only -- never a cloud/infrastructure
/// provider name. Not derived from the on-chain NodeRegistry, which carries no
/// geography.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSite {
    /// Short site code (e.g. `JHB`).
    pub code: String,
    /// City name (e.g. `Johannesburg`).
    pub city: String,
    /// ISO country code (e.g. `ZA`).
    pub country: String,
}

/// Session identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

/// Node identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub String);

/// Per-session validation context. Stateless validators read all session
/// state from here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionContext {
    /// Miner wallet.
    pub wallet: Address,
    /// Source IP, if known.
    pub ip: Option<IpAddr>,
    /// Client software type.
    pub client_type: ClientType,
    /// Count of concurrently active sessions for this wallet.
    pub active_sessions_count: u32,
    /// UTC timestamp when the session was created.
    pub started_at: DateTime<Utc>,
    /// Timestamp of the last submission, if any.
    pub last_submission_at: Option<DateTime<Utc>>,
    /// Node assigned to this session at creation. `None` until a node is assigned.
    pub assigned_node_id: Option<NodeId>,
    /// Commodity this session mines toward.
    pub commodity: Commodity,
    /// The chain this session mints to, chosen at creation (Decision 4c).
    pub target_chain: Chain,
    /// CPU worker threads reported by the client at session start. 0 if not
    /// reported (older clients and stored sessions predating this field).
    #[serde(default)]
    pub cpu_threads: u32,
    /// Resolved site of the assigned node, if known (from `NODE_SITES` config).
    #[serde(default)]
    pub assigned_site: Option<NodeSite>,
}

/// A partial proof submitted every 30s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialProof {
    /// Session this proof belongs to.
    pub session_id: SessionId,
    /// Miner wallet.
    pub wallet: Address,
    /// Proof index within the session.
    pub sequence: u32,
    /// Reported hashrate in H/s.
    pub hashrate: u64,
    /// Proof hash bytes: the RandomX hash of `proof_input`.
    pub proof_hash: [u8; 32],
    /// Exact RandomX preimage the client hashed. Empty when not yet captured.
    pub proof_input: Vec<u8>,
    /// Difficulty target this proof claims.
    pub difficulty: u64,
    /// Submission timestamp.
    pub submitted_at: DateTime<Utc>,
    /// Node signature over the proof, present only after Full-mode signing.
    pub signature: Option<Signature>,
}

/// A node's signature over an attested proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSignature {
    /// Signing node.
    pub node_id: NodeId,
    /// Signature produced by the node.
    pub signature: Signature,
    /// Signing timestamp.
    pub signed_at: DateTime<Utc>,
}

/// A persisted anomaly detection event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnomalyEvent {
    /// Session that produced the anomaly.
    pub session_id: SessionId,
    /// Miner wallet.
    pub wallet: Address,
    /// Anomaly score in basis points (0-10000), linear in the count of distinct
    /// anomaly signals (`anomaly_engine::anomaly_score_bps`). 0 means no signals.
    pub score: u32,
    /// Triggers that produced this event.
    pub triggers: Vec<InvalidReason>,
    /// Aggregate suspicion level.
    pub level: SuspicionLevel,
    /// Event timestamp.
    pub timestamp: DateTime<Utc>,
}

/// Multi-node attestation over a proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttestationResult {
    /// Attested session.
    pub session_id: SessionId,
    /// Node signatures; index 0 is the assigned node.
    pub signatures: Vec<NodeSignature>,
    /// Node identifiers parallel to `signatures`.
    pub node_ids: Vec<NodeId>,
    /// Recovered, registered signer addresses parallel to `signatures` (the
    /// distinct verified identities counted toward quorum).
    #[serde(default)]
    pub signers: Vec<Address>,
    /// Attested proof hash.
    pub proof_hash: [u8; 32],
    /// Attestation timestamp.
    pub timestamp: DateTime<Utc>,
}

/// Lifecycle state of a mint proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProposalStatus {
    /// Awaiting multi-sig approval.
    Pending,
    /// Approved by multi-sig.
    ApprovedByMultiSig,
    /// Submitted on-chain.
    Submitted,
    /// Confirmed on-chain.
    Confirmed,
    /// Rejected.
    Rejected,
}

/// A proposal to mint PRM for a validated session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MintProposal {
    /// Source session.
    pub session_id: SessionId,
    /// Recipient wallet.
    pub wallet: Address,
    /// Minted PRM in ERC-20 base units (18 decimals). Human PRM = `gross_prm / 10^18`.
    pub gross_prm: u128,
    /// Net payout in USD cents (redemption minus house edge, Spec 4.6). `None`
    /// for proposals created before USD figures were persisted (Spec 4.8 wiring).
    pub net_usd_cents: Option<i64>,
    /// Backing commodity.
    pub commodity: Commodity,
    /// The chain this proposal mints to (Decision 4c).
    pub chain: Chain,
    /// Supporting attestation.
    pub attestation: AttestationResult,
    /// Backend signature over the proposal.
    pub backend_sig: Signature,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Current status.
    pub status: ProposalStatus,
}

/// Validates a partial proof against the provided context.
///
/// Implementations must be stateless. All session state arrives via `ctx`.
pub trait ProofValidator {
    /// Validates `proof` at the requested `mode` using `ctx`.
    fn validate(
        &self,
        proof: &PartialProof,
        mode: ValidationMode,
        ctx: &SessionContext,
    ) -> ValidationResult;
}

impl ValidationResult {
    /// Returns true only when SuspicionLevel is High.
    /// Low and Medium do not trigger slash votes.
    pub fn should_trigger_slash_vote(&self) -> bool {
        matches!(self, Self::Suspicious(SuspicionLevel::High))
    }
}

impl AttestationResult {
    /// True when at least 3 signatures are present (the 3-of-4 BFT quorum).
    /// Assigned node signature is always index 0. Distinct-verified-signer
    /// counting lands with signer-identity verification (attestation fix #4).
    pub fn is_sufficient(&self) -> bool {
        self.signatures.len() >= 3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chain_id_roundtrip() {
        for chain in Chain::all() {
            assert_eq!(Chain::from_chain_id(chain.chain_id()), Some(chain));
        }
    }

    #[test]
    fn test_chain_str_roundtrip() {
        for chain in Chain::all() {
            assert_eq!(Chain::from_str_id(chain.as_str()), Some(chain));
        }
    }

    #[test]
    fn test_unknown_chain_id() {
        assert_eq!(Chain::from_chain_id(999), None);
    }

    #[test]
    fn test_canonical() {
        assert!(Chain::Ethereum.is_canonical());
        assert!(!Chain::Polygon.is_canonical());
    }

    #[test]
    fn test_serde_lowercase() {
        assert_eq!(
            serde_json::to_string(&Chain::Ethereum).unwrap(),
            "\"ethereum\""
        );
        assert_eq!(
            serde_json::to_string(&Chain::Polygon).unwrap(),
            "\"polygon\""
        );
        assert_eq!(
            serde_json::from_str::<Chain>("\"polygon\"").unwrap(),
            Chain::Polygon
        );
    }
}
