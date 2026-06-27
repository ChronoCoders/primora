#!/usr/bin/env bash
set -euo pipefail

KEY0="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
ADDR0="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
# vm.addr(1)/vm.addr(2) are registered signers; their integer private keys are
# 0x..01 / 0x..02. cast accepts the 0x prefix for these.
SIGNER1_KEY="0x0000000000000000000000000000000000000000000000000000000000000001"
SIGNER2_KEY="0x0000000000000000000000000000000000000000000000000000000000000002"
ETH_RPC="http://localhost:8545"
POL_RPC="http://localhost:8546"

cleanup() {
  echo "=== cleanup ==="
  pkill -f anvil 2>/dev/null || true
  docker compose stop postgres 2>/dev/null || true
}
trap cleanup EXIT

echo "=== 1. Start Postgres ==="
docker compose up -d postgres
sleep 5

echo "=== 2. Start two Anvil instances ==="
pkill -f anvil 2>/dev/null || true
sleep 1
anvil --chain-id 1   --port 8545 > /tmp/anvil_eth_mint.log 2>&1 &
anvil --chain-id 137 --port 8546 > /tmp/anvil_pol_mint.log 2>&1 &
sleep 3
echo "ETH chain-id: $(cast chain-id --rpc-url $ETH_RPC), POL chain-id: $(cast chain-id --rpc-url $POL_RPC)"

echo "=== 3. Deploy to both chains ==="
cd contracts
forge script script/Deploy.s.sol:DeployScript --rpc-url $ETH_RPC --private-key $KEY0 --broadcast > /tmp/deploy_eth_mint.log 2>&1
cp deployments/local.json /tmp/deploy_eth_mint.json
forge script script/Deploy.s.sol:DeployScript --rpc-url $POL_RPC --private-key $KEY0 --broadcast > /tmp/deploy_pol_mint.log 2>&1
cp deployments/local.json /tmp/deploy_pol_mint.json

ETH_MINING=$(python3 -c "import json; print(json.load(open('/tmp/deploy_eth_mint.json'))['MiningContract'])")
POL_MINING=$(python3 -c "import json; print(json.load(open('/tmp/deploy_pol_mint.json'))['MiningContract'])")
ETH_PRIM=$(python3 -c "import json; print(json.load(open('/tmp/deploy_eth_mint.json'))['PrimToken'])")
POL_PRIM=$(python3 -c "import json; print(json.load(open('/tmp/deploy_pol_mint.json'))['PrimToken'])")
echo "ETH Mining: $ETH_MINING | POL Mining: $POL_MINING"
cd ..

echo "=== 4. Build admin CLI ==="
cargo build -p admin-cli --bin primora-admin 2>&1 | tail -2
ADMIN="./target/debug/primora-admin"

# Per-chain env for admin-cli. The admin key is hex WITHOUT the 0x prefix
# (PrivateKeySigner::from_str expects bare hex, matching smoke_test_mint.sh).
export ETHEREUM_RPC_URL="$ETH_RPC"
export ETHEREUM_MINING_CONTRACT_ADDRESS="$ETH_MINING"
export ETHEREUM_ADMIN_KEY_HEX="${KEY0#0x}"
export POLYGON_RPC_URL="$POL_RPC"
export POLYGON_MINING_CONTRACT_ADDRESS="$POL_MINING"
export POLYGON_ADMIN_KEY_HEX="${KEY0#0x}"
export DATABASE_URL="postgres://primora:primora_dev@localhost:5432/primora"

# Fund the registered integer-key signer addresses for gas on BOTH chains.
SIGNER1_ADDR=$(cast wallet address --private-key $SIGNER1_KEY)
SIGNER2_ADDR=$(cast wallet address --private-key $SIGNER2_KEY)
for RPC in $ETH_RPC $POL_RPC; do
  cast send $SIGNER1_ADDR --value 1ether --private-key $KEY0 --rpc-url $RPC > /dev/null
  cast send $SIGNER2_ADDR --value 1ether --private-key $KEY0 --rpc-url $RPC > /dev/null
done

