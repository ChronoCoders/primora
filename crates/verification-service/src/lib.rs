#![deny(warnings)]
#![deny(missing_docs)]
//! Axum entry point and request routing for proof submissions.

/// Handles an incoming proof submission and returns a validation result.
pub fn handle_proof_submission(
    _proof: &common::PartialProof,
    _ctx: &common::SessionContext,
) -> common::ValidationResult {
    todo!()
}
