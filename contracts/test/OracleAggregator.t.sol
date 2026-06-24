// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";
import {OwnableUpgradeable} from "@openzeppelin/contracts-upgradeable/access/OwnableUpgradeable.sol";
import {OracleAggregator} from "../src/OracleAggregator.sol";

/// @title OracleAggregatorTest
/// @notice Unit tests for {OracleAggregator} price submission, the divergence
///         guard, staleness, and submitter rotation.
contract OracleAggregatorTest is Test {
    OracleAggregator internal oracle;

    address internal owner = address(0xA11CE);
    address internal submitter = address(0x5B17);
    address internal stranger = address(0xBAD);

    uint8 internal constant GOLD = uint8(OracleAggregator.Commodity.Gold);
    uint256 internal constant GOLD_PRICE = 320_400_000_000;

    /// @notice Deploys the implementation behind a proxy and initializes it.
    function setUp() public {
        OracleAggregator impl = new OracleAggregator();
        oracle = OracleAggregator(
            address(
                new ERC1967Proxy(address(impl), abi.encodeCall(OracleAggregator.initialize, (owner, submitter)))
            )
        );
    }

    /// @notice The authorized submitter is set at initialization.
    function test_initialize() public view {
        assertEq(oracle.authorizedSubmitter(), submitter);
        assertEq(oracle.owner(), owner);
    }

    /// @notice A first submission stores the price and emits the event.
    function test_submit_price_first() public {
        vm.expectEmit(true, false, false, true);
        emit OracleAggregator.PriceSubmitted(GOLD, GOLD_PRICE, block.timestamp);
        vm.prank(submitter);
        oracle.submitPrice(GOLD, GOLD_PRICE);
        (uint256 price, uint256 updatedAt, bool initialized) = oracle.getPriceUnchecked(GOLD);
        assertEq(price, GOLD_PRICE);
        assertEq(updatedAt, block.timestamp);
        assertTrue(initialized);
    }

    /// @notice A non-submitter cannot submit a price.
    function test_submit_price_revert_not_authorized() public {
        vm.expectRevert(OracleAggregator.NotAuthorizedSubmitter.selector);
        vm.prank(stranger);
        oracle.submitPrice(GOLD, GOLD_PRICE);
    }

    /// @notice A zero price is rejected.
    function test_submit_price_revert_zero() public {
        vm.expectRevert(OracleAggregator.ZeroPrice.selector);
        vm.prank(submitter);
        oracle.submitPrice(GOLD, 0);
    }

    /// @notice A second price within 2% of the stored price is accepted.
    function test_submit_within_divergence() public {
        vm.startPrank(submitter);
        oracle.submitPrice(GOLD, GOLD_PRICE);
        oracle.submitPrice(GOLD, 321_000_000_000);
        vm.stopPrank();
        (uint256 price,,) = oracle.getPriceUnchecked(GOLD);
        assertEq(price, 321_000_000_000);
    }

    /// @notice A second price beyond 2% of the stored price is rejected.
    function test_submit_revert_divergence() public {
        vm.startPrank(submitter);
        oracle.submitPrice(GOLD, GOLD_PRICE);
        uint256 diverged = 350_000_000_000;
        uint256 diff = diverged - GOLD_PRICE;
        uint256 bps = diff * 10_000 / GOLD_PRICE;
        vm.expectRevert(
            abi.encodeWithSelector(OracleAggregator.PriceDiverged.selector, diverged, GOLD_PRICE, bps)
        );
        oracle.submitPrice(GOLD, diverged);
        vm.stopPrank();
    }

    /// @notice A fresh price is returned with its timestamp.
    function test_get_price() public {
        vm.prank(submitter);
        oracle.submitPrice(GOLD, GOLD_PRICE);
        (uint256 price, uint256 updatedAt) = oracle.getPrice(GOLD);
        assertEq(price, GOLD_PRICE);
        assertEq(updatedAt, block.timestamp);
    }

    /// @notice A price older than {MAX_PRICE_AGE} reverts as stale.
    function test_get_price_revert_stale() public {
        vm.prank(submitter);
        oracle.submitPrice(GOLD, GOLD_PRICE);
        uint256 submittedAt = block.timestamp;
        vm.warp(block.timestamp + oracle.MAX_PRICE_AGE() + 1);
        vm.expectRevert(
            abi.encodeWithSelector(OracleAggregator.PriceStale.selector, submittedAt, block.timestamp)
        );
        oracle.getPrice(GOLD);
    }

    /// @notice Querying an uninitialized commodity reverts.
    function test_get_price_revert_not_initialized() public {
        vm.expectRevert(OracleAggregator.PriceNotInitialized.selector);
        oracle.getPrice(GOLD);
    }

    /// @notice The owner can rotate the authorized submitter.
    function test_set_submitter() public {
        vm.prank(owner);
        oracle.setSubmitter(stranger);
        assertEq(oracle.authorizedSubmitter(), stranger);
    }

    /// @notice A non-owner cannot rotate the submitter.
    function test_set_submitter_revert_not_owner() public {
        vm.expectRevert(
            abi.encodeWithSelector(OwnableUpgradeable.OwnableUnauthorizedAccount.selector, stranger)
        );
        vm.prank(stranger);
        oracle.setSubmitter(stranger);
    }

    /// @notice The owner force-submit override bypasses the divergence guard.
    function test_force_submit_bypasses_divergence() public {
        vm.prank(submitter);
        oracle.submitPrice(GOLD, GOLD_PRICE);
        vm.prank(owner);
        oracle.forceSubmitPrice(GOLD, 350_000_000_000);
        (uint256 price,,) = oracle.getPriceUnchecked(GOLD);
        assertEq(price, 350_000_000_000);
    }

    /// @notice getPriceUnchecked returns stale data without reverting.
    function test_get_price_unchecked() public {
        vm.prank(submitter);
        oracle.submitPrice(GOLD, GOLD_PRICE);
        uint256 submittedAt = block.timestamp;
        vm.warp(block.timestamp + 2 hours);
        (uint256 price, uint256 updatedAt, bool initialized) = oracle.getPriceUnchecked(GOLD);
        assertEq(price, GOLD_PRICE);
        assertEq(updatedAt, submittedAt);
        assertTrue(initialized);
    }
}
