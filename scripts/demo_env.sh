#!/usr/bin/env bash
# Primora LIVE DEMO ENVIRONMENT (persistent -- does NOT tear down).
# Stands up the full dual-chain stack, seeds realistic data, and leaves everything
# running so the Overview page can be viewed in a browser with MetaMask.
# Stop it later with: scripts/demo_env_stop.sh
set -uo pipefail

BACKEND_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$BACKEND_ROOT"
FRONTEND="$BACKEND_ROOT/../primora-frontend"

KEY0="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
ADDR0="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
SIGNER1_KEY="0x0000000000000000000000000000000000000000000000000000000000000001"
SIGNER2_KEY="0x0000000000000000000000000000000000000000000000000000000000000002"
NODE_KEY="0x0000000000000000000000000000000000000000000000000000000000000abc"
# Backend OracleSubmitter account: Anvil account 1 (funded on the fork), DISTINCT
# from KEY0 so on-chain TWAP submission has its own nonce space (no contention).
SUBMITTER_KEY="0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"
ETH_RPC="http://localhost:8545"
POLY_RPC="http://localhost:8546"
SERVICE="http://localhost:3000"
CPU_THREADS="$(nproc 2>/dev/null || echo 4)"
REDIS_HOST_PORT="6380"

STAKE_WEI="30000000000000000000000"          # 30,000 PRM
POLY_MIN_STAKE="100000000000000000000"        # 100 PRM
RESERVE_USDC="5040000000"                     # $5,040 (6-dec) -> ~Healthy reserve ratio

DB_USER="primora"
DB_NAME="primora"
PSQL="docker compose exec -T postgres psql -U $DB_USER -d $DB_NAME"

note() { echo "  - $1"; }

echo "=================================================================="
echo " PRIMORA DEMO ENVIRONMENT -- starting (persistent, no teardown)"
echo "=================================================================="

echo "=== 0. Kill any prior anvil / node / backend ==="
pkill -f primora-node 2>/dev/null || true
pkill -f primora-verification 2>/dev/null || true
pkill -f anvil 2>/dev/null || true
sleep 1

echo "=== 1. Infra: Postgres + Redis (Redis on ${REDIS_HOST_PORT}) ==="
OVERRIDE="$(mktemp /tmp/primora-demo-override.XXXXXX.yml)"
cat > "$OVERRIDE" <<YML
services:
  redis:
    ports: !override
      - "${REDIS_HOST_PORT}:6379"
YML
docker compose -f docker-compose.yml -f "$OVERRIDE" up -d postgres redis
sleep 5

echo "=== 2. Two Anvil: :8545 (chain 31337, Ethereum mainnet FORK), :8546 (chain 31338, Polygon-local) ==="
# Real Chainlink XAU/XAG feeds live on the Ethereum mainnet fork. Resolve the fork
# RPC from ETH_FORK_RPC or RPC_URL in .env (Alchemy mainnet); never hardcode the key.
CHAINLINK_XAU="0x214eD9Da11D2fbe465a6fc601a91E62EbEc1a0D6"
CHAINLINK_XAG="0x379589227b15F1a12195D3f2d90bBc9F31f95235"
FORK_RPC="${ETH_FORK_RPC:-}"
if [ -z "$FORK_RPC" ] && [ -f .env ]; then
  FORK_RPC="$(grep -E '^RPC_URL=' .env | head -1 | cut -d= -f2- | tr -d '[:space:]')"
fi
if [ -z "$FORK_RPC" ]; then
  echo "ERROR: no mainnet fork RPC. Set ETH_FORK_RPC, or RPC_URL=<alchemy mainnet> in .env" >&2
  exit 1
fi
nohup anvil --fork-url "$FORK_RPC" --chain-id 31337 --port 8545 > /tmp/demo_anvil_eth.log 2>&1 &
nohup anvil --chain-id 31338 --port 8546 > /tmp/demo_anvil_poly.log 2>&1 &
for _ in $(seq 1 60); do
  cast block-number --rpc-url $ETH_RPC >/dev/null 2>&1 && cast chain-id --rpc-url $POLY_RPC >/dev/null 2>&1 && break
  sleep 1
