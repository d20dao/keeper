// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {VRF} from "../vendor/VRF.sol";

/// @notice Stateless check of a secp256k1 VRF proof: the vendored VRF verification, unchanged, deployed as a contract of its own. The round
/// coordinator calls it instead of carrying the verification in its own code, which keeps the coordinator under EIP-170's 24,576-byte
/// limit. It holds no state and no owner; the coordinator implementation fixes its address in code.
contract D20VRFProofVerifier is VRF {
    /// @notice The VRF output of a proof of seed: reverts unless the proof is valid for its own public key and this seed.
    /// @dev Upstream ignores proof.seed in favor of the explicit seed; the caller compares the two and the proof's key.
    function randomValue(Proof calldata proof, uint256 seed) external view returns (uint256) {
        return _randomValueFromVRFProof(proof, seed);
    }
}
