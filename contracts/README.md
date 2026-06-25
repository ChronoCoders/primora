# Primora Contracts

On-chain contracts for Primora (PRM) -- mint, reserves, staking, and oracle
logic. Solidity 0.8.24, Foundry, OpenZeppelin v5, all UUPS-upgradeable and gated
by multi-sig and timelocks.

## Contracts

| Contract | Role |
|---|---|
| `PrimToken` | ERC-20 PRM with UUPS proxy, minter and burner roles, no pre-mint |
| `NodeRegistry` | Node staking (min 10k PRM), 2-of-3 slashing with a 4h operator veto window |
| `HouseEdge` | 10-25% bounded edge, 48h timelock, dynamic reserve-ratio triggers |
| `OracleAggregator` | Backend TWAP submission with a 2% divergence guard and staleness check |
| `MiningContract` | 3-of-5 multi-sig, 48h timelock, per-block mint ceiling, per-session replay guard |
| `Treasury` | USDC/USDT reserves, redemption payouts, reserve-ratio system state |
| `StakingContract` | 30/90/180-day locks, boost multipliers (capped 40%), pro-rata revenue share |

## Layout

```
src/        contract sources
test/       Foundry unit tests (one suite per contract)
script/     deployment scripts (see script/README.md)
lib/        dependencies (git submodules: forge-std, openzeppelin-contracts[-upgradeable])
```

## Requirements

- [Foundry](https://book.getfoundry.sh/) (`forge`, `anvil`, `cast`)
- Submodules initialized: `git submodule update --init --recursive`

## Build and test

```bash
forge build
forge test
```

The full suite is 100 unit tests across the seven contracts. `forge build`
runs the linter; the build is warning-free.

## Deployment

See [`script/README.md`](script/README.md) for local Anvil deployment. In short:

```bash
anvil   # in one terminal

forge script script/Deploy.s.sol:DeployScript \
  --rpc-url http://localhost:8545 \
  --private-key <anvil_account_0_key> \
  --broadcast
```

Addresses are written to `deployments/local.json`.

## Conventions

- **Upgradeable**: every contract is UUPS; upgrades are owner-gated via
  `_authorizeUpgrade`. Constructors are not used -- state is set in `initialize`.
- **Errors**: custom errors only, no `require` strings.
- **Docs**: NatSpec on all public functions and types.
- **Lint**: the `block-timestamp` lint is excluded project-wide in `foundry.toml`
  -- all time-based logic uses windows of hours to days, which validator
  timestamp drift cannot game. This is the only lint exception; everything else
  is fixed at the source.

## Production hardening (not yet done)

Multi-sig and timelock parameters are owner-operated placeholders for local and
test use. Before mainnet: external security audit, governance wiring for the
multi-sig/timelock roles, and payout calibration.

## License

Business Source License 1.1 (BUSL-1.1), converting to Apache-2.0 on 2030-06-24.
See [`../LICENSE`](../LICENSE).
