// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {D20VRFConsumer} from "../../D20VRFConsumer.sol";
import {ID20VRF} from "../../interfaces/ID20VRF.sol";
import {RandomnessMapping} from "../../libraries/RandomnessMapping.sol";

/// @dev TEST ONLY. A consumer written against ID20VRF and D20VRFConsumer alone, as an integrator writes one: it requests, stores its
/// results, reads its mapped results through ID20VRF and records refund notifications. requestMany sends n requests in one transaction, so
/// they share a block and a round. With `silent` set the callback does nothing, which measures a fulfillment without consumer work.
contract RoundConsumer is D20VRFConsumer {
    mapping(uint256 => bytes32) public results;
    mapping(uint256 => uint256) public refundNotices;
    uint256 public lastRequestId;
    uint256 public callbackCount;
    bool public silent;
    constructor(address coordinator) D20VRFConsumer(coordinator) {}
    function setSilent(bool next) external { silent = next; }
    function request(bytes32 seed, uint32 gasLimit, address refundTo) external payable returns (uint256) {
        lastRequestId = ID20VRF(vrfCoordinator).requestRandomness{value: msg.value}(seed, gasLimit, refundTo);
        return lastRequestId;
    }
    function requestMapped(bytes32 seed, uint32 gasLimit, address refundTo, RandomnessMapping.Spec calldata spec)
        external payable returns (uint256)
    {
        lastRequestId = ID20VRF(vrfCoordinator).requestMappedRandomness{value: msg.value}(seed, gasLimit, refundTo, spec);
        return lastRequestId;
    }
    function requestMany(uint256 n, uint32 gasLimit, address refundTo) external payable {
        uint256 fee = msg.value / n;
        for (uint256 i; i < n; ++i)
            lastRequestId = ID20VRF(vrfCoordinator).requestRandomness{value: fee}(bytes32(i + 1), gasLimit, refundTo);
    }
    function mappedResult(uint256 id) external view returns (uint256[] memory) {
        return ID20VRF(vrfCoordinator).getMappedResult(id);
    }
    function _fulfillRandomness(uint256 id, bytes32 randomness) internal override {
        if (silent) return;
        results[id] = randomness;
        callbackCount++;
    }
    function _onRefund(uint256 id) internal override { refundNotices[id]++; }
}
