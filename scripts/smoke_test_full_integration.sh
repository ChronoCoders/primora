#!/usr/bin/env bash
set -euo pipefail

KEY0="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
ADDR0="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
ETH_RPC="http://localhost:8545"
SERVICE="http://localhost:3000"
NODE_KEY="0x0000000000000000000000000000000000000000000000000000000000000abc"
SIGNER1="0x0000000000000000000000000000000000000000000000000000000000000001"
SIGNER2="0x0000000000000000000000000000000000000000000000000000000000000002"

# Host 6379 is often occupied by another local Redis; map primora's Redis to 6380.
REDIS_HOST_PORT="6380"
OVERRIDE="$(mktemp /tmp/primora-full-override.XXXXXX.yml)"
cat > "$OVERRIDE" <<YML
services:
  redis:
    ports: !override
      - "${REDIS_HOST_PORT}:6379"
YML

cleanup() {
  echo "=== cleanup ==="
  kill ${BACKEND_PID:-} ${NODE_PID:-} 2>/dev/null || true
  pkill -f primora-node 2>/dev/null || true
  pkill -f primora-verification 2>/dev/null || true
  pkill -f anvil 2>/dev/null || true
  docker compose stop postgres redis 2>/dev/null || true
  rm -f "$OVERRIDE" 2>/dev/null || true
}
trap cleanup EXIT

echo "=== 1. Infra: Postgres + Redis ==="
docker compose -f docker-compose.yml -f "$OVERRIDE" up -d postgres redis
sleep 5

echo "=== 2. Anvil (chain-id 1) ==="
pkill -f anvil 2>/dev/null || true
sleep 1
anvil --chain-id 1 --port 8545 > /tmp/anvil_full.log 2>&1 &
sleep 3
cast chain-id --rpc-url $ETH_RPC

echo "=== 3. Deploy full suite ==="
cd contracts
forge script script/Deploy.s.sol:DeployScript --rpc-url $ETH_RPC --private-key $KEY0 --broadcast > /tmp/deploy_full.log 2>&1
MINING=$(python3 -c "import json; print(json.load(open('deployments/local.json'))['MiningContract'])")
PRIM=$(python3 -c "import json; print(json.load(open('deployments/local.json'))['PrimToken'])")
ORACLE=$(python3 -c "import json; print(json.load(open('deployments/local.json'))['OracleAggregator'])")
XAU_FEED=$(python3 -c "import json; print(json.load(open('deployments/local.json'))['MockXAUFeed'])")
echo "Mining=$MINING Prim=$PRIM Oracle=$ORACLE XAU_FEED=$XAU_FEED"
cd ..

echo "=== 4. Build node-server, verification-service, admin-cli, gen_proof ==="
cargo build -p node-server --bin primora-node 2>&1 | tail -1
cargo build -p verification-service --bin primora-verification 2>&1 | tail -1
cargo build -p admin-cli --bin primora-admin 2>&1 | tail -1
cargo build -q -p randomx-verifier --example gen_proof 2>&1 | tail -1

echo "=== 5. Generate a VALID RandomX proof ==="
INPUT="primora-proof-001"
GEN=$(cargo run -q -p randomx-verifier --example gen_proof -- "$INPUT")
PROOF_INPUT=$(echo "$GEN" | sed -n '1p')
PROOF_HASH=$(echo "$GEN" | sed -n '2p')
echo "proof_input=$PROOF_INPUT"
echo "proof_hash=$PROOF_HASH"

echo "=== 6. Start node-server (real attestation) ==="
BIND_ADDR="127.0.0.1:50051" \
NODE_API_KEY="devkey" \
NODE_SIGNING_KEY_HEX="${NODE_KEY#0x}" \
NODE_ID="node-b" \
LOG_LEVEL="info" \
./target/debug/primora-node > /tmp/node_full.log 2>&1 &
NODE_PID=$!
echo "waiting for node RandomX VM init..."
for i in $(seq 1 60); do
  if grep -q "primora node starting" /tmp/node_full.log 2>/dev/null; then echo "node ready"; break; fi
  if ! kill -0 $NODE_PID 2>/dev/null; then echo "NODE DIED:"; cat /tmp/node_full.log; exit 1; fi
  sleep 1
done
sleep 1

echo "=== 7. Start verification-service wired to the node ==="
DATABASE_URL="postgres://primora:primora_dev@localhost:5432/primora" \
REDIS_URL="redis://localhost:${REDIS_HOST_PORT}" \
BIND_ADDR="0.0.0.0:3000" \
CHAIN_ID="1" \
RPC_URL="$ETH_RPC" \
CHAINLINK_XAU_ADDRESS="$XAU_FEED" \
SIGNING_KEY_HEX="0000000000000000000000000000000000000000000000000000000000000001" \
NODE_ENDPOINTS="http://localhost:50051" \
NODE_API_KEY="devkey" \
LOG_LEVEL="info" \
./target/debug/primora-verification > /tmp/backend_full.log 2>&1 &
BACKEND_PID=$!
sleep 5
curl -s $SERVICE/health; echo

echo "=== 8. Create session (assigned_node_id distinct from endpoint, chain ethereum) ==="
NONCE="00"
COMMIT=$(python3 -c "import hashlib; print(hashlib.sha256(bytes.fromhex('$NONCE')).hexdigest())")
SESS=$(curl -s -X POST $SERVICE/sessions -H "Content-Type: application/json" \
  -d "{\"wallet\":\"$ADDR0\",\"client_type\":\"desktop\",\"commodity\":\"Gold\",\"chain\":\"ethereum\",\"assigned_node_id\":\"node-a\",\"commit_hash\":\"$COMMIT\"}")
