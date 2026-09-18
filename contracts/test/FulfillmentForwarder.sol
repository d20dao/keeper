// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;
import {D20VRFCoordinator} from "../D20VRFCoordinator.sol";
import {VRF} from "../vendor/VRF.sol";

/// @dev Test wrapper: receipt evidence must remain recoverable without inspecting this wrapper's calldata.
contract FulfillmentForwarder {
    function forward(D20VRFCoordinator coordinator,uint256 id,VRF.Proof calldata proof) external {
        coordinator.fulfillRandomness(id,proof);
    }
}
