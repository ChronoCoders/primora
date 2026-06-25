// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.24;

/// @title MockChainlinkFeed
/// @notice Minimal Chainlink AggregatorV3 mock. LOCAL TEST ONLY -- never deploy
///         to a public network. It returns a fixed answer for `latestRoundData`
///         so the backend's oracle reader can resolve a price on a bare chain
///         (e.g. Anvil) that has no real Chainlink aggregators.
contract MockChainlinkFeed {
    int256 private answer;
    /// @notice Decimal precision of the reported answer.
    uint8 public immutable decimals;
    uint80 private roundId;

    /// @notice Creates a mock feed with an initial answer and decimals.
    /// @param initialAnswer The initial price answer, scaled to `decimals_`.
    /// @param decimals_ The decimal precision of the answer.
    constructor(int256 initialAnswer, uint8 decimals_) {
        answer = initialAnswer;
        decimals = decimals_;
        roundId = 1;
    }

    /// @notice Updates the reported answer and advances the round id.
    /// @param newAnswer The new price answer, scaled to `decimals`.
    function setAnswer(int256 newAnswer) external {
        answer = newAnswer;
        roundId++;
    }

    /// @notice Returns the latest round data in the AggregatorV3 shape.
    function latestRoundData()
        external
        view
        returns (uint80, int256, uint256, uint256, uint80)
    {
        return (roundId, answer, block.timestamp, block.timestamp, roundId);
    }
}
