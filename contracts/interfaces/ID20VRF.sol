// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {RandomnessMapping} from "../libraries/RandomnessMapping.sol";

interface ID20VRF {
    /// @notice Exact fee for a request sent in this same transaction. Through eth_call the base fee is often reported as 0,
    ///         so off-chain senders must quote with quoteFeeAt and the latest header baseFeePerGas plus a buffer.
    function quoteFee(uint32 callbackGasLimit) external view returns (uint256);
    /// @notice max(minFee, feeMultiplier * baseFee * (fulfillGasOverhead + callbackGasLimit)) over the current parameters.
    function quoteFeeAt(uint32 callbackGasLimit, uint256 baseFee) external view returns (uint256);
    function requestRandomness(bytes32 clientSeed, uint32 callbackGasLimit, address _refundAddress)
        external payable returns (uint256 requestId);
    function requestMappedRandomness(
        bytes32 clientSeed, uint32 callbackGasLimit, address _refundAddress, RandomnessMapping.Spec calldata spec
    ) external payable returns (uint256 requestId);
    function getMappedResult(uint256 requestId) external view returns (uint256[] memory);
}

interface ID20VRFConsumer {
    function rawFulfillRandomness(uint256 requestId, bytes32 randomness) external;
}

/// @notice Optional notification after the RNG fee was refunded or recorded as backed credit.
interface ID20VRFRefundConsumer {
    function onRefund(uint256 requestId) external;
}
