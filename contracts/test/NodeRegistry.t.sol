// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";
import {PrimToken} from "../src/PrimToken.sol";
import {NodeRegistry} from "../src/NodeRegistry.sol";

/// @title NodeRegistryTest
/// @notice Unit tests for {NodeRegistry} staking and slashing, exercised against
///         a live {PrimToken} proxy configured with the registry as burner.
contract NodeRegistryTest is Test {
    PrimToken internal token;
    NodeRegistry internal registry;

    address internal owner = address(0xA11CE);
    address internal minter = address(0xB0B);
    address internal operator = address(0x0DDE);

    bytes32 internal constant NODE_ID = keccak256("node-1");
    bytes32 internal constant PROPOSER = keccak256("observer-1");
    bytes32 internal constant CONFIRMER = keccak256("observer-2");

    uint256 internal constant STAKE = 10_000e18;

    /// @notice Deploys both contracts behind proxies, wires minter/burner, and
    ///         funds the operator with a registrable stake.
    function setUp() public {
        PrimToken tokenImpl = new PrimToken();
        token = PrimToken(
            address(new ERC1967Proxy(address(tokenImpl), abi.encodeCall(PrimToken.initialize, (owner))))
        );

        NodeRegistry registryImpl = new NodeRegistry();
        registry = NodeRegistry(
            address(
                new ERC1967Proxy(
                    address(registryImpl),
                    abi.encodeCall(NodeRegistry.initialize, (owner, address(token)))
                )
            )
        );

        vm.startPrank(owner);
        token.setMinter(minter);
        token.setBurner(address(registry));
        vm.stopPrank();

        vm.prank(minter);
        token.mint(operator, STAKE);
    }

    /// @notice Registers a node and stakes via allowance; verifies stored state.
    function _register() internal {
        vm.startPrank(operator);
        token.approve(address(registry), STAKE);
        registry.registerNode(NODE_ID, STAKE);
        vm.stopPrank();
    }

    /// @notice Proposes and confirms a slash so it is ready to execute.
    function _proposeAndConfirm(bytes32 proposalId, NodeRegistry.SlashReason reason) internal {
        vm.startPrank(owner);
        registry.proposeSlash(proposalId, NODE_ID, PROPOSER, reason);
        registry.confirmSlash(proposalId, CONFIRMER);
        vm.stopPrank();
    }

    /// @notice A valid stake registers the node as active.
    function test_register_node() public {
        _register();
        (address op, uint256 staked,, bool active,) = registry.nodes(NODE_ID);
        assertEq(op, operator);
        assertEq(staked, STAKE);
        assertTrue(active);
        assertEq(token.balanceOf(address(registry)), STAKE);
    }

    /// @notice Registering below {MIN_STAKE} reverts.
    function test_register_revert_insufficient_stake() public {
        vm.startPrank(operator);
        token.approve(address(registry), STAKE);
        vm.expectRevert(NodeRegistry.InsufficientStake.selector);
        registry.registerNode(NODE_ID, STAKE - 1);
        vm.stopPrank();
    }

    /// @notice Registering an already-active node id reverts.
    function test_register_revert_duplicate() public {
        _register();
        vm.prank(minter);
        token.mint(operator, STAKE);
        vm.startPrank(operator);
        token.approve(address(registry), STAKE);
        vm.expectRevert(NodeRegistry.NodeAlreadyRegistered.selector);
        registry.registerNode(NODE_ID, STAKE);
        vm.stopPrank();
    }

    /// @notice Deregistration returns the full stake and clears active state.
    function test_deregister_node() public {
        _register();
        vm.prank(operator);
        registry.deregisterNode(NODE_ID);
        (, uint256 staked,, bool active,) = registry.nodes(NODE_ID);
        assertFalse(active);
        assertEq(staked, 0);
        assertEq(token.balanceOf(operator), STAKE);
        assertEq(token.balanceOf(address(registry)), 0);
    }

    /// @notice A proposal records the proposer as the first confirmation.
    function test_propose_slash() public {
        _register();
        vm.prank(owner);
        registry.proposeSlash(bytes32("p1"), NODE_ID, PROPOSER, NodeRegistry.SlashReason.InvalidAttestation);
        (,, uint8 confirmations,,,) = registry.slashProposals(bytes32("p1"));
        assertEq(confirmations, 1);
    }

    /// @notice A second observer confirmation reaches the 2-of-3 threshold.
    function test_confirm_slash() public {
        _register();
        _proposeAndConfirm(bytes32("p1"), NodeRegistry.SlashReason.InvalidAttestation);
        (,, uint8 confirmations,,,) = registry.slashProposals(bytes32("p1"));
        assertEq(confirmations, 2);
    }

    /// @notice A first InvalidAttestation slash burns 10% after the veto window.
    function test_execute_slash_first_violation() public {
        _register();
        _proposeAndConfirm(bytes32("p1"), NodeRegistry.SlashReason.InvalidAttestation);
        vm.warp(block.timestamp + registry.VETO_WINDOW() + 1);
        vm.prank(owner);
        registry.executeSlash(bytes32("p1"));
        (, uint256 staked, uint8 violations,,) = registry.nodes(NODE_ID);
        assertEq(staked, 9_000e18);
        assertEq(violations, 1);
        assertEq(token.totalSupply(), 9_000e18);
        assertEq(token.balanceOf(address(registry)), 9_000e18);
    }

    /// @notice Executing before the veto window closes reverts.
    function test_execute_slash_revert_veto_window() public {
        _register();
        _proposeAndConfirm(bytes32("p1"), NodeRegistry.SlashReason.InvalidAttestation);
        vm.expectRevert(NodeRegistry.VetoWindowActive.selector);
        vm.prank(owner);
        registry.executeSlash(bytes32("p1"));
    }

    /// @notice An operator veto within the window halts the proposal.
    function test_veto_slash() public {
        _register();
        _proposeAndConfirm(bytes32("p1"), NodeRegistry.SlashReason.InvalidAttestation);
        vm.prank(owner);
        registry.vetoSlash(bytes32("p1"));
        (,,,,, bool vetoed) = registry.slashProposals(bytes32("p1"));
        assertTrue(vetoed);
    }

    /// @notice Vetoing after the window has closed reverts.
    function test_veto_slash_revert_expired() public {
        _register();
        _proposeAndConfirm(bytes32("p1"), NodeRegistry.SlashReason.InvalidAttestation);
        vm.warp(block.timestamp + registry.VETO_WINDOW() + 1);
        vm.expectRevert(NodeRegistry.VetoWindowExpired.selector);
        vm.prank(owner);
        registry.vetoSlash(bytes32("p1"));
    }

    /// @notice Three InvalidAttestation slashes escalate to full burn and deregister.
    function test_third_violation_deregisters() public {
        _register();

        _proposeAndConfirm(bytes32("p1"), NodeRegistry.SlashReason.InvalidAttestation);
        vm.warp(block.timestamp + registry.VETO_WINDOW() + 1);
        vm.prank(owner);
        registry.executeSlash(bytes32("p1"));

        _proposeAndConfirm(bytes32("p2"), NodeRegistry.SlashReason.InvalidAttestation);
        vm.warp(block.timestamp + registry.VETO_WINDOW() + 1);
        vm.prank(owner);
        registry.executeSlash(bytes32("p2"));

        _proposeAndConfirm(bytes32("p3"), NodeRegistry.SlashReason.InvalidAttestation);
        vm.warp(block.timestamp + registry.VETO_WINDOW() + 1);
        vm.prank(owner);
        registry.executeSlash(bytes32("p3"));

        (, uint256 staked, uint8 violations, bool active,) = registry.nodes(NODE_ID);
        assertFalse(active);
        assertEq(violations, 3);
        assertEq(staked, 0);
        assertEq(token.totalSupply(), 0);
    }
}
