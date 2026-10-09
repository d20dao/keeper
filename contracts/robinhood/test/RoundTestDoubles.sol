// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {D20VRFConsumer} from "../../D20VRFConsumer.sol";
import {ID20VRF} from "../../interfaces/ID20VRF.sol";
import {RandomnessMapping} from "../../libraries/RandomnessMapping.sol";
import {VRF} from "../../vendor/VRF.sol";
import {D20VRFCoordinatorRobinhood} from "../D20VRFCoordinatorRobinhood.sol";

/// @dev TEST ONLY. A consumer whose result callback and refund notification misbehave on demand, for the round coordinator's settlement,
/// retry and reentrancy tests. Callback modes: 0 stores the result; 1 reverts; 2 spends all the gas it is given; 3 reverts with 1,000,000
/// bytes of data; 4 tries to enter the coordinator again (fulfillRandomness, refundRequest, retryCallback, requestRandomness), announces
/// each answer with Reentry, then stores the result. Refund modes: 0 records the notice; 1 reverts; 2 spends all its gas; 3 reverts with
/// 1,000,000 bytes; 4 tries refundRequest, withdrawRefundCredit, retryRefundCallback and requestRandomness again, then records the notice.
/// Every notice also records whether the coordinator already showed the request as refunded.
contract HostileRoundConsumer is D20VRFConsumer {
    uint8 public callbackMode;
    uint8 public refundMode;
    mapping(uint256 => bytes32) public results;
    mapping(uint256 => uint256) public deliveries;
    mapping(uint256 => uint256) public refundNotices;
    mapping(uint256 => bool) public refundedWhenNotified;
    uint256 public lastRequestId;

    /// @notice One attempt to enter the coordinator from a callback: the selector called and the selector of its revert (zero on success).
    event Reentry(bytes4 indexed called, bool success, bytes4 error);

    constructor(address coordinator) D20VRFConsumer(coordinator) {}
    receive() external payable {}
    function setCallbackMode(uint8 next) external { callbackMode = next; }
    function setRefundMode(uint8 next) external { refundMode = next; }

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
    /// @notice A request paying exactly quoteFee in the requesting transaction; the rest of msg.value stays with this contract.
    function requestAtQuote(bytes32 seed, uint32 gasLimit, address refundTo) external payable returns (uint256 fee) {
        fee = ID20VRF(vrfCoordinator).quoteFee(gasLimit);
        lastRequestId = ID20VRF(vrfCoordinator).requestRandomness{value: fee}(seed, gasLimit, refundTo);
    }

    function _fulfillRandomness(uint256 id, bytes32 randomness) internal override {
        uint8 mode = callbackMode;
        if (mode == 1) revert("consumer failure");
        if (mode == 2) { assembly { for { } 1 { } { } } }
        if (mode == 3) { assembly { revert(0, 1000000) } }
        if (mode == 4) {
            VRF.Proof memory proof;
            _enter(abi.encodeCall(D20VRFCoordinatorRobinhood.fulfillRandomness, (id, proof, bytes(""))));
            _enter(abi.encodeCall(D20VRFCoordinatorRobinhood.refundRequest, (id)));
            _enter(abi.encodeCall(D20VRFCoordinatorRobinhood.retryCallback, (id, uint32(100_000))));
            _enter(abi.encodeCall(ID20VRF.requestRandomness, (bytes32(0), uint32(100_000), address(this))));
        }
        results[id] = randomness;
        deliveries[id]++;
    }

    function _onRefund(uint256 id) internal override {
        refundedWhenNotified[id] = D20VRFCoordinatorRobinhood(vrfCoordinator).getRoundRequest(id).refunded;
        uint8 mode = refundMode;
        if (mode == 1) revert("application failure");
        if (mode == 2) { assembly { for { } 1 { } { } } }
        if (mode == 3) { assembly { revert(0, 1000000) } }
        if (mode == 4) {
            _enter(abi.encodeCall(D20VRFCoordinatorRobinhood.refundRequest, (id)));
            _enter(abi.encodeCall(D20VRFCoordinatorRobinhood.withdrawRefundCredit, (payable(address(this)))));
            _enter(abi.encodeCall(D20VRFCoordinatorRobinhood.retryRefundCallback, (id, uint32(100_000))));
            _enter(abi.encodeCall(ID20VRF.requestRandomness, (bytes32(0), uint32(100_000), address(this))));
        }
        refundNotices[id]++;
    }

    function _enter(bytes memory data) private {
        (bool success, bytes memory reason) = vrfCoordinator.call(data);
        emit Reentry(bytes4(data), success, success ? bytes4(0) : bytes4(reason));
    }
}

/// @dev TEST ONLY. A contract account for the coordinator to pay: a keeper, a backup keeper, a fee recipient or a refund address. Modes
/// for a native payment: 0 accepts; 1 reverts; 2 spends all the gas it is given; 3 tries to request randomness from the coordinator in its
/// receive hook and announces the answer. execute() sends any call from this account and bubbles a revert, so it can submit proofs and
/// withdraw its credits.
contract RoundPayee {
    uint8 public mode;
    address public immutable coordinator;
    event Reentry(bool success, bytes4 error);
    constructor(address coordinator_) { coordinator = coordinator_; }
    function setMode(uint8 next) external { mode = next; }
    function execute(address target, bytes calldata data) external returns (bytes memory result) {
        bool success;
        (success, result) = target.call(data);
        if (!success) assembly { revert(add(result, 32), mload(result)) }
    }
    receive() external payable {
        uint8 current = mode;
        if (current == 1) revert("payment refused");
        if (current == 2) { assembly { for { } 1 { } { } } }
        if (current == 3) {
            (bool success, bytes memory reason) = coordinator.call(
                abi.encodeCall(ID20VRF.requestRandomness, (bytes32(0), uint32(100_000), address(this))));
            emit Reentry(success, success ? bytes4(0) : bytes4(reason));
        }
    }
}

/// @dev TEST ONLY. Requests randomness from its constructor, when it has no code yet: the coordinator must refuse it as an account.
contract ConstructorRoundRequester {
    constructor(address coordinator) payable {
        ID20VRF(coordinator).requestRandomness{value: msg.value}(bytes32(0), 100_000, msg.sender);
    }
}
