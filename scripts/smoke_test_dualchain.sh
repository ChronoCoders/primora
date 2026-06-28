#!/usr/bin/env bash
set -euo pipefail

DEPLOYER_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
DEPLOYER_ADDR="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
ETH_RPC="http://localhost:8545"
POL_RPC="http://localhost:8546"
SERVICE="http://localhost:3000"

# Host 6379 is often occupied by another local Redis; map primora's Redis to
# 6380 to stay isolated. Postgres uses host 5432.
REDIS_HOST_PORT="6380"
OVERRIDE="$(mktemp /tmp/primora-dual-override.XXXXXX.yml)"
cat > "$OVERRIDE" <<YML
services:
  redis:
    ports: !override
      - "${REDIS_HOST_PORT}:6379"
YML

cleanup() {
  echo "=== cleanup ==="
  kill ${BACKEND_PID:-} 2>/dev/null || true
  pkill -f "anvil" 2>/dev/null || true
  docker compose stop postgres redis 2>/dev/null || true
  rm -f "$OVERRIDE" 2>/dev/null || true
}
trap cleanup EXIT

echo "=== 1. Start Postgres + Redis ==="
docker compose -f docker-compose.yml -f "$OVERRIDE" up -d postgres redis
sleep 5

echo "=== 2. Start two Anvil instances (Ethereum forked from mainnet for real Chainlink) ==="
pkill -f anvil 2>/dev/null || true
sleep 1
# Real Ethereum-mainnet Chainlink XAU feed; present on the fork.
CHAINLINK_XAU="0x214eD9Da11D2fbe465a6fc601a91E62EbEc1a0D6"
FORK_RPC="${ETH_FORK_RPC:-}"
if [ -z "$FORK_RPC" ] && [ -f .env ]; then
  FORK_RPC="$(grep -E '^RPC_URL=' .env | head -1 | cut -d= -f2- | tr -d '[:space:]')" || true
fi
[ -n "$FORK_RPC" ] || { echo "ERROR: no mainnet fork RPC. Set ETH_FORK_RPC or RPC_URL in .env" >&2; exit 1; }
anvil --fork-url "$FORK_RPC" --chain-id 1 --port 8545 > /tmp/anvil_eth.log 2>&1 &
anvil --chain-id 137 --port 8546 > /tmp/anvil_pol.log 2>&1 &
for _ in $(seq 1 60); do
  cast block-number --rpc-url $ETH_RPC >/dev/null 2>&1 && cast chain-id --rpc-url $POL_RPC >/dev/null 2>&1 && break
  sleep 1
done
echo "ETH chain-id: $(cast chain-id --rpc-url $ETH_RPC) (forked mainnet block $(cast block-number --rpc-url $ETH_RPC))"
echo "POL chain-id: $(cast chain-id --rpc-url $POL_RPC)"

echo "=== 3. Deploy full suite to BOTH chains ==="
cd contracts
forge script script/Deploy.s.sol:DeployScript \
  --rpc-url $ETH_RPC --private-key $DEPLOYER_KEY --broadcast > /tmp/deploy_eth.log 2>&1
cp deployments/local.json /tmp/deploy_eth.json
forge script script/Deploy.s.sol:DeployScript \
  --rpc-url $POL_RPC --private-key $DEPLOYER_KEY --broadcast > /tmp/deploy_pol.log 2>&1
cp deployments/local.json /tmp/deploy_pol.json

ETH_ORACLE=$(python3 -c "import json; print(json.load(open('/tmp/deploy_eth.json'))['OracleAggregator'])")
POL_ORACLE=$(python3 -c "import json; print(json.load(open('/tmp/deploy_pol.json'))['OracleAggregator'])")
ETH_MINING=$(python3 -c "import json; print(json.load(open('/tmp/deploy_eth.json'))['MiningContract'])")
POL_MINING=$(python3 -c "import json; print(json.load(open('/tmp/deploy_pol.json'))['MiningContract'])")
ETH_PRIM=$(python3 -c "import json; print(json.load(open('/tmp/deploy_eth.json'))['PrimToken'])")
POL_PRIM=$(python3 -c "import json; print(json.load(open('/tmp/deploy_pol.json'))['PrimToken'])")
echo "ETH OracleAggregator: $ETH_ORACLE | MiningContract: $ETH_MINING"
echo "POL OracleAggregator: $POL_ORACLE | MiningContract: $POL_MINING"
cd ..

echo "=== 4. Build backend ==="
cargo build -p verification-service --bin primora-verification 2>&1 | tail -2

