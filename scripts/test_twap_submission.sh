#!/usr/bin/env bash
set -euo pipefail

# Anvil account 0 (well-known local key)
DEPLOYER_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
DEPLOYER_ADDR="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
RPC="http://localhost:8545"

echo "=== 1. Start Anvil ==="
pkill anvil 2>/dev/null || true
sleep 1
anvil > /tmp/anvil_twap.log 2>&1 &
sleep 3
cast block-number --rpc-url $RPC

echo "=== 2. Deploy contracts ==="
cd contracts
forge script script/Deploy.s.sol:DeployScript \
  --rpc-url $RPC \
  --private-key $DEPLOYER_KEY \
  --broadcast > /tmp/deploy_twap.log 2>&1

# Extract OracleAggregator address from deployments/local.json
ORACLE_ADDR=$(python3 -c "import json; print(json.load(open('deployments/local.json'))['OracleAggregator'])")
echo "OracleAggregator: $ORACLE_ADDR"
cd ..

echo "=== 3. Verify deployer is authorizedSubmitter ==="
SUBMITTER=$(cast call $ORACLE_ADDR "authorizedSubmitter()(address)" --rpc-url $RPC)
echo "authorizedSubmitter: $SUBMITTER"

echo "=== 4. Submit a price directly via cast (simulating backend) ==="
# Gold = 0, price = 320400000000 (8-decimal $3204)
cast send $ORACLE_ADDR "submitPrice(uint8,uint256)" 0 320400000000 \
  --private-key $DEPLOYER_KEY \
  --rpc-url $RPC

echo "=== 5. Read back the stored price ==="
cast call $ORACLE_ADDR "getPriceUnchecked(uint8)(uint256,uint256,bool)" 0 --rpc-url $RPC

echo "=== 6. Test divergence guard: submit a >2% jump, expect revert ==="
# 350000000000 is ~9% above 320400000000 -- should revert
set +e
cast send $ORACLE_ADDR "submitPrice(uint8,uint256)" 0 350000000000 \
  --private-key $DEPLOYER_KEY \
  --rpc-url $RPC 2>&1 | grep -i "revert\|PriceDiverged" && echo "DIVERGENCE GUARD WORKS" || echo "WARNING: divergence not caught"
set -e

echo "=== 7. Submit within 2%, expect success ==="
# 322000000000 is ~0.5% above -- should succeed
cast send $ORACLE_ADDR "submitPrice(uint8,uint256)" 0 322000000000 \
  --private-key $DEPLOYER_KEY \
  --rpc-url $RPC

echo "=== 8. Read final price ==="
cast call $ORACLE_ADDR "getPriceUnchecked(uint8)(uint256,uint256,bool)" 0 --rpc-url $RPC

echo "=== 9. Stop Anvil ==="
pkill anvil 2>/dev/null || true

echo "=== DONE ==="
