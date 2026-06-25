#!/usr/bin/env bash
set -euo pipefail

DEPLOYER_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
DEPLOYER_ADDR="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
RPC="http://localhost:8545"
SERVICE="http://localhost:3000"

# Host 6379 is often already occupied by another local Redis; map primora's
# Redis to 6380 to stay isolated. Postgres uses host 5432.
REDIS_HOST_PORT="6380"
OVERRIDE="$(mktemp /tmp/primora-smoke-override.XXXXXX.yml)"
cat > "$OVERRIDE" <<YML
services:
  redis:
    ports: !override
      - "${REDIS_HOST_PORT}:6379"
YML

cleanup() {
  echo "=== cleanup ==="
  kill ${BACKEND_PID:-} 2>/dev/null || true
  pkill anvil 2>/dev/null || true
  docker compose stop postgres redis 2>/dev/null || true
  rm -f "$OVERRIDE" 2>/dev/null || true
}
trap cleanup EXIT

echo "=== 1. Start Postgres + Redis ==="
docker compose -f docker-compose.yml -f "$OVERRIDE" up -d postgres redis
sleep 5

echo "=== 2. Start Anvil ==="
pkill anvil 2>/dev/null || true
sleep 1
anvil > /tmp/anvil_smoke.log 2>&1 &
sleep 3
cast block-number --rpc-url $RPC

echo "=== 3. Deploy contracts ==="
cd contracts
forge script script/Deploy.s.sol:DeployScript \
  --rpc-url $RPC --private-key $DEPLOYER_KEY --broadcast > /tmp/deploy_smoke.log 2>&1
ORACLE_ADDR=$(python3 -c "import json; print(json.load(open('deployments/local.json'))['OracleAggregator'])")
XAU_FEED=$(python3 -c "import json; print(json.load(open('deployments/local.json'))['MockXAUFeed'])")
XAG_FEED=$(python3 -c "import json; print(json.load(open('deployments/local.json'))['MockXAGFeed'])")
echo "OracleAggregator: $ORACLE_ADDR"
echo "MockXAUFeed: $XAU_FEED"
echo "MockXAGFeed: $XAG_FEED"
cd ..

echo "=== 4. Build backend ==="
cargo build -p verification-service --bin primora-verification 2>&1 | tail -3

echo "=== 5. Start backend with oracle submission enabled ==="
DATABASE_URL="postgres://primora:primora_dev@localhost:5432/primora" \
REDIS_URL="redis://localhost:${REDIS_HOST_PORT}" \
BIND_ADDR="0.0.0.0:3000" \
CHAIN_ID="31337" \
RPC_URL="$RPC" \
SIGNING_KEY_HEX="0000000000000000000000000000000000000000000000000000000000000001" \
ORACLE_SUBMITTER_KEY_HEX="ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80" \
ORACLE_AGGREGATOR_ADDRESS="$ORACLE_ADDR" \
CHAINLINK_XAU_ADDRESS="$XAU_FEED" \
CHAINLINK_XAG_ADDRESS="$XAG_FEED" \
LOG_LEVEL="info" \
./target/debug/primora-verification > /tmp/backend_smoke.log 2>&1 &
BACKEND_PID=$!
sleep 5

echo "=== 6. Health check ==="
curl -s $SERVICE/health
echo

# Commit-reveal: commit_hash must equal sha256(nonce_bytes) for the reveal to pass.
NONCE="00"
COMMIT=$(python3 -c "import hashlib; print(hashlib.sha256(bytes.fromhex('$NONCE')).hexdigest())")

echo "=== 7. Create a session ==="
SESSION_RESP=$(curl -s -X POST $SERVICE/sessions \
  -H "Content-Type: application/json" \
  -d "{\"wallet\":\"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266\",\"client_type\":\"Desktop\",\"commodity\":\"Gold\",\"assigned_node_id\":\"node-jhb-001\",\"commit_hash\":\"$COMMIT\"}")
echo "$SESSION_RESP"
SESSION_ID=$(echo "$SESSION_RESP" | python3 -c "import json,sys; print(json.load(sys.stdin)['session_id'])")
echo "Session: $SESSION_ID"

echo "=== 8. Submit a few proofs ==="
for i in 1 2 3; do
  PROOF_HASH=$(printf '%064d' "$i")
  curl -s -X POST $SERVICE/sessions/$SESSION_ID/proofs \
    -H "Content-Type: application/json" \
    -d "{\"sequence\":$i,\"hashrate\":2500,\"proof_hash\":\"$PROOF_HASH\"}"
  echo " <- proof $i"
  sleep 1
done

echo "=== 9. End the session (triggers TWAP submission) ==="
curl -s -X POST $SERVICE/sessions/$SESSION_ID/end \
  -H "Content-Type: application/json" \
  -d "{\"nonce\":\"$NONCE\"}"
echo

echo "=== 10. Check backend log for TWAP submission / oracle activity ==="
grep -i "TWAP submitted\|submission failed\|oracle\|no_samples\|twap" /tmp/backend_smoke.log | tail -10 || echo "(no matching log line)"

echo "=== 11. Read OracleAggregator on-chain for Gold price ==="
cast call $ORACLE_ADDR "getPriceUnchecked(uint8)(uint256,uint256,bool)" 0 --rpc-url $RPC

echo "=== 12. Check metrics ==="
curl -s $SERVICE/metrics | grep -E "session_active|proof_submissions" | head -5 || echo "(no matching metric line)"

echo "=== DONE ==="
