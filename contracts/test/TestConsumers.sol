// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {D20VRFConsumer} from "../D20VRFConsumer.sol";
import {D20VRFCoordinator} from "../D20VRFCoordinator.sol";
import {ID20VRF} from "../interfaces/ID20VRF.sol";
import {MiningRandomnessConsumer} from "../examples/MiningRandomnessConsumer.sol";
import {RandomnessMapping} from "../libraries/RandomnessMapping.sol";
import {D20VRFRequests} from "../libraries/D20VRFRequests.sol";

/// @dev TEST ONLY. Never use this unrestricted request harness as a real mining game.
contract TestConsumer is D20VRFConsumer {
    using D20VRFRequests for ID20VRF;
    mapping(uint256 => bytes32) public results;
    uint256 public lastRequestId;
    uint256 public callbackCount;
    uint8 public mode;
    bool public reentrySucceeded;
    constructor(address coordinator) D20VRFConsumer(coordinator) {}
    function request(bytes32 seed, uint32 gasLimit, address refundAddress) external payable returns (uint256) {
        lastRequestId = ID20VRF(vrfCoordinator).requestRandomness{value: msg.value}(seed, gasLimit, refundAddress);
        return lastRequestId;
    }
    function setMode(uint8 next) external { mode = next; }
    function requestMapped(bytes32 seed, uint32 gasLimit, address refundAddress, RandomnessMapping.Spec calldata spec)
        external payable returns (uint256)
    {
        lastRequestId = ID20VRF(vrfCoordinator).requestMappedRandomness{value: msg.value}(seed, gasLimit, refundAddress, spec);
        return lastRequestId;
    }
    function rollD20(address refundAddress) external payable returns (uint256) {
        require(msg.value == ID20VRF(vrfCoordinator).quoteFee(200000), "exact fee");
        lastRequestId = ID20VRF(vrfCoordinator).d20(D20VRFRequests.Options(bytes32(0), 200000, refundAddress));
        return lastRequestId;
    }
    function _fulfillRandomness(uint256 id, bytes32 value) internal override {
        if (mode == 1) revert("consumer failure");
        if (mode == 2) { assembly { for { } 1 { } { } } }
        if (mode == 3) { assembly { revert(0, 1000000) } }
        if (mode == 4) {
            (reentrySucceeded,) = vrfCoordinator.call(abi.encodeCall(
                ID20VRF.requestRandomness, (bytes32(0), uint32(100000), address(this))
            ));
        }
        results[id] = value;
        callbackCount++;
    }
}

contract TestMiningGame is MiningRandomnessConsumer {
    constructor(address coordinator) MiningRandomnessConsumer(coordinator) {}
    /// @dev No PoW validation in this test harness. Actual game must validate and lock the claim first.
    function submitVerifiedClaim(uint256 id, bytes32 work, address refundAddress) external payable returns (uint256) {
        require(msg.value == ID20VRF(vrfCoordinator).quoteFee(200000), "wrong game fee");
        return _requestForClaim(id, work, 200000, refundAddress);
    }
}

contract RejectingRefundRecipient {
    receive() external payable { revert("no native payments"); }
    function withdraw(address coordinator, address payable recipient) external {
        D20VRFCoordinator(coordinator).withdrawRefundCredit(recipient);
    }
}
