#![deny(warnings)]
#![deny(missing_docs)]
//! Tonic gRPC node server: proof metadata intake, session-end intake, and
//! attestation signing, authenticated with a shared API key.

use alloy_primitives::Address;
use chrono::Utc;
use common::{
    ClientType, Commodity, PartialProof, SessionContext, SessionId, SuspicionLevel, ValidationMode,
    ValidationResult,
};
use randomx_verifier::{RandomXError, RandomXVerifier};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tonic::{Request, Response, Status};

/// Generated gRPC message types and service definitions.
pub mod proto {
    #![allow(missing_docs, dead_code, clippy::all, clippy::pedantic)]
    tonic::include_proto!("primora.v1");
}

use proto::node_service_server::{NodeService, NodeServiceServer};

const API_KEY_HEADER: &str = "x-api-key";
const DEFAULT_NODE_ID: &str = "node-unknown";

/// RandomX seed (cache key) for the verifier. For Phase 2 this is a fixed
/// value shared by all nodes. In production the seed rotates per epoch so that
/// proofs are bound to an epoch and cannot be precomputed far ahead.
// TODO(phase3-epoch-seed): derive the seed from the active mining epoch.
const RANDOMX_SEED: &[u8] = b"primora-phase2-randomx-seed";

/// Difficulty target applied to every verified proof. Fixed for Phase 2; in
/// production this is set per epoch alongside the seed.
const PHASE2_DIFFICULTY: u64 = 1;

/// A RandomX verification request sent to the dedicated verifier thread.
struct VerifyJob {
    /// Reconstructed proof input.
    input: Vec<u8>,
    /// Claimed proof hash to match against.
    expected: [u8; 32],
    /// Difficulty target.
    difficulty: u64,
    /// Channel the worker replies on.
    reply: oneshot::Sender<Result<bool, RandomXError>>,
}

/// Spawns the dedicated RandomX verifier thread and returns a sender for jobs.
///
/// [`RandomXVerifier`] holds raw FFI pointers and is neither `Send` nor `Sync`,
/// so it cannot live inside the `Send + Sync` gRPC service nor be moved between
/// threads. Instead it is owned by a single dedicated thread; the service sends
/// [`VerifyJob`]s over a channel and awaits the reply. This serializes
/// verification through one VM (the Phase 2 throughput limit); a production
/// node would run a pool of verifier threads behind the same channel.
fn spawn_verifier(seed: &[u8]) -> Result<mpsc::UnboundedSender<VerifyJob>, RandomXError> {
    let seed = seed.to_vec();
    let (job_tx, mut job_rx) = mpsc::unbounded_channel::<VerifyJob>();
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), RandomXError>>();
    std::thread::spawn(move || {
        let mut verifier = match RandomXVerifier::new(&seed) {
            Ok(verifier) => {
                let _ = init_tx.send(Ok(()));
                verifier
            }
            Err(e) => {
                let _ = init_tx.send(Err(e));
                return;
            }
        };
        while let Some(job) = job_rx.blocking_recv() {
            let result = verifier.verify(&job.input, &job.expected, job.difficulty);
            let _ = job.reply.send(result);
        }
    });
    match init_rx.recv() {
        Ok(Ok(())) => Ok(job_tx),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(RandomXError::VmInit(e.to_string())),
    }
}

/// gRPC service implementation for a Primora mining node.
pub struct NodeServiceImpl {
    /// API key required on every inbound request.
    api_key: String,
    /// Sender to the dedicated RandomX verifier thread. See [`spawn_verifier`]
    /// for why verification runs on its own thread rather than inline.
    verify_tx: mpsc::UnboundedSender<VerifyJob>,
}

impl NodeServiceImpl {
    /// Creates a new `NodeServiceImpl` authenticating against `api_key`.
    ///
    /// Spawns the RandomX verifier thread, initializing it in light mode from
    /// [`RANDOMX_SEED`]; returns an error if the VM cannot be built.
    pub fn new(api_key: String) -> Result<Self, RandomXError> {
        let verify_tx = spawn_verifier(RANDOMX_SEED)?;
        Ok(Self { api_key, verify_tx })
    }

