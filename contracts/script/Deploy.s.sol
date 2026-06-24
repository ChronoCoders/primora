// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Script} from "forge-std/Script.sol";
import {console2} from "forge-std/console2.sol";
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {PrimToken} from "../src/PrimToken.sol";
import {NodeRegistry} from "../src/NodeRegistry.sol";
import {HouseEdge} from "../src/HouseEdge.sol";
import {OracleAggregator} from "../src/OracleAggregator.sol";
import {MiningContract} from "../src/MiningContract.sol";
import {Treasury} from "../src/Treasury.sol";
import {StakingContract} from "../src/StakingContract.sol";

/// @notice Configurable-decimal mock stablecoin for local testnet reserves.
contract MockUSD is ERC20 {
    uint8 private immutable _dec;

    constructor(string memory n, string memory s, uint8 d) ERC20(n, s) {
        _dec = d;
    }

    /// @notice Mints `amt` tokens to `to`.
    function mint(address to, uint256 amt) external {
        _mint(to, amt);
    }

    /// @notice Returns the configured decimals.
    function decimals() public view override returns (uint8) {
        return _dec;
    }
}

/// @title DeployScript
/// @notice Deploys all seven Primora contracts behind ERC1967 proxies, wires them
///         together for a local Anvil testnet, prints every address, and writes
///         them to deployments/local.json.
contract DeployScript is Script {
    /// @notice Deploys, initializes, and wires the full contract set.
    function run() external {
        vm.startBroadcast();
        address deployer = msg.sender;

        MockUSD usdc = new MockUSD("Mock USDC", "USDC", 6);
        MockUSD usdt = new MockUSD("Mock USDT", "USDT", 6);

        PrimToken primToken = PrimToken(
            address(
                new ERC1967Proxy(
                    address(new PrimToken()), abi.encodeCall(PrimToken.initialize, (deployer))
                )
            )
        );

        HouseEdge houseEdge = HouseEdge(
            address(
                new ERC1967Proxy(
                    address(new HouseEdge()), abi.encodeCall(HouseEdge.initialize, (deployer))
                )
            )
        );

        OracleAggregator oracle = OracleAggregator(
            address(
                new ERC1967Proxy(
                    address(new OracleAggregator()),
                    abi.encodeCall(OracleAggregator.initialize, (deployer, deployer))
                )
            )
        );

        Treasury treasury = Treasury(
            address(
                new ERC1967Proxy(
                    address(new Treasury()),
                    abi.encodeCall(
                        Treasury.initialize, (deployer, address(usdc), address(usdt), deployer)
                    )
                )
            )
        );

        NodeRegistry nodeRegistry = NodeRegistry(
            address(
                new ERC1967Proxy(
                    address(new NodeRegistry()),
                    abi.encodeCall(NodeRegistry.initialize, (deployer, address(primToken)))
                )
            )
        );

        StakingContract staking = StakingContract(
            address(
                new ERC1967Proxy(
                    address(new StakingContract()),
                    abi.encodeCall(StakingContract.initialize, (deployer, address(primToken)))
                )
            )
        );

        MiningContract mining = MiningContract(
            address(
                new ERC1967Proxy(
                    address(new MiningContract()),
                    abi.encodeCall(
                        MiningContract.initialize, (deployer, address(primToken), 1_000_000e18)
                    )
                )
            )
        );

        primToken.setMinter(address(mining));
        primToken.setBurner(address(nodeRegistry));

        mining.addSigner(vm.addr(1));
        mining.addSigner(vm.addr(2));
        mining.addSigner(vm.addr(3));
        mining.addSigner(vm.addr(4));
        mining.addSigner(deployer);

        vm.stopBroadcast();

        console2.log("PrimToken:", address(primToken));
        console2.log("HouseEdge:", address(houseEdge));
        console2.log("OracleAggregator:", address(oracle));
        console2.log("Treasury:", address(treasury));
        console2.log("NodeRegistry:", address(nodeRegistry));
        console2.log("StakingContract:", address(staking));
        console2.log("MiningContract:", address(mining));
        console2.log("MockUSDC:", address(usdc));
        console2.log("MockUSDT:", address(usdt));

        string memory key = "primora-local";
        vm.serializeAddress(key, "PrimToken", address(primToken));
        vm.serializeAddress(key, "HouseEdge", address(houseEdge));
        vm.serializeAddress(key, "OracleAggregator", address(oracle));
        vm.serializeAddress(key, "Treasury", address(treasury));
        vm.serializeAddress(key, "NodeRegistry", address(nodeRegistry));
        vm.serializeAddress(key, "StakingContract", address(staking));
        vm.serializeAddress(key, "MiningContract", address(mining));
        vm.serializeAddress(key, "MockUSDC", address(usdc));
        string memory out = vm.serializeAddress(key, "MockUSDT", address(usdt));
        vm.writeJson(out, "./deployments/local.json");
    }
}