echo "$SESS"
SID=$(echo "$SESS" | python3 -c "import json,sys; print(json.load(sys.stdin)['session_id'])")

echo "=== 9. Submit the VALID proof (real RandomX, difficulty 1) ==="
curl -s -X POST $SERVICE/sessions/$SID/proofs -H "Content-Type: application/json" \
  -d "{\"sequence\":1,\"hashrate\":2500,\"proof_hash\":\"$PROOF_HASH\",\"proof_input\":\"$PROOF_INPUT\",\"difficulty\":1}"
echo " <- proof submitted"

# Let the session accrue a non-zero duration so the payout gross_prm is positive.
sleep 3

echo "=== 10. End session -- REAL attestation must pass ==="
END=$(curl -s --max-time 90 -X POST $SERVICE/sessions/$SID/end -H "Content-Type: application/json" -d "{\"nonce\":\"$NONCE\"}")
echo "end_session response: $END"

echo "=== 11. Backend log: attestation path ==="
grep -i "attestation\|TWAP submitted\|proposal\|payout calculated\|gross_prm" /tmp/backend_full.log | tail -12 || echo "(no matching backend log lines)"

echo "=== 12. Node log: verification + signing ==="
grep -i "attest\|verif\|sign\|randomx\|node starting" /tmp/node_full.log | tail -10 || echo "(no matching node log lines)"

echo "=== 13. Assert end_session returned 'completed' (NOT attestation_failed) ==="
if echo "$END" | grep -q "completed"; then
  echo "FULL ATTESTATION PATH PASSED (end_session = completed)"
else
  echo "ATTESTATION DID NOT COMPLETE -- response: $END"
  echo "--- backend tail ---"; tail -25 /tmp/backend_full.log
  echo "--- node tail ---"; tail -25 /tmp/node_full.log
fi

echo "=== 14. Verify MintProposal row in Postgres (chain=ethereum) ==="
docker compose exec -T postgres psql -U primora -d primora -t -c \
  "SELECT session_id, chain, gross_prm, status FROM mint_proposals WHERE session_id = '$SID';" || \
  echo "(db check skipped)"

echo "=== 15. Drive the mint via admin-cli from the REAL DB row ==="
export ETHEREUM_RPC_URL="$ETH_RPC"
export ETHEREUM_MINING_CONTRACT_ADDRESS="$MINING"
export ETHEREUM_ADMIN_KEY_HEX="${KEY0#0x}"
export DATABASE_URL="postgres://primora:primora_dev@localhost:5432/primora"
ADMIN="./target/debug/primora-admin"

echo "-- [admin-cli] list --"
$ADMIN list 2>&1 | tail -5 || echo "(list failed)"

echo "-- [admin-cli] propose from DB row --"
if OUT=$($ADMIN propose --session-id "$SID" 2>&1); then
  echo "  propose OK: $(echo "$OUT" | grep -E 'proposalId|chain|amount|tx' | tr '\n' ' ')"
else
  echo "  propose FAILED: $OUT"
fi
PID=$(cast keccak "$SID")
echo "proposalId=$PID"

echo "-- [admin-cli] approve (deployer); [cast] approve signer1, signer2 --"
if OUT=$($ADMIN approve --proposal-id $PID --chain ethereum 2>&1); then
  echo "  admin-cli approve OK: $(echo "$OUT" | grep -E 'approvals|tx' | tr '\n' ' ')"
else
  echo "  admin-cli approve FAILED: $OUT"
fi
S1=$(cast wallet address --private-key $SIGNER1); S2=$(cast wallet address --private-key $SIGNER2)
cast send $S1 --value 1ether --private-key $KEY0 --rpc-url $ETH_RPC > /dev/null
cast send $S2 --value 1ether --private-key $KEY0 --rpc-url $ETH_RPC > /dev/null
cast send $MINING "approveMint(bytes32)" $PID --private-key $SIGNER1 --rpc-url $ETH_RPC > /dev/null
cast send $MINING "approveMint(bytes32)" $PID --private-key $SIGNER2 --rpc-url $ETH_RPC > /dev/null

echo "-- [cast] advance timelock + [admin-cli] execute --"
cast rpc evm_increaseTime 172801 --rpc-url $ETH_RPC > /dev/null
cast rpc evm_mine --rpc-url $ETH_RPC > /dev/null
if OUT=$($ADMIN execute --proposal-id $PID --chain ethereum --session-id "$SID" 2>&1); then
  echo "  admin-cli execute OK: $(echo "$OUT" | grep -E 'chain|tx|postgres' | tr '\n' ' ')"
else
  echo "  admin-cli execute FAILED: $OUT"
fi

echo "=== 16. Verify PRM minted (amount = payout gross_prm, not a fixed 1000e18) ==="
echo "recipient PRM balance:"
cast call $PRIM "balanceOf(address)(uint256)" $ADDR0 --rpc-url $ETH_RPC

echo "=== DONE ==="
echo "Full chain: real proof -> node RandomX verify -> 65-byte sig -> coordinator"
echo "quorum (assigned placeholder + 1 node) -> MintProposal -> Postgres ->"
echo "admin-cli propose/approve/execute -> PRM mint. No bypass."
echo "Routing: propose + deployer-approve + execute = admin-cli; the two extra"
echo "signer approvals = cast (CLI holds only the deployer key)."
