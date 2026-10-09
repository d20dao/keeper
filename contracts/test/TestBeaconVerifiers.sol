// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {IBeaconVerifier} from "../interfaces/IBeaconVerifier.sol";

/// @dev TEST ONLY. A beacon verifier that misbehaves on demand, separately for each of its two questions. In mode 0 it accepts every
/// key and round, so a beacon can register with it; setMode breaks verifyRound and setKeyMode breaks isValidPublicKey: 1 spends all the
/// gas it is given, 2 reverts, 3 answers false, 4 answers with a word that is not a boolean, 5 answers with nothing, 6 answers true
/// followed by another word, 7 answers true only if it was given nearly all of the registry's 400,000-gas allowance, and 8 answers
/// true followed by 200,000 more bytes, which costs it the memory for them (95,071 gas) and a caller that copies them as much again.
contract TestBeaconVerifier is IBeaconVerifier {
    uint8 public mode;
    uint8 public keyMode;
    function setMode(uint8 next) external { mode = next; }
    function setKeyMode(uint8 next) external { keyMode = next; }
    // The gas is read first, before the storage read of the mode, which costs 2,100 gas cold.
    function isValidPublicKey(bytes calldata) external view returns (bool) { return _answer(gasleft(), keyMode); }
    function verifyRound(bytes calldata, uint64, bytes calldata) external view returns (bool) { return _answer(gasleft(), mode); }
    function _answer(uint256 gasGiven, uint8 current) private pure returns (bool) {
        if (current == 1) { assembly { for { } 1 { } { } } }
        if (current == 2) revert("verifier failure");
        if (current == 4) { assembly { mstore(0, 2) return(0, 32) } }
        if (current == 5) { assembly { return(0, 0) } }
        if (current == 6) { assembly { mstore(0, 1) mstore(32, 1) return(0, 64) } }
        // The dispatcher and the argument checks before the gas was read cost a few hundred gas; 1,000 is the tolerance.
        if (current == 7) return gasGiven >= 399_000;
        if (current == 8) { assembly { mstore(0, 1) return(0, 200032) } }
        return current == 0;
    }
}
