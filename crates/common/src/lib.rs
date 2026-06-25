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
    /// Count of recent proofs observed in this session.
    pub recent_proof_count: u32,
    /// Node assigned to this session at creation. `None` until a node is assigned.
    pub assigned_node_id: Option<NodeId>,
    /// Commodity this session mines toward.
    pub commodity: Commodity,
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
    /// Scaled integer score in basis points (0-10000). Reserved for Phase 2
    /// weighted scoring; currently always 0.
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
    /// Gross PRM as a scaled integer.
    pub gross_prm: u128,
    /// Backing commodity.
    pub commodity: Commodity,
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
    /// True when at least 2 signatures are present.
    /// Assigned node signature is always index 0.
    pub fn is_sufficient(&self) -> bool {
        self.signatures.len() >= 2
    }
}
