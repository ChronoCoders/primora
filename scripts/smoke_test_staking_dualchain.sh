#!/usr/bin/env bash
set -euo pipefail

# Live dual-chain staking boost end-to-end (Decision 4d).
#
# A wallet stakes on BOTH chains, then mines one Ethereum session. The backend
# reads both chains' StakingContracts, computes the combined cross-chain tier
# boost, and the minted gross_prm in mint_proposals carries that boost.
#
#   Ethereum: stake 30,000 PRM, 90-day lock (Days90 -> 1.3x multiplier)
#   Polygon:  stake 30,000 PRM, no lock (1.0x)
#   Combined: 60,000 total -> base 1000 bps (10%) -> x1.3 (ETH lock) = 1300 bps
#   => minted gross_prm == base_gross * 11300 / 10000  (a 13% boost)

KEY0="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
ADDR0="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
ETH_RPC="http://localhost:8545"
POLY_RPC="http://localhost:8546"
SERVICE="http://localhost:3000"
NODE1_KEY="0x00000000000000000000000000000000000000000000000000000000000000a1"
NODE2_KEY="0x00000000000000000000000000000000000000000000000000000000000000a2"
NODE3_KEY="0x00000000000000000000000000000000000000000000000000000000000000a3"
NODE4_KEY="0x00000000000000000000000000000000000000000000000000000000000000a4"

# 30,000 PRM in wei (30000 * 1e18).
STAKE_WEI="30000000000000000000000"
# Polygon per-chain deploy overrides: 100 PRM minimum, no lock required.
POLY_MIN_STAKE="100000000000000000000"

# Host 6379 is often occupied by another local Redis; map primora's Redis to 6380.
REDIS_HOST_PORT="6380"
OVERRIDE="$(mktemp /tmp/primora-staking-override.XXXXXX.yml)"
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

echo "=== 1. Infra: Postgres + Redis (Redis on ${REDIS_HOST_PORT}) ==="
docker compose -f docker-compose.yml -f "$OVERRIDE" up -d postgres redis
sleep 5

echo "=== 2. Two Anvil: :8545 (chain 31337, Ethereum mainnet FORK), :8546 (chain 31338, Polygon) ==="
pkill -f anvil 2>/dev/null || true
sleep 1
# Real Ethereum-mainnet Chainlink XAU feed; present on the fork.
CHAINLINK_XAU="0x214eD9Da11D2fbe465a6fc601a91E62EbEc1a0D6"
FORK_RPC="${ETH_FORK_RPC:-}"
if [ -z "$FORK_RPC" ] && [ -f .env ]; then
  FORK_RPC="$(grep -E '^RPC_URL=' .env | head -1 | cut -d= -f2- | tr -d '[:space:]')" || true
fi
[ -n "$FORK_RPC" ] || { echo "ERROR: no mainnet fork RPC. Set ETH_FORK_RPC or RPC_URL in .env" >&2; exit 1; }
anvil --fork-url "$FORK_RPC" --chain-id 31337 --port 8545 > /tmp/anvil_eth.log  2>&1 &
anvil --chain-id 31338 --port 8546 > /tmp/anvil_poly.log 2>&1 &
for _ in $(seq 1 60); do
  cast block-number --rpc-url $ETH_RPC >/dev/null 2>&1 && cast chain-id --rpc-url $POLY_RPC >/dev/null 2>&1 && break
  sleep 1
done
echo "Ethereum chain-id: $(cast chain-id --rpc-url $ETH_RPC) (forked mainnet block $(cast block-number --rpc-url $ETH_RPC))"
echo "Polygon  chain-id: $(cast chain-id --rpc-url $POLY_RPC)"