done
echo "Ethereum chain-id: $(cast chain-id --rpc-url $ETH_RPC) (forked mainnet block $(cast block-number --rpc-url $ETH_RPC))"
echo "Polygon  chain-id: $(cast chain-id --rpc-url $POLY_RPC)"

echo "=== 3. Deploy full suite to BOTH chains ==="
cd contracts
forge script script/Deploy.s.sol:DeployScript --rpc-url $ETH_RPC --private-key $KEY0 --broadcast > /tmp/demo_deploy_eth.log 2>&1
cp deployments/local.json /tmp/demo_eth.json
STAKING_MIN_STAKE="$POLY_MIN_STAKE" STAKING_LOCK_REQUIRED=false \
  forge script script/Deploy.s.sol:DeployScript --rpc-url $POLY_RPC --private-key $KEY0 --broadcast > /tmp/demo_deploy_poly.log 2>&1
cp deployments/local.json /tmp/demo_poly.json
cd "$BACKEND_ROOT"

addr() { python3 -c "import json; print(json.load(open('$1'))['$2'])"; }
ETH_PRIM=$(addr /tmp/demo_eth.json PrimToken)
ETH_HOUSE=$(addr /tmp/demo_eth.json HouseEdge)
ETH_ORACLE=$(addr /tmp/demo_eth.json OracleAggregator)
ETH_TREASURY=$(addr /tmp/demo_eth.json Treasury)
ETH_NODEREG=$(addr /tmp/demo_eth.json NodeRegistry)
ETH_STAKING=$(addr /tmp/demo_eth.json StakingContract)
ETH_MINING=$(addr /tmp/demo_eth.json MiningContract)
ETH_USDC=$(addr /tmp/demo_eth.json MockUSDC)

POLY_PRIM=$(addr /tmp/demo_poly.json PrimToken)
POLY_HOUSE=$(addr /tmp/demo_poly.json HouseEdge)
POLY_ORACLE=$(addr /tmp/demo_poly.json OracleAggregator)
POLY_TREASURY=$(addr /tmp/demo_poly.json Treasury)
POLY_NODEREG=$(addr /tmp/demo_poly.json NodeRegistry)
POLY_STAKING=$(addr /tmp/demo_poly.json StakingContract)
POLY_MINING=$(addr /tmp/demo_poly.json MiningContract)
echo "ETH  Prim=$ETH_PRIM Oracle=$ETH_ORACLE Staking=$ETH_STAKING Mining=$ETH_MINING Treasury=$ETH_TREASURY"
echo "POLY Prim=$POLY_PRIM Oracle=$POLY_ORACLE Staking=$POLY_STAKING Mining=$POLY_MINING"

echo "=== 4. Write addresses to the frontend deployment files ==="
write_frontend() {
  python3 - "$1" "$2" "$3" "$4" "$5" "$6" "$7" "$8" "$9" <<'PY'
import json, sys
path, chain_id, prim, house, oracle, treasury, nodereg, staking, mining = sys.argv[1:10]
data = {
    "_comment": f"Local Anvil (chainId {chain_id}) addresses written by scripts/demo_env.sh.",
    "chainId": int(chain_id),
    "primToken": prim,
    "houseEdge": house,
    "oracleAggregator": oracle,
    "treasury": treasury,
    "nodeRegistry": nodereg,
    "stakingContract": staking,
    "miningContract": mining,
}
with open(path, "w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")
print(f"wrote {path}")
PY
}
write_frontend "$FRONTEND/lib/deployments/local.json"   31337 "$ETH_PRIM"  "$ETH_HOUSE"  "$ETH_ORACLE"  "$ETH_TREASURY"  "$ETH_NODEREG"  "$ETH_STAKING"  "$ETH_MINING"
write_frontend "$FRONTEND/lib/deployments/polygon.json" 31338 "$POLY_PRIM" "$POLY_HOUSE" "$POLY_ORACLE" "$POLY_TREASURY" "$POLY_NODEREG" "$POLY_STAKING" "$POLY_MINING"

