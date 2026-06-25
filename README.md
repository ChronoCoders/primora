# primora-backend

Backend and smart contracts for Primora -- a mineable virtual currency (PRM)
settled on Ethereum. The Rust backend sits between user mining clients and
regional mining nodes: it validates proofs, detects anomalies, and coordinates
attestation before any mint proposal reaches the chain. The Solidity contracts
hold the on-chain mint, reserve, staking, and oracle logic.

## What this does

- Receives partial proofs from Browser, Desktop, and CLI mining clients every 30 seconds
- Runs fast pre-filter checks (timing, hashrate, duplicate sessions) before forwarding to nodes
- Scores sessions against five anomaly triggers -- two or more triggers flags the session for review
- Samples commodity oracle prices (Chainlink + Pyth) and computes a TWAP per session
- Orchestrates 2-of-3 node attestation at session end
- Calculates the payout (Spec 4.6) and produces a signed mint proposal for 3-of-5 multi-sig approval -- never submits on-chain directly

## Architecture

Two halves of one system:

- **Off-chain (Rust workspace, `crates/`)** -- the verification service, proof
  validation, anomaly scoring, session/oracle/payout logic, and the gRPC node
  server. Backed by Redis (session state) and PostgreSQL (audit log).
- **On-chain (Solidity, `contracts/`)** -- seven UUPS-upgradeable contracts that
  mint PRM, custody reserves, manage staking, and normalize oracle prices, all
  gated by multi-sig and timelocks.

## Crates

| Crate | Role |
|---|---|
| `common` | Shared types: ValidationResult, SessionContext, MintProposal, Commodity |
| `proof-validator` | PreFilter (fast, no crypto) and Full (RandomX, future) validators |
| `anomaly-engine` | Five-trigger scoring, AnomalyEvent publishing |
| `session-manager` | Redis-backed session state, commit-reveal, proof tracking |
| `rate-limiter` | Per-wallet, per-IP, per-node rate limiting |
| `mint-ceiling` | Daily/per-block ceiling calculation and proposal generation |
| `twap-calculator` | Time-weighted average price accumulation per session |
| `oracle-reader` | Chainlink (XAU/XAG) on-chain + Pyth (XPT/WTI) Hermes price reads |
| `payout-calculator` | Spec 4.6 payout formula in scaled integer arithmetic |
| `onchain-client` | Alloy-based block queries and mint-proposal signing |
| `node-coordinator` | 2-of-3 attestation orchestration with deterministic node selection |
| `node-server` | Tonic gRPC node server with API-key auth |
| `postgres-store` | PostgreSQL persistence for proposals, anomalies, audit log |
| `metrics` | Prometheus registry and collectors |
| `verification-service` | Axum HTTP entry point, wires all crates together |
| `integration-tests` | End-to-end tests against live services |

## Smart contracts

Solidity 0.8.24, Foundry, OpenZeppelin v5 (UUPS upgradeable). In `contracts/src/`:

| Contract | Role |
|---|---|
| `PrimToken` | ERC-20 PRM with UUPS proxy, minter and burner roles, no pre-mint |
| `NodeRegistry` | Node staking (min 10k PRM), 2-of-3 slashing with a 4h operator veto window |
| `HouseEdge` | 10-25% bounded edge, 48h timelock, dynamic reserve-ratio triggers |
| `OracleAggregator` | Backend TWAP submission with a 2% divergence guard and staleness check |
| `MiningContract` | 3-of-5 multi-sig, 48h timelock, per-block mint ceiling, per-session replay guard |
| `Treasury` | USDC/USDT reserves, redemption payouts, reserve-ratio system state |
| `StakingContract` | 30/90/180-day locks, boost multipliers (capped 40%), pro-rata revenue share |

## Requirements

- Rust 1.90+ (the release Docker image uses `rust:slim-bookworm`)
- Redis (session state and rate limiting)
- PostgreSQL (audit log, mint proposals, anomaly events)
- Foundry (`forge`/`anvil`) for the contracts
- Docker + Docker Compose (optional, for the full local stack)

