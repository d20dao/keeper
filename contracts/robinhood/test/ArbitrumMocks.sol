// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

// TEST ONLY. Test doubles of the Arbitrum system contracts, installed with setCode at their real addresses (test/helpers/robinhood.ts).
// L2 block numbers sit OFFSET above the local ones, so a contract that reads block.number instead of ArbSys disagrees with them by a
// million blocks and fails loudly. Settings live in hashed storage slots.

/// @notice ArbSys at 0x64. arbBlockNumber() is the local block number plus OFFSET.
/// @dev Mode 0 answers; 1 reverts; 2 answers 64 bytes; 3 answers 31 bytes; 4 answers nothing.
contract MockArbSys {
    uint256 public constant OFFSET = 1_000_000;
    bytes32 private constant MODE = keccak256("d20.mock.arbsys.mode");
    function mode() public view returns (uint8 value) { bytes32 slot = MODE; assembly { value := sload(slot) } }
    function setMode(uint8 next) external { bytes32 slot = MODE; assembly { sstore(slot, next) } }
    fallback() external {
        require(msg.data.length == 4 && bytes4(msg.data) == 0xa3b1b31d, "MockArbSys: only arbBlockNumber()");
        uint8 current = mode();
        if (current == 1) revert("MockArbSys: down");
        uint256 n = block.number + OFFSET;
        assembly {
            switch current
            case 2 { mstore(0, n) mstore(32, n) return(0, 64) }
            case 3 { mstore(0, n) return(1, 31) }
            case 4 { return(0, 0) }
            default { mstore(0, n) return(0, 32) }
        }
    }
}

/// @notice NodeInterface at 0xc8, which a node serves to eth_call only. gasEstimateL1Component answers the configured values.
contract MockNodeInterface {
    uint64 public gasEstimateForL1;
    uint256 public baseFee;
    uint256 public l1BaseFeeEstimate;
    function setL1Component(uint64 gas, uint256 base, uint256 l1Base) external { gasEstimateForL1 = gas; baseFee = base; l1BaseFeeEstimate = l1Base; }
    function gasEstimateL1Component(address, bool, bytes calldata) external payable returns (uint64, uint256, uint256) {
        return (gasEstimateForL1, baseFee, l1BaseFeeEstimate);
    }
}