# Run a full mint on one chain. Routing under test: admin-cli --chain drives
# approve/status/execute; propose uses cast (admin-cli propose needs a Postgres
# row, out of scope here); the other two signer approvals use cast.
run_mint_on_chain() {
  local CHAIN_NAME=$1 RPC=$2 MINING=$3 PRIM=$4
  echo "--- mint on $CHAIN_NAME (MiningContract $MINING) ---"
  local PID
  PID=$(cast keccak "session-$CHAIN_NAME")
  echo "proposalId=$PID"

  echo "[cast] proposeMint"
  cast send $MINING "proposeMint(bytes32,bytes32,address,uint256)" $PID $PID $ADDR0 1000000000000000000000 \
    --private-key $KEY0 --rpc-url $RPC > /dev/null
  echo "proposed on $CHAIN_NAME"

  echo "[admin-cli --chain $CHAIN_NAME] approve (deployer / signer 5)"
  if OUT=$($ADMIN approve --proposal-id $PID --chain $CHAIN_NAME 2>&1); then
    echo "  admin-cli approve OK: $(echo "$OUT" | grep -E 'approvals|tx' | tr '\n' ' ')"
  else
    echo "  admin-cli approve FAILED: $OUT"
  fi

  echo "[cast] approveMint signer1, signer2"
  cast send $MINING "approveMint(bytes32)" $PID --private-key $SIGNER1_KEY --rpc-url $RPC > /dev/null
  cast send $MINING "approveMint(bytes32)" $PID --private-key $SIGNER2_KEY --rpc-url $RPC > /dev/null

  echo "[admin-cli --chain $CHAIN_NAME] status (expect approvals: 3)"
  if OUT=$($ADMIN status --proposal-id $PID --chain $CHAIN_NAME 2>&1); then
    echo "$OUT" | grep -E 'chain|approvals|executed|timelock' | sed 's/^/  /'
  else
    echo "  admin-cli status FAILED: $OUT"
  fi

  echo "[cast] advance timelock 48h+1s"
  cast rpc evm_increaseTime 172801 --rpc-url $RPC > /dev/null
  cast rpc evm_mine --rpc-url $RPC > /dev/null

  echo "[admin-cli --chain $CHAIN_NAME] execute"
  if OUT=$($ADMIN execute --proposal-id $PID --chain $CHAIN_NAME --session-id "session-$CHAIN_NAME" 2>&1); then
    echo "  admin-cli execute OK: $(echo "$OUT" | grep -E 'chain|tx|postgres' | tr '\n' ' ')"
  else
    echo "  admin-cli execute FAILED: $OUT"
  fi

  echo "$CHAIN_NAME recipient PRM balance:"
  cast call $PRIM "balanceOf(address)(uint256)" $ADDR0 --rpc-url $RPC
}

echo "=== 5. Mint on ETHEREUM ==="
run_mint_on_chain ethereum $ETH_RPC $ETH_MINING $ETH_PRIM

echo "=== 6. Mint on POLYGON ==="
run_mint_on_chain polygon $POL_RPC $POL_MINING $POL_PRIM

echo "=== 7. Cross-chain isolation check (each chain == 1000 PRM base units) ==="
EXPECTED="1000000000000000000000"
ETH_BAL=$(cast call $ETH_PRIM "balanceOf(address)(uint256)" $ADDR0 --rpc-url $ETH_RPC | awk '{print $1}')
POL_BAL=$(cast call $POL_PRIM "balanceOf(address)(uint256)" $ADDR0 --rpc-url $POL_RPC | awk '{print $1}')
echo "ETH PrimToken balance: $ETH_BAL (expected $EXPECTED = 1000 PRM)"
echo "POL PrimToken balance: $POL_BAL (expected $EXPECTED = 1000 PRM)"
ISO_PASS=1
[ "$ETH_BAL" = "$EXPECTED" ] && echo "OK: ETH minted 1000 PRM (base units)" || { echo "FAIL: ETH balanceOf=$ETH_BAL"; ISO_PASS=0; }
[ "$POL_BAL" = "$EXPECTED" ] && echo "OK: POL minted 1000 PRM (base units)" || { echo "FAIL: POL balanceOf=$POL_BAL"; ISO_PASS=0; }
echo "These are SEPARATE token contracts on SEPARATE chains -- each 1000e18,"
echo "proving no bridge and correct per-chain isolation (Decision 4a)."
[ "$ISO_PASS" = "1" ] || exit 1

echo "=== DONE -- dual-chain mint lifecycle verified ==="
echo "Routing summary: propose=cast; approve/status/execute=admin-cli --chain"
echo "(deployer approval + execute go through admin-cli's per-chain MiningWriter)."
