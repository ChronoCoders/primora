// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {OwnableUpgradeable} from "@openzeppelin/contracts-upgradeable/access/OwnableUpgradeable.sol";
import {UUPSUpgradeable} from "@openzeppelin/contracts-upgradeable/proxy/utils/UUPSUpgradeable.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

/// @title StakingContract
/// @notice Ethereum mainnet PRM staking tier (Spec 3). Users stake PRM under a
///         lock period to earn a mining boost (capped at 40%) and a weekly share
///         of platform revenue distributed pro-rata by stake.
contract StakingContract is OwnableUpgradeable, UUPSUpgradeable {
    using SafeERC20 for IERC20;

    /// @notice Minimum PRM stake required.
    uint256 public constant MIN_STAKE = 10_000e18;
    /// @notice Hard cap on effective mining boost (40%).
    uint256 public constant MAX_BOOST_BPS = 4_000;
    /// @notice 30-day lock duration.
    uint256 public constant LOCK_30 = 30 days;
    /// @notice 90-day lock duration.
    uint256 public constant LOCK_90 = 90 days;
    /// @notice 180-day lock duration.
    uint256 public constant LOCK_180 = 180 days;
    /// @notice 30-day lock multiplier (1.0x, scaled by 100).
    uint256 public constant MULT_30 = 100;
    /// @notice 90-day lock multiplier (1.3x, scaled by 100).
    uint256 public constant MULT_90 = 130;
    /// @notice 180-day lock multiplier (1.6x, scaled by 100).
    uint256 public constant MULT_180 = 160;
    /// @notice Lock-multiplier scale factor.
    uint256 public constant MULT_SCALE = 100;

    /// @notice Selectable lock periods.
    enum LockPeriod {
        Days30,
        Days90,
        Days180
    }

    /// @notice The PRM token staked by users.
    IERC20 public primToken;
    /// @notice Total PRM currently staked across all active stakers.
    uint256 public totalStaked;

    /// @notice A user's active stake.
    struct StakeInfo {
        /// @notice Amount of PRM staked, in wei.
        uint256 amount;
        /// @notice The chosen lock period.
        LockPeriod lockPeriod;
        /// @notice Block timestamp at which the stake was created.
        uint256 stakedAt;
        /// @notice Block timestamp at which the stake unlocks.
        uint256 unlockAt;
        /// @notice Whether the stake is currently active.
        bool active;
    }

    /// @notice Active stake per user.
    mapping(address => StakeInfo) public stakes;
    /// @notice All addresses that have ever staked.
    address[] public stakers;
    /// @notice Whether an address is recorded in {stakers}.
    mapping(address => bool) public isStaker;

    /// @notice Emitted when a user stakes.
    event Staked(address indexed user, uint256 amount, LockPeriod lockPeriod, uint256 unlockAt);
    /// @notice Emitted when a user unstakes.
    event Unstaked(address indexed user, uint256 amount);
    /// @notice Emitted when a revenue distribution completes.
    event RevenueDistributed(uint256 totalAmount, uint256 stakerCount);
    /// @notice Emitted when a staker receives a revenue-share reward.
    event RewardPaid(address indexed user, uint256 amount);

    /// @notice Thrown when a stake is below {MIN_STAKE}.
    error BelowMinimumStake(uint256 provided, uint256 minimum);
    /// @notice Thrown when staking while an active stake exists.
    error AlreadyStaking();
    /// @notice Thrown when an operation requires an active stake.
    error NotStaking();
    /// @notice Thrown when unstaking before the lock expires.
    error StillLocked(uint256 unlockAt, uint256 currentTime);
    /// @notice Thrown when a zero amount is supplied.
    error ZeroAmount();
    /// @notice Thrown when a zero address is supplied where disallowed.
    error ZeroAddress();

    /// @notice Initializes ownership and the staking token.
    /// @param initialOwner The address granted ownership of the contract.
    /// @param _primToken The PRM token users stake.
    function initialize(address initialOwner, address _primToken) external initializer {
        if (_primToken == address(0)) revert ZeroAddress();
        __Ownable_init(initialOwner);
        primToken = IERC20(_primToken);
    }

    /// @notice Stakes PRM under a lock period, pulling tokens via allowance.
    /// @param amount The amount of PRM to stake; must be at least {MIN_STAKE}.
    /// @param lockPeriod The chosen lock period.
    function stake(uint256 amount, LockPeriod lockPeriod) external {
        if (amount < MIN_STAKE) revert BelowMinimumStake(amount, MIN_STAKE);
        if (stakes[msg.sender].active) revert AlreadyStaking();
        uint256 unlockAt = block.timestamp + _lockDuration(lockPeriod);
        primToken.safeTransferFrom(msg.sender, address(this), amount);
        stakes[msg.sender] = StakeInfo({
            amount: amount,
            lockPeriod: lockPeriod,
            stakedAt: block.timestamp,
            unlockAt: unlockAt,
            active: true
        });
        if (!isStaker[msg.sender]) {
            isStaker[msg.sender] = true;
            stakers.push(msg.sender);
        }
        totalStaked += amount;
        emit Staked(msg.sender, amount, lockPeriod, unlockAt);
    }

    /// @notice Unstakes PRM once the lock period has expired.
    function unstake() external {
        StakeInfo storage info = stakes[msg.sender];
        if (!info.active) revert NotStaking();
        if (block.timestamp < info.unlockAt) revert StillLocked(info.unlockAt, block.timestamp);
        uint256 amount = info.amount;
        info.active = false;
        info.amount = 0;
        totalStaked -= amount;
        primToken.safeTransfer(msg.sender, amount);
        emit Unstaked(msg.sender, amount);
    }

    /// @notice Returns the lock multiplier for a period, scaled by {MULT_SCALE}.
    /// @param lockPeriod The lock period.
    function lockMultiplier(LockPeriod lockPeriod) public pure returns (uint256) {
        if (lockPeriod == LockPeriod.Days30) return MULT_30;
        if (lockPeriod == LockPeriod.Days90) return MULT_90;
        return MULT_180;
    }

    /// @notice Returns the base boost in basis points for a staked amount.
    /// @param amount The staked amount, in wei.
    function baseBoostBps(uint256 amount) public pure returns (uint256) {
        if (amount >= 500_000e18) return 2_500;
        if (amount >= 100_000e18) return 1_800;
        if (amount >= 50_000e18) return 1_000;
        if (amount >= 10_000e18) return 500;
        return 0;
    }

    /// @notice Returns a user's effective boost in basis points, capped at
    ///         {MAX_BOOST_BPS}.
    /// @param user The staker to query.
    function effectiveBoostBps(address user) public view returns (uint256) {
        StakeInfo storage info = stakes[user];
        if (!info.active) return 0;
        uint256 effective = baseBoostBps(info.amount) * lockMultiplier(info.lockPeriod) / MULT_SCALE;
        if (effective > MAX_BOOST_BPS) return MAX_BOOST_BPS;
        return effective;
    }

    /// @notice Distributes revenue pro-rata to active stakers by stake share.
    ///         Weekly, owner-operated; the reward token is pulled from the caller.
    ///         Gas scales linearly in staker count; a Merkle-claim pattern would
    ///         be required for large staker sets (Phase 4).
    /// @param rewardToken The token distributed as revenue share.
    /// @param totalAmount The total reward amount to distribute.
    function distributeRevenue(address rewardToken, uint256 totalAmount) external onlyOwner {
        if (totalAmount == 0) revert ZeroAmount();
        IERC20(rewardToken).safeTransferFrom(msg.sender, address(this), totalAmount);
        uint256 staked = totalStaked;
        uint256 activeStakerCount;
        uint256 length = stakers.length;
        for (uint256 i = 0; i < length; i++) {
            address user = stakers[i];
            StakeInfo storage info = stakes[user];
            if (!info.active) continue;
            activeStakerCount++;
            uint256 reward = totalAmount * info.amount / staked;
            if (reward > 0) {
                IERC20(rewardToken).safeTransfer(user, reward);
                emit RewardPaid(user, reward);
            }
        }
        emit RevenueDistributed(totalAmount, activeStakerCount);
    }

    /// @notice Returns the number of recorded stakers.
    function getStakerCount() external view returns (uint256) {
        return stakers.length;
    }

    /// @notice Authorizes a UUPS implementation upgrade. Restricted to the owner.
    function _authorizeUpgrade(address) internal override onlyOwner {}

    /// @notice Returns the lock duration in seconds for a period.
    function _lockDuration(LockPeriod lockPeriod) internal pure returns (uint256) {
        if (lockPeriod == LockPeriod.Days30) return LOCK_30;
        if (lockPeriod == LockPeriod.Days90) return LOCK_90;
        return LOCK_180;
    }
}
