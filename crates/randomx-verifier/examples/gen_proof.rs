//! Proof-generation helper for integration tests.
//!
//! Prints the `proof_input` and `proof_hash` a client should submit so the node
//! (which verifies RandomX under the same seed) accepts the proof.
//!
//! Usage:
//!   cargo run -q -p randomx-verifier --example gen_proof -- <input> [seed]
//!
//! Output (two lines on stdout):
//!   line 1: hex(input bytes)     -> submit as proof_input
//!   line 2: hex(RandomX(input))  -> submit as proof_hash
//!
//! With the default seed (PHASE2_SEED) line 2 is exactly what the node computes,
//! so a proof built from these two values passes attestation. An optional second
//! argument overrides the seed (used only for experimentation).

use std::process::exit;

use alloy_primitives::hex;
use randomx_verifier::{RandomXVerifier, PHASE2_SEED};

fn main() {
    let mut args = std::env::args().skip(1);
    let input = match args.next() {
        Some(input) => input,
        None => {
            eprintln!("usage: gen_proof <input> [seed]");
            eprintln!("  line 1 = hex(input) -> proof_input; line 2 = hex(randomx hash) -> proof_hash");
            exit(1);
        }
    };
    let seed: Vec<u8> = match args.next() {
        Some(seed) => seed.into_bytes(),
        None => PHASE2_SEED.to_vec(),
    };

    let mut verifier = match RandomXVerifier::new(&seed) {
        Ok(verifier) => verifier,
        Err(e) => {
            eprintln!("error: failed to init RandomX verifier: {e}");
            exit(1);
        }
    };
    let hash = match verifier.hash(input.as_bytes()) {
        Ok(hash) => hash,
        Err(e) => {
            eprintln!("error: failed to compute RandomX hash: {e}");
            exit(1);
        }
    };

    println!("{}", hex::encode(input.as_bytes()));
    println!("{}", hex::encode(hash));
}