echo "=== 3a. Deploy to ETHEREUM (defaults: 10k min, lock required) ==="
cd contracts
forge script script/Deploy.s.sol:DeployScript --rpc-url $ETH_RPC --private-key $KEY0 --broadcast > /tmp/deploy_eth.log 2>&1
ETH_PRIM=$(python3 -c "import json; print(json.load(open('deployments/local.json'))['PrimToken'])")
ETH_STAKING=$(python3 -c "import json; print(json.load(open('deployments/local.json'))['StakingContract'])")
ETH_MINING=$(python3 -c "import json; print(json.load(open('deployments/local.json'))['MiningContract'])")
ETH_ORACLE=$(python3 -c "import json; print(json.load(open('deployments/local.json'))['OracleAggregator'])")
echo "ETH  Prim=$ETH_PRIM Staking=$ETH_STAKING Mining=$ETH_MINING Oracle=$ETH_ORACLE XAU=$CHAINLINK_XAU (real, on fork)"

echo "=== 3b. Deploy to POLYGON (override: 100 PRM min, no lock) ==="
STAKING_MIN_STAKE="$POLY_MIN_STAKE" STAKING_LOCK_REQUIRED=false \
  forge script script/Deploy.s.sol:DeployScript --rpc-url $POLY_RPC --private-key $KEY0 --broadcast > /tmp/deploy_poly.log 2>&1
POLY_PRIM=$(python3 -c "import json; print(json.load(open('deployments/local.json'))['PrimToken'])")
POLY_STAKING=$(python3 -c "import json; print(json.load(open('deployments/local.json'))['StakingContract'])")
POLY_MINING=$(python3 -c "import json; print(json.load(open('deployments/local.json'))['MiningContract'])")
POLY_ORACLE=$(python3 -c "import json; print(json.load(open('deployments/local.json'))['OracleAggregator'])")
echo "POLY Prim=$POLY_PRIM Staking=$POLY_STAKING Mining=$POLY_MINING Oracle=$POLY_ORACLE"
cd ..

# --- TEST-ONLY PRM FUNDING ---------------------------------------------------
# The wallet needs PRM to stake, but PrimToken.mint is restricted to the
# MiningContract (set as minter during deploy). For THIS staking test we
# temporarily point the minter at the deployer (who is the token owner), mint
# the stake amount to the wallet, then restore the MiningContract as minter.
# This is test scaffolding to obtain stakeable PRM, NOT production behavior.
fund_prm() {
  local rpc="$1" prim="$2" mining="$3"
  cast send "$prim" "setMinter(address)" "$ADDR0" --private-key $KEY0 --rpc-url "$rpc" > /dev/null
  cast send "$prim" "mint(address,uint256)" "$ADDR0" "$STAKE_WEI" --private-key $KEY0 --rpc-url "$rpc" > /dev/null
  cast send "$prim" "setMinter(address)" "$mining" --private-key $KEY0 --rpc-url "$rpc" > /dev/null
}

echo "=== 4a. Fund + stake on ETHEREUM (30,000 PRM, Days90 lock = ordinal 1) ==="
fund_prm "$ETH_RPC" "$ETH_PRIM" "$ETH_MINING"
cast send "$ETH_PRIM" "approve(address,uint256)" "$ETH_STAKING" "$STAKE_WEI" --private-key $KEY0 --rpc-url $ETH_RPC > /dev/null
cast send "$ETH_STAKING" "stake(uint256,uint8)" "$STAKE_WEI" 1 --private-key $KEY0 --rpc-url $ETH_RPC > /dev/null
echo "ETH stake: $(cast call $ETH_STAKING 'stakes(address)(uint256,uint8,uint256,uint256,bool)' $ADDR0 --rpc-url $ETH_RPC | tr '\n' ' ')"

echo "=== 4b. Fund + stake on POLYGON (30,000 PRM, no lock; period ignored) ==="
fund_prm "$POLY_RPC" "$POLY_PRIM" "$POLY_MINING"
cast send "$POLY_PRIM" "approve(address,uint256)" "$POLY_STAKING" "$STAKE_WEI" --private-key $KEY0 --rpc-url $POLY_RPC > /dev/null
cast send "$POLY_STAKING" "stake(uint256,uint8)" "$STAKE_WEI" 0 --private-key $KEY0 --rpc-url $POLY_RPC > /dev/null
echo "POLY stake: $(cast call $POLY_STAKING 'stakes(address)(uint256,uint8,uint256,uint256,bool)' $ADDR0 --rpc-url $POLY_RPC | tr '\n' ' ')"

