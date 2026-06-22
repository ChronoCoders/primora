#![deny(warnings)]
#![deny(missing_docs)]
//! PreFilter and Full proof validation implementations.

/// Stateless validator running ValidationMode::PreFilter and Full checks.
pub struct PreFilterValidator;

impl common::ProofValidator for PreFilterValidator {
    fn validate(
        &self,
        _proof: &common::PartialProof,
        _mode: common::ValidationMode,
        _ctx: &common::SessionContext,
    ) -> common::ValidationResult {
        todo!()
    }
}
