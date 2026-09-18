// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {ID20VRF} from "../interfaces/ID20VRF.sol";
import {RandomnessMapping as M} from "./RandomnessMapping.sol";

/// @notice Built-in request helpers for game contracts. All requests still callback with (id, rawWord).
/// @dev Usage: using D20VRFRequests for ID20VRF; rng.d20(options). Inspect getMappedResult(id) after fulfillment.
///      Each helper pays the exact same-transaction quote from the calling contract balance.
library D20VRFRequests {
    struct Options { bytes32 clientSeed; uint32 callbackGasLimit; address refundAddress; }
    function diceRoll(ID20VRF rng, uint256 sides, uint32 count, Options memory o) internal returns (uint256) {
        return _send(rng, M.Spec(M.Operation.DiceRoll, 0, sides, count, 0), o);
    }
    function dN(ID20VRF rng, uint256 sides, Options memory o) internal returns (uint256) {
        return diceRoll(rng, sides, 1, o);
    }
    function d20(ID20VRF rng, Options memory o) internal returns (uint256) { return dN(rng, 20, o); }
    function d12(ID20VRF rng, Options memory o) internal returns (uint256) { return dN(rng, 12, o); }
    function d10(ID20VRF rng, Options memory o) internal returns (uint256) { return dN(rng, 10, o); }
    function d8(ID20VRF rng, Options memory o) internal returns (uint256) { return dN(rng, 8, o); }
    function d6(ID20VRF rng, Options memory o) internal returns (uint256) { return dN(rng, 6, o); }
    function d4(ID20VRF rng, Options memory o) internal returns (uint256) { return dN(rng, 4, o); }
    function coinFlip(ID20VRF rng, Options memory o) internal returns (uint256) {
        return _send(rng, M.Spec(M.Operation.CoinFlip, 0, 0, 1, 0), o);
    }
    function numberRange(ID20VRF rng, uint256 min, uint256 max, Options memory o) internal returns (uint256) {
        return _send(rng, M.Spec(M.Operation.NumberRange, min, max, 1, 0), o);
    }
    function chooseOne(ID20VRF rng, uint32 population, Options memory o) internal returns (uint256) {
        return _send(rng, M.Spec(M.Operation.ChooseOne, 0, 0, 1, population), o);
    }
    function chooseMany(ID20VRF rng, uint32 population, uint32 count, Options memory o) internal returns (uint256) {
        return _send(rng, M.Spec(M.Operation.ChooseMany, 0, 0, count, population), o);
    }
    function shuffle(ID20VRF rng, uint32 population, Options memory o) internal returns (uint256) {
        return _send(rng, M.Spec(M.Operation.Shuffle, 0, 0, population, population), o);
    }
    function _send(ID20VRF rng, M.Spec memory spec, Options memory o) private returns (uint256) {
        return rng.requestMappedRandomness{value: rng.quoteFee(o.callbackGasLimit)}(o.clientSeed, o.callbackGasLimit, o.refundAddress, spec);
    }
}
