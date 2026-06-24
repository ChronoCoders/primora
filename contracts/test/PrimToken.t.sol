// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
import {PrimToken} from "../src/PrimToken.sol";

/// @title PrimTokenTest
/// @notice Unit tests for {PrimToken} deployed behind an ERC1967 proxy.
contract PrimTokenTest is Test {
    PrimToken internal token;

    address internal owner = address(0xA11CE);
    address internal minter = address(0xB0B);
    address internal user = address(0xCA47);

    /// @notice Deploys the implementation behind an ERC1967 proxy and initializes it.
    function setUp() public {
        PrimToken impl = new PrimToken();
        bytes memory data = abi.encodeCall(PrimToken.initialize, (owner));
        ERC1967Proxy proxy = new ERC1967Proxy(address(impl), data);
        token = PrimToken(address(proxy));
    }

    /// @notice Verifies token metadata after initialization.
    function test_initialize() public view {
        assertEq(token.name(), "Primora");
        assertEq(token.symbol(), "PRM");
        assertEq(token.decimals(), 18);
        assertEq(token.owner(), owner);
        assertEq(token.minter(), address(0));
    }

    /// @notice Owner can set the minter and the change is recorded and emitted.
    function test_set_minter() public {
        vm.expectEmit(true, true, false, false);
        emit PrimToken.MinterUpdated(address(0), minter);
        vm.prank(owner);
        token.setMinter(minter);
        assertEq(token.minter(), minter);
    }

    /// @notice The configured minter can mint and balances update accordingly.
    function test_mint_by_minter() public {
        vm.prank(owner);
        token.setMinter(minter);
        vm.prank(minter);
        token.mint(user, 1000e18);
        assertEq(token.balanceOf(user), 1000e18);
        assertEq(token.totalSupply(), 1000e18);
    }

    /// @notice A non-minter calling mint reverts with {NotMinter}.
    function test_mint_revert_not_minter() public {
        vm.prank(owner);
        token.setMinter(minter);
        vm.expectRevert(PrimToken.NotMinter.selector);
        vm.prank(user);
        token.mint(user, 1000e18);
    }

    /// @notice Setting the minter to the zero address reverts with {ZeroAddress}.
    function test_set_minter_revert_zero_address() public {
        vm.expectRevert(PrimToken.ZeroAddress.selector);
        vm.prank(owner);
        token.setMinter(address(0));
    }

    /// @notice Minting to the zero address reverts with {ZeroAddress}.
    function test_mint_revert_zero_address() public {
        vm.prank(owner);
        token.setMinter(minter);
        vm.expectRevert(PrimToken.ZeroAddress.selector);
        vm.prank(minter);
        token.mint(address(0), 1000e18);
    }

    /// @notice A non-owner calling setMinter reverts via Ownable.
    function test_only_owner_can_set_minter() public {
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, user));
        vm.prank(user);
        token.setMinter(minter);
    }

    /// @notice A non-owner attempting a UUPS upgrade reverts via Ownable.
    function test_upgrade_only_owner() public {
        PrimToken newImpl = new PrimToken();
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, user));
        vm.prank(user);
        token.upgradeToAndCall(address(newImpl), "");
    }
}
