#![deny(warnings)]
#![deny(missing_docs)]
//! Node attestation orchestration: selection, parallel requests, and result assembly.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use common::{AttestationResult, NodeId, NodeSignature, PartialProof, SessionId};
use sha2::{Digest, Sha256};

const REQUIRED_SIGNATURES: usize = 2;
const ATTESTATION_TIMEOUT_SECS: u64 = 15;

/// Abstraction over a node attestation transport. The real Tonic gRPC client
/// implements this in a later change; mocks implement it for testing.
pub trait NodeClient: Send + Sync {
    /// Requests an attestation signature from `node_id` over `proof_set`.
    fn request_attestation(
        &self,
        node_id: &NodeId,
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

/// Orchestrates 2-of-3 node attestation for a session.
pub struct NodeCoordinator<C: NodeClient> {
    client: Arc<C>,
    eligible_nodes: Vec<NodeId>,
    required_signatures: usize,
    attestation_timeout_secs: u64,
}

impl<C: NodeClient> NodeCoordinator<C> {
    /// Creates a coordinator requiring 2 signatures and a 15-second timeout.
    pub fn new(client: Arc<C>, eligible_nodes: Vec<NodeId>) -> Self {
        Self {
            client,
            eligible_nodes,
            required_signatures: REQUIRED_SIGNATURES,
            attestation_timeout_secs: ATTESTATION_TIMEOUT_SECS,
        }
    }

    /// Deterministically selects up to 2 nodes by Fisher-Yates shuffling the
    /// eligible pool with `seed` as entropy, optionally excluding one node.
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
        pool.into_iter().take(REQUIRED_SIGNATURES).collect()
    }

    async fn try_request(
        &self,
        node_id: &NodeId,
        proof_set: &[PartialProof],
        timeout: Duration,
    ) -> Option<NodeSignature> {
        match tokio::time::timeout(timeout, self.client.request_attestation(node_id, proof_set))
            .await
        {
            Ok(Ok(signature)) => Some(signature),
            _ => None,
        }
    }

    /// Collects 2-of-3 attestation for a session: selects Node B and Node C
    /// (excluding the assigned node), requests their signatures in parallel,
    /// and assembles the result around the assigned node's signature.
    pub async fn coordinate_attestation(
        &self,
        session_id: SessionId,
        assigned_node_sig: NodeSignature,
        proof_set: Vec<PartialProof>,
        seed: [u8; 32],
        assigned_node_id: &NodeId,
    ) -> Result<AttestationResult, NodeCoordinatorError> {
        let selected = self.select_nodes(seed, Some(assigned_node_id));
        let timeout = Duration::from_secs(self.attestation_timeout_secs);
        let responses: Vec<Option<NodeSignature>> = match selected.as_slice() {
            [] => Vec::new(),
            [a] => vec![self.try_request(a, &proof_set, timeout).await],
            [a, b, ..] => {
                let (ra, rb) = tokio::join!(
                    self.try_request(a, &proof_set, timeout),
                    self.try_request(b, &proof_set, timeout),
                );
                vec![ra, rb]
            }
        };

        let mut signatures = vec![assigned_node_sig];
        let mut node_ids = vec![assigned_node_id.clone()];
        for (node_id, response) in selected.iter().zip(responses) {
            if let Some(signature) = response {
                signatures.push(signature);
                node_ids.push(node_id.clone());
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
    }

    impl NodeClient for MockNodeClient {
        fn request_attestation(
            &self,
            node_id: &NodeId,
            _proof_set: &[PartialProof],
        ) -> impl Future<Output = Result<NodeSignature, NodeCoordinatorError>> + Send {
            let result = match self {
                MockNodeClient::AlwaysSucceed => Ok(dummy_sig(&node_id.0)),
                MockNodeClient::AlwaysFail => {
                    Err(NodeCoordinatorError::NodeError("mock failure".to_string()))
                }
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
        NodeCoordinator::new(Arc::new(variant), nodes(eligible))
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

    #[tokio::test]
    async fn test_coordinate_attestation_success() {
        let coord = coordinator(MockNodeClient::AlwaysSucceed, &["n1", "n2", "n3"]);
        let assigned = NodeId("n0".to_string());
        let result = coord
            .coordinate_attestation(
                SessionId("s".to_string()),
                dummy_sig("n0"),
                Vec::new(),
                [7u8; 32],
                &assigned,
            )
            .await;
        assert!(result.is_ok());
        let attestation = result.unwrap();
        assert!(attestation.is_sufficient());
    }

    #[tokio::test]
    async fn test_coordinate_attestation_insufficient() {
        let coord = coordinator(MockNodeClient::AlwaysFail, &["n0", "n1"]);
        let assigned = NodeId("n0".to_string());
        let result = coord
            .coordinate_attestation(
                SessionId("s".to_string()),
                dummy_sig("n0"),
                Vec::new(),
                [3u8; 32],
                &assigned,
            )
            .await;
        assert!(matches!(
            result,
            Err(NodeCoordinatorError::InsufficientAttestations { got: 1, required: 2 })
        ));
    }
}
