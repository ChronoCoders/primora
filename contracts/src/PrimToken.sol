// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.24;

import {ERC20Upgradeable} from "@openzeppelin/contracts-upgradeable/token/ERC20/ERC20Upgradeable.sol";
import {ERC20PermitUpgradeable} from
    "@openzeppelin/contracts-upgradeable/token/ERC20/extensions/ERC20PermitUpgradeable.sol";
import {OwnableUpgradeable} from "@openzeppelin/contracts-upgradeable/access/OwnableUpgradeable.sol";
import {UUPSUpgradeable} from "@openzeppelin/contracts-upgradeable/proxy/utils/UUPSUpgradeable.sol";

/// @title PrimToken
/// @notice ERC-20 PRM token with a single authorized minter (the MiningContract)
///         and UUPS upgradeability. No pre-mint and no initial supply; all supply
///         originates from minter-issued mints.
contract PrimToken is ERC20Upgradeable, ERC20PermitUpgradeable, OwnableUpgradeable, UUPSUpgradeable {
    /// @notice The only address permitted to mint PRM.
    address public minter;

    /// @notice The only address permitted to burn PRM (the NodeRegistry).
    address public burner;

    /// @notice Emitted when the minter address is changed.
    /// @param oldMinter The previous minter address.
    /// @param newMinter The new minter address.
    event MinterUpdated(address indexed oldMinter, address indexed newMinter);

    /// @notice Emitted when the burner address is changed.
    /// @param oldBurner The previous burner address.
    /// @param newBurner The new burner address.
    event BurnerUpdated(address indexed oldBurner, address indexed newBurner);

    /// @notice Emitted when new PRM is minted.
    /// @param to The recipient of the minted tokens.
    /// @param amount The amount minted, in wei (18 decimals).
    event TokensMinted(address indexed to, uint256 amount);

    /// @notice Thrown when a non-minter calls a minter-only function.
    error NotMinter();

    /// @notice Thrown when a non-burner calls a burner-only function.
    error NotBurner();

    /// @notice Thrown when a zero address is supplied where it is not allowed.
    error ZeroAddress();

    /// @notice Initializes the token, permit domain, ownership, and proxy. The
    ///         minter is left unset and must be configured via {setMinter}.
    /// @param initialOwner The address granted ownership of the contract.
    function initialize(address initialOwner) external initializer {
        __ERC20_init("Primora", "PRM");
        __ERC20Permit_init("Primora");
        __Ownable_init(initialOwner);
        minter = address(0);
    }

    /// @notice Sets the authorized minter. Timelock enforcement is external.
    /// @param newMinter The address to grant minting authority.
    function setMinter(address newMinter) external onlyOwner {
        if (newMinter == address(0)) revert ZeroAddress();
        emit MinterUpdated(minter, newMinter);
        minter = newMinter;
    }

    /// @notice Sets the authorized burner. Timelock enforcement is external.
    /// @param newBurner The address to grant burning authority.
    function setBurner(address newBurner) external onlyOwner {
        if (newBurner == address(0)) revert ZeroAddress();
        emit BurnerUpdated(burner, newBurner);
        burner = newBurner;
    }

    /// @notice Mints PRM to a recipient. Callable only by the configured minter.
    /// @param to The recipient of the minted tokens.
    /// @param amount The amount to mint, in wei (18 decimals).
    function mint(address to, uint256 amount) external {
        if (msg.sender != minter) revert NotMinter();
        if (to == address(0)) revert ZeroAddress();
        emit TokensMinted(to, amount);
        _mint(to, amount);
    }

    /// @notice Burns PRM from an account. Callable only by the configured burner
    ///         (the NodeRegistry, which burns slashed stake it custodies).
    /// @param from The account whose tokens are burned.
    /// @param amount The amount to burn, in wei (18 decimals).
    function burn(address from, uint256 amount) external {
        if (msg.sender != burner) revert NotBurner();
        _burn(from, amount);
    }

    /// @notice Authorizes a UUPS implementation upgrade. Restricted to the owner.
    /// @param newImplementation The address of the new implementation contract.
    function _authorizeUpgrade(address newImplementation) internal override onlyOwner {}
}
