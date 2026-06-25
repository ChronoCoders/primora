// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {StakingContract} from "../src/StakingContract.sol";

/// @notice 18-decimal mock PRM token for staking tests.
contract MockPRM is ERC20 {
    constructor() ERC20("Mock PRM", "mPRM") {}

    /// @notice Mints `amt` tokens to `to`.
    function mint(address to, uint256 amt) external {
        _mint(to, amt);
    }
}

/// @notice 6-decimal mock reward token for distribution tests.
contract MockReward is ERC20 {
    constructor() ERC20("Mock Reward", "mRWD") {}

    /// @notice Mints `amt` tokens to `to`.
    function mint(address to, uint256 amt) external {
        _mint(to, amt);
    }

    /// @notice Overrides decimals to 6.
    function decimals() public pure override returns (uint8) {
        return 6;
    }
}

/// @title StakingContractTest
/// @notice Unit tests for {StakingContract} staking, locking, boosts, and
///         pro-rata revenue distribution.
contract StakingContractTest is Test {
    StakingContract internal staking;
    MockPRM internal prm;
    MockReward internal reward;

    address internal owner = address(0xA11CE);
    address internal alice = address(0xA11);
    address internal bob = address(0xB0B);

    /// @notice Deploys the proxy and mock tokens, funding two users.
    function setUp() public {
        prm = new MockPRM();
        reward = new MockReward();

        StakingContract impl = new StakingContract();
        staking = StakingContract(
            address(
                new ERC1967Proxy(
                    address(impl), abi.encodeCall(StakingContract.initialize, (owner, address(prm)))
                )
            )
        );

        prm.mint(alice, 1_000_000e18);
        prm.mint(bob, 1_000_000e18);
    }

    /// @notice Stakes `amount` for `period` as `user`.
    function _stake(address user, uint256 amount, StakingContract.LockPeriod period) internal {
        vm.startPrank(user);
        prm.approve(address(staking), amount);
        staking.stake(amount, period);
        vm.stopPrank();
    }

    /// @notice Initialization wires the staking token.
    function test_initialize() public view {
        assertEq(address(staking.primToken()), address(prm));
        assertEq(staking.owner(), owner);
    }

    /// @notice Initializing with a zero token address reverts.
    function test_initialize_revert_zero_token() public {
        StakingContract impl = new StakingContract();
        vm.expectRevert(StakingContract.ZeroAddress.selector);
        new ERC1967Proxy(address(impl), abi.encodeCall(StakingContract.initialize, (owner, address(0))));
    }

    /// @notice A valid stake is recorded with the correct unlock time.
    function test_stake_success() public {
        _stake(alice, 10_000e18, StakingContract.LockPeriod.Days30);
        (uint256 amount,, uint256 stakedAt, uint256 unlockAt, bool active) = staking.stakes(alice);
        assertEq(amount, 10_000e18);
        assertEq(unlockAt, stakedAt + 30 days);
        assertTrue(active);
        assertEq(staking.totalStaked(), 10_000e18);
    }

    /// @notice Staking below the minimum reverts.
    function test_stake_revert_below_min() public {
        vm.startPrank(alice);
        prm.approve(address(staking), 9_999e18);
        vm.expectRevert(
            abi.encodeWithSelector(StakingContract.BelowMinimumStake.selector, uint256(9_999e18), uint256(10_000e18))
        );
        staking.stake(9_999e18, StakingContract.LockPeriod.Days30);
        vm.stopPrank();
    }

    /// @notice Staking twice without unstaking reverts.
    function test_stake_revert_already_staking() public {
        _stake(alice, 10_000e18, StakingContract.LockPeriod.Days30);
        vm.startPrank(alice);
        prm.approve(address(staking), 10_000e18);
        vm.expectRevert(StakingContract.AlreadyStaking.selector);
        staking.stake(10_000e18, StakingContract.LockPeriod.Days30);
        vm.stopPrank();
    }

    /// @notice Unstaking after the lock returns the staked PRM.
    function test_unstake_after_lock() public {
        _stake(alice, 10_000e18, StakingContract.LockPeriod.Days30);
        vm.warp(block.timestamp + 30 days + 1);
        vm.prank(alice);
        staking.unstake();
        (uint256 amount,,,, bool active) = staking.stakes(alice);
        assertEq(amount, 0);
        assertFalse(active);
        assertEq(prm.balanceOf(alice), 1_000_000e18);
        assertEq(staking.totalStaked(), 0);
    }

    /// @notice Unstaking before the lock expires reverts.
    function test_unstake_revert_still_locked() public {
        _stake(alice, 10_000e18, StakingContract.LockPeriod.Days30);
        (,,, uint256 unlockAt,) = staking.stakes(alice);
        vm.expectRevert(abi.encodeWithSelector(StakingContract.StillLocked.selector, unlockAt, block.timestamp));
        vm.prank(alice);
        staking.unstake();
    }

    /// @notice Lock multipliers match the three periods.
    function test_lock_multiplier() public view {
        assertEq(staking.lockMultiplier(StakingContract.LockPeriod.Days30), 100);
        assertEq(staking.lockMultiplier(StakingContract.LockPeriod.Days90), 130);
        assertEq(staking.lockMultiplier(StakingContract.LockPeriod.Days180), 160);
    }

    /// @notice Base boost tiers match the four thresholds, zero below.
    function test_base_boost_tiers() public view {
        assertEq(staking.baseBoostBps(9_999e18), 0);
        assertEq(staking.baseBoostBps(10_000e18), 500);
        assertEq(staking.baseBoostBps(50_000e18), 1_000);
        assertEq(staking.baseBoostBps(100_000e18), 1_800);
        assertEq(staking.baseBoostBps(500_000e18), 2_500);
    }

    /// @notice 100k staked for 30 days yields 1800 * 1.0 = 1800 bps.
    function test_effective_boost_30d() public {
        _stake(alice, 100_000e18, StakingContract.LockPeriod.Days30);
        assertEq(staking.effectiveBoostBps(alice), 1_800);
    }

    /// @notice 100k staked for 180 days yields 1800 * 1.6 = 2880 bps.
    function test_effective_boost_180d() public {
        _stake(alice, 100_000e18, StakingContract.LockPeriod.Days180);
        assertEq(staking.effectiveBoostBps(alice), 2_880);
    }

    /// @notice 500k staked for 180 days lands exactly on the 4000 bps cap.
    function test_effective_boost_capped() public {
        _stake(alice, 500_000e18, StakingContract.LockPeriod.Days180);
        assertEq(staking.effectiveBoostBps(alice), 4_000);
    }

    /// @notice The effective boost never exceeds {MAX_BOOST_BPS}.
    function test_effective_boost_hard_cap() public {
        _stake(alice, 500_000e18, StakingContract.LockPeriod.Days180);
        assertLe(staking.effectiveBoostBps(alice), staking.MAX_BOOST_BPS());
        assertEq(staking.effectiveBoostBps(alice), staking.MAX_BOOST_BPS());
    }

    /// @notice A single staker receives the entire distribution.
    function test_distribute_revenue_single_staker() public {
        _stake(alice, 10_000e18, StakingContract.LockPeriod.Days30);
        reward.mint(owner, 1_000e6);
        vm.startPrank(owner);
        reward.approve(address(staking), 1_000e6);
        staking.distributeRevenue(address(reward), 1_000e6);
        vm.stopPrank();
        assertEq(reward.balanceOf(alice), 1_000e6);
    }

    /// @notice Two stakers split a distribution in proportion to their stake.
    function test_distribute_revenue_proportional() public {
        _stake(alice, 10_000e18, StakingContract.LockPeriod.Days30);
        _stake(bob, 30_000e18, StakingContract.LockPeriod.Days30);
        reward.mint(owner, 4_000e6);
        vm.startPrank(owner);
        reward.approve(address(staking), 4_000e6);
        staking.distributeRevenue(address(reward), 4_000e6);
        vm.stopPrank();
        assertEq(reward.balanceOf(alice), 1_000e6);
        assertEq(reward.balanceOf(bob), 3_000e6);
    }

    /// @notice Distributing zero reverts.
    function test_distribute_revenue_revert_zero() public {
        vm.expectRevert(StakingContract.ZeroAmount.selector);
        vm.prank(owner);
        staking.distributeRevenue(address(reward), 0);
    }

    /// @notice The staker count reflects distinct stakers.
    function test_get_staker_count() public {
        _stake(alice, 10_000e18, StakingContract.LockPeriod.Days30);
        _stake(bob, 10_000e18, StakingContract.LockPeriod.Days30);
        assertEq(staking.getStakerCount(), 2);
    }
}