echo "=== 5. Build node-server, verification-service, admin-cli, gen_proof ==="
cargo build -p node-server --bin primora-node 2>&1 | tail -1
cargo build -p verification-service --bin primora-verification 2>&1 | tail -1
cargo build -q -p randomx-verifier --example gen_proof 2>&1 | tail -1

echo "=== 6. Generate a VALID RandomX proof ==="
INPUT="primora-staking-001"
GEN=$(cargo run -q -p randomx-verifier --example gen_proof -- "$INPUT")
PROOF_INPUT=$(echo "$GEN" | sed -n '1p')
PROOF_HASH=$(echo "$GEN" | sed -n '2p')
echo "proof_input=$PROOF_INPUT"
echo "proof_hash=$PROOF_HASH"

echo "=== 7. Start 4 node-servers (genuine 3-of-4 attestation) ==="
NODE1_ADDR=$(cast wallet address --private-key "$NODE1_KEY")
NODE2_ADDR=$(cast wallet address --private-key "$NODE2_KEY")
NODE3_ADDR=$(cast wallet address --private-key "$NODE3_KEY")
NODE4_ADDR=$(cast wallet address --private-key "$NODE4_KEY")
start_node() {
  local id="$1" port="$2" key="$3"
  BIND_ADDR="127.0.0.1:$port" NODE_API_KEY="devkey" NODE_SIGNING_KEY_HEX="${key#0x}" NODE_ID="$id" LOG_LEVEL="info" \
    nohup ./target/debug/primora-node > "/tmp/node_staking_${id}.log" 2>&1 &
  for _ in $(seq 1 60); do
    grep -q "primora node starting" "/tmp/node_staking_${id}.log" 2>/dev/null && { echo "  $id ready on :$port"; return 0; }
    sleep 1
  done
  echo "  $id FAILED to start on :$port" >&2; cat "/tmp/node_staking_${id}.log"; exit 1
}
start_node "node-1" 50051 "$NODE1_KEY"
start_node "node-2" 50052 "$NODE2_KEY"
start_node "node-3" 50053 "$NODE3_KEY"
start_node "node-4" 50054 "$NODE4_KEY"
echo "node signers: node-1=$NODE1_ADDR node-2=$NODE2_ADDR node-3=$NODE3_ADDR node-4=$NODE4_ADDR"

echo "=== 8. Start verification-service wired to BOTH chains' staking contracts ==="
# The per-chain {PREFIX}_RPC_URL is shared by the 4b oracle submitter and the 4d
# staking reader. build_oracle_submitters() treats an RPC URL present without
# its submitter key + aggregator as a FATAL partial config, so wiring the
# staking readers requires also supplying the full oracle submitter triple per
# chain. The deployer (KEY0) is the authorized submitter on each deployed
# OracleAggregator, so TWAP submission succeeds as a side effect.
DATABASE_URL="postgres://primora:primora_dev@localhost:5432/primora" \
REDIS_URL="redis://localhost:${REDIS_HOST_PORT}" \
BIND_ADDR="0.0.0.0:3000" \
CHAIN_ID="31337" \
RPC_URL="$ETH_RPC" \
CHAINLINK_XAU_ADDRESS="$CHAINLINK_XAU" \
SIGNING_KEY_HEX="0000000000000000000000000000000000000000000000000000000000000001" \
ETHEREUM_RPC_URL="$ETH_RPC" \
ETHEREUM_ORACLE_SUBMITTER_KEY_HEX="${KEY0#0x}" \
ETHEREUM_ORACLE_AGGREGATOR_ADDRESS="$ETH_ORACLE" \
ETHEREUM_STAKING_ADDRESS="$ETH_STAKING" \
POLYGON_RPC_URL="$POLY_RPC" \
POLYGON_ORACLE_SUBMITTER_KEY_HEX="${KEY0#0x}" \
POLYGON_ORACLE_AGGREGATOR_ADDRESS="$POLY_ORACLE" \
POLYGON_STAKING_ADDRESS="$POLY_STAKING" \
NODE_ENDPOINTS="node-1=http://localhost:50051,node-2=http://localhost:50052,node-3=http://localhost:50053,node-4=http://localhost:50054" \
NODE_SIGNERS="node-1=$NODE1_ADDR,node-2=$NODE2_ADDR,node-3=$NODE3_ADDR,node-4=$NODE4_ADDR" \
NODE_API_KEY="devkey" \
LOG_LEVEL="info" \
./target/debug/primora-verification > /tmp/backend_staking.log 2>&1 &
BACKEND_PID=$!
sleep 5
curl -s $SERVICE/health; echo

