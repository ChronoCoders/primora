# Deployment

## Local Anvil

Start Anvil in one terminal:

    anvil

Deploy in another:

    forge script script/Deploy.s.sol:DeployScript \
      --rpc-url http://localhost:8545 \
      --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
      --broadcast

The private key above is Anvil's default account 0 (well-known, local only,
never use on a real network).

Addresses are written to `deployments/local.json`.