    fn authenticate<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let presented = request
            .metadata()
            .get(API_KEY_HEADER)
            .and_then(|value| value.to_str().ok());
        match presented {
            Some(key) if key == self.api_key => Ok(()),
            _ => Err(Status::unauthenticated("invalid api key")),
        }
    }
}

fn proof_hash_bytes(hash: &proto::ProofHash) -> [u8; 32] {
    let mut out = [0u8; 32];
    let len = hash.value.len().min(32);
    out[..len].copy_from_slice(&hash.value[..len]);
    out
}

/// Reconstructs the RandomX proof input from the attestation metadata.
///
/// The preimage is the canonical concatenation of the session id, wallet, and
/// commodity. Note that the raw proof nonce is not carried over the
/// attestation RPC, so this reconstruction binds a proof to its session
/// identity but not yet to a specific nonce; transmitting the nonce in the
/// proof metadata is required to fully close the verification loop.
// TODO(phase3-proof-nonce): carry the proof nonce so the input matches the
// exact preimage the client hashed.
fn reconstruct_proof_input(session_id: &str, wallet: &str, commodity: &str) -> Vec<u8> {
    let mut input = Vec::with_capacity(session_id.len() + wallet.len() + commodity.len());
    input.extend_from_slice(session_id.as_bytes());
    input.extend_from_slice(wallet.as_bytes());
    input.extend_from_slice(commodity.as_bytes());
    input
}

#[tonic::async_trait]
impl NodeService for NodeServiceImpl {
    async fn submit_proof_metadata(
        &self,
        request: Request<proto::PartialProofMetadata>,
    ) -> Result<Response<proto::SubmitProofMetadataResponse>, Status> {
        self.authenticate(&request)?;
        let metadata = request.into_inner();
        tracing::debug!(
            session_id = %metadata.session_id,
            sequence = metadata.sequence,
            hashrate = metadata.hashrate,
            "received proof metadata"
        );
        Ok(Response::new(proto::SubmitProofMetadataResponse {
            accepted: true,
            reason: "ok".to_string(),
        }))
    }

    async fn session_ended(
        &self,
        request: Request<proto::SessionEndedRequest>,
    ) -> Result<Response<proto::SessionEndedResponse>, Status> {
        self.authenticate(&request)?;
        let ended = request.into_inner();
        tracing::info!(session_id = %ended.session_id, "session ended");
        Ok(Response::new(proto::SessionEndedResponse {
            accepted: true,
            reason: "ok".to_string(),
        }))
    }

