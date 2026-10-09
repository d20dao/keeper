// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

/// @notice The L2 block number on an Arbitrum (Nitro) chain such as Robinhood Chain. Solidity's block.number is the parent chain's block
/// number there, so it is never used.
library ArbitrumBlocks {
    /// @notice ArbSys(0x64).arbBlockNumber(). Reverts if the precompile does not answer one word.
    function number() internal view returns (uint256 n) {
        assembly ("memory-safe") {
            mstore(0, shl(224, 0xa3b1b31d)) // arbBlockNumber()
            // Yul evaluates arguments right to left: the call must complete before returndatasize() is read.
            let ok := staticcall(gas(), 0x64, 0, 4, 0, 32)
            if iszero(and(ok, eq(returndatasize(), 32))) { revert(0, 0) }
            n := mload(0)
        }
    }
}
