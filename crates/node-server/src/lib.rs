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
use sha2::{Digest, Sha256};
use tonic::{Request, Response, Status};

/// Generated gRPC message types and service definitions.
pub mod proto {
    #![allow(missing_docs, dead_code, clippy::all, clippy::pedantic)]
    tonic::include_proto!("primora.v1");
}

use proto::node_service_server::{NodeService, NodeServiceServer};

const API_KEY_HEADER: &str = "x-api-key";
const DEFAULT_NODE_ID: &str = "node-unknown";

/// gRPC service implementation for a Primora mining node.
pub struct NodeServiceImpl {
    /// API key required on every inbound request.
    api_key: String,
}

impl NodeServiceImpl {
    /// Creates a new `NodeServiceImpl` authenticating against `api_key`.
    pub fn new(api_key: String) -> Self {
        Self { api_key }
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

        let proof_hashes: Vec<[u8; 32]> = attestation_request
            .proof_hash
            .iter()
            .map(proof_hash_bytes)
            .collect();

        let validator = proof_validator::validator(ValidationMode::PreFilter);
        for (index, hash) in proof_hashes.iter().enumerate() {
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
                }
                ValidationResult::Suspicious(SuspicionLevel::High) => {
                    tracing::warn!(index, "proof flagged high suspicion");
                }
                ValidationResult::Valid | ValidationResult::Suspicious(_) => {}
            }
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
pub fn build_server(api_key: String) -> NodeServiceServer<NodeServiceImpl> {
    NodeServiceServer::new(NodeServiceImpl::new(api_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_node_service_rejects_wrong_api_key() {
        let service = NodeServiceImpl::new("correct-key".to_string());
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
        let service = NodeServiceImpl::new("correct-key".to_string());
        let mut request = Request::new(proto::PartialProofMetadata::default());
        request
            .metadata_mut()
            .insert(API_KEY_HEADER, "correct-key".parse().unwrap());
        let response = service.submit_proof_metadata(request).await;
        assert!(response.is_ok());
        assert!(response.unwrap().into_inner().accepted);
    }
}