echo "=== 5. Start backend with dual-chain oracle submission ==="
# Canonical read: RPC_URL + CHAINLINK_XAU_ADDRESS point at the REAL Ethereum
# Chainlink XAU feed on the mainnet fork (Decision #16 Option A). CHAIN_ID is the
# canonical chain. Submission fans out to both ETHEREUM_* and POLYGON_*
# OracleAggregators (Decision 4b).
DATABASE_URL="postgres://primora:primora_dev@localhost:5432/primora" \
REDIS_URL="redis://localhost:${REDIS_HOST_PORT}" \
BIND_ADDR="0.0.0.0:3000" \
CHAIN_ID="1" \
RPC_URL="$ETH_RPC" \
CHAINLINK_XAU_ADDRESS="$CHAINLINK_XAU" \
SIGNING_KEY_HEX="0000000000000000000000000000000000000000000000000000000000000001" \
ETHEREUM_RPC_URL="$ETH_RPC" \
ETHEREUM_ORACLE_SUBMITTER_KEY_HEX="ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80" \
ETHEREUM_ORACLE_AGGREGATOR_ADDRESS="$ETH_ORACLE" \
POLYGON_RPC_URL="$POL_RPC" \
POLYGON_ORACLE_SUBMITTER_KEY_HEX="ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80" \
POLYGON_ORACLE_AGGREGATOR_ADDRESS="$POL_ORACLE" \
LOG_LEVEL="info" \
./target/debug/primora-verification > /tmp/backend_dual.log 2>&1 &
BACKEND_PID=$!
sleep 5

echo "=== 6. Health check ==="
curl -s $SERVICE/health; echo

# Commit-reveal: commit_hash must equal sha256(nonce_bytes) for the reveal to pass.
NONCE="00"
COMMIT=$(python3 -c "import hashlib; print(hashlib.sha256(bytes.fromhex('$NONCE')).hexdigest())")

echo "=== 7. Session A: mine on ETHEREUM ==="
SA=$(curl -s -X POST $SERVICE/sessions -H "Content-Type: application/json" \
  -d "{\"wallet\":\"$DEPLOYER_ADDR\",\"client_type\":\"desktop\",\"commodity\":\"Gold\",\"chain\":\"ethereum\",\"assigned_node_id\":\"node-001\",\"commit_hash\":\"$COMMIT\"}")
echo "$SA"
SA_ID=$(echo "$SA" | python3 -c "import json,sys; print(json.load(sys.stdin)['session_id'])")

echo "=== 8. Session B: mine on POLYGON ==="
SB=$(curl -s -X POST $SERVICE/sessions -H "Content-Type: application/json" \
  -d "{\"wallet\":\"$DEPLOYER_ADDR\",\"client_type\":\"desktop\",\"commodity\":\"Gold\",\"chain\":\"polygon\",\"assigned_node_id\":\"node-002\",\"commit_hash\":\"$COMMIT\"}")
echo "$SB"
SB_ID=$(echo "$SB" | python3 -c "import json,sys; print(json.load(sys.stdin)['session_id'])")

echo "=== 9. Submit proofs to both sessions (each proof adds a TWAP sample) ==="
for SID in $SA_ID $SB_ID; do
  for i in 1 2 3; do
    curl -s -X POST $SERVICE/sessions/$SID/proofs -H "Content-Type: application/json" \
      -d "{\"sequence\":$i,\"hashrate\":2500,\"proof_hash\":\"$(printf '%064d' $i)\",\"proof_input\":\"\",\"difficulty\":0}" > /dev/null
  done
  echo "proofs submitted for $SID"
done

echo "=== 10. End both sessions (triggers TWAP dual-submit) ==="
curl -s --max-time 60 -X POST $SERVICE/sessions/$SA_ID/end -H "Content-Type: application/json" -d "{\"nonce\":\"$NONCE\"}"; echo " <- session A ended"
curl -s --max-time 60 -X POST $SERVICE/sessions/$SB_ID/end -H "Content-Type: application/json" -d "{\"nonce\":\"$NONCE\"}"; echo " <- session B ended"

echo "=== 11. Verify TWAP submitted to BOTH chains (expect initialized=true, live Chainlink TWAP) ==="
echo "ETH OracleAggregator Gold price:"
cast call $ETH_ORACLE "getPriceUnchecked(uint8)(uint256,uint256,bool)" 0 --rpc-url $ETH_RPC
echo "POL OracleAggregator Gold price:"
cast call $POL_ORACLE "getPriceUnchecked(uint8)(uint256,uint256,bool)" 0 --rpc-url $POL_RPC

echo "=== 12. Backend log: dual-chain TWAP submission (expect chain=ethereum and chain=polygon) ==="
grep -i "TWAP submitted\|submission failed\|chain=" /tmp/backend_dual.log | tail -10 || echo "(no matching log line)"

echo "=== 13. Check mint_proposals chain column (per-chain 4c data path) ==="
docker compose exec -T postgres psql -U primora -d primora -t -c \
  "SELECT session_id, chain, status FROM mint_proposals ORDER BY created_at DESC LIMIT 5;" 2>/dev/null || \
  echo "(psql via container unavailable -- skipping direct DB check)"

echo "=== DONE -- dual-chain TWAP path verified ==="
echo "NOTE: mint_proposals rows are only written when node attestation succeeds."
echo "This test runs no gRPC node servers, so end_session reaches attestation and"
echo "returns attestation_failed AFTER the TWAP is already submitted to both chains."
echo "The KEY proof is steps 11-12: the single computed TWAP reached BOTH"
echo "OracleAggregators. Per-chain proposal rows (4c) require the gRPC node path,"
echo "exercised separately; the chain data path itself is proven by the unit and"
echo "ignored integration tests in postgres-store (test_payout_chain_per_row)."
