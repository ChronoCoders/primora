// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";
import {OwnableUpgradeable} from "@openzeppelin/contracts-upgradeable/access/OwnableUpgradeable.sol";
import {PrimToken} from "../src/PrimToken.sol";
import {MiningContract} from "../src/MiningContract.sol";

/// @title MiningContractTest
/// @notice Unit tests for {MiningContract} multi-sig, timelock, per-block ceiling,
///         and replay protection, exercised against a live {PrimToken} proxy.
contract MiningContractTest is Test {
    PrimToken internal token;
    MiningContract internal mining;

    address internal owner = address(0xA11CE);
    address internal recipient = address(0xCAFE);
    address internal stranger = address(0xBAD);

    address[5] internal signers;

    uint256 internal constant CEILING = 1_000_000e18;
    uint256 internal constant MINT_AMOUNT = 1_000e18;

    bytes32 internal constant PID = keccak256("proposal-1");
    bytes32 internal constant SID = keccak256("session-1");

    /// @notice Deploys both proxies, wires the mining contract as PRM minter, and
    ///         registers five signers.
    function setUp() public {
        PrimToken tokenImpl = new PrimToken();
        token = PrimToken(
            address(new ERC1967Proxy(address(tokenImpl), abi.encodeCall(PrimToken.initialize, (owner))))
        );

        mining = _deployMining(CEILING);

        vm.prank(owner);
        token.setMinter(address(mining));

        signers =
            [address(0x1001), address(0x1002), address(0x1003), address(0x1004), address(0x1005)];
        for (uint256 i = 0; i < 5; i++) {
            vm.prank(owner);
            mining.addSigner(signers[i]);
        }
    }

    /// @notice Deploys a fresh mining contract proxy owned by `owner`.
    function _deployMining(uint256 ceiling) internal returns (MiningContract m) {
        MiningContract impl = new MiningContract();
        m = MiningContract(
            address(
                new ERC1967Proxy(
                    address(impl),
                    abi.encodeCall(MiningContract.initialize, (owner, address(token), ceiling))
                )
            )
        );
    }

    /// @notice Proposes a mint and collects the three required signer approvals.
    function _proposeAndApprove(bytes32 proposalId, bytes32 sessionId, address to, uint256 amount)
        internal
    {
        vm.prank(owner);
        mining.proposeMint(proposalId, sessionId, to, amount);
        for (uint256 i = 0; i < 3; i++) {
            vm.prank(signers[i]);
            mining.approveMint(proposalId);
        }
    }

    /// @notice Constructor wiring sets the token and ceiling.
    function test_initialize() public view {
        assertEq(address(mining.primToken()), address(token));
        assertEq(mining.mintCeilingPerBlock(), CEILING);
    }

    /// @notice A signer can be added on a fresh contract.
    function test_add_signer() public {
        MiningContract m = _deployMining(CEILING);
        vm.prank(owner);
        m.addSigner(signers[0]);
        assertTrue(m.isSigner(signers[0]));
        assertEq(m.signerCount(), 1);
    }

    /// @notice Adding a sixth signer reverts at the limit.
    function test_add_signer_revert_limit() public {
        vm.expectRevert(MiningContract.SignerLimitReached.selector);
        vm.prank(owner);
        mining.addSigner(address(0x2000));
    }

    /// @notice Adding an existing signer reverts.
    function test_add_signer_revert_duplicate() public {
        vm.expectRevert(MiningContract.AlreadySigner.selector);
        vm.prank(owner);
        mining.addSigner(signers[0]);
    }

    /// @notice A signer can be removed.
    function test_remove_signer() public {
        MiningContract m = _deployMining(CEILING);
        vm.startPrank(owner);
        m.addSigner(signers[0]);
        m.removeSigner(signers[0]);
        vm.stopPrank();
        assertFalse(m.isSigner(signers[0]));
        assertEq(m.signerCount(), 0);
    }

    /// @notice A proposal stores its parameters.
    function test_propose_mint() public {
        vm.prank(owner);
        mining.proposeMint(PID, SID, recipient, MINT_AMOUNT);
        (bytes32 sessionId, address rcpt, uint256 amount,, uint8 approvals, bool executed, bool cancelled)
        = mining.proposals(PID);
        assertEq(sessionId, SID);
        assertEq(rcpt, recipient);
        assertEq(amount, MINT_AMOUNT);
        assertEq(approvals, 0);
        assertFalse(executed);
        assertFalse(cancelled);
    }

    /// @notice Proposing against an already-minted session reverts.
    function test_propose_revert_session_already_minted() public {
        _proposeAndApprove(PID, SID, recipient, MINT_AMOUNT);
        vm.warp(block.timestamp + mining.TIMELOCK_DELAY() + 1);
        vm.prank(owner);
        mining.executeMint(PID);

        vm.expectRevert(MiningContract.SessionAlreadyMinted.selector);
        vm.prank(owner);
        mining.proposeMint(keccak256("proposal-2"), SID, recipient, MINT_AMOUNT);
    }

    /// @notice Three approvals are recorded on a proposal.
    function test_approve_mint() public {
        _proposeAndApprove(PID, SID, recipient, MINT_AMOUNT);
        (,,,, uint8 approvals,,) = mining.proposals(PID);
        assertEq(approvals, 3);
    }

    /// @notice A non-signer cannot approve.
    function test_approve_revert_not_signer() public {
        vm.prank(owner);
        mining.proposeMint(PID, SID, recipient, MINT_AMOUNT);
        vm.expectRevert(MiningContract.NotSigner.selector);
        vm.prank(stranger);
        mining.approveMint(PID);
    }

    /// @notice A signer cannot approve the same proposal twice.
    function test_approve_revert_double() public {
        vm.prank(owner);
        mining.proposeMint(PID, SID, recipient, MINT_AMOUNT);
        vm.prank(signers[0]);
        mining.approveMint(PID);
        vm.expectRevert(MiningContract.AlreadyApproved.selector);
        vm.prank(signers[0]);
        mining.approveMint(PID);
    }

    /// @notice A fully-approved, timelock-elapsed mint executes and mints PRM.
    function test_execute_mint_success() public {
        _proposeAndApprove(PID, SID, recipient, MINT_AMOUNT);
        vm.warp(block.timestamp + mining.TIMELOCK_DELAY() + 1);
        vm.prank(owner);
        mining.executeMint(PID);
        assertEq(token.balanceOf(recipient), MINT_AMOUNT);
        (,,,,, bool executed,) = mining.proposals(PID);
        assertTrue(executed);
    }

    /// @notice Executing with fewer than three approvals reverts.
    function test_execute_revert_insufficient_approvals() public {
        vm.prank(owner);
        mining.proposeMint(PID, SID, recipient, MINT_AMOUNT);
        vm.prank(signers[0]);
        mining.approveMint(PID);
        vm.prank(signers[1]);
        mining.approveMint(PID);
        vm.warp(block.timestamp + mining.TIMELOCK_DELAY() + 1);
        vm.expectRevert(abi.encodeWithSelector(MiningContract.InsufficientApprovals.selector, uint8(2), uint8(3)));
        vm.prank(owner);
        mining.executeMint(PID);
    }

    /// @notice Executing before the timelock elapses reverts.
    function test_execute_revert_timelock() public {
        _proposeAndApprove(PID, SID, recipient, MINT_AMOUNT);
        uint256 executionTime = block.timestamp + mining.TIMELOCK_DELAY();
        vm.expectRevert(
            abi.encodeWithSelector(MiningContract.TimelockNotExpired.selector, executionTime, block.timestamp)
        );
        vm.prank(owner);
        mining.executeMint(PID);
    }

    /// @notice A mint over the per-block ceiling reverts.
    function test_execute_revert_ceiling() public {
        vm.prank(owner);
        mining.setMintCeiling(100e18);
        _proposeAndApprove(PID, SID, recipient, MINT_AMOUNT);
        vm.warp(block.timestamp + mining.TIMELOCK_DELAY() + 1);
        vm.expectRevert(abi.encodeWithSelector(MiningContract.CeilingExceeded.selector, MINT_AMOUNT, uint256(100e18)));
        vm.prank(owner);
        mining.executeMint(PID);
    }

    /// @notice A proposal cannot be executed twice.
    function test_execute_replay_protection() public {
        _proposeAndApprove(PID, SID, recipient, MINT_AMOUNT);
        vm.warp(block.timestamp + mining.TIMELOCK_DELAY() + 1);
        vm.startPrank(owner);
        mining.executeMint(PID);
        vm.expectRevert(MiningContract.AlreadyExecuted.selector);
        mining.executeMint(PID);
        vm.stopPrank();
    }

    /// @notice A cancelled proposal cannot be executed.
    function test_cancel_mint() public {
        _proposeAndApprove(PID, SID, recipient, MINT_AMOUNT);
        vm.prank(owner);
        mining.cancelMint(PID);
        (,,,,,, bool cancelled) = mining.proposals(PID);
        assertTrue(cancelled);
        vm.warp(block.timestamp + mining.TIMELOCK_DELAY() + 1);
        vm.expectRevert(MiningContract.AlreadyCancelled.selector);
        vm.prank(owner);
        mining.executeMint(PID);
    }

    /// @notice Mints accumulate within a block and the second over-ceiling reverts.
    function test_ceiling_accumulates_in_block() public {
        vm.prank(owner);
        mining.setMintCeiling(1_500e18);

        bytes32 pid2 = keccak256("proposal-2");
        bytes32 sid2 = keccak256("session-2");
        _proposeAndApprove(PID, SID, recipient, MINT_AMOUNT);
        _proposeAndApprove(pid2, sid2, recipient, MINT_AMOUNT);
        vm.warp(block.timestamp + mining.TIMELOCK_DELAY() + 1);

        vm.startPrank(owner);
        mining.executeMint(PID);
        vm.expectRevert(abi.encodeWithSelector(MiningContract.CeilingExceeded.selector, uint256(2_000e18), uint256(1_500e18)));
        mining.executeMint(pid2);
        vm.stopPrank();
    }
}
