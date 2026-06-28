#![deny(warnings)]
#![deny(missing_docs)]
//! Node attestation orchestration: selection, parallel requests, and result assembly.

pub mod grpc;

pub use grpc::GrpcNodeClient;

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use common::{AttestationResult, NodeId, NodeSignature, PartialProof, SessionId};
use sha2::{Digest, Sha256};

const REQUIRED_SIGNATURES: usize = 3;
const ATTESTATION_TIMEOUT_SECS: u64 = 15;

/// Total nodes in a 3-of-4 attestation set: the assigned node plus
/// `ATTESTATION_NODE_COUNT - 1` selected others. BFT f=1 (N=3f+1=4, quorum=2f+1=3):
/// the pool must hold at least this many nodes to reach quorum.
pub const ATTESTATION_NODE_COUNT: usize = 4;

/// Abstraction over a node attestation transport. The real Tonic gRPC client
/// implements this in a later change; mocks implement it for testing.
pub trait NodeClient: Send + Sync {
    /// Requests an attestation signature from `target_node_id` over `proof_set`,
    /// identifying `assigned_node_id` as the node that produced the assigned
    /// signature this attestation is built around.
    fn request_attestation(
        &self,
        target_node_id: &NodeId,
        assigned_node_id: &NodeId,
        proof_set: &[PartialProof],
    ) -> impl Future<Output = Result<NodeSignature, NodeCoordinatorError>> + Send;
}

/// Errors produced while coordinating attestations.
#[derive(Debug)]
pub enum NodeCoordinatorError {
    /// Fewer signatures were collected than required.
    InsufficientAttestations {
        /// Signatures collected, including the assigned node.
        got: usize,
        /// Signatures required for a valid attestation.
        required: usize,
    },
    /// A node did not respond within the timeout.
    AttestationTimeout {
        /// Node that timed out.
        node_id: NodeId,
    },
    /// An on-chain read failed.
    OnchainError(String),
    /// A node returned an error.
    NodeError(String),
}

impl std::fmt::Display for NodeCoordinatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InsufficientAttestations { got, required } => {
                write!(f, "insufficient attestations: got {got}, required {required}")
            }
            Self::AttestationTimeout { node_id } => {
                write!(f, "attestation timeout from node {}", node_id.0)
            }
            Self::OnchainError(msg) => write!(f, "onchain error: {msg}"),
            Self::NodeError(msg) => write!(f, "node error: {msg}"),
        }
    }
}

impl std::error::Error for NodeCoordinatorError {}

