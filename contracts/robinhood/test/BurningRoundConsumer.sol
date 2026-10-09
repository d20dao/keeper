// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {D20VRFConsumer} from "../../D20VRFConsumer.sol";
import {ID20VRF} from "../../interfaces/ID20VRF.sol";

/// @dev TEST ONLY. A consumer whose result callback spends every unit of gas it is given, for the round coordinator's gas table.
/// requestMany sends n raw requests with one callback gas limit in one transaction, so they share a block and a round.
contract BurningRoundConsumer is D20VRFConsumer {
    uint256 public lastRequestId;
    constructor(address coordinator) D20VRFConsumer(coordinator) {}
    function requestMany(uint256 n, uint32 gasLimit, address refundTo) external payable {
        uint256 fee = msg.value / n;
        for (uint256 i; i < n; ++i)
            lastRequestId = ID20VRF(vrfCoordinator).requestRandomness{value: fee}(bytes32(i + 1), gasLimit, refundTo);
    }
    function _fulfillRandomness(uint256, bytes32) internal pure override {
        assembly { for { } 1 { } { } }
    }
}