echo "=== 8b. Confirm BOTH staking readers were built at startup ==="
grep -i "staking reader" /tmp/backend_staking.log || echo "(no staking reader log lines)"

echo "=== 9. Create session (chain ethereum, assigned node distinct from endpoint) ==="
NONCE="00"
COMMIT=$(python3 -c "import hashlib; print(hashlib.sha256(bytes.fromhex('$NONCE')).hexdigest())")
SESS=$(curl -s -X POST $SERVICE/sessions -H "Content-Type: application/json" \
  -d "{\"wallet\":\"$ADDR0\",\"client_type\":\"desktop\",\"commodity\":\"Gold\",\"chain\":\"ethereum\",\"assigned_node_id\":\"node-1\",\"commit_hash\":\"$COMMIT\"}")
echo "$SESS"
SID=$(echo "$SESS" | python3 -c "import json,sys; print(json.load(sys.stdin)['session_id'])")

echo "=== 10. Submit proofs: seq1 diff=1 (attested), seq2 high diff so server-derived hashrate is non-zero ==="
curl -s -X POST $SERVICE/sessions/$SID/proofs -H "Content-Type: application/json" \
  -d "{\"sequence\":1,\"proof_hash\":\"$PROOF_HASH\",\"proof_input\":\"$PROOF_INPUT\",\"difficulty\":1}"
echo " <- proof 1 submitted"
sleep 5
curl -s -X POST $SERVICE/sessions/$SID/proofs -H "Content-Type: application/json" \
  -d "{\"sequence\":2,\"proof_hash\":\"$PROOF_HASH\",\"proof_input\":\"$PROOF_INPUT\",\"difficulty\":19000}"
echo " <- proof 2 submitted"
sleep 1

echo "=== 11. End session -- REAL attestation must pass ==="
END=$(curl -s --max-time 90 -X POST $SERVICE/sessions/$SID/end -H "Content-Type: application/json" -d "{\"nonce\":\"$NONCE\"}")
echo "end_session response: $END"

echo "=== 12. THE staking boost log line (base_gross, boost_bps, boosted_gross) ==="
grep -i "staking boost applied\|staking read failed\|staking readers configured" /tmp/backend_staking.log || echo "(no staking boost log lines)"

echo "=== 13. Read mint_proposals.gross_prm from Postgres ==="
DB_GROSS=$(docker compose exec -T postgres psql -U primora -d primora -t -A -c \
  "SELECT gross_prm FROM mint_proposals WHERE session_id = '$SID';" | tr -d '[:space:]')
echo "mint_proposals.gross_prm = $DB_GROSS"
docker compose exec -T postgres psql -U primora -d primora -t -c \
  "SELECT session_id, chain, gross_prm, status FROM mint_proposals WHERE session_id = '$SID';" || true

echo "=== 14. VERIFY: combined boost = 1300 bps, boosted gross minted ==="
if ! echo "$END" | grep -q "completed"; then
  echo "FAIL: end_session did not complete -- response: $END"
  echo "--- backend tail ---"; tail -30 /tmp/backend_staking.log
  echo "--- node-1 tail ---"; tail -20 /tmp/node_staking_node-1.log
  exit 1
fi

BOOST_LINE=$(grep "staking boost applied" /tmp/backend_staking.log | tail -1 || true)
if [ -z "$BOOST_LINE" ]; then
  echo "FAIL: boost was NOT applied (no 'staking boost applied' line) -- boost_bps=0."
  echo "Debug: were both staking readers built? did the reads succeed?"
  grep -i "staking" /tmp/backend_staking.log || true
  exit 1
