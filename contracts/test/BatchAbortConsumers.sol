// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {D20VRFConsumer} from "../D20VRFConsumer.sol";
import {ID20VRF} from "../interfaces/ID20VRF.sol";
import {RandomnessMapping} from "../libraries/RandomnessMapping.sol";

/// @dev TEST ONLY. The SDK's RaffleConsumer example: one winner drawn from a frozen list of entrants. An expired draw is
///      refunded and may be drawn again with a fresh request, which is what a batch abort exploits.
contract BatchAbortRaffle is D20VRFConsumer {
    uint32 private constant CALLBACK_GAS = 100_000;
    address[] public entrants;
    uint256 public drawId;
    bytes32 public word;
    bool public closed;
    bool public drawn;

    constructor(address coordinator) D20VRFConsumer(coordinator) {}

    function enter() external {
        require(!closed && entrants.length < 256, "closed");
        entrants.push(msg.sender);
    }

    function draw() external payable {
        require(drawId == 0 && entrants.length != 0, "drawn");
        closed = true;
        drawId = ID20VRF(vrfCoordinator).requestMappedRandomness{value: msg.value}(
            keccak256(abi.encode(entrants)), CALLBACK_GAS, msg.sender,
            RandomnessMapping.Spec(RandomnessMapping.Operation.ChooseOne, 0, 0, 1, uint32(entrants.length))
        );
    }

    function _fulfillRandomness(uint256 requestId, bytes32 randomness) internal override {
        require(requestId == drawId && !drawn, "unexpected");
        word = randomness;
        drawn = true;
    }

    function _onRefund(uint256 requestId) internal override {
        if (requestId == drawId && !drawn) drawId = 0;
    }

    function winner() external view returns (address) {
        require(drawn, "not drawn");
        return entrants[ID20VRF(vrfCoordinator).getMappedResult(drawId)[0]];
    }
}

/// @dev TEST ONLY. The batch-abort drill's helper request with a 1,000,000-gas callback. Armed, it burns its whole callback budget,
///      but only in a real transaction: Arc runs eth_estimateGas with tx.gasprice == 0, so a keeper's estimate sees a
///      cheap callback.
contract BatchAbortHelper {
    ID20VRF private immutable coordinator;
    BatchAbortAttacker private immutable boss;

    constructor(ID20VRF coordinator_, BatchAbortAttacker boss_) {
        coordinator = coordinator_;
        boss = boss_;
    }

    function request(bytes32 seed) external payable returns (uint256) {
        return coordinator.requestRandomness{value: msg.value}(seed, 1_000_000, address(boss));
    }

    function rawFulfillRandomness(uint256, bytes32) external view {
        if (tx.gasprice != 0 && boss.shouldAbort()) {
            while (true) {}
        }
    }
}

/// @dev TEST ONLY. A raffle entrant that, in one transaction, draws and opens two helper requests, so the three share a
///      block, a deadline and consecutive ids and a keeper batches them as [draw, first helper, second helper]. During
///      the first helper's callback the draw is already fulfilled in the same batch, so it knows whether it lost.
contract BatchAbortAttacker {
    ID20VRF private immutable coordinator;
    BatchAbortRaffle private immutable raffle;
    BatchAbortHelper public immutable first;
    BatchAbortHelper public immutable second;

    constructor(ID20VRF coordinator_, BatchAbortRaffle raffle_) {
        coordinator = coordinator_;
        raffle = raffle_;
        first = new BatchAbortHelper(coordinator_, this);
        second = new BatchAbortHelper(coordinator_, this);
    }

    function enter() external {
        raffle.enter();
    }

    function drawAndArm(bytes32 salt) external returns (uint256 drawId, uint256 firstId, uint256 secondId) {
        raffle.draw{value: coordinator.quoteFee(100_000)}();
        drawId = raffle.drawId();
        firstId = first.request{value: coordinator.quoteFee(1_000_000)}(keccak256(abi.encode(salt, "first")));
        secondId = second.request{value: coordinator.quoteFee(1_000_000)}(keccak256(abi.encode(salt, "second")));
    }

    function shouldAbort() external view returns (bool) {
        return raffle.drawn() && raffle.winner() != address(this);
    }

    /// Reverts unless called with tx.gasprice == 0: an eth_estimateGas of it tells whether estimates run unpriced.
    function requireUnpricedSimulation() external view {
        require(tx.gasprice == 0, "priced simulation");
    }

    receive() external payable {}
}
