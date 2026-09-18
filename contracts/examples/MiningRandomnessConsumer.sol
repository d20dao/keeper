// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {D20VRFConsumer} from "../D20VRFConsumer.sol";
import {ID20VRF} from "../interfaces/ID20VRF.sol";

/// @notice Integration building block, not a replacement for the game's mining/economy contracts.
/// @dev The actual game validates PoW, locks payment and immutable claim attributes BEFORE
///      calling _requestForClaim inside that same transaction. No user reveal/contribution call.
abstract contract MiningRandomnessConsumer is D20VRFConsumer {
    struct ClaimRandomness { uint256 requestId; bytes32 randomness; bool requested; bool ready; }
    mapping(uint256 => ClaimRandomness) public claimRandomness;
    mapping(uint256 => uint256) public requestToClaim;
    mapping(uint256 => bool) private knownRequest;
    event ClaimRandomnessRequested(uint256 indexed claimId, uint256 indexed requestId);
    event ClaimReady(uint256 indexed claimId, uint256 indexed requestId, bytes32 randomness);
    error ClaimAlreadyRequested();
    error UnexpectedCallback();
    error ClaimNotReady();
    error InvalidCandidate();

    constructor(address coordinator) D20VRFConsumer(coordinator) {}

    function _requestForClaim(uint256 claimId, bytes32 lockedWork, uint32 callbackGasLimit, address refundAddress)
        internal returns (uint256 requestId)
    {
        if (claimRandomness[claimId].requested) revert ClaimAlreadyRequested();
        claimRandomness[claimId].requested = true;
        ID20VRF rng = ID20VRF(vrfCoordinator);
        // The same-transaction quote is exact; only off-chain senders need a buffer.
        requestId = rng.requestRandomness{value: rng.quoteFee(callbackGasLimit)}(
            keccak256(abi.encode(claimId, lockedWork)), callbackGasLimit, refundAddress
        );
        claimRandomness[claimId].requestId = requestId;
        requestToClaim[requestId] = claimId;
        knownRequest[requestId] = true;
        emit ClaimRandomnessRequested(claimId, requestId);
    }

    function _fulfillRandomness(uint256 requestId, bytes32 randomness) internal override {
        if (!knownRequest[requestId]) revert UnexpectedCallback();
        uint256 claimId = requestToClaim[requestId];
        ClaimRandomness storage claim = claimRandomness[claimId];
        if (claim.ready) revert UnexpectedCallback();
        claim.randomness = randomness;
        claim.ready = true;
        emit ClaimReady(claimId, requestId, randomness);
    }

    function candidateSeed(uint256 claimId, uint8 candidate) public view returns (bytes32) {
        ClaimRandomness storage claim = claimRandomness[claimId];
        if (!claim.ready) revert ClaimNotReady();
        if (candidate >= 3) revert InvalidCandidate();
        return keccak256(abi.encode(
            keccak256("VRF_D20DAO_CARD_V1"), block.chainid, address(this),
            claimId, claim.requestId, claim.randomness, candidate
        ));
    }
}
