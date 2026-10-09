// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {D20VRFCoordinatorRobinhood} from "../D20VRFCoordinatorRobinhood.sol";
import {D20VRFProofVerifier} from "../D20VRFProofVerifier.sol";

/// @dev TEST ONLY. A storage-identical probe of the round coordinator for the local upgrade continuity tests, not a deployment target. The
/// coordinator's runtime code is close to the 24,576-byte limit (OpcodeScan.test.ts prints the margin), so the marker is a small number: a
/// hash constant takes more bytes.
contract CoordinatorRobinhoodUpgradeProbe is D20VRFCoordinatorRobinhood {
    constructor(D20VRFProofVerifier verifier) D20VRFCoordinatorRobinhood(verifier) {}
    function upgradeMarker() external pure returns (uint256) { return 2; }
}
