// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {RandomnessMapping} from "../libraries/RandomnessMapping.sol";

/// @notice RandomnessMapping.map as a linked library: its sampling code lives at the library's address instead of in the round
/// coordinator, which keeps the coordinator under EIP-170's 24,576-byte limit. Request validation and the mapping hash stay inline in the
/// coordinator, so a request never calls the library; only the mapped-result views do.
library LinkedRandomnessMapping {
    function map(bytes32 randomness, RandomnessMapping.Spec memory spec) external pure returns (uint256[] memory) {
        return RandomnessMapping.map(randomness, spec);
    }
}
