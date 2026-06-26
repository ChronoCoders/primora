#![deny(warnings)]
#![deny(missing_docs)]
//! Tonic gRPC node server: proof metadata intake, session-end intake, and
//! attestation signing, authenticated with a shared API key.

use alloy::signers::local::PrivateKeySigner;
use alloy::signers::SignerSync;
use alloy_primitives::Address;
use chrono::Utc;
use common::{
    Chain, ClientType, Commodity, PartialProof, SessionContext, SessionId, SuspicionLevel,
    ValidationMode, ValidationResult,
};
use randomx_verifier::{RandomXError, RandomXVerifier};
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
/// proofs are bound to an epoch and cannot be precomputed far ahead. Clients
/// must hash their proof input under this same key for verification to succeed.
// TODO(phase3-epoch-seed): derive the seed from the active mining epoch.
pub const RANDOMX_SEED: &[u8] = b"primora-phase2-randomx-seed";

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
    /// secp256k1 key this node signs attestations with. Its address is the
    /// node's attestation identity.
    signer: PrivateKeySigner,
    /// Sender to the dedicated RandomX verifier thread. See [`spawn_verifier`]
    /// for why verification runs on its own thread rather than inline.
    verify_tx: mpsc::UnboundedSender<VerifyJob>,
}

impl NodeServiceImpl {
    /// Creates a new `NodeServiceImpl` authenticating against `api_key` and
    /// signing attestations with `signer`.
    ///
    /// Spawns the RandomX verifier thread, initializing it in light mode from
    /// [`RANDOMX_SEED`]; returns an error if the VM cannot be built.
    pub fn new(api_key: String, signer: PrivateKeySigner) -> Result<Self, RandomXError> {
        let verify_tx = spawn_verifier(RANDOMX_SEED)?;
        Ok(Self {
            api_key,
            signer,
            verify_tx,
        })
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

        let invalid = |session_id: String| {
            Ok(Response::new(proto::AttestationResponse {
                session_id,
                valid: false,
                signature: None,
            }))
        };

        let proof_hash = match attestation_request.proof_hash.as_ref() {
            Some(hash) => proof_hash_bytes(hash),
            None => {
                tracing::warn!("attestation request missing proof hash");
                return invalid(attestation_request.session_id);
            }
        };

        // Structural pre-filter (sync; the validator is `!Send`, so it is fully
        // scoped and dropped before the `.await` below).
        let prefilter_ok = {
            let validator = proof_validator::validator(ValidationMode::PreFilter);
            let proof = PartialProof {
                session_id: SessionId(attestation_request.session_id.clone()),
                wallet: Address::ZERO,
                sequence: 0,
                hashrate: 0,
                proof_hash,
                proof_input: attestation_request.proof_input.clone(),
                difficulty: attestation_request.difficulty,
                submitted_at: Utc::now(),
                signature: None,
            };
            let ctx = SessionContext {
                wallet: Address::ZERO,
                ip: None,
                client_type: ClientType::Desktop,
                active_sessions_count: 1,
                started_at: Utc::now(),
                last_submission_at: None,
                recent_proof_count: 0,
                assigned_node_id: None,
                commodity: Commodity::Gold,
                // Attestation is verification-only and never mints; the target
                // chain is unused here, so default to the canonical chain.
                target_chain: Chain::Ethereum,
            };
            match validator.validate(&proof, ValidationMode::PreFilter, &ctx) {
                ValidationResult::Invalid(reason) => {
                    tracing::warn!(?reason, "proof failed pre-filter");
                    false
                }
                ValidationResult::Suspicious(SuspicionLevel::High) => {
                    tracing::warn!("proof flagged high suspicion");
                    true
                }
                ValidationResult::Valid | ValidationResult::Suspicious(_) => true,
            }
        };
        if !prefilter_ok {
            return invalid(attestation_request.session_id);
        }

        // RandomX verification: re-hash the exact proof input on the dedicated
        // verifier thread and confirm it matches the claimed hash and meets the
        // claimed difficulty.
        let (reply_tx, reply_rx) = oneshot::channel();
        let job = VerifyJob {
            input: attestation_request.proof_input.clone(),
            expected: proof_hash,
            difficulty: attestation_request.difficulty,
            reply: reply_tx,
        };
        if self.verify_tx.send(job).is_err() {
            return Err(Status::internal("randomx verifier unavailable"));
        }
        match reply_rx.await {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => {
                tracing::warn!("proof failed randomx verification");
                return invalid(attestation_request.session_id);
            }
            Ok(Err(e)) => {
                tracing::error!(error = %e, "randomx verification errored");
                return invalid(attestation_request.session_id);
            }
            Err(_) => return Err(Status::internal("randomx verifier dropped reply")),
        }

        let signature = match self.signer.sign_message_sync(&proof_hash) {
            Ok(signature) => signature,
            Err(e) => {
                tracing::error!(error = %e, "attestation signing failed");
                return Err(Status::internal("attestation signing failed"));
            }
        };

        let node_id = std::env::var("NODE_ID").unwrap_or_else(|_| DEFAULT_NODE_ID.to_string());
        let now = Utc::now();
        let node_signature = proto::NodeSignature {
            node_id,
            signature: signature.as_bytes().to_vec(),
            signed_at: Some(prost_types::Timestamp {
                seconds: now.timestamp(),
                nanos: now.timestamp_subsec_nanos() as i32,
            }),
        };

        Ok(Response::new(proto::AttestationResponse {
            session_id: attestation_request.session_id,
            valid: true,
            signature: Some(node_signature),
        }))
    }
}

/// Builds a `NodeServiceServer` ready to register with a Tonic server, signing
/// attestations with `signer`.
///
/// Returns an error if the RandomX verifier thread cannot be initialized.
pub fn build_server(
    api_key: String,
    signer: PrivateKeySigner,
) -> Result<NodeServiceServer<NodeServiceImpl>, RandomXError> {
    Ok(NodeServiceServer::new(NodeServiceImpl::new(api_key, signer)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_signer() -> PrivateKeySigner {
        "0x0000000000000000000000000000000000000000000000000000000000000001"
            .parse()
            .unwrap()
    }

    #[tokio::test]
    async fn test_node_service_rejects_wrong_api_key() {
        let service = NodeServiceImpl::new("correct-key".to_string(), test_signer()).unwrap();
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
        let service = NodeServiceImpl::new("correct-key".to_string(), test_signer()).unwrap();
        let mut request = Request::new(proto::PartialProofMetadata::default());
        request
            .metadata_mut()
            .insert(API_KEY_HEADER, "correct-key".parse().unwrap());
        let response = service.submit_proof_metadata(request).await;
        assert!(response.is_ok());
        assert!(response.unwrap().into_inner().accepted);
    }
}
