//! Tonic gRPC implementation of the [`NodeClient`] trait.

use alloy_primitives::Signature;
use chrono::{DateTime, Utc};
use common::{NodeId, NodeSignature, PartialProof};
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::{Channel, Endpoint};

use crate::{NodeClient, NodeCoordinatorError};

mod proto {
    #![allow(missing_docs, dead_code, clippy::all, clippy::pedantic)]
    tonic::include_proto!("primora.v1");
}

const API_KEY_HEADER: &str = "x-api-key";

/// gRPC client that requests attestation signatures from a single Primora node
/// over a lazily-connected channel, authenticating with a shared API key.
pub struct GrpcNodeClient {
    /// Full URI of the node this client connects to.
    endpoint: String,
    /// API key sent as metadata on every request.
    api_key: String,
    /// Lazily-connected channel to the node endpoint.
    channel: Channel,
}

impl GrpcNodeClient {
    /// Creates a new `GrpcNodeClient`.
    ///
    /// `endpoint` is the full URI of the node, for example
    /// `"http://node1.primora.internal:50051"`. `api_key` is the shared secret
    /// used for node authentication. The channel connects lazily on the first
    /// request rather than during construction.
    pub async fn new(endpoint: String, api_key: String) -> Result<Self, tonic::transport::Error> {
        let channel = Endpoint::from_shared(endpoint.clone())?.connect_lazy();
        Ok(Self {
            endpoint,
            api_key,
            channel,
        })
    }

    /// Returns the endpoint URI this client connects to.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Parses the configured API key into a metadata value.
    fn api_key_value(&self) -> Result<MetadataValue<Ascii>, NodeCoordinatorError> {
        self.api_key
            .parse()
            .map_err(|_| NodeCoordinatorError::NodeError("invalid api key metadata".to_string()))
    }
}

impl NodeClient for GrpcNodeClient {
    async fn request_attestation(
        &self,
        target_node_id: &NodeId,
        assigned_node_id: &NodeId,
        proof_set: &[PartialProof],
    ) -> Result<NodeSignature, NodeCoordinatorError> {
        tracing::debug!(
            target_node = %target_node_id.0,
            assigned_node = %assigned_node_id.0,
            endpoint = %self.endpoint,
            "requesting attestation"
        );
        // RandomX verification is per-proof: the node re-hashes the exact
        // preimage and checks it against the claimed hash. The attestation
        // therefore carries the representative (first) proof of the set with
        // its input, hash, and difficulty so the node can verify real work.
        let first = proof_set.first();
        let request_body = proto::AttestationRequest {
            session_id: first
                .map(|proof| proof.session_id.0.clone())
                .unwrap_or_default(),
            wallet: first.map(|proof| proof.wallet.to_string()).unwrap_or_default(),
            commodity: String::new(),
            proof_hash: first.map(|proof| proto::ProofHash {
                value: proof.proof_hash.to_vec(),
            }),
            requesting_node_id: assigned_node_id.0.clone(),
            requested_at: None,
            proof_input: first.map(|proof| proof.proof_input.clone()).unwrap_or_default(),
            difficulty: first.map(|proof| proof.difficulty).unwrap_or(0),
        };

        let mut request = tonic::Request::new(request_body);
        request
            .metadata_mut()
            .insert(API_KEY_HEADER, self.api_key_value()?);

        let mut client = proto::node_service_client::NodeServiceClient::new(self.channel.clone());
        let response = client
            .request_attestation(request)
            .await
            .map_err(|status| NodeCoordinatorError::NodeError(status.message().to_string()))?
            .into_inner();

        if !response.valid {
            return Err(NodeCoordinatorError::NodeError(format!(
                "node {} reported an invalid attestation",
                target_node_id.0
            )));
        }

        let signature_proto = response.signature.ok_or_else(|| {
            NodeCoordinatorError::NodeError("attestation response missing signature".to_string())
        })?;

        let signature = Signature::try_from(signature_proto.signature.as_slice()).map_err(|err| {
            NodeCoordinatorError::NodeError(format!("invalid signature bytes: {err}"))
        })?;

        let signed_at = signature_proto
            .signed_at
            .and_then(|ts| DateTime::from_timestamp(ts.seconds, u32::try_from(ts.nanos).unwrap_or(0)))
            .unwrap_or_else(Utc::now);

        Ok(NodeSignature {
            node_id: NodeId(signature_proto.node_id),
            signature,
            signed_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_grpc_client_new() {
        let endpoint = "http://node1.primora.internal:50051".to_string();
        let client = GrpcNodeClient::new(endpoint.clone(), "shared-secret".to_string())
            .await
            .expect("lazy channel construction should not connect");
        assert_eq!(client.endpoint(), endpoint);
    }
}