fn hash_proofs(proof_set: &[PartialProof]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for proof in proof_set {
        hasher.update(proof.proof_hash);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(hasher.finalize().as_ref());
    out
}

/// Orchestrates 3-of-4 node attestation for a session (BFT f=1).
pub struct NodeCoordinator<C: NodeClient> {
    clients: HashMap<NodeId, Arc<C>>,
    eligible_nodes: Vec<NodeId>,
    required_signatures: usize,
    attestation_timeout_secs: u64,
}

impl<C: NodeClient> NodeCoordinator<C> {
    /// Creates a coordinator requiring 3 signatures and a 15-second timeout.
    ///
    /// `clients` maps each node id to the client bound to that node's endpoint, so
    /// a request for a given node id reaches that distinct physical node.
    /// `eligible_nodes` is the ordered selection pool (kept separate from the map
    /// so node selection stays deterministic under the seed).
    pub fn new(clients: HashMap<NodeId, Arc<C>>, eligible_nodes: Vec<NodeId>) -> Self {
        Self {
            clients,
            eligible_nodes,
            required_signatures: REQUIRED_SIGNATURES,
            attestation_timeout_secs: ATTESTATION_TIMEOUT_SECS,
        }
    }

    /// Deterministically selects up to `ATTESTATION_NODE_COUNT - 1` other nodes by
    /// Fisher-Yates shuffling the eligible pool with `seed` as entropy, optionally
    /// excluding one node (the assigned node, which is the fourth attester).
    pub fn select_nodes(&self, seed: [u8; 32], exclude: Option<&NodeId>) -> Vec<NodeId> {
        let mut pool: Vec<NodeId> = self
            .eligible_nodes
            .iter()
            .filter(|node| exclude != Some(*node))
            .cloned()
            .collect();
        let len = pool.len();
        for i in (1..len).rev() {
            let entropy = u64::from(seed[i % 32]);
            let j = (entropy % (i as u64 + 1)) as usize;
            pool.swap(i, j);
        }
        pool.into_iter().take(ATTESTATION_NODE_COUNT - 1).collect()
    }

    async fn try_request(
        &self,
        target_node_id: &NodeId,
        assigned_node_id: &NodeId,
        proof_set: &[PartialProof],
        timeout: Duration,
    ) -> Option<NodeSignature> {
        let Some(client) = self.clients.get(target_node_id) else {
            tracing::warn!(
                node_id = %target_node_id.0,
                "no client configured for selected node; skipping (no fallback)"
            );
            return None;
        };
        match tokio::time::timeout(
            timeout,
            client.request_attestation(target_node_id, assigned_node_id, proof_set),
        )
        .await
        {
            Ok(Ok(signature)) => Some(signature),
            _ => None,
        }
    }

    /// Collects 3-of-4 attestation for a session: the attesting set is the
    /// assigned node plus up to 3 others selected (excluding the assigned). Every
    /// node, including the assigned one, is requested for a genuine signature via
    /// its own client; a node with no configured client or no response is skipped
    /// (never rerouted, never a placeholder). Succeeds only when at least
    /// `required_signatures` (3) genuine signatures are collected.
    pub async fn coordinate_attestation(
        &self,
        session_id: SessionId,
        proof_set: Vec<PartialProof>,
        seed: [u8; 32],
        assigned_node_id: &NodeId,
    ) -> Result<AttestationResult, NodeCoordinatorError> {
        let mut targets = vec![assigned_node_id.clone()];
        targets.extend(self.select_nodes(seed, Some(assigned_node_id)));
        let timeout = Duration::from_secs(self.attestation_timeout_secs);

        let mut signatures = Vec::with_capacity(targets.len());
        let mut node_ids = Vec::with_capacity(targets.len());
        for target in &targets {
            if let Some(signature) = self
                .try_request(target, assigned_node_id, &proof_set, timeout)
                .await
            {
                signatures.push(signature);
                node_ids.push(target.clone());
            }
        }

        let result = AttestationResult {
            session_id,
            signatures,
            node_ids,
            proof_hash: hash_proofs(&proof_set),
            timestamp: Utc::now(),
        };

        if result.is_sufficient() {
            Ok(result)
        } else {
            Err(NodeCoordinatorError::InsufficientAttestations {
                got: result.signatures.len(),
                required: self.required_signatures,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Signature, U256};

    enum MockNodeClient {
        AlwaysSucceed,
        AlwaysFail,
        Tagged(&'static str),
    }

    impl NodeClient for MockNodeClient {
        fn request_attestation(
            &self,
            target_node_id: &NodeId,
            _assigned_node_id: &NodeId,
            _proof_set: &[PartialProof],
        ) -> impl Future<Output = Result<NodeSignature, NodeCoordinatorError>> + Send {
            let result = match self {
                MockNodeClient::AlwaysSucceed => Ok(dummy_sig(&target_node_id.0)),
                MockNodeClient::AlwaysFail => {
                    Err(NodeCoordinatorError::NodeError("mock failure".to_string()))
                }
                MockNodeClient::Tagged(tag) => Ok(dummy_sig(tag)),
            };
            async move { result }
        }
    }

    fn dummy_sig(id: &str) -> NodeSignature {
        NodeSignature {
            node_id: NodeId(id.to_string()),
            signature: Signature::new(U256::ZERO, U256::ZERO, false),
            signed_at: Utc::now(),
        }
    }

    fn nodes(ids: &[&str]) -> Vec<NodeId> {
        ids.iter().map(|id| NodeId(id.to_string())).collect()
    }

    fn coordinator(variant: MockNodeClient, eligible: &[&str]) -> NodeCoordinator<MockNodeClient> {
        let client = Arc::new(variant);
        let eligible_nodes = nodes(eligible);
        let clients: HashMap<NodeId, Arc<MockNodeClient>> = eligible_nodes
            .iter()
            .cloned()
            .map(|id| (id, Arc::clone(&client)))
            .collect();
        NodeCoordinator::new(clients, eligible_nodes)
    }

    #[test]
    fn test_select_nodes_deterministic() {
        let coord = coordinator(MockNodeClient::AlwaysSucceed, &["n0", "n1", "n2", "n3", "n4"]);
        let first = coord.select_nodes([42u8; 32], None);
        let second = coord.select_nodes([42u8; 32], None);
        assert_eq!(first, second);
    }

    #[test]
    fn test_select_nodes_excludes_assigned() {
        let coord = coordinator(MockNodeClient::AlwaysSucceed, &["n0", "n1", "n2", "n3", "n4"]);
        let excluded = NodeId("n2".to_string());
        let result = coord.select_nodes([5u8; 32], Some(&excluded));
        assert!(!result.contains(&excluded));
    }

    #[test]
    fn test_select_nodes_different_seeds() {
        let coord = coordinator(MockNodeClient::AlwaysSucceed, &["n0", "n1", "n2", "n3", "n4"]);
        let first = coord.select_nodes([0u8; 32], None);
        let second = coord.select_nodes([1u8; 32], None);
        assert_ne!(first, second);
    }

    fn tagged_map(ids: &[&'static str]) -> HashMap<NodeId, Arc<MockNodeClient>> {
        ids.iter()
            .map(|id| {
                (
                    NodeId(id.to_string()),
                    Arc::new(MockNodeClient::Tagged(id)),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn test_assigned_node_is_a_real_signer() {
        // Assigned node has its own client and signs alongside 3 others (4 total).
        let coord = NodeCoordinator::new(tagged_map(&["n0", "n1", "n2", "n3"]), nodes(&["n1", "n2", "n3"]));
        let assigned = NodeId("n0".to_string());
        let result = coord
            .coordinate_attestation(SessionId("s".to_string()), Vec::new(), [7u8; 32], &assigned)
            .await
            .unwrap();
        assert!(result.is_sufficient());
        assert_eq!(result.node_ids.len(), 4);
        assert!(result.node_ids.contains(&assigned));
    }

    #[tokio::test]
    async fn test_quorum_met_with_one_other_failing() {
        // Assigned + 2 others sign; the 4th (n3) has no client and is skipped -> 3.
        let coord =
            NodeCoordinator::new(tagged_map(&["n0", "n1", "n2"]), nodes(&["n1", "n2", "n3"]));
        let assigned = NodeId("n0".to_string());
        let result = coord
            .coordinate_attestation(SessionId("s".to_string()), Vec::new(), [7u8; 32], &assigned)
            .await
            .unwrap();
        assert!(result.is_sufficient());
        assert_eq!(result.signatures.len(), 3);
        assert!(result.node_ids.contains(&assigned));
        assert!(!result.node_ids.contains(&NodeId("n3".to_string())));
    }

    #[tokio::test]
    async fn test_assigned_unreachable_is_not_placeholdered() {
        // Assigned has no client: it must be absent from the result, never a
        // zero-placeholder slot. The 3 reachable others still reach quorum.
        let coord =
            NodeCoordinator::new(tagged_map(&["n1", "n2", "n3"]), nodes(&["n1", "n2", "n3"]));
        let assigned = NodeId("n0".to_string());
        let result = coord
            .coordinate_attestation(SessionId("s".to_string()), Vec::new(), [7u8; 32], &assigned)
            .await
            .unwrap();
        assert_eq!(result.signatures.len(), 3);
        assert!(!result.node_ids.contains(&assigned));
        assert!(result.signatures.iter().all(|s| s.node_id != assigned));
    }

    #[tokio::test]
    async fn test_routes_to_distinct_clients() {
        let coord = NodeCoordinator::new(tagged_map(&["assigned", "nA", "nB"]), nodes(&["nA", "nB"]));
        let assigned = NodeId("assigned".to_string());
        let result = coord
            .coordinate_attestation(SessionId("s".to_string()), Vec::new(), [9u8; 32], &assigned)
            .await
            .unwrap();
        assert_eq!(result.node_ids.len(), 3);
        for (node_id, sig) in result.node_ids.iter().zip(result.signatures.iter()) {
            assert_eq!(sig.node_id.0, node_id.0);
        }
    }

    #[tokio::test]
    async fn test_missing_client_skipped_no_fallback() {
        // nC has no client; it is skipped (not rerouted). Assigned + nA + nB = 3.
        let coord =
            NodeCoordinator::new(tagged_map(&["assigned", "nA", "nB"]), nodes(&["nA", "nB", "nC"]));
        let assigned = NodeId("assigned".to_string());
        let result = coord
            .coordinate_attestation(SessionId("s".to_string()), Vec::new(), [9u8; 32], &assigned)
            .await
            .unwrap();
        assert_eq!(result.signatures.len(), 3);
        assert!(!result.node_ids.contains(&NodeId("nC".to_string())));
        for tag in ["assigned", "nA", "nB"] {
            assert_eq!(result.signatures.iter().filter(|s| s.node_id.0 == tag).count(), 1);
        }
    }

    #[tokio::test]
    async fn test_two_signatures_insufficient_for_quorum() {
        let coord = NodeCoordinator::new(tagged_map(&["assigned", "nA"]), nodes(&["nA"]));
        let assigned = NodeId("assigned".to_string());
        let result = coord
            .coordinate_attestation(SessionId("s".to_string()), Vec::new(), [9u8; 32], &assigned)
            .await;
        assert!(matches!(
            result,
            Err(NodeCoordinatorError::InsufficientAttestations { got: 2, required: 3 })
        ));
    }

    #[tokio::test]
    async fn test_coordinate_attestation_insufficient() {
        let coord = coordinator(MockNodeClient::AlwaysFail, &["n0", "n1"]);
        let assigned = NodeId("n0".to_string());
        let result = coord
            .coordinate_attestation(SessionId("s".to_string()), Vec::new(), [3u8; 32], &assigned)
            .await;
        assert!(matches!(
            result,
            Err(NodeCoordinatorError::InsufficientAttestations { got: 0, required: 3 })
        ));
    }
}