    async fn request_attestation(
        &self,
        request: Request<proto::AttestationRequest>,
    ) -> Result<Response<proto::AttestationResponse>, Status> {
        self.authenticate(&request)?;
        let attestation_request = request.into_inner();

        let proof_hashes: Vec<[u8; 32]> = attestation_request
            .proof_hash
            .iter()
            .map(proof_hash_bytes)
            .collect();

        // A proof set with no hashes attests nothing.
        let mut valid = !proof_hashes.is_empty();

        // Structural pre-filter pass. The validator is `!Send`, so it is fully
        // scoped here and dropped before any `.await` below.
        let prefilter_ok: Vec<bool> = {
            let validator = proof_validator::validator(ValidationMode::PreFilter);
            proof_hashes
                .iter()
                .enumerate()
                .map(|(index, hash)| {
                    let proof = PartialProof {
                        session_id: SessionId(attestation_request.session_id.clone()),
                        wallet: Address::ZERO,
                        sequence: index as u32,
                        hashrate: 0,
                        proof_hash: *hash,
                        submitted_at: Utc::now(),
                        signature: None,
                    };
                    let ctx = SessionContext {
                        wallet: Address::ZERO,
                        ip: None,
                        client_type: ClientType::Desktop,
                        active_sessions_count: 1,
                        last_submission_at: None,
                        recent_proof_count: index as u32,
                        assigned_node_id: None,
                        commodity: Commodity::Gold,
                    };
                    match validator.validate(&proof, ValidationMode::PreFilter, &ctx) {
                        ValidationResult::Invalid(reason) => {
                            tracing::warn!(?reason, index, "proof failed pre-filter");
                            false
                        }
                        ValidationResult::Suspicious(SuspicionLevel::High) => {
                            tracing::warn!(index, "proof flagged high suspicion");
                            true
                        }
                        ValidationResult::Valid | ValidationResult::Suspicious(_) => true,
                    }
                })
                .collect()
        };

        // RandomX verification pass. Each proof input is reconstructed and sent
        // to the dedicated verifier thread; a failed structural check skips it.
        for (index, hash) in proof_hashes.iter().enumerate() {
            if !prefilter_ok[index] {
                valid = false;
                continue;
            }
            let input = reconstruct_proof_input(
                &attestation_request.session_id,
                &attestation_request.wallet,
                &attestation_request.commodity,
            );
            let (reply_tx, reply_rx) = oneshot::channel();
            let job = VerifyJob {
                input,
                expected: *hash,
                difficulty: PHASE2_DIFFICULTY,
                reply: reply_tx,
            };
            if self.verify_tx.send(job).is_err() {
                return Err(Status::internal("randomx verifier unavailable"));
            }
            match reply_rx.await {
                Ok(Ok(true)) => {}
                Ok(Ok(false)) => {
                    tracing::warn!(index, "proof failed randomx verification");
                    valid = false;
                }
                Ok(Err(e)) => {
                    tracing::error!(error = %e, index, "randomx verification errored");
                    valid = false;
                }
                Err(_) => return Err(Status::internal("randomx verifier dropped reply")),
            }
        }

        if !valid {
            return Ok(Response::new(proto::AttestationResponse {
                session_id: attestation_request.session_id,
                valid: false,
                signature: None,
            }));
        }

        let mut hasher = Sha256::new();
        for hash in &proof_hashes {
            hasher.update(hash);
        }
        let mut signature_bytes = [0u8; 32];
        signature_bytes.copy_from_slice(&hasher.finalize());

        let node_id = std::env::var("NODE_ID").unwrap_or_else(|_| DEFAULT_NODE_ID.to_string());
        let now = Utc::now();
        let signature = proto::NodeSignature {
            node_id,
            signature: signature_bytes.to_vec(),
            signed_at: Some(prost_types::Timestamp {
                seconds: now.timestamp(),
                nanos: now.timestamp_subsec_nanos() as i32,
            }),
        };

        Ok(Response::new(proto::AttestationResponse {
            session_id: attestation_request.session_id,
            valid: true,
            signature: Some(signature),
        }))
    }
}

/// Builds a `NodeServiceServer` ready to register with a Tonic server.
///
/// Returns an error if the RandomX verifier thread cannot be initialized.
pub fn build_server(api_key: String) -> Result<NodeServiceServer<NodeServiceImpl>, RandomXError> {
    Ok(NodeServiceServer::new(NodeServiceImpl::new(api_key)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_node_service_rejects_wrong_api_key() {
        let service = NodeServiceImpl::new("correct-key".to_string()).unwrap();
        let mut request = Request::new(proto::PartialProofMetadata::default());
        request
            .metadata_mut()
            .insert(API_KEY_HEADER, "wrong-key".parse().unwrap());
        let result = service.submit_proof_metadata(request).await;
        assert!(result.is_err());
        assert_eq!(result.err().unwrap().code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn test_node_service_accepts_correct_api_key() {
        let service = NodeServiceImpl::new("correct-key".to_string()).unwrap();
        let mut request = Request::new(proto::PartialProofMetadata::default());
        request
            .metadata_mut()
            .insert(API_KEY_HEADER, "correct-key".parse().unwrap());
        let response = service.submit_proof_metadata(request).await;
        assert!(response.is_ok());
        assert!(response.unwrap().into_inner().accepted);
    }
}
