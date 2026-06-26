#![deny(warnings)]
#![deny(missing_docs)]
//! RandomX proof-of-work verification for Primora mining proofs.
//!
//! A [`RandomXVerifier`] recomputes the RandomX hash of a proof input and
//! checks that it both matches the claimed hash and meets a difficulty target.
//! This proves the submitted mining work is real CPU work rather than a
//! fabricated value; it is not a consensus mechanism.

use std::error::Error;
use std::fmt;

use alloy_primitives::U256;
use randomx_rs::{RandomXCache, RandomXFlag, RandomXVM};

/// The RandomX seed used for proof verification in Phase 2. In production this
/// rotates per epoch (see TODO phase3-epoch-seed). This is the single source of
/// truth for the seed; the node server and proof-generation helper both use it
/// so a client's proof and the node's verification agree.
pub const PHASE2_SEED: &[u8] = b"primora-phase2-randomx-seed";

/// Error raised while initializing or running a [`RandomXVerifier`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RandomXError {
    /// Building the RandomX cache from the seed key failed.
    CacheInit(String),
    /// Building the RandomX VM from the cache failed.
    VmInit(String),
    /// Computing a RandomX hash failed.
    HashFailed(String),
}

impl fmt::Display for RandomXError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CacheInit(e) => write!(f, "randomx cache init failed: {e}"),
            Self::VmInit(e) => write!(f, "randomx vm init failed: {e}"),
            Self::HashFailed(e) => write!(f, "randomx hash failed: {e}"),
        }
    }
}

impl Error for RandomXError {}

/// A single-threaded RandomX verifier holding one VM and its backing cache.
///
/// The underlying [`randomx_rs::RandomXVM`] wraps raw FFI pointers and is not
/// suitable for sharing across threads (`Send`/`Sync` cannot be relied on).
/// Each instance must therefore be used from one thread at a time; concurrent
/// verification requires a pool of verifiers, one per worker thread.
pub struct RandomXVerifier {
    vm: RandomXVM,
}

impl RandomXVerifier {
    /// Builds a verifier seeded from `key`.
    ///
    /// The VM is created in light mode: it holds only the ~256MB cache and no
    /// ~2GB dataset. Light mode is slower per hash but is the correct tradeoff
    /// for a verifier, which checks a handful of hashes rather than mining at
    /// volume. [`RandomXFlag::default`] is used for portability: no JIT, no
    /// large pages, and no hardware-AES requirement.
    pub fn new(key: &[u8]) -> Result<Self, RandomXError> {
        let flags = RandomXFlag::default();
        let cache =
            RandomXCache::new(flags, key).map_err(|e| RandomXError::CacheInit(e.to_string()))?;
        let vm = RandomXVM::new(flags, Some(cache), None)
            .map_err(|e| RandomXError::VmInit(e.to_string()))?;
        Ok(Self { vm })
    }

    /// Computes the RandomX hash of `input` under this verifier's key.
    ///
    /// Useful for producing the expected hash a proof must match. Returns an
    /// error if the underlying VM reports an unexpected hash length.
    pub fn hash(&mut self, input: &[u8]) -> Result<[u8; 32], RandomXError> {
        let computed = self
            .vm
            .calculate_hash(input)
            .map_err(|e| RandomXError::HashFailed(e.to_string()))?;
        if computed.len() != 32 {
            return Err(RandomXError::HashFailed(format!(
                "unexpected hash length {}",
                computed.len()
            )));
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&computed);
        Ok(out)
    }

    /// Verifies that `input` hashes under RandomX to `expected_hash` and that
    /// the resulting hash meets `difficulty`.
    ///
    /// The difficulty target is `U256::MAX / difficulty`. The computed hash,
    /// read as a big-endian 256-bit integer, must be less than or equal to the
    /// target. Returns `Ok(true)` only when the computed hash both equals
    /// `expected_hash` and meets the target. A `difficulty` of `0` is treated
    /// as `1` (every hash meets the target) to avoid division by zero.
    pub fn verify(
        &mut self,
        input: &[u8],
        expected_hash: &[u8; 32],
        difficulty: u64,
    ) -> Result<bool, RandomXError> {
        let computed = self.hash(input)?;
        if &computed != expected_hash {
            tracing::trace!("randomx hash mismatch against expected proof hash");
            return Ok(false);
        }
        let divisor = U256::from(difficulty.max(1));
        let target = U256::MAX / divisor;
        let hash_value = U256::from_be_slice(&computed);
        Ok(hash_value <= target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_phase2_seed_bytes_locked() {
        assert_eq!(PHASE2_SEED, b"primora-phase2-randomx-seed");
    }

    #[test]
    fn test_difficulty_target() {
        // This mirrors the difficulty arithmetic inside `verify`: a hash of all
        // 0xFF bytes is the largest possible value and fails difficulty 2,
        // while a hash of all 0x00 bytes meets any difficulty.
        let max_bytes = [0xFFu8; 32];
        let target_two = U256::MAX / U256::from(2u64);
        assert!(U256::from_be_slice(&max_bytes) > target_two);

        let zero_bytes = [0x00u8; 32];
        let target_hard = U256::MAX / U256::from(1_000_000u64);
        assert!(U256::from_be_slice(&zero_bytes) <= target_hard);
    }

    // Heavy: light VM init allocates ~256MB and fills the cache, which takes a
    // moment. Run with `cargo test -p randomx-verifier -- --ignored`.
    #[test]
    #[ignore = "RandomX light VM init is slow; run with --ignored"]
    fn test_verifier_creates() {
        assert!(RandomXVerifier::new(b"test-key").is_ok());
    }

    // Heavy: see `test_verifier_creates`.
    #[test]
    #[ignore = "RandomX light VM init is slow; run with --ignored"]
    fn test_known_hash() {
        let mut verifier = RandomXVerifier::new(b"test-key").unwrap();
        let input = b"primora-determinism-check";
        let first = verifier.vm.calculate_hash(input).unwrap();
        let second = verifier.vm.calculate_hash(input).unwrap();
        assert_eq!(first, second, "RandomX hashing must be deterministic");

        let mut expected = [0u8; 32];
        expected.copy_from_slice(&first);
        assert!(verifier.verify(input, &expected, 1).unwrap());

        let mut wrong = expected;
        wrong[0] ^= 0xFF;
        assert!(!verifier.verify(input, &wrong, 1).unwrap());
    }

    // Heavy: see `test_verifier_creates`. Confirms two independent verifiers
    // built from PHASE2_SEED hash the same input identically, so a client's
    // generated proof matches the node's verification.
    #[test]
    #[ignore = "RandomX light VM init is slow; run with --ignored"]
    fn test_phase2_seed_deterministic_across_instances() {
        let input = b"primora-proof-001";
        let mut a = RandomXVerifier::new(PHASE2_SEED).unwrap();
        let mut b = RandomXVerifier::new(PHASE2_SEED).unwrap();
        assert_eq!(a.hash(input).unwrap(), b.hash(input).unwrap());
    }
}