fi

# Parse base_gross / boost_bps / boosted_gross (calibration values) out of the
# "staking boost applied" line.
read BASE_GROSS BOOST_BPS BOOSTED_GROSS < <(python3 - "$BOOST_LINE" <<'PY'
import re, sys
# Strip ANSI color codes the tracing fmt layer wraps around field separators.
line = re.sub(r'\x1b\[[0-9;]*m', '', sys.argv[1])
def grab(key):
    m = re.search(rf'{key}=(\d+)', line)
    return m.group(1) if m else ''
print(grab('base_gross'), grab('boost_bps'), grab('boosted_gross'))
PY
)
echo "parsed (calib): base_gross=$BASE_GROSS boost_bps=$BOOST_BPS boosted_gross=$BOOSTED_GROSS"

# Parse the base-unit mint amount from the "payout computed" line (mint-scale fix).
PAYOUT_LINE=$(grep "payout computed (mint amount in base units)" /tmp/backend_staking.log | tail -1 || true)
MINT_WEI=$(python3 - "$PAYOUT_LINE" <<'PY'
import re, sys
line = re.sub(r'\x1b\[[0-9;]*m', '', sys.argv[1])
m = re.search(r'mint_amount_wei=(\d+)', line)
print(m.group(1) if m else '')
PY
)
echo "parsed (wei): mint_amount_wei=$MINT_WEI"

EXPECTED_BOOSTED=$(python3 -c "print(($BASE_GROSS * 11300) // 10000)")
EXPECTED_WEI=$(python3 -c "print(int('${BOOSTED_GROSS:-0}') * 10**13)")

PASS=1
if [ "$BOOST_BPS" != "1300" ]; then echo "FAIL: boost_bps=$BOOST_BPS, expected 1300"; PASS=0; fi
if [ "$BOOSTED_GROSS" != "$EXPECTED_BOOSTED" ]; then
  echo "FAIL: boosted_gross(calib)=$BOOSTED_GROSS, expected base*1.13=$EXPECTED_BOOSTED"; PASS=0
fi
if [ -n "$MINT_WEI" ] && [ "$MINT_WEI" = "$EXPECTED_WEI" ]; then
  echo "OK: mint_amount_wei == boosted_calib * 10^13"
else
  echo "FAIL: mint_amount_wei=$MINT_WEI != boosted_calib*10^13=$EXPECTED_WEI"; PASS=0
fi
if [ "$DB_GROSS" = "$MINT_WEI" ]; then
  echo "OK: mint_proposals.gross_prm == mint_amount_wei (wei, boost-carried)"
else
  echo "FAIL: mint_proposals.gross_prm=$DB_GROSS != mint_amount_wei=$MINT_WEI"; PASS=0
fi
if python3 -c "exit(0 if int('${MINT_WEI:-0}') < 10**24 else 1)"; then
  echo "OK: mint $MINT_WEI < ceiling 1e24 (no ceiling revert)"
else
  echo "FAIL: mint exceeds 1e24 ceiling"; PASS=0
fi

echo
if [ "$PASS" = "1" ]; then
  echo "PASS: 4d + mint-scale fix proven end-to-end."
  echo "  - backend read BOTH chains' StakingContracts (see step 8b / step 12)"
  echo "  - combined cross-chain tier: 60,000 PRM total -> 1000 bps base (10%)"
  echo "  - Ethereum 90-day lock -> x1.3 multiplier -> boost_bps=1300 (13%)"
  echo "  - base_gross(calib)=$BASE_GROSS -> boosted_gross(calib)=$BOOSTED_GROSS (= base * 1.13)"
  echo "  - mint_amount_wei=$MINT_WEI = boosted_calib * 10^13 (= $(python3 -c "print(int('${MINT_WEI:-0}')/10**18)") PRM)"
  echo "  - mint_proposals.gross_prm=$DB_GROSS carries the boost in base units"
  echo "  - TEST-ONLY scaffolding: temporary setMinter(deployer) to fund stakeable PRM"
else
  echo "RESULT: FAIL -- see assertions above."
  exit 1
fi

echo "=== DONE ==="
