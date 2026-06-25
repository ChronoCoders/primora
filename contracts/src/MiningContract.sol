// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.24;

import {OwnableUpgradeable} from "@openzeppelin/contracts-upgradeable/access/OwnableUpgradeable.sol";
import {UUPSUpgradeable} from "@openzeppelin/contracts-upgradeable/proxy/utils/UUPSUpgradeable.sol";

/// @notice Minimal PRM token interface used by the mint contract.
interface IPrimToken {
    /// @notice Mints `amount` PRM to `to`. Restricted to the configured minter.
    function mint(address to, uint256 amount) external;
}

/// @title MiningContract
/// @notice Core mint contract (Spec 5, 6, Backend Arch 10). The backend produces
///         a signed mint proposal off-chain and submits it here; minting requires
///         3-of-5 signer approval plus a 48-hour timelock, is bounded by a
///         per-block ceiling, and is protected against per-session replay.
contract MiningContract is OwnableUpgradeable, UUPSUpgradeable {
    /// @notice Delay enforced between proposal and execution.
    uint256 public constant TIMELOCK_DELAY = 48 hours;
    /// @notice Approvals required to execute a mint.
    uint8 public constant REQUIRED_APPROVALS = 3;
    /// @notice Maximum number of authorized signers.
    uint256 public constant TOTAL_SIGNERS = 5;

    /// @notice The PRM token this contract mints.
    IPrimToken public primToken;
    /// @notice Maximum total PRM mintable within a single block.
    uint256 public mintCeilingPerBlock;

    /// @notice Whether an address is an authorized signer.
    mapping(address => bool) public isSigner;
    /// @notice Current number of authorized signers.
    uint256 public signerCount;

    /// @notice Whether a session has already been minted (replay guard).
    mapping(bytes32 => bool) public sessionMinted;

    /// @notice A timelocked, multi-sig mint proposal.
    struct MintProposal {
        /// @notice The session this mint settles.
        bytes32 sessionId;
        /// @notice The recipient of the minted PRM.
        address recipient;
        /// @notice The amount of PRM to mint, in wei.
        uint256 amount;
        /// @notice Block timestamp at which the proposal was created.
        uint256 proposedAt;
        /// @notice Number of signer approvals collected.
        uint8 approvals;
        /// @notice Whether the proposal has executed.
        bool executed;
        /// @notice Whether the proposal has been cancelled.
        bool cancelled;
    }

    /// @notice Mint proposals keyed by proposal id.
    mapping(bytes32 => MintProposal) public proposals;
    /// @notice Tracks which signers have approved each proposal.
    mapping(bytes32 => mapping(address => bool)) public hasApproved;

    /// @notice Total PRM minted in each block, keyed by block number.
    mapping(uint256 => uint256) public mintedInBlock;

    /// @notice Emitted when a signer is added.
    event SignerAdded(address indexed signer);
    /// @notice Emitted when a signer is removed.
    event SignerRemoved(address indexed signer);
    /// @notice Emitted when a mint is proposed.
    event MintProposed(
        bytes32 indexed proposalId, bytes32 indexed sessionId, address indexed recipient, uint256 amount
    );
    /// @notice Emitted when a signer approves a proposal.
    event MintApproved(bytes32 indexed proposalId, address indexed signer, uint8 approvals);
    /// @notice Emitted when a mint executes.
    event MintExecuted(
        bytes32 indexed proposalId, bytes32 indexed sessionId, address recipient, uint256 amount
    );
    /// @notice Emitted when a proposal is cancelled.
    event MintCancelled(bytes32 indexed proposalId);
    /// @notice Emitted when the per-block mint ceiling changes.
    event MintCeilingUpdated(uint256 oldCeiling, uint256 newCeiling);

    /// @notice Thrown when a non-signer attempts a signer-only action.
    error NotSigner();
    /// @notice Thrown when adding an address that is already a signer.
    error AlreadySigner();
    /// @notice Thrown when adding a signer past {TOTAL_SIGNERS}.
    error SignerLimitReached();
    /// @notice Thrown when a zero address is supplied where disallowed.
    error ZeroAddress();
    /// @notice Thrown when a zero amount is proposed.
    error ZeroAmount();
    /// @notice Thrown when a session has already been minted.
    error SessionAlreadyMinted();
    /// @notice Thrown when the referenced proposal does not exist.
    error ProposalNotFound();
    /// @notice Thrown when a signer approves a proposal twice.
    error AlreadyApproved();
    /// @notice Thrown when acting on an already-executed proposal.
    error AlreadyExecuted();
    /// @notice Thrown when acting on an already-cancelled proposal.
    error AlreadyCancelled();
    /// @notice Thrown when executing without the required approvals.
    error InsufficientApprovals(uint8 got, uint8 required);
    /// @notice Thrown when executing before the timelock has elapsed.
    error TimelockNotExpired(uint256 executionTime, uint256 currentTime);
    /// @notice Thrown when a mint would exceed the per-block ceiling.
    error CeilingExceeded(uint256 attempted, uint256 ceiling);

    /// @notice Initializes ownership, the PRM token, and the per-block ceiling.
    /// @param initialOwner The address granted ownership of the contract.
    /// @param _primToken The PRM token this contract mints.
    /// @param _initialCeiling The initial per-block mint ceiling, in wei.
    function initialize(address initialOwner, address _primToken, uint256 _initialCeiling)
        external
        initializer
    {
        if (_primToken == address(0)) revert ZeroAddress();
        __Ownable_init(initialOwner);
        primToken = IPrimToken(_primToken);
        mintCeilingPerBlock = _initialCeiling;
    }

    /// @notice Adds an authorized signer.
    /// @param signer The address to authorize.
    function addSigner(address signer) external onlyOwner {
        if (signer == address(0)) revert ZeroAddress();
        if (isSigner[signer]) revert AlreadySigner();
        if (signerCount >= TOTAL_SIGNERS) revert SignerLimitReached();
        isSigner[signer] = true;
        signerCount++;
        emit SignerAdded(signer);
    }

    /// @notice Removes an authorized signer.
    /// @param signer The address to deauthorize.
    function removeSigner(address signer) external onlyOwner {
        if (!isSigner[signer]) revert NotSigner();
        isSigner[signer] = false;
        signerCount--;
        emit SignerRemoved(signer);
    }

    /// @notice Sets the per-block mint ceiling. In production this is governed by
    ///         multi-sig and a 48-hour timelock enforced externally.
    /// @param newCeiling The new per-block ceiling, in wei.
    function setMintCeiling(uint256 newCeiling) external onlyOwner {
        emit MintCeilingUpdated(mintCeilingPerBlock, newCeiling);
        mintCeilingPerBlock = newCeiling;
    }

    /// @notice Proposes a mint settling a session to a recipient.
    /// @param proposalId The unique proposal identifier.
    /// @param sessionId The session this mint settles.
    /// @param recipient The recipient of the minted PRM.
    /// @param amount The amount of PRM to mint, in wei.
    function proposeMint(bytes32 proposalId, bytes32 sessionId, address recipient, uint256 amount)
        external
        onlyOwner
    {
        if (recipient == address(0)) revert ZeroAddress();
        if (amount == 0) revert ZeroAmount();
        if (sessionMinted[sessionId]) revert SessionAlreadyMinted();
        proposals[proposalId] = MintProposal({
            sessionId: sessionId,
            recipient: recipient,
            amount: amount,
            proposedAt: block.timestamp,
            approvals: 0,
            executed: false,
            cancelled: false
        });
        emit MintProposed(proposalId, sessionId, recipient, amount);
    }

    /// @notice Approves a pending mint proposal. Callable only by signers.
    /// @param proposalId The proposal to approve.
    function approveMint(bytes32 proposalId) external {
        if (!isSigner[msg.sender]) revert NotSigner();
        MintProposal storage proposal = proposals[proposalId];
        if (proposal.proposedAt == 0) revert ProposalNotFound();
        if (proposal.executed) revert AlreadyExecuted();
        if (proposal.cancelled) revert AlreadyCancelled();
        if (hasApproved[proposalId][msg.sender]) revert AlreadyApproved();
        hasApproved[proposalId][msg.sender] = true;
        proposal.approvals++;
        emit MintApproved(proposalId, msg.sender, proposal.approvals);
    }

    /// @notice Executes a fully-approved proposal after its timelock, subject to
    ///         the per-block ceiling and per-session replay guard.
    /// @param proposalId The proposal to execute.
    function executeMint(bytes32 proposalId) external onlyOwner {
        MintProposal storage proposal = proposals[proposalId];
        if (proposal.proposedAt == 0) revert ProposalNotFound();
        if (proposal.executed) revert AlreadyExecuted();
        if (proposal.cancelled) revert AlreadyCancelled();
        if (proposal.approvals < REQUIRED_APPROVALS) {
            revert InsufficientApprovals(proposal.approvals, REQUIRED_APPROVALS);
        }
        uint256 executionTime = proposal.proposedAt + TIMELOCK_DELAY;
        if (block.timestamp < executionTime) revert TimelockNotExpired(executionTime, block.timestamp);
        if (sessionMinted[proposal.sessionId]) revert SessionAlreadyMinted();
        uint256 newBlockTotal = mintedInBlock[block.number] + proposal.amount;
        if (newBlockTotal > mintCeilingPerBlock) {
            revert CeilingExceeded(newBlockTotal, mintCeilingPerBlock);
        }
        mintedInBlock[block.number] = newBlockTotal;
        sessionMinted[proposal.sessionId] = true;
        proposal.executed = true;
        primToken.mint(proposal.recipient, proposal.amount);
        emit MintExecuted(proposalId, proposal.sessionId, proposal.recipient, proposal.amount);
    }

    /// @notice Cancels a pending proposal.
    /// @param proposalId The proposal to cancel.
    function cancelMint(bytes32 proposalId) external onlyOwner {
        MintProposal storage proposal = proposals[proposalId];
        if (proposal.executed) revert AlreadyExecuted();
        if (proposal.cancelled) revert AlreadyCancelled();
        proposal.cancelled = true;
        emit MintCancelled(proposalId);
    }

    /// @notice Authorizes a UUPS implementation upgrade. Restricted to the owner.
    function _authorizeUpgrade(address) internal override onlyOwner {}
}
