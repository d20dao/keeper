// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {ID20VRFConsumer, ID20VRFRefundConsumer} from "./interfaces/ID20VRF.sol";

/// @notice Every consumer must authenticate callbacks before changing application state.
abstract contract D20VRFConsumer is ID20VRFConsumer, ID20VRFRefundConsumer {
    address public immutable vrfCoordinator;
    error OnlyCoordinator();
    error InvalidCoordinator();

    constructor(address coordinator) {
        if (coordinator.code.length == 0) revert InvalidCoordinator();
        vrfCoordinator = coordinator;
    }

    function rawFulfillRandomness(uint256 requestId, bytes32 randomness) external {
        if (msg.sender != vrfCoordinator) revert OnlyCoordinator();
        _fulfillRandomness(requestId, randomness);
    }

    function onRefund(uint256 requestId) external {
        if (msg.sender != vrfCoordinator) revert OnlyCoordinator();
        _onRefund(requestId);
    }

    /// @dev Optional lifecycle notification. Funds may be paid to a different refundAddress
    ///      or held as credit. Correlate the request and handle application refunds separately.
    function _onRefund(uint256 requestId) internal virtual {}

    /// @dev Store randomness only. Keep minting and transfers out of the callback.
    function _fulfillRandomness(uint256 requestId, bytes32 randomness) internal virtual;
}
