// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.24;

import {OwnableUpgradeable} from "@openzeppelin/contracts-upgradeable/access/OwnableUpgradeable.sol";
import {UUPSUpgradeable} from "@openzeppelin/contracts-upgradeable/proxy/utils/UUPSUpgradeable.sol";

/// @notice Minimal PRM token interface used by the registry for stake custody
///         and burning slashed stake.
interface IPrimToken {
    /// @notice Transfers `amount` from `from` to `to`, pulling via allowance.
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    /// @notice Transfers `amount` from the caller to `to`.
    function transfer(address to, uint256 amount) external returns (bool);
    /// @notice Burns `amount` from `from`. Restricted to the configured burner.
    function burn(address from, uint256 amount) external;
}

/// @title NodeRegistry
/// @notice Node registration, staking, and consensus-gated slashing (Spec 9.2,
///         Backend Arch 5.7). Nodes stake PRM to register; misbehaviour is
///         slashed by 2-of-3 observer consensus after a 4-hour operator veto
///         window, with slashed stake burned via the PRM token.
contract NodeRegistry is OwnableUpgradeable, UUPSUpgradeable {
    /// @notice Reason a node is being slashed.
    enum SlashReason {
        InvalidAttestation,
        Downtime
    }

    /// @notice Registered node state.
    struct NodeInfo {
        /// @notice The operator address that owns the stake.
        address operator;
        /// @notice PRM currently staked by this node, in wei.
        uint256 stakedAmount;
        /// @notice Number of slashing violations executed against this node.
        uint8 violationCount;
        /// @notice Whether the node is currently registered and active.
        bool active;
        /// @notice Block timestamp at which the node registered.
        uint256 registeredAt;
    }

    /// @notice A pending slashing proposal awaiting consensus and veto expiry.
    struct SlashProposal {
        /// @notice The node targeted by the proposal.
        bytes32 nodeId;
        /// @notice The reason for the proposed slash.
        SlashReason reason;
        /// @notice Number of distinct observer confirmations.
        uint8 confirmations;
        /// @notice Tracks which observing nodes have confirmed.
        mapping(bytes32 => bool) confirmed;
        /// @notice Block timestamp at which the proposal was created.
        uint256 proposedAt;
        /// @notice Whether the proposal has been executed.
        bool executed;
        /// @notice Whether the operator vetoed the proposal.
        bool vetoed;
    }

    /// @notice The PRM token used for stake custody and burning.
    IPrimToken public primToken;

    /// @notice Minimum PRM stake required to register a node.
    uint256 public constant MIN_STAKE = 10_000e18;

    /// @notice Window during which an operator may veto a slash proposal.
    uint256 public constant VETO_WINDOW = 4 hours;

    /// @notice Registered nodes keyed by node id.
    mapping(bytes32 => NodeInfo) public nodes;

    /// @notice Active slash proposals keyed by proposal id.
    mapping(bytes32 => SlashProposal) public slashProposals;

    /// @notice Emitted when a node registers and stakes PRM.
    event NodeRegistered(bytes32 indexed nodeId, address indexed operator, uint256 stake);
    /// @notice Emitted when a node is deregistered (voluntarily or by full slash).
    event NodeDeregistered(bytes32 indexed nodeId, address indexed operator);
    /// @notice Emitted when a slash is proposed.
    event SlashProposed(bytes32 indexed proposalId, bytes32 indexed nodeId, SlashReason reason);
    /// @notice Emitted when an observer confirms a slash proposal.
    event SlashConfirmed(bytes32 indexed proposalId, bytes32 indexed confirmingNodeId);
    /// @notice Emitted when a slash is executed and stake burned.
    event SlashExecuted(bytes32 indexed proposalId, bytes32 indexed nodeId, uint256 burned);
    /// @notice Emitted when a slash proposal is vetoed by the operator.
    event SlashVetoed(bytes32 indexed proposalId);

    /// @notice Thrown when a registration stake is below {MIN_STAKE}.
    error InsufficientStake();
    /// @notice Thrown when registering a node id that is already active.
    error NodeAlreadyRegistered();
    /// @notice Thrown when the referenced node does not exist.
    error NodeNotFound();
    /// @notice Thrown when an operation requires an active node.
    error NodeNotActive();
    /// @notice Thrown when a node confirms a proposal it already confirmed.
    error AlreadyConfirmed();
    /// @notice Thrown when vetoing after the veto window has closed.
    error VetoWindowExpired();
    /// @notice Thrown when an operation requires a vetoed proposal.
    error NotVetoed();
    /// @notice Thrown when acting on an already-executed proposal.
    error AlreadyExecuted();
    /// @notice Thrown when executing without the required confirmations.
    error InsufficientConfirmations();
    /// @notice Thrown when executing before the veto window has closed.
    error VetoWindowActive();
    /// @notice Thrown when a zero address is supplied where disallowed.
    error ZeroAddress();
    /// @notice Thrown when a caller is not the node's operator.
    error NotOperator();
    /// @notice Thrown when acting on an already-vetoed proposal.
    error AlreadyVetoed();
    /// @notice Thrown when a PRM transfer returns false.
    error TransferFailed();

    /// @notice Initializes ownership and the PRM token reference.
    /// @param initialOwner The address granted ownership of the registry.
    /// @param _primToken The PRM token used for staking and burning.
    function initialize(address initialOwner, address _primToken) external initializer {
        if (_primToken == address(0)) revert ZeroAddress();
        __Ownable_init(initialOwner);
        primToken = IPrimToken(_primToken);
    }

    /// @notice Registers a node, pulling `stakeAmount` PRM from the caller.
    /// @param nodeId The unique node identifier.
    /// @param stakeAmount The PRM amount to stake, must be at least {MIN_STAKE}.
    function registerNode(bytes32 nodeId, uint256 stakeAmount) external {
        if (stakeAmount < MIN_STAKE) revert InsufficientStake();
        NodeInfo storage node = nodes[nodeId];
        if (node.active) revert NodeAlreadyRegistered();
        if (!primToken.transferFrom(msg.sender, address(this), stakeAmount)) revert TransferFailed();
        node.operator = msg.sender;
        node.stakedAmount = stakeAmount;
        node.active = true;
        node.registeredAt = block.timestamp;
        emit NodeRegistered(nodeId, msg.sender, stakeAmount);
    }

    /// @notice Deregisters a node and returns its remaining stake to the operator.
    /// @param nodeId The node to deregister; caller must be its operator.
    function deregisterNode(bytes32 nodeId) external {
        NodeInfo storage node = nodes[nodeId];
        if (!node.active) revert NodeNotActive();
        if (node.operator != msg.sender) revert NotOperator();
        uint256 amount = node.stakedAmount;
        node.active = false;
        node.stakedAmount = 0;
        if (!primToken.transfer(node.operator, amount)) revert TransferFailed();
        emit NodeDeregistered(nodeId, node.operator);
    }

    /// @notice Proposes slashing a node, recording the proposer as the first
    ///         confirmation. Restricted to the owner (the consensus coordinator).
    /// @param proposalId The unique proposal identifier.
    /// @param targetNodeId The node to slash.
    /// @param proposingNodeId The observing node that proposes the slash.
    /// @param reason The slashing reason.
    function proposeSlash(
        bytes32 proposalId,
        bytes32 targetNodeId,
        bytes32 proposingNodeId,
        SlashReason reason
    ) external onlyOwner {
        NodeInfo storage node = nodes[targetNodeId];
        if (node.operator == address(0)) revert NodeNotFound();
        if (!node.active) revert NodeNotActive();
        SlashProposal storage proposal = slashProposals[proposalId];
        proposal.nodeId = targetNodeId;
        proposal.reason = reason;
        proposal.proposedAt = block.timestamp;
        proposal.confirmations = 1;
        proposal.confirmed[proposingNodeId] = true;
        emit SlashProposed(proposalId, targetNodeId, reason);
        emit SlashConfirmed(proposalId, proposingNodeId);
    }

    /// @notice Adds an observer confirmation to a pending slash proposal.
    /// @param proposalId The proposal to confirm.
    /// @param confirmingNodeId The observing node adding its confirmation.
    function confirmSlash(bytes32 proposalId, bytes32 confirmingNodeId) external onlyOwner {
        SlashProposal storage proposal = slashProposals[proposalId];
        if (proposal.executed) revert AlreadyExecuted();
        if (proposal.vetoed) revert AlreadyVetoed();
        if (proposal.confirmed[confirmingNodeId]) revert AlreadyConfirmed();
        proposal.confirmed[confirmingNodeId] = true;
        proposal.confirmations += 1;
        emit SlashConfirmed(proposalId, confirmingNodeId);
    }

    /// @notice Executes a confirmed slash after the veto window, burning the
    ///         slashed stake. Slash severity escalates with prior violations.
    /// @param proposalId The proposal to execute.
    function executeSlash(bytes32 proposalId) external onlyOwner {
        SlashProposal storage proposal = slashProposals[proposalId];
        if (proposal.executed) revert AlreadyExecuted();
        if (proposal.vetoed) revert AlreadyVetoed();
        if (proposal.confirmations < 2) revert InsufficientConfirmations();
        if (block.timestamp < proposal.proposedAt + VETO_WINDOW) revert VetoWindowActive();
        NodeInfo storage node = nodes[proposal.nodeId];
        uint8 currentViolations = node.violationCount;
        uint256 bps = _slashBps(proposal.reason, currentViolations);
        uint256 slashAmount = node.stakedAmount * bps / 10_000;
        node.stakedAmount -= slashAmount;
        node.violationCount = currentViolations + 1;
        if (proposal.reason == SlashReason.InvalidAttestation && currentViolations >= 2) {
            node.active = false;
            emit NodeDeregistered(proposal.nodeId, node.operator);
        }
        proposal.executed = true;
        primToken.burn(address(this), slashAmount);
        emit SlashExecuted(proposalId, proposal.nodeId, slashAmount);
    }

    /// @notice Vetoes a slash proposal within the veto window, halting execution.
    /// @param proposalId The proposal to veto.
    function vetoSlash(bytes32 proposalId) external onlyOwner {
        SlashProposal storage proposal = slashProposals[proposalId];
        if (proposal.executed) revert AlreadyExecuted();
        if (block.timestamp > proposal.proposedAt + VETO_WINDOW) revert VetoWindowExpired();
        proposal.vetoed = true;
        emit SlashVetoed(proposalId);
    }

    /// @notice Authorizes a UUPS implementation upgrade. Restricted to the owner.
    function _authorizeUpgrade(address) internal override onlyOwner {}

    /// @notice Returns the slash size in basis points for a reason and prior
    ///         violation count: InvalidAttestation escalates 10% / 25% / 100%;
    ///         Downtime is a flat 5%.
    function _slashBps(SlashReason reason, uint8 violationCount) internal pure returns (uint256) {
        if (reason == SlashReason.Downtime) return 500;
        if (violationCount == 0) return 1_000;
        if (violationCount == 1) return 2_500;
        return 10_000;
    }
}
