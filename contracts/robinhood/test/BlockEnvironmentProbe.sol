// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

/// @dev TEST ONLY. Reads every block value that the opcode scan of the Robinhood contracts looks for, so that the scan has a compiled contract
/// it must find each of them in. Never deployed.
contract BlockEnvironmentProbe {
    function read() external view returns (uint256 number, bytes32 hash, address coinbase, uint256 prevrandao, uint256 gaslimit) {
        number = block.number;
        hash = blockhash(block.number - 1);
        coinbase = block.coinbase;
        prevrandao = block.prevrandao;
        gaslimit = block.gaslimit;
    }
}
