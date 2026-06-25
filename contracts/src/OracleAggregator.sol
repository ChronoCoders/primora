// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.24;

import {OwnableUpgradeable} from "@openzeppelin/contracts-upgradeable/access/OwnableUpgradeable.sol";
import {UUPSUpgradeable} from "@openzeppelin/contracts-upgradeable/proxy/utils/UUPSUpgradeable.sol";

/// @title OracleAggregator
/// @notice On-chain store for backend-computed TWAP prices (Spec 9, Backend Arch
///         13). The backend computes the TWAP off-chain and submits one final,
///         8-decimal-normalized value per commodity at session end. A 2%
///         divergence guard rejects suspicious jumps for manual review.
contract OracleAggregator is OwnableUpgradeable, UUPSUpgradeable {
    /// @notice Commodity identifiers, mirroring the Rust `common` crate.
    enum Commodity {
        Gold,
        Platinum,
        Silver,
        CrudeOil
    }

    /// @notice Maximum permitted divergence from the last stored price (2%).
    uint256 public constant DIVERGENCE_THRESHOLD_BPS = 200;
    /// @notice Decimal precision all submitted prices are normalized to.
    uint256 public constant PRICE_DECIMALS = 8;
    /// @notice Maximum age before a stored price is considered stale.
    uint256 public constant MAX_PRICE_AGE = 1 hours;
    /// @notice Basis-point denominator (10_000 bps = 100%).
    uint256 public constant BPS_DENOMINATOR = 10_000;

    /// @notice The only address permitted to submit prices (the backend signer).
    address public authorizedSubmitter;

    /// @notice A stored price for a commodity.
    struct PriceData {
        /// @notice The price, scaled to {PRICE_DECIMALS} decimals.
        uint256 price;
        /// @notice Block timestamp at which the price was stored.
        uint256 updatedAt;
        /// @notice Whether a price has ever been stored for this commodity.
        bool initialized;
    }

    /// @notice Stored prices keyed by commodity ordinal.
    mapping(uint8 => PriceData) public prices;

    /// @notice Emitted when a price is accepted and stored.
    event PriceSubmitted(uint8 indexed commodity, uint256 price, uint256 timestamp);
    /// @notice Emitted when the authorized submitter is rotated.
    event SubmitterUpdated(address indexed oldSubmitter, address indexed newSubmitter);

    /// @notice Thrown when a non-submitter attempts to submit a price.
    error NotAuthorizedSubmitter();
    /// @notice Thrown when a zero address is supplied where disallowed.
    error ZeroAddress();
    /// @notice Thrown when a zero price is submitted.
    error ZeroPrice();
    /// @notice Thrown when a submitted price exceeds the divergence threshold.
    error PriceDiverged(uint256 submitted, uint256 lastStored, uint256 divergenceBps);
    /// @notice Thrown when a queried price is older than {MAX_PRICE_AGE}.
    error PriceStale(uint256 updatedAt, uint256 currentTime);
    /// @notice Thrown when a queried commodity has no stored price.
    error PriceNotInitialized();

    /// @notice Initializes ownership and the authorized submitter.
    /// @param initialOwner The address granted ownership of the contract.
    /// @param _submitter The address authorized to submit prices.
    function initialize(address initialOwner, address _submitter) external initializer {
        if (_submitter == address(0)) revert ZeroAddress();
        __Ownable_init(initialOwner);
        authorizedSubmitter = _submitter;
    }

    /// @notice Rotates the authorized submitter. Timelock enforcement is external.
    /// @param newSubmitter The address to grant submission authority.
    function setSubmitter(address newSubmitter) external onlyOwner {
        if (newSubmitter == address(0)) revert ZeroAddress();
        emit SubmitterUpdated(authorizedSubmitter, newSubmitter);
        authorizedSubmitter = newSubmitter;
    }

    /// @notice Submits a normalized TWAP price for a commodity, subject to the
    ///         divergence guard once an initial price exists.
    /// @param commodity The commodity ordinal.
    /// @param price The 8-decimal-normalized price; must be non-zero.
    function submitPrice(uint8 commodity, uint256 price) external {
        if (msg.sender != authorizedSubmitter) revert NotAuthorizedSubmitter();
        if (price == 0) revert ZeroPrice();
        PriceData storage existing = prices[commodity];
        if (existing.initialized) {
            uint256 stored = existing.price;
            uint256 diff = price > stored ? price - stored : stored - price;
            uint256 divergenceBps = diff * BPS_DENOMINATOR / stored;
            if (divergenceBps > DIVERGENCE_THRESHOLD_BPS) {
                revert PriceDiverged(price, stored, divergenceBps);
            }
        }
        prices[commodity] = PriceData({price: price, updatedAt: block.timestamp, initialized: true});
        emit PriceSubmitted(commodity, price, block.timestamp);
    }

    /// @notice Returns a stored price, reverting if absent or stale.
    /// @param commodity The commodity ordinal.
    /// @return price The stored price, scaled to {PRICE_DECIMALS} decimals.
    /// @return updatedAt The timestamp at which the price was stored.
    function getPrice(uint8 commodity) external view returns (uint256 price, uint256 updatedAt) {
        PriceData storage data = prices[commodity];
        if (!data.initialized) revert PriceNotInitialized();
        if (block.timestamp > data.updatedAt + MAX_PRICE_AGE) {
            revert PriceStale(data.updatedAt, block.timestamp);
        }
        return (data.price, data.updatedAt);
    }

    /// @notice Returns raw stored price data without a staleness check, for
    ///         off-chain inspection.
    /// @param commodity The commodity ordinal.
    /// @return price The stored price, scaled to {PRICE_DECIMALS} decimals.
    /// @return updatedAt The timestamp at which the price was stored.
    /// @return initialized Whether a price has ever been stored.
    function getPriceUnchecked(uint8 commodity)
        external
        view
        returns (uint256 price, uint256 updatedAt, bool initialized)
    {
        PriceData storage data = prices[commodity];
        return (data.price, data.updatedAt, data.initialized);
    }

    /// @notice Emergency owner override that stores a price while bypassing the
    ///         divergence guard. Intended for cases where a legitimate large
    ///         market move triggers a false divergence rejection; must only be
    ///         used by the owner under multi-sig and timelock controls.
    /// @param commodity The commodity ordinal.
    /// @param price The 8-decimal-normalized price; must be non-zero.
    function forceSubmitPrice(uint8 commodity, uint256 price) external onlyOwner {
        if (price == 0) revert ZeroPrice();
        prices[commodity] = PriceData({price: price, updatedAt: block.timestamp, initialized: true});
        emit PriceSubmitted(commodity, price, block.timestamp);
    }

    /// @notice Authorizes a UUPS implementation upgrade. Restricted to the owner.
    function _authorizeUpgrade(address) internal override onlyOwner {}
}
