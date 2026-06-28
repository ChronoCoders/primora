#![deny(warnings)]
#![deny(missing_docs)]
//! Node attestation orchestration: selection, parallel requests, and result assembly.

pub mod grpc;

pub use grpc::GrpcNodeClient;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::Address;
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
    signers: HashMap<NodeId, Address>,
    required_signatures: usize,
    attestation_timeout_secs: u64,
}

impl<C: NodeClient> NodeCoordinator<C> {
    /// Creates a coordinator requiring 3 signatures and a 15-second timeout.
    ///
    /// `clients` maps each node id to the client bound to that node's endpoint, so
    /// a request for a given node id reaches that distinct physical node.
    /// `eligible_nodes` is the ordered selection pool (kept separate from the map
    /// so node selection stays deterministic under the seed). `signers` maps each
    /// node id to its registered signing address; a returned signature is counted
    /// only when it recovers to the expected address for the node that produced it.
    pub fn new(
        clients: HashMap<NodeId, Arc<C>>,
        eligible_nodes: Vec<NodeId>,
        signers: HashMap<NodeId, Address>,
    ) -> Self {
        Self {
            clients,
            eligible_nodes,
            signers,
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
    /// its own client. Each returned signature is verified by recovering its
    /// signer over the exact message the node signed -- the representative
    /// (first) proof's `proof_hash` -- and counting it only when the recovered
    /// address equals that node's registered signing address and has not already
    /// been counted. A node with no client, no response, an unregistered signer,
    /// a key mismatch, or a duplicate signer is not counted (never rerouted, never
    /// a placeholder). Succeeds only when at least `required_signatures` (3)
    /// distinct, verified, registered signers are collected.
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
        let signed_message = proof_set.first().map(|proof| proof.proof_hash);

        let mut signatures = Vec::with_capacity(targets.len());
        let mut node_ids = Vec::with_capacity(targets.len());
        let mut signers = Vec::with_capacity(targets.len());
        let mut seen: HashSet<Address> = HashSet::new();
        for target in &targets {
            let Some(node_sig) = self
                .try_request(target, assigned_node_id, &proof_set, timeout)
                .await
            else {
                continue;
            };
            let Some(message) = signed_message else {
                tracing::warn!(node_id = %target.0, "no proof to verify attestation signer against");
                continue;
            };
            let Some(expected) = self.signers.get(target) else {
                tracing::warn!(node_id = %target.0, "no registered signing address for node; rejecting signature");
                continue;
            };
            let recovered = match node_sig.signature.recover_address_from_msg(message) {
                Ok(address) => address,
                Err(e) => {
                    tracing::warn!(error = %e, node_id = %target.0, "failed to recover attestation signer");
                    continue;
                }
            };
            if recovered != *expected {
                tracing::warn!(node_id = %target.0, "attestation signer does not match registered address; rejecting");
                continue;
            }
            if !seen.insert(recovered) {
                tracing::warn!(node_id = %target.0, "duplicate attestation signer; counted once");
                continue;
            }
            signatures.push(node_sig);
            node_ids.push(target.clone());
            signers.push(recovered);
        }

        let result = AttestationResult {
            session_id,
            signatures,
            node_ids,
            signers,
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
    use alloy::signers::local::PrivateKeySigner;
    use alloy::signers::SignerSync;

    const PROOF_HASH: [u8; 32] = [7u8; 32];

    enum MockNodeClient {
        Fail,
        Sign(PrivateKeySigner),
        SignMessage(PrivateKeySigner, [u8; 32]),
    }

    impl NodeClient for MockNodeClient {
        fn request_attestation(
            &self,
            target_node_id: &NodeId,
            _assigned_node_id: &NodeId,
            proof_set: &[PartialProof],
        ) -> impl Future<Output = Result<NodeSignature, NodeCoordinatorError>> + Send {
            let message = proof_set.first().map(|p| p.proof_hash).unwrap_or([0u8; 32]);
            let result = match self {
                MockNodeClient::Fail => {
                    Err(NodeCoordinatorError::NodeError("mock failure".to_string()))
                }
                MockNodeClient::Sign(signer) => Ok(NodeSignature {
                    node_id: target_node_id.clone(),
                    signature: signer.sign_message_sync(&message).expect("sign"),
                    signed_at: Utc::now(),
                }),
                MockNodeClient::SignMessage(signer, msg) => Ok(NodeSignature {
                    node_id: target_node_id.clone(),
                    signature: signer.sign_message_sync(msg).expect("sign"),
                    signed_at: Utc::now(),
                }),
            };
            async move { result }
        }
    }

    fn nodes(ids: &[&str]) -> Vec<NodeId> {
        ids.iter().map(|id| NodeId(id.to_string())).collect()
    }

    fn proofs() -> Vec<PartialProof> {
        vec![PartialProof {
            session_id: SessionId("s".to_string()),
            wallet: Address::ZERO,
            sequence: 0,
            hashrate: 0,
            proof_hash: PROOF_HASH,
            proof_input: Vec::new(),
            difficulty: 1,
            submitted_at: Utc::now(),
            signature: None,
        }]
    }

    fn coordinator(variant: MockNodeClient, eligible: &[&str]) -> NodeCoordinator<MockNodeClient> {
        let client = Arc::new(variant);
        let eligible_nodes = nodes(eligible);
        let clients: HashMap<NodeId, Arc<MockNodeClient>> = eligible_nodes
            .iter()
            .cloned()
            .map(|id| (id, Arc::clone(&client)))
            .collect();
        NodeCoordinator::new(clients, eligible_nodes, HashMap::new())
    }

    #[test]
    fn test_select_nodes_deterministic() {
        let coord = coordinator(MockNodeClient::Fail, &["n0", "n1", "n2", "n3", "n4"]);
        let first = coord.select_nodes([42u8; 32], None);
        let second = coord.select_nodes([42u8; 32], None);
        assert_eq!(first, second);
    }

    #[test]
    fn test_select_nodes_excludes_assigned() {
        let coord = coordinator(MockNodeClient::Fail, &["n0", "n1", "n2", "n3", "n4"]);
        let excluded = NodeId("n2".to_string());
        let result = coord.select_nodes([5u8; 32], Some(&excluded));
        assert!(!result.contains(&excluded));
    }

    #[test]
    fn test_select_nodes_different_seeds() {
        let coord = coordinator(MockNodeClient::Fail, &["n0", "n1", "n2", "n3", "n4"]);
        let first = coord.select_nodes([0u8; 32], None);
        let second = coord.select_nodes([1u8; 32], None);
        assert_ne!(first, second);
    }

    /// Builds a coordinator where each `(id, registered)` node signs the proof
    /// hash with its own fresh key; `registered` nodes have their address in the
    /// signer set. `assigned` is one node id; the rest are the eligible others.
    fn signing_coordinator(
        ids: &[(&str, bool)],
        assigned: &str,
    ) -> NodeCoordinator<MockNodeClient> {
        let mut clients = HashMap::new();
        let mut signers = HashMap::new();
        let mut eligible = Vec::new();
        for (id, registered) in ids {
            let signer = PrivateKeySigner::random();
            if *registered {
                signers.insert(NodeId(id.to_string()), signer.address());
            }
            clients.insert(NodeId(id.to_string()), Arc::new(MockNodeClient::Sign(signer)));
            if *id != assigned {
                eligible.push(NodeId(id.to_string()));
            }
        }
        NodeCoordinator::new(clients, eligible, signers)
    }

    #[tokio::test]
    async fn test_three_distinct_registered_signers_meet_quorum() {
        let coord =
            signing_coordinator(&[("n0", true), ("n1", true), ("n2", true), ("n3", true)], "n0");
        let result = coord
            .coordinate_attestation(SessionId("s".to_string()), proofs(), [7u8; 32], &NodeId("n0".to_string()))
            .await
            .unwrap();
        assert!(result.is_sufficient());
        assert_eq!(result.signatures.len(), 4);
        let distinct: HashSet<&Address> = result.signers.iter().collect();
        assert_eq!(distinct.len(), 4);
    }

    #[tokio::test]
    async fn test_unregistered_signer_rejected_but_quorum_met_by_three() {
        // n3 signs but is NOT registered -> rejected; n0,n1,n2 registered -> 3.
        let coord =
            signing_coordinator(&[("n0", true), ("n1", true), ("n2", true), ("n3", false)], "n0");
        let result = coord
            .coordinate_attestation(SessionId("s".to_string()), proofs(), [7u8; 32], &NodeId("n0".to_string()))
            .await
            .unwrap();
        assert_eq!(result.signers.len(), 3);
        assert!(!result.node_ids.contains(&NodeId("n3".to_string())));
    }

    #[tokio::test]
    async fn test_quorum_fails_with_too_few_registered() {
        // Only n0 and n1 registered -> 2 verified < 3.
        let coord =
            signing_coordinator(&[("n0", true), ("n1", true), ("n2", false), ("n3", false)], "n0");
        let result = coord
            .coordinate_attestation(SessionId("s".to_string()), proofs(), [7u8; 32], &NodeId("n0".to_string()))
            .await;
        assert!(matches!(
            result,
            Err(NodeCoordinatorError::InsufficientAttestations { got: 2, required: 3 })
        ));
    }

    #[tokio::test]
    async fn test_duplicate_signer_counted_once() {
        // n0 and n2 share the SAME key/address (both registered to it). The shared
        // address counts once: distinct signers = {n0/n2 addr, n1 addr} = 2 < 3.
        let shared = PrivateKeySigner::random();
        let n1 = PrivateKeySigner::random();
        let mut clients = HashMap::new();
        let mut signers = HashMap::new();
        clients.insert(NodeId("n0".to_string()), Arc::new(MockNodeClient::Sign(shared.clone())));
        clients.insert(NodeId("n1".to_string()), Arc::new(MockNodeClient::Sign(n1.clone())));
        clients.insert(NodeId("n2".to_string()), Arc::new(MockNodeClient::Sign(shared.clone())));
        signers.insert(NodeId("n0".to_string()), shared.address());
        signers.insert(NodeId("n1".to_string()), n1.address());
        signers.insert(NodeId("n2".to_string()), shared.address());
        let coord = NodeCoordinator::new(clients, nodes(&["n1", "n2"]), signers);
        let result = coord
            .coordinate_attestation(SessionId("s".to_string()), proofs(), [7u8; 32], &NodeId("n0".to_string()))
            .await;
        assert!(matches!(
            result,
            Err(NodeCoordinatorError::InsufficientAttestations { got: 2, required: 3 })
        ));
    }

    #[tokio::test]
    async fn test_key_mismatch_rejected() {
        // n0 signs with its key but is registered to a DIFFERENT address -> rejected.
        let n0_key = PrivateKeySigner::random();
        let wrong_addr = PrivateKeySigner::random().address();
        let mut coord = signing_coordinator(&[("n1", true), ("n2", true), ("n3", true)], "n0");
        // Insert n0's client (signs with n0_key) but register it to wrong_addr.
        coord
            .clients
            .insert(NodeId("n0".to_string()), Arc::new(MockNodeClient::Sign(n0_key)));
        coord.signers.insert(NodeId("n0".to_string()), wrong_addr);
        let result = coord
            .coordinate_attestation(SessionId("s".to_string()), proofs(), [7u8; 32], &NodeId("n0".to_string()))
            .await
            .unwrap();
        assert_eq!(result.signers.len(), 3);
        assert!(!result.signers.contains(&wrong_addr));
        assert!(!result.node_ids.contains(&NodeId("n0".to_string())));
    }

    #[tokio::test]
    async fn test_recovery_is_over_proof_hash() {
        // n0 signs a DIFFERENT message than the proof hash. Recovering over the
        // proof hash yields an address != n0's registered address -> rejected.
        let n0_key = PrivateKeySigner::random();
        let mut coord = signing_coordinator(&[("n1", true), ("n2", true), ("n3", true)], "n0");
        coord.signers.insert(NodeId("n0".to_string()), n0_key.address());
        coord.clients.insert(
            NodeId("n0".to_string()),
            Arc::new(MockNodeClient::SignMessage(n0_key.clone(), [0xFFu8; 32])),
        );
        let result = coord
            .coordinate_attestation(SessionId("s".to_string()), proofs(), [7u8; 32], &NodeId("n0".to_string()))
            .await
            .unwrap();
        assert!(!result.signers.contains(&n0_key.address()));
        assert!(!result.node_ids.contains(&NodeId("n0".to_string())));
    }

    #[tokio::test]
    async fn test_all_fail_insufficient() {
        let coord = coordinator(MockNodeClient::Fail, &["n0", "n1", "n2", "n3"]);
        let result = coord
            .coordinate_attestation(SessionId("s".to_string()), proofs(), [3u8; 32], &NodeId("n0".to_string()))
            .await;
        assert!(matches!(
            result,
            Err(NodeCoordinatorError::InsufficientAttestations { got: 0, required: 3 })
        ));
    }
}