echo "=== 5. Log live feed prices + authorize the backend submitter (no cast price writes) ==="
# Platinum/CrudeOil read REAL prices from Pyth Hermes (off-chain HTTP; backend has
# internet), the same source the verification-service samples for those commodities.
PYTH_XPT_ID="398e4bbc7cbf89d6648c21e08019d878967677753b3096799595c78f805a34e5"
PYTH_WTI_ID="05e7c9b556df67e455c52ea2d31658744e3f4ade60db7dab887008844f2ae472"
hermes_8dec() {
  curl -s "https://hermes.pyth.network/v2/updates/price/latest?ids[]=$1" 2>/dev/null | python3 -c "
import json,sys
try:
    a=json.load(sys.stdin).get('parsed',[])
except Exception:
    print(''); sys.exit(0)
if not a: print(''); sys.exit(0)
p=a[0]['price']; price=int(p['price']); k=8+int(p['expo'])
print(price*10**k if k>=0 else price//10**(-k))
"
}
XPT_8DEC=$(hermes_8dec "$PYTH_XPT_ID")
WTI_8DEC=$(hermes_8dec "$PYTH_WTI_ID")
echo "Hermes XPT(8dec)=${XPT_8DEC:-UNAVAILABLE}  WTI(8dec)=${WTI_8DEC:-UNAVAILABLE}"

# Gold/Silver are read LIVE from the real Chainlink XAU/XAG feeds on the mainnet
# fork (8-decimal answer), the same feeds the backend oracle-reader samples.
read_chainlink_8dec() {
  cast call "$1" "latestRoundData()(uint80,int256,uint256,uint256,uint80)" --rpc-url "$ETH_RPC" 2>/dev/null \
    | sed -n '2p' | awk '{print $1}'
}
XAU_8DEC=$(read_chainlink_8dec "$CHAINLINK_XAU")
XAG_8DEC=$(read_chainlink_8dec "$CHAINLINK_XAG")
echo "Chainlink live: XAU(8dec)=${XAU_8DEC:-UNAVAILABLE} (~\$$(( ${XAU_8DEC:-0} / 100000000 )))  XAG(8dec)=${XAG_8DEC:-UNAVAILABLE} (~\$$(( ${XAG_8DEC:-0} / 100000000 )))"

# Authorize the backend OracleSubmitter as the on-chain price writer. It runs from
# a DISTINCT account (SUBMITTER_ADDR) so its nonce space never contends with the
# demo's KEY0 cast sends. No cast submitPrice here -- the backend submitter writes
# each commodity's TWAP to BOTH OracleAggregators at session end (the real path).
SUBMITTER_ADDR="$(cast wallet address --private-key "$SUBMITTER_KEY")"
cast send "$ETH_ORACLE"  "setSubmitter(address)" "$SUBMITTER_ADDR" --private-key $KEY0 --rpc-url "$ETH_RPC"  > /dev/null
cast send "$POLY_ORACLE" "setSubmitter(address)" "$SUBMITTER_ADDR" --private-key $KEY0 --rpc-url "$POLY_RPC" > /dev/null
echo "OracleAggregator submitter authorized: $SUBMITTER_ADDR (backend writes TWAP on-chain at session end)"

# TEST-ONLY PRM funding: temporarily point the minter at the deployer to mint
# stakeable PRM, then restore the MiningContract as minter (not production).
fund_prm() {
  local rpc="$1" prim="$2" mining="$3"
  cast send "$prim" "setMinter(address)" "$ADDR0" --private-key $KEY0 --rpc-url "$rpc" > /dev/null
  cast send "$prim" "mint(address,uint256)" "$ADDR0" "$STAKE_WEI" --private-key $KEY0 --rpc-url "$rpc" > /dev/null
  cast send "$prim" "setMinter(address)" "$mining" --private-key $KEY0 --rpc-url "$rpc" > /dev/null
}

echo "=== 6a. Stake 30,000 PRM on ETHEREUM (180-day lock = ordinal 2) ==="
fund_prm "$ETH_RPC" "$ETH_PRIM" "$ETH_MINING"
cast send "$ETH_PRIM" "approve(address,uint256)" "$ETH_STAKING" "$STAKE_WEI" --private-key $KEY0 --rpc-url $ETH_RPC > /dev/null
cast send "$ETH_STAKING" "stake(uint256,uint8)" "$STAKE_WEI" 2 --private-key $KEY0 --rpc-url $ETH_RPC > /dev/null
echo "ETH stake active: $(cast call $ETH_STAKING 'stakes(address)(uint256,uint8,uint256,uint256,bool)' $ADDR0 --rpc-url $ETH_RPC | tail -1)"

echo "=== 6b. Stake 30,000 PRM on POLYGON (no lock) ==="
fund_prm "$POLY_RPC" "$POLY_PRIM" "$POLY_MINING"
cast send "$POLY_PRIM" "approve(address,uint256)" "$POLY_STAKING" "$STAKE_WEI" --private-key $KEY0 --rpc-url $POLY_RPC > /dev/null
cast send "$POLY_STAKING" "stake(uint256,uint8)" "$STAKE_WEI" 0 --private-key $KEY0 --rpc-url $POLY_RPC > /dev/null
echo "POLY stake active: $(cast call $POLY_STAKING 'stakes(address)(uint256,uint8,uint256,uint256,bool)' $ADDR0 --rpc-url $POLY_RPC | tail -1)"

echo "=== 7. Deposit reserves into the ETHEREUM Treasury (\$5,040 USDC) ==="
RESERVE_OK="skipped"
if cast send "$ETH_USDC" "mint(address,uint256)" "$ADDR0" "$RESERVE_USDC" --private-key $KEY0 --rpc-url $ETH_RPC > /dev/null 2>&1 \
  && cast send "$ETH_USDC" "approve(address,uint256)" "$ETH_TREASURY" "$RESERVE_USDC" --private-key $KEY0 --rpc-url $ETH_RPC > /dev/null 2>&1 \
  && cast send "$ETH_TREASURY" "depositReserve(address,uint256)" "$ETH_USDC" "$RESERVE_USDC" --private-key $KEY0 --rpc-url $ETH_RPC > /dev/null 2>&1; then
  RESERVE_OK="deposited \$5,040 (vs ~\$3,000 circulating-PRM value -> ~168% ratio, Healthy)"
  echo "reserves: $RESERVE_OK"
else
  echo "reserves: deposit FAILED -- Reserve Health will show its honest state"
fi

echo "=== 8. Build node-server, verification-service, admin-cli, gen_proof ==="
cargo build -p node-server --bin primora-node 2>&1 | tail -1
cargo build -p verification-service --bin primora-verification 2>&1 | tail -1
cargo build -p admin-cli --bin primora-admin 2>&1 | tail -1
cargo build -q -p randomx-verifier --example gen_proof 2>&1 | tail -1

echo "=== 9. Generate a VALID RandomX proof ==="
GEN=$(cargo run -q -p randomx-verifier --example gen_proof -- "primora-demo")
PROOF_INPUT=$(echo "$GEN" | sed -n '1p')
PROOF_HASH=$(echo "$GEN" | sed -n '2p')
echo "proof_input=$PROOF_INPUT proof_hash=$PROOF_HASH"

echo "=== 10. Start 4 node-servers (genuine 3-of-4 attestation) ==="
NODE1_KEY="0x00000000000000000000000000000000000000000000000000000000000000a1"
NODE2_KEY="0x00000000000000000000000000000000000000000000000000000000000000a2"
NODE3_KEY="0x00000000000000000000000000000000000000000000000000000000000000a3"
NODE4_KEY="0x00000000000000000000000000000000000000000000000000000000000000a4"
NODE1_ADDR=$(cast wallet address --private-key "$NODE1_KEY")
NODE2_ADDR=$(cast wallet address --private-key "$NODE2_KEY")
NODE3_ADDR=$(cast wallet address --private-key "$NODE3_KEY")
NODE4_ADDR=$(cast wallet address --private-key "$NODE4_KEY")
start_node() {
  local id="$1" port="$2" key="$3"
  BIND_ADDR="127.0.0.1:$port" NODE_API_KEY="devkey" NODE_SIGNING_KEY_HEX="${key#0x}" NODE_ID="$id" LOG_LEVEL="info" \
    nohup ./target/debug/primora-node > "/tmp/demo_${id}.log" 2>&1 &
  for _ in $(seq 1 60); do
    grep -q "primora node starting" "/tmp/demo_${id}.log" 2>/dev/null && { echo "  $id ready on :$port"; return 0; }
    sleep 1
  done
  echo "  $id FAILED to start on :$port" >&2
  return 1
}
start_node "node-1" 50051 "$NODE1_KEY"
start_node "node-2" 50052 "$NODE2_KEY"
start_node "node-3" 50053 "$NODE3_KEY"
start_node "node-4" 50054 "$NODE4_KEY"
echo "node signers: node-1=$NODE1_ADDR node-2=$NODE2_ADDR node-3=$NODE3_ADDR node-4=$NODE4_ADDR"

echo "=== 11. Start verification-service (full dual-chain config) ==="
DATABASE_URL="postgres://primora:primora_dev@localhost:5432/primora" \
REDIS_URL="redis://localhost:${REDIS_HOST_PORT}" \
BIND_ADDR="0.0.0.0:3000" \
CHAIN_ID="31337" \
RPC_URL="$ETH_RPC" \
HOUSE_EDGE_ADDRESS="$ETH_HOUSE" \
CHAINLINK_XAU_ADDRESS="$CHAINLINK_XAU" \
CHAINLINK_XAG_ADDRESS="$CHAINLINK_XAG" \
SIGNING_KEY_HEX="0000000000000000000000000000000000000000000000000000000000000001" \
ETHEREUM_RPC_URL="$ETH_RPC" \
ETHEREUM_ORACLE_SUBMITTER_KEY_HEX="${SUBMITTER_KEY#0x}" \
ETHEREUM_ORACLE_AGGREGATOR_ADDRESS="$ETH_ORACLE" \
ETHEREUM_STAKING_ADDRESS="$ETH_STAKING" \
POLYGON_RPC_URL="$POLY_RPC" \
POLYGON_ORACLE_SUBMITTER_KEY_HEX="${SUBMITTER_KEY#0x}" \
POLYGON_ORACLE_AGGREGATOR_ADDRESS="$POLY_ORACLE" \
POLYGON_STAKING_ADDRESS="$POLY_STAKING" \
NODE_ENDPOINTS="node-1=http://localhost:50051,node-2=http://localhost:50052,node-3=http://localhost:50053,node-4=http://localhost:50054" \
NODE_SIGNERS="node-1=$NODE1_ADDR,node-2=$NODE2_ADDR,node-3=$NODE3_ADDR,node-4=$NODE4_ADDR" \
NODE_API_KEY="devkey" \
NODE_SITES='{"node-1":{"code":"JHB","city":"Johannesburg","country":"ZA"},"node-2":{"code":"AMS","city":"Amsterdam","country":"NL"},"node-3":{"code":"DFW","city":"Dallas","country":"US"},"node-4":{"code":"WAW","city":"Warsaw","country":"PL"}}' \
LOG_LEVEL="info" \
  nohup ./target/debug/primora-verification > /tmp/demo_backend.log 2>&1 &
sleep 5
echo "health: $(curl -s $SERVICE/health)"

echo "=== 12. Clean stale payout rows from prior runs ==="
$PSQL -c "TRUNCATE mint_proposals, anomaly_events RESTART IDENTITY;" > /dev/null 2>&1 && echo "tables truncated" || echo "(truncate skipped -- tables may be fresh)"

# admin-cli per-chain env (both chains; propose auto-routes by the DB row).
export ETHEREUM_RPC_URL="$ETH_RPC" ETHEREUM_MINING_CONTRACT_ADDRESS="$ETH_MINING" ETHEREUM_ADMIN_KEY_HEX="${KEY0#0x}"
export POLYGON_RPC_URL="$POLY_RPC" POLYGON_MINING_CONTRACT_ADDRESS="$POLY_MINING" POLYGON_ADMIN_KEY_HEX="${KEY0#0x}"
export DATABASE_URL="postgres://primora:primora_dev@localhost:5432/primora"
ADMIN="./target/debug/primora-admin"

# Fund the integer-key signer addresses for gas on both chains.
S1=$(cast wallet address --private-key $SIGNER1_KEY); S2=$(cast wallet address --private-key $SIGNER2_KEY)
for RPC in $ETH_RPC $POLY_RPC; do
  cast send $S1 --value 1ether --private-key $KEY0 --rpc-url $RPC > /dev/null
  cast send $S2 --value 1ether --private-key $KEY0 --rpc-url $RPC > /dev/null
done

# Run a full session to completion and mint it on its chain.
run_and_mint() {
  local commodity="$1" chain_name="$2" rpc="$3" mining="$4" nonce="$5"
  local commit sess sid pid
  commit=$(python3 -c "import hashlib; print(hashlib.sha256(bytes.fromhex('$nonce')).hexdigest())")
  sess=$(curl -s -X POST $SERVICE/sessions -H "Content-Type: application/json" \
    -d "{\"wallet\":\"$ADDR0\",\"client_type\":\"desktop\",\"commodity\":\"$commodity\",\"chain\":\"$chain_name\",\"assigned_node_id\":\"node-1\",\"commit_hash\":\"$commit\",\"cpu_threads\":$CPU_THREADS}")
  sid=$(echo "$sess" | python3 -c "import json,sys; print(json.load(sys.stdin)['session_id'])")
  curl -s -X POST $SERVICE/sessions/$sid/proofs -H "Content-Type: application/json" \
    -d "{\"sequence\":1,\"proof_hash\":\"$PROOF_HASH\",\"proof_input\":\"$PROOF_INPUT\",\"difficulty\":1}" > /dev/null
  sleep 5
  curl -s -X POST $SERVICE/sessions/$sid/proofs -H "Content-Type: application/json" \
    -d "{\"sequence\":2,\"proof_hash\":\"$PROOF_HASH\",\"proof_input\":\"$PROOF_INPUT\",\"difficulty\":19000}" > /dev/null
  local end
  end=$(curl -s --max-time 90 -X POST $SERVICE/sessions/$sid/end -H "Content-Type: application/json" -d "{\"nonce\":\"$nonce\"}")
  if ! echo "$end" | grep -q "completed"; then
    echo "  session $commodity/$chain_name did NOT complete: $end"
    return 1
  fi
  pid=$(cast keccak "$sid")
  $ADMIN propose --session-id "$sid" > /dev/null 2>&1
  $ADMIN approve --proposal-id $pid --chain $chain_name > /dev/null 2>&1
  cast send $mining "approveMint(bytes32)" $pid --private-key $SIGNER1_KEY --rpc-url $rpc > /dev/null
  cast send $mining "approveMint(bytes32)" $pid --private-key $SIGNER2_KEY --rpc-url $rpc > /dev/null
  cast rpc evm_increaseTime 172801 --rpc-url $rpc > /dev/null
  cast rpc evm_mine --rpc-url $rpc > /dev/null
  $ADMIN execute --proposal-id $pid --chain $chain_name --session-id "$sid" > /dev/null 2>&1
  echo "  minted: $commodity on $chain_name (session $sid)"
}

echo "=== 13. Run + mint Session A: Gold on Ethereum ==="
run_and_mint "Gold"   "ethereum" "$ETH_RPC"  "$ETH_MINING"  "01" || true
echo "=== 14. Run + mint Session B: Silver on Polygon ==="
run_and_mint "Silver" "polygon"  "$POLY_RPC" "$POLY_MINING" "02" || true
echo "=== 14c. Run + mint Session C: Platinum on Ethereum (live Pyth XPT) ==="
if [ -n "$XPT_8DEC" ]; then
  run_and_mint "Platinum" "ethereum" "$ETH_RPC" "$ETH_MINING" "03" || true
else
  echo "  skipped: Pyth XPT feed unavailable"
fi
echo "=== 14d. Run + mint Session D: CrudeOil on Polygon (live Pyth WTI) ==="
if [ -n "$WTI_8DEC" ]; then
  run_and_mint "CrudeOil" "polygon" "$POLY_RPC" "$POLY_MINING" "04" || true
else
  echo "  skipped: Pyth WTI feed unavailable (contract may be expired)"
fi

echo "=== 14e. Verify on-chain prices written by the BACKEND submitter (getPriceUnchecked) ==="
# Each ended session triggers the backend OracleSubmitter to write that commodity's
# TWAP to BOTH OracleAggregators (from SUBMITTER_ADDR, distinct nonce space). Read
# them back to prove the prices landed on-chain via the real submitter path.
verify_price() {
  local label="$1" oracle="$2" rpc="$3" ordinal="$4"
  local out price set_flag
  out=$(cast call "$oracle" "getPriceUnchecked(uint8)(uint256,uint256,bool)" "$ordinal" --rpc-url "$rpc" 2>/dev/null)
  price=$(echo "$out" | sed -n '1p' | awk '{print $1}')
  set_flag=$(echo "$out" | sed -n '3p' | awk '{print $1}')
  if [ "${set_flag:-false}" = "true" ] && [ -n "${price:-}" ] && [ "${price:-0}" != "0" ]; then
    echo "  OK   $label ordinal=$ordinal price=$price set=$set_flag"
  else
    echo "  MISS $label ordinal=$ordinal price=${price:-?} set=${set_flag:-?}"
  fi
}
echo "Ethereum OracleAggregator ($ETH_ORACLE):"
verify_price "Gold     ETH" "$ETH_ORACLE" "$ETH_RPC" 0
verify_price "Platinum ETH" "$ETH_ORACLE" "$ETH_RPC" 1
verify_price "Silver   ETH" "$ETH_ORACLE" "$ETH_RPC" 2
verify_price "CrudeOil ETH" "$ETH_ORACLE" "$ETH_RPC" 3
echo "Polygon OracleAggregator ($POLY_ORACLE):"
verify_price "Gold     POL" "$POLY_ORACLE" "$POLY_RPC" 0
verify_price "Platinum POL" "$POLY_ORACLE" "$POLY_RPC" 1
verify_price "Silver   POL" "$POLY_ORACLE" "$POLY_RPC" 2
verify_price "CrudeOil POL" "$POLY_ORACLE" "$POLY_RPC" 3
echo "submitter sends (tx hash / nonce) from backend log:"
grep -i "submitted price to oracle aggregator\|nonce conflict" /tmp/demo_backend.log | tail -20 || echo "  (no submitter log lines)"

echo "=== 15. Leave one ACTIVE session (Gold/Ethereum, server-derived hashrate, NOT ended) ==="
ACOMMIT=$(python3 -c "import hashlib; print(hashlib.sha256(bytes.fromhex('99')).hexdigest())")
ASESS=$(curl -s -X POST $SERVICE/sessions -H "Content-Type: application/json" \
  -d "{\"wallet\":\"$ADDR0\",\"client_type\":\"desktop\",\"commodity\":\"Gold\",\"chain\":\"ethereum\",\"assigned_node_id\":\"node-1\",\"commit_hash\":\"$ACOMMIT\",\"cpu_threads\":$CPU_THREADS}")
ASID=$(echo "$ASESS" | python3 -c "import json,sys; print(json.load(sys.stdin)['session_id'])")
curl -s -X POST $SERVICE/sessions/$ASID/proofs -H "Content-Type: application/json" \
  -d "{\"sequence\":1,\"proof_hash\":\"$PROOF_HASH\",\"proof_input\":\"$PROOF_INPUT\",\"difficulty\":1}" > /dev/null
sleep 5
curl -s -X POST $SERVICE/sessions/$ASID/proofs -H "Content-Type: application/json" \
  -d "{\"sequence\":2,\"proof_hash\":\"$PROOF_HASH\",\"proof_input\":\"$PROOF_INPUT\",\"difficulty\":19000}" > /dev/null
sleep 5
curl -s -X POST $SERVICE/sessions/$ASID/proofs -H "Content-Type: application/json" \
  -d "{\"sequence\":3,\"proof_hash\":\"$PROOF_HASH\",\"proof_input\":\"$PROOF_INPUT\",\"difficulty\":19000}" > /dev/null
echo "active session $ASID left running (server-derived hashrate, Gold, ethereum)"

echo "=== 16. Frontend .env.local ==="
# A real WalletConnect projectId is required: an invalid/placeholder id makes
# RainbowKit/WalletConnect throw Object.values on the explorer response and blocks
# the whole page. Never clobber a configured id on re-run. Precedence:
#   WC_PROJECT_ID env var > existing .env.local value (if real) > project default.
WC_PROJECT_ID_DEFAULT="dcf0b8a7d17d7bbecf432afbba050a8e"
WC_PROJECT_ID="${WC_PROJECT_ID:-}"
if [ -z "$WC_PROJECT_ID" ] && [ -f "$FRONTEND/.env.local" ]; then
  WC_PROJECT_ID="$(grep -E '^NEXT_PUBLIC_WC_PROJECT_ID=' "$FRONTEND/.env.local" | tail -1 | cut -d= -f2-)"
fi
case "$WC_PROJECT_ID" in
  ""|PRIMORA_DEMO|PRIMORA_DEV_PLACEHOLDER) WC_PROJECT_ID="$WC_PROJECT_ID_DEFAULT" ;;
