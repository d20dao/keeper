// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;
import {D20VRFConsumer} from "../D20VRFConsumer.sol";
import {D20VRFCoordinator} from "../D20VRFCoordinator.sol";
import {ID20VRF} from "../interfaces/ID20VRF.sol";
import {RandomnessMapping} from "../libraries/RandomnessMapping.sol";

contract TestRefundConsumer is D20VRFConsumer {
    uint256 public lastRequestId;
    uint256 public lastRefundId;
    uint256 public refundCount;
    uint8 public mode;
    bool public reentrySucceeded;
    bytes4 public reentryError;
    constructor(address coordinator) D20VRFConsumer(coordinator) {}
    function setMode(uint8 next) external { mode=next; }
    function fund() external payable {}
    function request(address refundAddress) external payable returns(uint256) {
        lastRequestId=ID20VRF(vrfCoordinator).requestRandomness{value:msg.value}(bytes32(0),100000,refundAddress);
        return lastRequestId;
    }
    function _fulfillRandomness(uint256,bytes32) internal override {}
    function _onRefund(uint256 id) internal override {
        require(D20VRFCoordinator(vrfCoordinator).getRequest(id).refunded,"refund must settle first");
        if(mode==1) revert("application failure");
        if(mode==2) { assembly { for {} 1 {} {} } }
        if(mode==3) { assembly { revert(0,1000000) } }
        if(mode==4) {
            (bool a,)=vrfCoordinator.call(abi.encodeCall(D20VRFCoordinator.refundRequest,(id)));
            (bool b,)=vrfCoordinator.call(abi.encodeCall(D20VRFCoordinator.withdrawRefundCredit,(payable(address(this)))));
            (bool c,)=vrfCoordinator.call(abi.encodeCall(D20VRFCoordinator.retryRefundCallback,(id,uint32(100000))));
            reentrySucceeded=a||b||c;
        }
        if(mode==5) { assembly { return(0,1000000) } }
        if(mode==6 || mode==7) {
            bytes memory callData;
            if(mode==6) callData=abi.encodeCall(ID20VRF.requestRandomness,(bytes32(0),uint32(100000),address(this)));
            else {
                RandomnessMapping.Spec memory spec;
                callData=abi.encodeCall(ID20VRF.requestMappedRandomness,(bytes32(0),uint32(100000),address(this),spec));
            }
            bytes memory reason;
            (reentrySucceeded,reason)=vrfCoordinator.call{value:ID20VRF(vrfCoordinator).quoteFee(100000)}(callData);
            reentryError=bytes4(reason);
        }
        lastRefundId=id;
        refundCount++;
    }
}

contract LegacyRefundConsumer {
    address public immutable coordinator;
    uint256 public lastRequestId;
    constructor(address c){coordinator=c;}
    function request(address refundAddress) external payable {
        lastRequestId=ID20VRF(coordinator).requestRandomness{value:msg.value}(bytes32(0),100000,refundAddress);
    }
    function rawFulfillRandomness(uint256,bytes32) external view {require(msg.sender==coordinator);}
}

contract ReentrantRefundReceiver {
    address public immutable coordinator;
    event ReentryAttempted(bool success,bytes4 reason);
    constructor(address c){coordinator=c;}
    receive() external payable {
        (bool success,bytes memory reason)=coordinator.call{value:msg.value}(
            abi.encodeCall(ID20VRF.requestRandomness,(bytes32(0),uint32(100000),address(this)))
        );
        emit ReentryAttempted(success,bytes4(reason));
    }
}
