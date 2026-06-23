use alloy_primitives::{Address, Signature, U256};
use chrono::{Duration, Utc};
use common::{
    ClientType, Commodity, InvalidReason, PartialProof, ProofValidator, SessionContext, SessionId,
    SuspicionLevel, ValidationMode, ValidationResult,
};
use proof_validator::PreFilterValidator;

fn ctx(client_type: ClientType, active_sessions_count: u32, last: Option<chrono::DateTime<Utc>>) -> SessionContext {
    SessionContext {
        wallet: Address::ZERO,
        ip: None,
        client_type,
        active_sessions_count,
        last_submission_at: last,
        recent_proof_count: 5,
        assigned_node_id: None,
        commodity: Commodity::Gold,
    }
}

fn proof(hashrate: u64, signature: Option<Signature>) -> PartialProof {
    PartialProof {
        session_id: SessionId("integration".to_string()),
        wallet: Address::ZERO,
        sequence: 1,
        hashrate,
        proof_hash: [0u8; 32],
        submitted_at: Utc::now(),
        signature,
    }
}

#[test]
fn test_prefilter_valid_proof() {
    let context = ctx(ClientType::Desktop, 1, Some(Utc::now() - Duration::seconds(30)));
    let partial = proof(2000, Some(Signature::new(U256::ZERO, U256::ZERO, false)));
    assert_eq!(
        PreFilterValidator.validate(&partial, ValidationMode::PreFilter, &context),
        ValidationResult::Valid
    );
}

#[test]
fn test_prefilter_rejects_hashrate() {
    let context = ctx(ClientType::Browser, 1, None);
    let partial = proof(500, None);
    assert_eq!(
        PreFilterValidator.validate(&partial, ValidationMode::PreFilter, &context),
        ValidationResult::Invalid(InvalidReason::HashrateImpossible)
    );
}

#[test]
fn test_prefilter_duplicate_session() {
    let context = ctx(ClientType::Desktop, 2, None);
    let partial = proof(100, None);
    assert_eq!(
        PreFilterValidator.validate(&partial, ValidationMode::PreFilter, &context),
        ValidationResult::Suspicious(SuspicionLevel::High)
    );
}
