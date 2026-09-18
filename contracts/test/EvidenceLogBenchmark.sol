// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

/// @dev Isolated local measurement, not a deployed oracle or a mainnet fee promise.
contract EvidenceLogBenchmark {
    bytes public storedPacket;
    event Evidence(uint256 indexed id,bytes32 indexed transcriptHash,bytes packet);
    function baseline(bytes calldata packet) external pure returns(bytes32) {return keccak256(packet);}
    function logPacket(bytes calldata packet) external {emit Evidence(1,keccak256(packet),packet);}
    function measureLogSegment(bytes calldata packet) external returns(uint256 used) {
        uint256 beforeGas=gasleft();
        emit Evidence(1,keccak256(packet),packet);
        return beforeGas-gasleft();
    }
    function storePacket(bytes calldata packet) external {storedPacket=packet;}
}
