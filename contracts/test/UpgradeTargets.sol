// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {D20VRFCoordinator} from "../D20VRFCoordinator.sol";
import {EpochEntropy} from "../EpochEntropy.sol";

/// @dev Storage-identical probes for local upgrade continuity tests, not deployment targets.
contract CoordinatorUpgradeProbe is D20VRFCoordinator {
    function upgradeMarker() external pure returns(bytes32) { return keccak256("coordinator-storage-probe"); }
}

contract EpochUpgradeProbe is EpochEntropy {
    function upgradeMarker() external pure returns(bytes32) { return keccak256("epoch-storage-probe"); }
}
