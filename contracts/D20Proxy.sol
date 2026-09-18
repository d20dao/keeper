// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";

/// @notice Stable service address; initialize the implementation atomically at construction.
contract D20Proxy is ERC1967Proxy {
    error MissingInitialization();
    constructor(address implementation, bytes memory initData) ERC1967Proxy(implementation, initData) {
        if(initData.length==0) revert MissingInitialization();
    }
}
