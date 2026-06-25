//! End-to-end RandomX attestation test.
//!
//! Drives [`NodeServiceImpl::request_attestation`] in-process with a real
//! RandomX proof to prove the verification loop is closed: a correct
//! (input, hash, difficulty) triple is attested, a tampered hash is rejected.
//!
//! These tests build RandomX VMs (both a verifier to produce the hash and the
//! service's own verifier thread), which is slow, so they are `#[ignore]`d.
//! Run with `cargo test -p node-server -- --ignored`.

use node_server::proto::node_service_server::NodeService;
use node_server::{proto, NodeServiceImpl, RANDOMX_SEED};
use randomx_verifier::RandomXVerifier;
use tonic::Request;

const API_KEY: &str = "test-key";
const WALLET: &str = "0x0000000000000000000000000000000000000000";

fn authed(body: proto::AttestationRequest) -> Request<proto::AttestationRequest> {
    let mut request = Request::new(body);
    request
        .metadata_mut()
        .insert("x-api-key", API_KEY.parse().expect("valid metadata"));
    request
}

fn attestation(proof_hash: Vec<u8>, proof_input: Vec<u8>, difficulty: u64) -> proto::AttestationRequest {
    proto::AttestationRequest {
        session_id: "sess-e2e".to_string(),
        wallet: WALLET.to_string(),
        commodity: "Gold".to_string(),
        proof_hash: Some(proto::ProofHash { value: proof_hash }),
        requesting_node_id: "node-a".to_string(),
        requested_at: None,
        proof_input,
        difficulty,
    }
}

#[tokio::test]
#[ignore = "builds RandomX VMs; slow. Run with --ignored"]
async fn test_real_proof_verifies_valid() {
    let input = b"primora-attestation-e2e-input".to_vec();

    // Compute the genuine RandomX hash under the same key the node uses.
    let mut verifier = RandomXVerifier::new(RANDOMX_SEED).expect("verifier init");
    let hash = verifier.hash(&input).expect("hash");

    let service = NodeServiceImpl::new(API_KEY.to_string()).expect("service init");

    // Difficulty 1 means any hash meets the target, isolating the hash match.
    let request = authed(attestation(hash.to_vec(), input, 1));
    let response = service
        .request_attestation(request)
        .await
        .expect("attestation call")
        .into_inner();

    assert!(response.valid, "a real proof must verify valid=true");
    assert!(response.signature.is_some(), "valid attestation must be signed");
}

#[tokio::test]
#[ignore = "builds RandomX VMs; slow. Run with --ignored"]
async fn test_tampered_hash_rejected() {
    let input = b"primora-attestation-e2e-input".to_vec();

    let mut verifier = RandomXVerifier::new(RANDOMX_SEED).expect("verifier init");
    let mut hash = verifier.hash(&input).expect("hash");
    // Flip a byte so the claimed hash no longer matches the real RandomX hash.
    hash[0] ^= 0xFF;

    let service = NodeServiceImpl::new(API_KEY.to_string()).expect("service init");

    let request = authed(attestation(hash.to_vec(), input, 1));
    let response = service
        .request_attestation(request)
        .await
        .expect("attestation call")
        .into_inner();

    assert!(!response.valid, "a tampered hash must be rejected");
    assert!(response.signature.is_none(), "invalid attestation must not be signed");
}
