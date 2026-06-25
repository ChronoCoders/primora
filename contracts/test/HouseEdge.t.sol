// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.24;

import {Test, Vm} from "forge-std/Test.sol";
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";
import {HouseEdge} from "../src/HouseEdge.sol";

/// @title HouseEdgeTest
/// @notice Unit tests for {HouseEdge} timelocked edge changes and reserve-driven
///         dynamic adjustment, exercised against an ERC1967 proxy.
contract HouseEdgeTest is Test {
    HouseEdge internal edge;

    address internal owner = address(0xA11CE);

    bytes32 internal constant P1 = keccak256("proposal-1");

    /// @notice Deploys the implementation behind a proxy and initializes it.
    function setUp() public {
        HouseEdge impl = new HouseEdge();
        edge = HouseEdge(
            address(new ERC1967Proxy(address(impl), abi.encodeCall(HouseEdge.initialize, (owner))))
        );
    }

    /// @notice The contract initializes to the default operating edge.
    function test_initialize() public view {
        assertEq(edge.currentEdgeBps(), 1_700);
        assertEq(edge.operatorDefaultBps(), 1_700);
    }

    /// @notice A proposal executes after the timelock and updates the edge.
    function test_propose_and_execute() public {
        vm.prank(owner);
        edge.proposeEdgeChange(P1, 2_000);
        vm.warp(block.timestamp + edge.TIMELOCK_DELAY() + 1);
        vm.prank(owner);
        edge.executeEdgeChange(P1);
        assertEq(edge.currentEdgeBps(), 2_000);
        assertEq(edge.operatorDefaultBps(), 2_000);
    }

    /// @notice Executing before the timelock elapses reverts.
    function test_execute_revert_timelock() public {
        vm.prank(owner);
        edge.proposeEdgeChange(P1, 2_000);
        uint256 executionTime = block.timestamp + edge.TIMELOCK_DELAY();
        vm.expectRevert(
            abi.encodeWithSelector(HouseEdge.TimelockNotExpired.selector, executionTime, block.timestamp)
        );
        vm.prank(owner);
        edge.executeEdgeChange(P1);
    }

    /// @notice A pending proposal can be cancelled.
    function test_cancel_proposal() public {
        vm.prank(owner);
        edge.proposeEdgeChange(P1, 2_000);
        vm.prank(owner);
        edge.cancelEdgeChange(P1);
        (,,, bool cancelled) = edge.proposals(P1);
        assertTrue(cancelled);
    }

    /// @notice Executing a cancelled proposal reverts.
    function test_execute_revert_cancelled() public {
        vm.startPrank(owner);
        edge.proposeEdgeChange(P1, 2_000);
        edge.cancelEdgeChange(P1);
        vm.stopPrank();
        vm.warp(block.timestamp + edge.TIMELOCK_DELAY() + 1);
        vm.expectRevert(HouseEdge.ProposalAlreadyCancelled.selector);
        vm.prank(owner);
        edge.executeEdgeChange(P1);
    }

    /// @notice Proposing below the minimum edge reverts.
    function test_out_of_range_revert() public {
        vm.expectRevert(abi.encodeWithSelector(HouseEdge.EdgeOutOfRange.selector, uint256(900), uint256(1_000), uint256(2_500)));
        vm.prank(owner);
        edge.proposeEdgeChange(P1, 900);
    }

    /// @notice Proposing above the maximum edge reverts.
    function test_out_of_range_max_revert() public {
        vm.expectRevert(abi.encodeWithSelector(HouseEdge.EdgeOutOfRange.selector, uint256(2_600), uint256(1_000), uint256(2_500)));
        vm.prank(owner);
        edge.proposeEdgeChange(P1, 2_600);
    }

    /// @notice A reserve ratio below 110% raises the edge to 24%.
    function test_adjust_reserve_below_110() public {
        vm.prank(owner);
        edge.adjustForReserve(10_500);
        assertEq(edge.currentEdgeBps(), 2_400);
    }

    /// @notice A reserve ratio below 120% raises the edge to 22%.
    function test_adjust_reserve_below_120() public {
        vm.prank(owner);
        edge.adjustForReserve(11_500);
        assertEq(edge.currentEdgeBps(), 2_200);
    }

    /// @notice A reserve ratio at or above 150% reverts the edge to the default.
    function test_adjust_reserve_above_150() public {
        vm.startPrank(owner);
        edge.adjustForReserve(11_500);
        assertEq(edge.currentEdgeBps(), 2_200);
        edge.adjustForReserve(16_000);
        vm.stopPrank();
        assertEq(edge.currentEdgeBps(), edge.operatorDefaultBps());
    }

    /// @notice Adjusting to the already-current edge emits no event.
    function test_adjust_reserve_no_change() public {
        vm.recordLogs();
        vm.prank(owner);
        edge.adjustForReserve(16_000);
        Vm.Log[] memory logs = vm.getRecordedLogs();
        assertEq(logs.length, 0);
        assertEq(edge.currentEdgeBps(), 1_700);
    }

    /// @notice Applying the default edge to a gross amount returns the net amount.
    function test_apply_edge() public view {
        assertEq(edge.applyEdge(1_000_000), 830_000);
    }

    /// @notice Executing a proposal twice reverts on the second call.
    function test_double_execute_revert() public {
        vm.prank(owner);
        edge.proposeEdgeChange(P1, 2_000);
        vm.warp(block.timestamp + edge.TIMELOCK_DELAY() + 1);
        vm.startPrank(owner);
        edge.executeEdgeChange(P1);
        vm.expectRevert(HouseEdge.ProposalAlreadyExecuted.selector);
        edge.executeEdgeChange(P1);
        vm.stopPrank();
    }
}
