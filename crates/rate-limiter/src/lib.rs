#![deny(warnings)]
#![deny(missing_docs)]
//! Per-wallet, per-IP, and per-node rate limiting.

/// Checks per-wallet, per-IP, and per-node limits for the given context.
pub fn check_limits(_ctx: &common::SessionContext) -> common::ValidationResult {
    todo!()
}
