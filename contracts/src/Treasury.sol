// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {OwnableUpgradeable} from "@openzeppelin/contracts-upgradeable/access/OwnableUpgradeable.sol";
import {UUPSUpgradeable} from "@openzeppelin/contracts-upgradeable/proxy/utils/UUPSUpgradeable.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

/// @title Treasury
/// @notice Holds USDC/USDT reserves and pays redemptions (Spec 2.5, 6.3, 7). The
///         reserve ratio (reserves / PRM circulating value) drives the system
///         state. All amounts are in 6-decimal stablecoin format.
contract Treasury is OwnableUpgradeable, UUPSUpgradeable {
    using SafeERC20 for IERC20;

    /// @notice Reserve-ratio-driven system states.
    enum SystemState {
        Healthy,
        Caution,
        StakingPaused,
        AllPaused
    }

    /// @notice Reserve ratio (150%) at or above which the system is healthy.
    uint256 public constant HEALTHY_THRESHOLD_BPS = 15_000;
    /// @notice Reserve ratio (120%) at or above which the system is in caution.
    uint256 public constant CAUTION_THRESHOLD_BPS = 12_000;
    /// @notice Reserve ratio (100%) at or above which only staking is paused.
    uint256 public constant STAKING_PAUSE_THRESHOLD_BPS = 10_000;
    /// @notice Basis-point denominator (10_000 bps = 100%).
    uint256 public constant BPS_DENOMINATOR = 10_000;

    /// @notice The USDC reserve token.
    IERC20 public usdc;
    /// @notice The USDT reserve token.
    IERC20 public usdt;
    /// @notice The only address permitted to trigger redemption payouts.
    address public authorizedRedeemer;
    /// @notice Cumulative USD redeemed, in 6-decimal format.
    uint256 public totalRedeemedUsd;

    /// @notice Emitted when reserves are deposited.
    event ReserveDeposited(address indexed token, address indexed from, uint256 amount);
    /// @notice Emitted when reserves are withdrawn.
    event ReserveWithdrawn(address indexed token, address indexed to, uint256 amount);
    /// @notice Emitted when a redemption is paid.
    event RedemptionPaid(address indexed recipient, address indexed token, uint256 amount);
    /// @notice Emitted when the authorized redeemer is rotated.
    event RedeemerUpdated(address indexed oldRedeemer, address indexed newRedeemer);

    /// @notice Thrown when a non-redeemer attempts to pay a redemption.
    error NotAuthorizedRedeemer();
    /// @notice Thrown when a zero address is supplied where disallowed.
    error ZeroAddress();
    /// @notice Thrown when a zero amount is supplied.
    error ZeroAmount();
    /// @notice Thrown when a transfer would exceed the available reserve.
    error InsufficientReserve(uint256 requested, uint256 available);
    /// @notice Thrown when a token is neither USDC nor USDT.
    error UnsupportedToken();

    /// @notice Initializes ownership, reserve tokens, and the authorized redeemer.
    /// @param initialOwner The address granted ownership of the contract.
    /// @param _usdc The USDC reserve token.
    /// @param _usdt The USDT reserve token.
    /// @param _redeemer The address authorized to trigger redemptions.
    function initialize(address initialOwner, address _usdc, address _usdt, address _redeemer)
        external
        initializer
    {
        if (_usdc == address(0) || _usdt == address(0) || _redeemer == address(0)) {
            revert ZeroAddress();
        }
        __Ownable_init(initialOwner);
        usdc = IERC20(_usdc);
        usdt = IERC20(_usdt);
        authorizedRedeemer = _redeemer;
    }

    /// @notice Rotates the authorized redeemer.
    /// @param newRedeemer The address to grant redemption authority.
    function setRedeemer(address newRedeemer) external onlyOwner {
        if (newRedeemer == address(0)) revert ZeroAddress();
        emit RedeemerUpdated(authorizedRedeemer, newRedeemer);
        authorizedRedeemer = newRedeemer;
    }

    /// @notice Deposits reserves of a supported token, pulling via allowance.
    /// @param token The reserve token; must be USDC or USDT.
    /// @param amount The amount to deposit, in 6-decimal format.
    function depositReserve(address token, uint256 amount) external {
        if (amount == 0) revert ZeroAmount();
        if (token != address(usdc) && token != address(usdt)) revert UnsupportedToken();
        IERC20(token).safeTransferFrom(msg.sender, address(this), amount);
        emit ReserveDeposited(token, msg.sender, amount);
    }

    /// @notice Withdraws reserves to an address. In production this is governed
    ///         by multi-sig and a 48-hour timelock enforced externally.
    /// @param token The reserve token; must be USDC or USDT.
    /// @param to The recipient of the withdrawn reserves.
    /// @param amount The amount to withdraw, in 6-decimal format.
    function withdrawReserve(address token, address to, uint256 amount) external onlyOwner {
        if (amount == 0) revert ZeroAmount();
        if (to == address(0)) revert ZeroAddress();
        if (token != address(usdc) && token != address(usdt)) revert UnsupportedToken();
        uint256 bal = IERC20(token).balanceOf(address(this));
        if (amount > bal) revert InsufficientReserve(amount, bal);
        IERC20(token).safeTransfer(to, amount);
        emit ReserveWithdrawn(token, to, amount);
    }

    /// @notice Pays a redemption from reserves. Callable only by the redeemer.
    /// @param recipient The recipient of the payout.
    /// @param token The reserve token; must be USDC or USDT.
    /// @param amount The amount to pay, in 6-decimal format.
    function payRedemption(address recipient, address token, uint256 amount) external {
        if (msg.sender != authorizedRedeemer) revert NotAuthorizedRedeemer();
        if (recipient == address(0)) revert ZeroAddress();
        if (amount == 0) revert ZeroAmount();
        if (token != address(usdc) && token != address(usdt)) revert UnsupportedToken();
        uint256 bal = IERC20(token).balanceOf(address(this));
        if (amount > bal) revert InsufficientReserve(amount, bal);
        totalRedeemedUsd += amount;
        IERC20(token).safeTransfer(recipient, amount);
        emit RedemptionPaid(recipient, token, amount);
    }

    /// @notice Returns total reserves in USD (6-decimal), summing USDC and USDT.
    function totalReserveUsd() public view returns (uint256) {
        return usdc.balanceOf(address(this)) + usdt.balanceOf(address(this));
    }

    /// @notice Returns the reserve ratio in basis points against the PRM
    ///         circulating value. Returns the maximum when circulating value is
    ///         zero (an infinite, fully healthy ratio).
    /// @param prmCirculatingValueUsd PRM circulating value in 6-decimal USD.
    function reserveRatioBps(uint256 prmCirculatingValueUsd) public view returns (uint256) {
        if (prmCirculatingValueUsd == 0) return type(uint256).max;
        return totalReserveUsd() * BPS_DENOMINATOR / prmCirculatingValueUsd;
    }

    /// @notice Returns the system state for a given PRM circulating value.
    /// @param prmCirculatingValueUsd PRM circulating value in 6-decimal USD.
    function systemState(uint256 prmCirculatingValueUsd) external view returns (SystemState) {
        uint256 ratio = reserveRatioBps(prmCirculatingValueUsd);
        if (ratio >= HEALTHY_THRESHOLD_BPS) return SystemState.Healthy;
        if (ratio >= CAUTION_THRESHOLD_BPS) return SystemState.Caution;
        if (ratio >= STAKING_PAUSE_THRESHOLD_BPS) return SystemState.StakingPaused;
        return SystemState.AllPaused;
    }

    /// @notice Authorizes a UUPS implementation upgrade. Restricted to the owner.
    function _authorizeUpgrade(address) internal override onlyOwner {}
}