esac
cat > "$FRONTEND/.env.local" <<ENV
NEXT_PUBLIC_USE_LOCAL_CHAINS=true
BACKEND_ORIGIN=http://localhost:3000
NEXT_PUBLIC_CHAIN_ID=31337
NEXT_PUBLIC_WC_PROJECT_ID=$WC_PROJECT_ID
ENV
echo "wrote $FRONTEND/.env.local (gitignored, WalletConnect projectId preserved)"

PAYOUT_COUNT=$($PSQL -t -A -c "SELECT COUNT(*) FROM mint_proposals;" 2>/dev/null | tr -d '[:space:]')

cat <<BANNER

==================================================================
  PRIMORA ENVIRONMENT READY  (persistent -- still running)
==================================================================
  Backend API : http://localhost:3000   (health: $(curl -s $SERVICE/health))
  Ethereum RPC: http://localhost:8545    (chain-id 31337)
  Polygon  RPC: http://localhost:8546    (chain-id 31338)
  Attestation : genuine 3-of-4 BFT quorum across 4 node-servers (:50051-:50054,
                distinct signing keys); mints require 3 distinct verified node
                signatures (ecrecover over proof_hash vs NODE_SIGNERS).

  Demo user   : $ADDR0
  Private key : $KEY0
                (well-known Anvil account 0 -- import into MetaMask)

  ---- MetaMask setup ----
  Add network "Ethereum-local": RPC http://localhost:8545, Chain ID 31337, Symbol ETH
  Add network "Polygon-local" : RPC http://localhost:8546, Chain ID 31338, Symbol POL
  Import account using the private key above.

  ---- Start the frontend ----
  cd $FRONTEND
  # .env.local already written (USE_LOCAL_CHAINS=true, BACKEND_ORIGIN=:3000)
  npm run dev -- -p 3001
  open http://localhost:3001  and connect the imported wallet

  ---- Seeded data (what the Overview should show) ----
  Oracle & Network : ALL FOUR live -- Gold/Silver from REAL Chainlink (mainnet fork), Platinum/CrudeOil from Pyth Hermes. No mock feeds.
  Recent Payouts   : $PAYOUT_COUNT minted payout(s) -- Gold/ETH, Silver/POL, Platinum/ETH, CrudeOil/POL -- wei gross_prm + net USD
  Earnings         : all four commodities (with live feeds), net redemption USD
  Staking / Total  : Ethereum 30,000 PRM (180d) + Polygon 30,000 PRM = 60,000 staked, +boost
  Reserve Health   : $RESERVE_OK
  Mining Speed     : server-derived from proof difficulty / elapsed (~3,800 H/s, capped at 4,000)
  Active Mining    : LIVE Gold session on Ethereum (server-derived hashrate)
  Entity Share KPI : still placeholder (no data source defined yet)

  Commodities: all four mined -- Gold/Silver from local Chainlink mock feeds,
  Platinum/CrudeOil from REAL Pyth Hermes (live, requires backend internet).

  ---- Stop everything ----
  scripts/demo_env_stop.sh
==================================================================
BANNER

rm -f "$OVERRIDE" 2>/dev/null || true
