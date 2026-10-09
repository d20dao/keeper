// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {ArbitrumBlocks} from "../ArbitrumBlocks.sol";

/// @dev TEST ONLY. ArbitrumBlocks through a contract, as the round coordinator calls it. sample() records in a transaction, so a test knows
/// the block it ran in.
contract ArbitrumBlocksHarness {
    uint256 public sampledNumber;
    function number() external view returns (uint256) { return ArbitrumBlocks.number(); }
    function sample() external { sampledNumber = ArbitrumBlocks.number(); }
}
