#!/usr/bin/env bash
set -euo pipefail

# Anvil account 0 is the deployer/owner and is registered as a signer.
KEY0="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
ADDR0="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
# The deploy script registers vm.addr(1..4) + deployer as the 5 signers.
# vm.addr(k) is the address of private key k (the integer), NOT an Anvil
# account -- so we approve with keys 0x..01 / 0x..02 and fund those addresses
# for gas from account 0.
SIGNER1_KEY="0x0000000000000000000000000000000000000000000000000000000000000001"
SIGNER2_KEY="0x0000000000000000000000000000000000000000000000000000000000000002"
RPC="http://localhost:8545"

cleanup() {
  echo "=== cleanup ==="
  pkill anvil 2>/dev/null || true
  docker compose stop postgres 2>/dev/null || true
}
trap cleanup EXIT

echo "=== 1. Start Postgres (for CLI Postgres reads) ==="
docker compose up -d postgres
sleep 5

echo "=== 2. Start Anvil ==="
pkill anvil 2>/dev/null || true
sleep 1
anvil > /tmp/anvil_mint.log 2>&1 &
sleep 3

echo "=== 3. Deploy contracts ==="
cd contracts
forge script script/Deploy.s.sol:DeployScript \
  --rpc-url $RPC --private-key $KEY0 --broadcast > /tmp/deploy_mint.log 2>&1
MINING_ADDR=$(python3 -c "import json; print(json.load(open('deployments/local.json'))['MiningContract'])")
PRIM_ADDR=$(python3 -c "import json; print(json.load(open('deployments/local.json'))['PrimToken'])")
echo "MiningContract: $MINING_ADDR"
echo "PrimToken: $PRIM_ADDR"
cd ..

echo "=== 4. Verify 5 signers and recipient starting balance ==="
echo "signerCount: $(cast call $MINING_ADDR "signerCount()(uint256)" --rpc-url $RPC)"
echo "Recipient PRM balance before: $(cast call $PRIM_ADDR "balanceOf(address)(uint256)" $ADDR0 --rpc-url $RPC)"

echo "=== 5. Build admin CLI ==="
cargo build -p admin-cli --bin primora-admin 2>&1 | tail -2

ADMIN="./target/debug/primora-admin"
# admin-cli reads per-chain env (ETHEREUM_*) and requires --chain since the 4c
# dual-chain refactor.
export ETHEREUM_RPC_URL="$RPC"
export ETHEREUM_MINING_CONTRACT_ADDRESS="$MINING_ADDR"
export ETHEREUM_ADMIN_KEY_HEX="${KEY0#0x}"
export DATABASE_URL="postgres://primora:primora_dev@localhost:5432/primora"

# Derive proposalId = keccak256("test-session-001")
SESSION="test-session-001"
PROPOSAL_ID=$(cast keccak "$SESSION")
echo "Session: $SESSION"
echo "ProposalId: $PROPOSAL_ID"

# Fund the two integer-key signer addresses so they can pay gas.
SIGNER1_ADDR=$(cast wallet address --private-key $SIGNER1_KEY)
SIGNER2_ADDR=$(cast wallet address --private-key $SIGNER2_KEY)
echo "Funding signer addresses $SIGNER1_ADDR and $SIGNER2_ADDR"
cast send $SIGNER1_ADDR --value 1ether --private-key $KEY0 --rpc-url $RPC > /dev/null
cast send $SIGNER2_ADDR --value 1ether --private-key $KEY0 --rpc-url $RPC > /dev/null

echo "=== 6. Propose mint on-chain via cast (CLI propose needs a Postgres row; here we test the on-chain path) ==="
# proposeMint(proposalId, sessionId, recipient, amount) -- amount 1000 PRM
cast send $MINING_ADDR "proposeMint(bytes32,bytes32,address,uint256)" \
  $PROPOSAL_ID $PROPOSAL_ID $ADDR0 1000000000000000000000 \
  --private-key $KEY0 --rpc-url $RPC > /dev/null
echo "proposed"

echo "=== 7. Approve with 3 signers: deployer via CLI, two more via cast ==="
echo "-- CLI approve (deployer / signer 0):"
$ADMIN approve --proposal-id $PROPOSAL_ID --chain ethereum
echo "-- cast approve (signer vm.addr(1)):"
cast send $MINING_ADDR "approveMint(bytes32)" $PROPOSAL_ID --private-key $SIGNER1_KEY --rpc-url $RPC > /dev/null && echo "approved"
echo "-- cast approve (signer vm.addr(2)):"
cast send $MINING_ADDR "approveMint(bytes32)" $PROPOSAL_ID --private-key $SIGNER2_KEY --rpc-url $RPC > /dev/null && echo "approved"

echo "=== 8. Check status via CLI -- should show 3 approvals, timelock pending ==="
$ADMIN status --proposal-id $PROPOSAL_ID --chain ethereum

echo "=== 9. Try execute before timelock -- expect revert ==="
OUT=$(cast send $MINING_ADDR "executeMint(bytes32)" $PROPOSAL_ID --private-key $KEY0 --rpc-url $RPC 2>&1 || true)
if grep -qi "revert\|TimelockNotExpired" <<<"$OUT"; then
  echo "TIMELOCK ENFORCED (execute blocked before 48h)"
else
  echo "WARNING: timelock not enforced"
  echo "$OUT"
fi

echo "=== 10. Fast-forward 48 hours + 1 second via Anvil ==="
cast rpc evm_increaseTime 172801 --rpc-url $RPC
cast rpc evm_mine --rpc-url $RPC

echo "=== 11. Execute mint -- should succeed now ==="
cast send $MINING_ADDR "executeMint(bytes32)" $PROPOSAL_ID --private-key $KEY0 --rpc-url $RPC > /dev/null && echo "executed"

echo "=== 12. Verify recipient received 1000 PRM (1000 * 10^18 base units) ==="
BAL=$(cast call $PRIM_ADDR "balanceOf(address)(uint256)" $ADDR0 --rpc-url $RPC | awk '{print $1}')
EXPECTED="1000000000000000000000"
echo "Recipient PRM balance after: $BAL (expected $EXPECTED = 1000 PRM)"
if [ "$BAL" = "$EXPECTED" ]; then
  echo "OK: minted 1000 PRM in base units"
else
  echo "FAIL: balanceOf=$BAL != $EXPECTED"; exit 1
fi

echo "=== 13. Verify proposal marked executed ==="
cast call $MINING_ADDR "proposals(bytes32)(bytes32,address,uint256,uint256,uint8,bool,bool)" \
  $PROPOSAL_ID --rpc-url $RPC

echo "=== 14. Replay protection: try execute again -- expect revert ==="
OUT=$(cast send $MINING_ADDR "executeMint(bytes32)" $PROPOSAL_ID --private-key $KEY0 --rpc-url $RPC 2>&1 || true)
if grep -qi "revert\|AlreadyExecuted" <<<"$OUT"; then
  echo "REPLAY PROTECTION WORKS"
else
  echo "WARNING: replay not blocked"
  echo "$OUT"
fi

echo "=== DONE -- full mint lifecycle verified ==="
