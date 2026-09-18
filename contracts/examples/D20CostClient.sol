// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {Ownable2StepUpgradeable} from "@openzeppelin/contracts-upgradeable/access/Ownable2StepUpgradeable.sol";
import {UUPSUpgradeable} from "@openzeppelin/contracts/proxy/utils/UUPSUpgradeable.sol";
import {ID20VRF, ID20VRFConsumer} from "../interfaces/ID20VRF.sol";

/// @notice Restricted, supervised cost-measurement client; not a production application.
contract D20CostClient is Ownable2StepUpgradeable, UUPSUpgradeable, ID20VRFConsumer {
    ID20VRF public coordinator;
    address public tester;
    uint256 public lastRequestId;
    mapping(uint256 => bool) public requested;
    mapping(uint256 => bool) public callbackFailure;
    mapping(uint256 => bytes32) public results;
    mapping(uint256 => bool) public completed;
    uint256[40] private __gap;

    error InvalidConfig();
    error OnlyTester();
    error IncorrectFee(uint256 expected, uint256 actual);
    error OnlyCoordinator();
    error UnknownRequest();
    error DuplicateRequest();
    error AlreadyCompleted();
    error DeliberateCallbackFailure();

    /// @custom:oz-upgrades-unsafe-allow constructor
    constructor() { _disableInitializers(); }

    function initialize(address coordinator_, address initialOwner, address tester_) external initializer {
        __Ownable_init(initialOwner);
        __Ownable2Step_init();
        if(coordinator_.code.length==0 || tester_==address(0)) revert InvalidConfig();
        coordinator=ID20VRF(coordinator_);
        tester=tester_;
    }

    modifier onlyTester() {
        if(msg.sender!=tester) revert OnlyTester();
        _;
    }

    function request(bytes32 seed, uint32 callbackGas, bool failCallback)
        external payable onlyTester returns(uint256 requestId)
    {
        uint256 fee=coordinator.quoteFee(callbackGas);
        if(msg.value<fee) revert IncorrectFee(fee,msg.value);
        // The tester quotes off-chain with a buffer; the coordinator credits any excess to msg.sender as refund credit.
        requestId=coordinator.requestRandomness{value:msg.value}(seed,callbackGas,msg.sender);
        if(requestId==0) revert UnknownRequest();
        if(requested[requestId]) revert DuplicateRequest();
        requested[requestId]=true;
        callbackFailure[requestId]=failCallback;
        lastRequestId=requestId;
    }

    function rawFulfillRandomness(uint256 requestId, bytes32 randomness) external {
        if(msg.sender!=address(coordinator)) revert OnlyCoordinator();
        if(!requested[requestId]) revert UnknownRequest();
        if(completed[requestId]) revert AlreadyCompleted();
        if(callbackFailure[requestId]) revert DeliberateCallbackFailure();
        results[requestId]=randomness;
        completed[requestId]=true;
    }

    function setCallbackFailure(uint256 requestId, bool failCallback) external onlyTester {
        if(!requested[requestId]) revert UnknownRequest();
        if(completed[requestId]) revert AlreadyCompleted();
        callbackFailure[requestId]=failCallback;
    }

    function _authorizeUpgrade(address) internal override onlyOwner {}
}
