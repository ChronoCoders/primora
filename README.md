# primora-backend

Rust backend for the Primora platform. Sits between user mining clients and regional
mining nodes -- validates proofs, detects anomalies, and coordinates the attestation
process before any mint proposal reaches the chain.

## What this does

- Receives partial proofs from Browser, Desktop, and CLI mining clients every 30 seconds
- Runs fast pre-filter checks (timing, hashrate, duplicate sessions) before forwarding to nodes
- Scores sessions against five anomaly triggers -- two or more triggers flags the session for review
- Orchestrates 2-of-3 node attestation at session end
- Produces signed mint proposals for admin multi-sig approval -- never submits on-chain directly

## Crates

| Crate | Role |
|---|---|
| `common` | Shared types: ValidationResult, SessionContext, MintProposal, AttestationResult |
| `proof-validator` | PreFilter (fast, no crypto) and Full (RandomX, Phase 2) validators |
| `anomaly-engine` | Five-trigger scoring, AnomalyEvent publishing |
| `session-manager` | Redis-backed session state, commit-reveal tracking |
| `rate-limiter` | Per-wallet, per-IP, per-node rate limiting |
| `mint-ceiling` | Daily block ceiling calculation and proposal generation |
| `onchain-client` | Alloy-based block queries and proposal signing |
| `node-coordinator` | 2-of-3 attestation orchestration with deterministic node selection |
| `verification-service` | Axum HTTP entry point, wires all crates together |

## Requirements

- Rust 1.75+
- Redis (session state and rate limiting)
- PostgreSQL (audit log, mint proposals, anomaly events)

## Build

```bash
cargo build --workspace
cargo test --workspace
```

Integration tests (require live Redis) run with:

```bash
cargo test --workspace -- --ignored
```

## Status

Phase 1 scaffold complete. Attestation flow, gRPC node client (Tonic), and
RandomX proof validation are next.
