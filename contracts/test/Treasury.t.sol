// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {OwnableUpgradeable} from "@openzeppelin/contracts-upgradeable/access/OwnableUpgradeable.sol";
import {Treasury} from "../src/Treasury.sol";

/// @notice 6-decimal mock stablecoin for reserve tests.
contract MockUSD is ERC20 {
    constructor() ERC20("Mock", "MUSD") {}

    /// @notice Mints `amt` tokens to `to`.
    function mint(address to, uint256 amt) external {
        _mint(to, amt);
    }

    /// @notice Overrides decimals to 6 to match USDC/USDT.
    function decimals() public pure override returns (uint8) {
        return 6;
    }
}

/// @title TreasuryTest
/// @notice Unit tests for {Treasury} reserve management, redemption, and the
///         reserve-ratio-driven system state.
contract TreasuryTest is Test {
    Treasury internal treasury;
    MockUSD internal usdc;
    MockUSD internal usdt;
    MockUSD internal other;

    address internal owner = address(0xA11CE);
    address internal redeemer = address(0x5EED);
    address internal user = address(0xCAFE);
    address internal stranger = address(0xBAD);

    /// @notice Deploys mocks and the treasury proxy, funding the depositor.
    function setUp() public {
        usdc = new MockUSD();
        usdt = new MockUSD();
        other = new MockUSD();

        Treasury impl = new Treasury();
        treasury = Treasury(
            address(
                new ERC1967Proxy(
                    address(impl),
                    abi.encodeCall(
                        Treasury.initialize, (owner, address(usdc), address(usdt), redeemer)
                    )
                )
            )
        );

        usdc.mint(owner, 1_000_000e6);
        usdt.mint(owner, 1_000_000e6);
    }

    /// @notice Deposits `amount` of `token` from the owner into the treasury.
    function _deposit(MockUSD token, uint256 amount) internal {
        vm.startPrank(owner);
        token.approve(address(treasury), amount);
        treasury.depositReserve(address(token), amount);
        vm.stopPrank();
    }

    /// @notice Initialization wires the reserve tokens and redeemer.
    function test_initialize() public view {
        assertEq(address(treasury.usdc()), address(usdc));
        assertEq(address(treasury.usdt()), address(usdt));
        assertEq(treasury.authorizedRedeemer(), redeemer);
    }

    /// @notice A USDC deposit credits the treasury balance and emits the event.
    function test_deposit_reserve() public {
        vm.startPrank(owner);
        usdc.approve(address(treasury), 1_000e6);
        vm.expectEmit(true, true, false, true);
        emit Treasury.ReserveDeposited(address(usdc), owner, 1_000e6);
        treasury.depositReserve(address(usdc), 1_000e6);
        vm.stopPrank();
        assertEq(usdc.balanceOf(address(treasury)), 1_000e6);
    }

    /// @notice Depositing an unsupported token reverts.
    function test_deposit_revert_unsupported() public {
        other.mint(owner, 1_000e6);
        vm.startPrank(owner);
        other.approve(address(treasury), 1_000e6);
        vm.expectRevert(Treasury.UnsupportedToken.selector);
        treasury.depositReserve(address(other), 1_000e6);
        vm.stopPrank();
    }

    /// @notice Depositing zero reverts.
    function test_deposit_revert_zero() public {
        vm.expectRevert(Treasury.ZeroAmount.selector);
        vm.prank(owner);
        treasury.depositReserve(address(usdc), 0);
    }

    /// @notice The owner can withdraw deposited reserves.
    function test_withdraw_reserve() public {
        _deposit(usdc, 1_000e6);
        vm.prank(owner);
        treasury.withdrawReserve(address(usdc), user, 400e6);
        assertEq(usdc.balanceOf(user), 400e6);
        assertEq(usdc.balanceOf(address(treasury)), 600e6);
    }

    /// @notice Withdrawing more than the balance reverts.
    function test_withdraw_revert_insufficient() public {
        _deposit(usdc, 1_000e6);
        vm.expectRevert(abi.encodeWithSelector(Treasury.InsufficientReserve.selector, uint256(2_000e6), uint256(1_000e6)));
        vm.prank(owner);
        treasury.withdrawReserve(address(usdc), user, 2_000e6);
    }

    /// @notice A non-owner cannot withdraw.
    function test_withdraw_revert_not_owner() public {
        _deposit(usdc, 1_000e6);
        vm.expectRevert(
            abi.encodeWithSelector(OwnableUpgradeable.OwnableUnauthorizedAccount.selector, stranger)
        );
        vm.prank(stranger);
        treasury.withdrawReserve(address(usdc), user, 100e6);
    }

    /// @notice The authorized redeemer pays a redemption from reserves.
    function test_pay_redemption() public {
        _deposit(usdc, 1_000e6);
        vm.prank(redeemer);
        treasury.payRedemption(user, address(usdc), 300e6);
        assertEq(usdc.balanceOf(user), 300e6);
        assertEq(treasury.totalRedeemedUsd(), 300e6);
    }

    /// @notice A non-redeemer cannot pay a redemption.
    function test_pay_redemption_revert_not_authorized() public {
        _deposit(usdc, 1_000e6);
        vm.expectRevert(Treasury.NotAuthorizedRedeemer.selector);
        vm.prank(stranger);
        treasury.payRedemption(user, address(usdc), 100e6);
    }

    /// @notice Paying more than the reserve reverts.
    function test_pay_redemption_revert_insufficient() public {
        _deposit(usdc, 100e6);
        vm.expectRevert(abi.encodeWithSelector(Treasury.InsufficientReserve.selector, uint256(1_000e6), uint256(100e6)));
        vm.prank(redeemer);
        treasury.payRedemption(user, address(usdc), 1_000e6);
    }

    /// @notice Total reserves sum USDC and USDT balances.
    function test_total_reserve_usd() public {
        _deposit(usdc, 600e6);
        _deposit(usdt, 400e6);
        assertEq(treasury.totalReserveUsd(), 1_000e6);
    }

    /// @notice A 150% reserve ratio computes to 15000 bps.
    function test_reserve_ratio_healthy() public {
        _deposit(usdc, 1_500e6);
        assertEq(treasury.reserveRatioBps(1_000e6), 15_000);
    }

    /// @notice Zero circulating value yields the maximum ratio.
    function test_reserve_ratio_zero_circulating() public view {
        assertEq(treasury.reserveRatioBps(0), type(uint256).max);
    }

    /// @notice A 160% ratio is Healthy.
    function test_system_state_healthy() public {
        _deposit(usdc, 1_600e6);
        assertEq(uint256(treasury.systemState(1_000e6)), uint256(Treasury.SystemState.Healthy));
    }

    /// @notice A 130% ratio is Caution.
    function test_system_state_caution() public {
        _deposit(usdc, 1_300e6);
        assertEq(uint256(treasury.systemState(1_000e6)), uint256(Treasury.SystemState.Caution));
    }

    /// @notice A 110% ratio pauses staking.
    function test_system_state_staking_paused() public {
        _deposit(usdc, 1_100e6);
        assertEq(uint256(treasury.systemState(1_000e6)), uint256(Treasury.SystemState.StakingPaused));
    }

    /// @notice A 90% ratio pauses everything.
    function test_system_state_all_paused() public {
        _deposit(usdc, 900e6);
        assertEq(uint256(treasury.systemState(1_000e6)), uint256(Treasury.SystemState.AllPaused));
    }

    /// @notice The owner can rotate the redeemer.
    function test_set_redeemer() public {
        vm.prank(owner);
        treasury.setRedeemer(stranger);
        assertEq(treasury.authorizedRedeemer(), stranger);
    }
}