## Build and test

Backend:

```bash
cargo build --workspace
cargo test --workspace
```

Integration tests (require live Redis/Postgres) run with:

```bash
cargo test --workspace -- --ignored
```

Contracts:

```bash
cd contracts
forge build
forge test
```

## Configuration

The verification service reads all configuration from the environment. Copy
[`.env.example`](.env.example) to `.env` and fill in real values; `.env` is
gitignored and must never be committed.

| Variable | Required | Purpose |
|---|---|---|
| `DATABASE_URL` | yes | PostgreSQL connection string |
| `REDIS_URL` | yes | Redis connection string |
| `BIND_ADDR` | yes | Address/port the HTTP server binds to |
| `CHAIN_ID` | yes | Ethereum chain id |
| `RPC_URL` | yes | Ethereum JSON-RPC endpoint |
| `SIGNING_KEY_HEX` | yes | Mint-proposal signing key (see below) |
| `LOG_LEVEL` | no | Tracing level (default `info`) |
| `ORACLE_AGGREGATOR_ADDRESS` | no | Deployed OracleAggregator address (enables on-chain TWAP submission) |
| `ORACLE_SUBMITTER_KEY_HEX` | no | Oracle submitter key (see below) |
| `NODE_ENDPOINTS` | no | Comma-separated gRPC node endpoints |
| `NODE_API_KEY` | no | API key for node gRPC auth |

### Key separation

The service uses two distinct private keys with deliberately different
authority. They must never be the same key.

- **`SIGNING_KEY_HEX`** — signs mint proposals only. The signature is verified
  off-chain by the admin multi-sig panel. This key **never submits transactions**
  and holds **no on-chain mint authority**. In production it should live in an
  HSM or Vault transit engine, not a plain env var.
- **`ORACLE_SUBMITTER_KEY_HEX`** (optional) — a low-privilege key used **only**
  to call `OracleAggregator.submitPrice`. It must be registered as the
  `authorizedSubmitter` on the deployed OracleAggregator and hold just enough ETH
  for gas. It is **never** the minting or admin key. If unset (or if
  `ORACLE_AGGREGATOR_ADDRESS` is unset), the TWAP is computed off-chain but not
  submitted on-chain.

Minting authority itself lives on-chain behind the MiningContract's 3-of-5
multi-sig and 48-hour timelock — neither backend key can mint.

## Local stack

Bring up Postgres, Redis, Prometheus, Grafana, and the verification service:

```bash
docker compose up
```

## Contract deployment (local Anvil)

```bash
anvil   # in one terminal

cd contracts
forge script script/Deploy.s.sol:DeployScript \
  --rpc-url http://localhost:8545 \
  --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
  --broadcast
```

Deploys all seven contracts behind ERC1967 proxies, wires them together, and
writes addresses to `contracts/deployments/local.json`. See
`contracts/script/README.md` for details.

## Status

Phases 1-3 complete:

- **Phase 1** -- backend scaffold: proof validation, anomaly scoring, session management
- **Phase 2** -- full payout pipeline: oracle integration, TWAP, Section 4.6 payout, signed mint proposals, metrics, gRPC node server
- **Phase 3** -- smart contracts: all 7 core contracts, 100 Foundry tests passing, full Anvil deployment verified end-to-end

Hard gates before mainnet: legal opinion, third-party security audit, payout
calibration and stress testing, and RandomX integration in the node server.

Phase 4 (next): Next.js frontend (wagmi/RainbowKit), full 3-of-5 + 48h mint
execution path, and backend-to-contract integration wiring.

## License

Business Source License 1.1 (BUSL-1.1). Non-production use is permitted; see
[`LICENSE`](LICENSE) for terms. The Licensed Work converts to Apache-2.0 on the
Change Date (2030-06-24). Licensor: Chronocoders.
