// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {OwnableUpgradeable} from "@openzeppelin/contracts-upgradeable/access/OwnableUpgradeable.sol";
import {UUPSUpgradeable} from "@openzeppelin/contracts-upgradeable/proxy/utils/UUPSUpgradeable.sol";

/// @title HouseEdge
/// @notice Manages the house edge rate (Spec 2.5, 7.1). The edge is bounded to
///         10%-25%, defaults to 17%, and admin changes pass through a 48-hour
///         timelock. The edge may also auto-adjust based on the reserve ratio.
contract HouseEdge is OwnableUpgradeable, UUPSUpgradeable {
    /// @notice Minimum permitted edge (10%).
    uint256 public constant MIN_EDGE_BPS = 1_000;
    /// @notice Maximum permitted edge (25%).
    uint256 public constant MAX_EDGE_BPS = 2_500;
    /// @notice Default operating edge (17%).
    uint256 public constant DEFAULT_EDGE_BPS = 1_700;
    /// @notice Delay enforced on admin edge changes.
    uint256 public constant TIMELOCK_DELAY = 48 hours;
    /// @notice Reserve ratio (120%) below which the edge rises to {DYNAMIC_EDGE_120}.
    uint256 public constant DYNAMIC_THRESHOLD_120 = 12_000;
    /// @notice Reserve ratio (110%) below which the edge rises to {DYNAMIC_EDGE_110}.
    uint256 public constant DYNAMIC_THRESHOLD_110 = 11_000;
    /// @notice Edge applied when reserve ratio is below 120% (22%).
    uint256 public constant DYNAMIC_EDGE_120 = 2_200;
    /// @notice Edge applied when reserve ratio is below 110% (24%).
    uint256 public constant DYNAMIC_EDGE_110 = 2_400;
    /// @notice Reserve ratio (150%) at or above which the edge reverts to default.
    uint256 public constant RESERVE_RESET_BPS = 15_000;
    /// @notice Basis-point denominator (10_000 bps = 100%).
    uint256 public constant BPS_DENOMINATOR = 10_000;

    /// @notice The edge currently in effect, in basis points.
    uint256 public currentEdgeBps;
    /// @notice The operator-configured default edge, in basis points.
    uint256 public operatorDefaultBps;

    /// @notice A timelocked edge-change proposal.
    struct EdgeProposal {
        /// @notice The proposed new edge, in basis points.
        uint256 newEdgeBps;
        /// @notice Block timestamp at which the proposal was created.
        uint256 proposedAt;
        /// @notice Whether the proposal has been executed.
        bool executed;
        /// @notice Whether the proposal has been cancelled.
        bool cancelled;
    }

    /// @notice Edge-change proposals keyed by proposal id.
    mapping(bytes32 => EdgeProposal) public proposals;

    /// @notice Emitted when an edge change is proposed.
    event EdgeProposed(bytes32 indexed proposalId, uint256 newEdgeBps, uint256 executionTime);
    /// @notice Emitted when a timelocked edge change executes.
    event EdgeExecuted(bytes32 indexed proposalId, uint256 oldEdgeBps, uint256 newEdgeBps);
    /// @notice Emitted when an edge-change proposal is cancelled.
    event EdgeCancelled(bytes32 indexed proposalId);
    /// @notice Emitted when the edge auto-adjusts from a reserve-ratio update.
    event EdgeAdjustedByReserve(uint256 reserveRatioBps, uint256 oldEdgeBps, uint256 newEdgeBps);

    /// @notice Thrown when a proposed edge is outside the permitted range.
    error EdgeOutOfRange(uint256 provided, uint256 min, uint256 max);
    /// @notice Thrown when executing before the timelock has elapsed.
    error TimelockNotExpired(uint256 executionTime, uint256 currentTime);
    /// @notice Thrown when acting on an already-executed proposal.
    error ProposalAlreadyExecuted();
    /// @notice Thrown when acting on an already-cancelled proposal.
    error ProposalAlreadyCancelled();
    /// @notice Thrown when the referenced proposal does not exist.
    error ProposalNotFound();
    /// @notice Thrown when a proposal's execution window has lapsed.
    error TimelockExpired();

    /// @notice Initializes ownership and sets the edge to the default rate.
    /// @param initialOwner The address granted ownership of the contract.
    function initialize(address initialOwner) external initializer {
        __Ownable_init(initialOwner);
        currentEdgeBps = DEFAULT_EDGE_BPS;
        operatorDefaultBps = DEFAULT_EDGE_BPS;
    }

    /// @notice Proposes a timelocked edge change.
    /// @param proposalId The unique proposal identifier.
    /// @param newEdgeBps The proposed edge, in basis points.
    function proposeEdgeChange(bytes32 proposalId, uint256 newEdgeBps) external onlyOwner {
        if (newEdgeBps < MIN_EDGE_BPS || newEdgeBps > MAX_EDGE_BPS) {
            revert EdgeOutOfRange(newEdgeBps, MIN_EDGE_BPS, MAX_EDGE_BPS);
        }
        proposals[proposalId] =
            EdgeProposal({newEdgeBps: newEdgeBps, proposedAt: block.timestamp, executed: false, cancelled: false});
        emit EdgeProposed(proposalId, newEdgeBps, block.timestamp + TIMELOCK_DELAY);
    }

    /// @notice Executes a proposal once its timelock has elapsed.
    /// @param proposalId The proposal to execute.
    function executeEdgeChange(bytes32 proposalId) external onlyOwner {
        EdgeProposal storage proposal = proposals[proposalId];
        if (proposal.proposedAt == 0) revert ProposalNotFound();
        if (proposal.executed) revert ProposalAlreadyExecuted();
        if (proposal.cancelled) revert ProposalAlreadyCancelled();
        uint256 executionTime = proposal.proposedAt + TIMELOCK_DELAY;
        if (block.timestamp < executionTime) revert TimelockNotExpired(executionTime, block.timestamp);
        uint256 oldEdge = currentEdgeBps;
        currentEdgeBps = proposal.newEdgeBps;
        operatorDefaultBps = proposal.newEdgeBps;
        proposal.executed = true;
        emit EdgeExecuted(proposalId, oldEdge, proposal.newEdgeBps);
    }

    /// @notice Cancels a pending proposal.
    /// @param proposalId The proposal to cancel.
    function cancelEdgeChange(bytes32 proposalId) external onlyOwner {
        EdgeProposal storage proposal = proposals[proposalId];
        if (proposal.proposedAt == 0) revert ProposalNotFound();
        if (proposal.executed) revert ProposalAlreadyExecuted();
        if (proposal.cancelled) revert ProposalAlreadyCancelled();
        proposal.cancelled = true;
        emit EdgeCancelled(proposalId);
    }

    /// @notice Auto-adjusts the edge from a reserve-ratio update (Spec 2.5). At or
    ///         above 150% the edge reverts to the operator default; below 110% it
    ///         rises to 24%; below 120% it rises to 22%; otherwise it is the default.
    /// @param reserveRatioBps The current reserve ratio, in basis points.
    function adjustForReserve(uint256 reserveRatioBps) external onlyOwner {
        uint256 newEdge;
        if (reserveRatioBps >= RESERVE_RESET_BPS) {
            newEdge = operatorDefaultBps;
        } else if (reserveRatioBps < DYNAMIC_THRESHOLD_110) {
            newEdge = _min(DYNAMIC_EDGE_110, MAX_EDGE_BPS);
        } else if (reserveRatioBps < DYNAMIC_THRESHOLD_120) {
            newEdge = _min(DYNAMIC_EDGE_120, MAX_EDGE_BPS);
        } else {
            newEdge = operatorDefaultBps;
        }
        if (newEdge == currentEdgeBps) return;
        uint256 oldEdge = currentEdgeBps;
        currentEdgeBps = newEdge;
        emit EdgeAdjustedByReserve(reserveRatioBps, oldEdge, newEdge);
    }

    /// @notice Returns the edge currently in effect, in basis points.
    function getCurrentEdgeBps() external view returns (uint256) {
        return currentEdgeBps;
    }

    /// @notice Applies the current edge to a gross amount.
    /// @param grossAmount The amount before the house edge.
    /// @return netAmount The amount remaining after deducting the current edge.
    function applyEdge(uint256 grossAmount) external view returns (uint256 netAmount) {
        netAmount = grossAmount * (BPS_DENOMINATOR - currentEdgeBps) / BPS_DENOMINATOR;
    }

    /// @notice Authorizes a UUPS implementation upgrade. Restricted to the owner.
    function _authorizeUpgrade(address) internal override onlyOwner {}

    /// @notice Returns the smaller of two values.
    function _min(uint256 a, uint256 b) internal pure returns (uint256) {
        return a < b ? a : b;
    }
}
