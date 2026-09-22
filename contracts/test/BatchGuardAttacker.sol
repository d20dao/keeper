// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {D20VRFConsumer} from "../D20VRFConsumer.sol";
import {ID20VRF} from "../interfaces/ID20VRF.sol";

/// @dev TEST ONLY. A consumer whose 1,000,000-gas callback burns its whole budget when its owner's draw, served
///      earlier in the same batch, lost. Mode 1 burns only in a priced transaction (Arc runs eth_estimateGas with
///      tx.gasprice == 0, so estimation sees a cheap callback); mode 2 always burns.
contract AbortHelper is D20VRFConsumer {
    BatchGuardAttacker private immutable boss;
    constructor(address coordinator, BatchGuardAttacker owner_) D20VRFConsumer(coordinator) { boss = owner_; }
    function request(bytes32 seed) external payable returns (uint256) {
        return ID20VRF(vrfCoordinator).requestRandomness{value: msg.value}(seed, 1_000_000, address(boss));
    }
    function _fulfillRandomness(uint256, bytes32) internal view override {
        uint8 mode = boss.mode();
        if ((mode == 2 || (mode == 1 && tx.gasprice != 0)) && boss.lost()) { assembly { for { } 1 { } { } } }
    }
}

/// @dev TEST ONLY. Reproduces the batch-abort issue: one transaction opens a draw (1 in 8 wins) and two helper
///      requests, so a keeper batch serves them in order [draw, helper, helper] and the helpers see the draw's result.
contract BatchGuardAttacker is D20VRFConsumer {
    AbortHelper public immutable m;
    AbortHelper public immutable n;
    uint8 public mode;
    uint256 public drawId;
    bool public drawn;
    bool public won;
    constructor(address coordinator) D20VRFConsumer(coordinator) {
        m = new AbortHelper(coordinator, this);
        n = new AbortHelper(coordinator, this);
    }
    function setMode(uint8 next) external { mode = next; }
    function lost() external view returns (bool) { return drawn && !won; }
    function drawAndArm(uint256 fee, uint256 helperFee) external payable returns (uint256 draw, uint256 mId, uint256 nId) {
        drawn = false;
        won = false;
        draw = drawId = ID20VRF(vrfCoordinator).requestRandomness{value: fee}(keccak256(abi.encode("draw", block.number)), 100_000, address(this));
        mId = m.request{value: helperFee}(keccak256("m"));
        nId = n.request{value: helperFee}(keccak256("n"));
    }
    function _fulfillRandomness(uint256 id, bytes32 value) internal override {
        if (id != drawId) return;
        drawn = true;
        won = uint256(value) % 8 == 0;
    }
    receive() external payable {}
}

/// @dev TEST ONLY. A committer that burns the whole 30000-gas keeper payment call, the costliest keeper share path.
contract BurningRecipient {
    receive() external payable { assembly { for { } 1 { } { } } }
}
