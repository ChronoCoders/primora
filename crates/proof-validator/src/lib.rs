#![deny(warnings)]
#![deny(missing_docs)]
//! PreFilter and Full proof validation implementations.

use common::{
    ClientType, InvalidReason, PartialProof, ProofValidator, SessionContext, SuspicionLevel,
    ValidationMode, ValidationResult,
};

const MAX_HASHRATE_BROWSER: u64 = 400;
const MAX_HASHRATE_DESKTOP: u64 = 4_000;
const MAX_HASHRATE_CLI: u64 = 5_000;

const TIMING_MIN_SECS: i64 = 2;
const TIMING_MAX_SECS: i64 = 300;

fn max_hashrate(client: ClientType) -> u64 {
    match client {
        ClientType::Browser => MAX_HASHRATE_BROWSER,
        ClientType::Desktop => MAX_HASHRATE_DESKTOP,
        ClientType::Cli => MAX_HASHRATE_CLI,
    }
}

/// Runs Spec 5.6 conditions 2-4 (timing, hashrate, duplicate session).
/// Conditions 1 (HashMismatch) and 5 (ProofDeficit) resolve only at session
/// end and are not checkable on a single partial proof.
fn prefilter_checks(proof: &PartialProof, ctx: &SessionContext) -> ValidationResult {
    if let Some(last) = ctx.last_submission_at {
        let interval = (proof.submitted_at - last).num_seconds();
        if !(TIMING_MIN_SECS..=TIMING_MAX_SECS).contains(&interval) {
            return ValidationResult::Invalid(InvalidReason::TimingAnomaly);
        }
    }
    if proof.hashrate > max_hashrate(ctx.client_type) {
        return ValidationResult::Invalid(InvalidReason::HashrateImpossible);
    }
    if ctx.active_sessions_count > 1 {
        return ValidationResult::Suspicious(SuspicionLevel::High);
    }
    ValidationResult::Valid
}

/// Stateless validator for [`ValidationMode::PreFilter`]: fast non-cryptographic
/// checks run by the backend before forwarding a proof to a node.
pub struct PreFilterValidator;

impl ProofValidator for PreFilterValidator {
    fn validate(
        &self,
        proof: &PartialProof,
        _mode: ValidationMode,
        ctx: &SessionContext,
    ) -> ValidationResult {
        prefilter_checks(proof, ctx)
    }
}

/// Stateless validator for [`ValidationMode::Full`]: runs the PreFilter checks
/// then the node-side signature-presence check. This is the pre-attestation
/// structural gate only.
///
/// Actual RandomX proof-of-work verification is not performed here: it requires
/// a RandomX VM (a heavy FFI dependency) and runs on the node that holds the
/// VM. See `node-server`'s `request_attestation`, which recomputes the RandomX
/// hash of each proof and checks it against the difficulty target before
/// signing an attestation.
pub struct FullValidator;

impl ProofValidator for FullValidator {
    fn validate(
        &self,
        proof: &PartialProof,
        _mode: ValidationMode,
        ctx: &SessionContext,
    ) -> ValidationResult {
        let pre = prefilter_checks(proof, ctx);
        if pre != ValidationResult::Valid {
            return pre;
        }
        // RandomX verification lives in node-server's request_attestation; this
        // validator is the structural pre-attestation check only.
        if proof.signature.is_none() {
            return ValidationResult::Invalid(InvalidReason::InvalidSignature);
        }
        ValidationResult::Valid
    }
}

/// Returns the validator implementation for the requested `mode`.
pub fn validator(mode: ValidationMode) -> Box<dyn ProofValidator> {
    match mode {
        ValidationMode::PreFilter => Box::new(PreFilterValidator),
        ValidationMode::Full => Box::new(FullValidator),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;
    use chrono::{DateTime, Duration, TimeZone, Utc};
    use common::SessionId;

    fn base() -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000, 0).unwrap()
    }

    fn mk_ctx(client: ClientType, active: u32, last: Option<DateTime<Utc>>) -> SessionContext {
        SessionContext {
            wallet: Address::ZERO,
            ip: None,
            client_type: client,
            active_sessions_count: active,
            last_submission_at: last,
            recent_proof_count: 0,
            assigned_node_id: None,
            commodity: common::Commodity::Gold,
        }
    }

    fn mk_proof(hashrate: u64, at: DateTime<Utc>) -> PartialProof {
        PartialProof {
            session_id: SessionId("s".into()),
            wallet: Address::ZERO,
            sequence: 1,
            hashrate,
            proof_hash: [0u8; 32],
            proof_input: Vec::new(),
            difficulty: 0,
            submitted_at: at,
            signature: None,
        }
    }

    #[test]
    fn test_timing_anomaly_too_fast() {
        let ctx = mk_ctx(ClientType::Desktop, 1, Some(base()));
        let p = mk_proof(100, base() + Duration::seconds(1));
        assert_eq!(
            PreFilterValidator.validate(&p, ValidationMode::PreFilter, &ctx),
            ValidationResult::Invalid(InvalidReason::TimingAnomaly)
        );
    }

    #[test]
    fn test_timing_anomaly_too_slow() {
        let ctx = mk_ctx(ClientType::Desktop, 1, Some(base()));
        let p = mk_proof(100, base() + Duration::seconds(301));
        assert_eq!(
            PreFilterValidator.validate(&p, ValidationMode::PreFilter, &ctx),
            ValidationResult::Invalid(InvalidReason::TimingAnomaly)
        );
    }

    #[test]
    fn test_timing_valid() {
        let ctx = mk_ctx(ClientType::Desktop, 1, Some(base()));
        let p = mk_proof(100, base() + Duration::seconds(30));
        assert_eq!(
            PreFilterValidator.validate(&p, ValidationMode::PreFilter, &ctx),
            ValidationResult::Valid
        );
    }

    #[test]
    fn test_hashrate_impossible_browser() {
        let ctx = mk_ctx(ClientType::Browser, 1, None);
        let p = mk_proof(401, base());
        assert_eq!(
            PreFilterValidator.validate(&p, ValidationMode::PreFilter, &ctx),
            ValidationResult::Invalid(InvalidReason::HashrateImpossible)
        );
    }

    #[test]
    fn test_hashrate_valid_desktop() {
        let ctx = mk_ctx(ClientType::Desktop, 1, None);
        let p = mk_proof(3000, base());
        assert_eq!(
            PreFilterValidator.validate(&p, ValidationMode::PreFilter, &ctx),
            ValidationResult::Valid
        );
    }

    #[test]
    fn test_duplicate_session() {
        let ctx = mk_ctx(ClientType::Cli, 2, None);
        let p = mk_proof(100, base());
        assert_eq!(
            PreFilterValidator.validate(&p, ValidationMode::PreFilter, &ctx),
            ValidationResult::Suspicious(SuspicionLevel::High)
        );
    }

    #[test]
    fn test_full_missing_signature() {
        let ctx = mk_ctx(ClientType::Cli, 1, None);
        let p = mk_proof(100, base());
        assert_eq!(
            FullValidator.validate(&p, ValidationMode::Full, &ctx),
            ValidationResult::Invalid(InvalidReason::InvalidSignature)
        );
    }
}
