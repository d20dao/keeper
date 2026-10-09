// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {IBeaconVerifier} from "../interfaces/IBeaconVerifier.sol";

/// @notice The drand beacons a D20 round coordinator binds requests to: their registrations, the schedule of the one in force, and
/// the rounds verified on chain. A request binds the first round of the beacon in force that is scheduled at least ROUND_LEAD
/// seconds after its block's timestamp. The first fulfillment that serves a request of a round verifies the round's signature once
/// and caches its randomness; every later request of the round reads the cache.
/// @dev Abstract base of D20VRFCoordinatorRobinhood: one proxy, one storage, no external call between the two. Its state lives in the
/// ERC-7201 namespace below, apart from the coordinator's linear layout and independent of the inheritance order. Owner-only entry
/// points live in the coordinator.
abstract contract BeaconBook {
    /// @notice A request binds the first round scheduled at or after its block's timestamp plus this many seconds.
    uint64 public constant ROUND_LEAD = 3;
    uint256 public constant MAX_BEACONS = 256;
    /// @notice The longest round period a beacon may have. ROUND_LEAD + period, publication and fulfillment must fit well inside a
    /// request's 60-second window, and a backup keeper that waits on a request's age must not race the primary.
    uint64 public constant MAX_BEACON_PERIOD = 10;
    /// @notice A scheduled beacon change takes effect at least this long after the transaction that schedules it.
    uint64 public constant MIN_SCHEDULE_LEAD = 10 minutes;
    /// @notice Gas a beacon verifier gets for one question. D20BeaconVerifier uses about 175,000 to 230,000 of it to verify a round;
    /// a direct call needs a gas limit of about 285,000 because its pairing reserves 200,000.
    uint256 public constant ROUND_VERIFY_GAS = 400_000;
    // The verifier must really get ROUND_VERIFY_GAS (EIP-150 forwards at most 63/64), or the question reverts BeaconGasTooLow
    // instead of reading a valid signature cut short as invalid.
    uint256 internal constant ROUND_VERIFY_GAS_NEEDED = ROUND_VERIFY_GAS + ROUND_VERIFY_GAS / 63 + 5_000;
    /// @notice Domain of a beacon registration's identity (beaconIdentity).
    bytes32 public constant ROUND_BEACON_DOMAIN = keccak256("D20_ROUND_BEACON_V1");

    /// @dev getBeacon's tuple.
    struct Beacon { address verifier; uint64 genesis; uint64 period; bytes32 chainHash; bytes publicKey; }
    /// @dev A beacon to register and a past round of it whose signature must verify under the gas allowance of a fulfillment.
    struct BeaconRegistration {
        address verifier; bytes32 chainHash; bytes publicKey; uint64 genesis; uint64 period; uint64 sampleRound; bytes sampleSignature;
    }
    struct StoredBeacon { address verifier; uint40 genesis; uint16 period; bytes32 chainHash; bytes publicKey; }
    /// @dev A beacon in force from `since` on, with the genesis and period its rounds are assigned by.
    struct Era { uint8 beaconId; uint40 since; uint40 genesis; uint16 period; }
    /// @dev One slot: the era in force and, when nextFrom is not zero, the beacon that replaces it from that timestamp on.
    struct Schedule {
        uint8 beaconId; uint40 since; uint40 genesis; uint16 period;
        uint8 nextBeaconId; uint40 nextFrom; uint40 nextGenesis; uint16 nextPeriod;
    }
    /// @custom:storage-location erc7201:d20.storage.BeaconBook
    struct BeaconBookStorage {
        StoredBeacon[] beacons;                                         // append-only: a beacon's id is its index
        Schedule schedule;
        Era[] past;                                                     // eras a later one replaced, oldest first
        mapping(bytes32 => bool) registered;                            // registration identities
        mapping(uint8 => mapping(uint64 => bytes32)) roundRandomness;   // zero while the round is not verified on chain
    }
    // keccak256(abi.encode(uint256(keccak256("d20.storage.BeaconBook")) - 1)) & ~bytes32(uint256(0xff))
    bytes32 private constant BEACON_BOOK_LOCATION = 0xf6225eefaeaf4ae83c5e55b6cf54b47540db50cae2f6ec243c42137121953f00;

    error InvalidBeacon();
    error InvalidSchedule();
    /// @notice Too little gas was left to give a beacon verifier its whole allowance.
    error BeaconGasTooLow();
    /// @notice A beacon verifier reverted, ran out of its allowance or answered something other than one boolean word.
    error BeaconVerifierFailed();
    error InvalidRoundSignature();
    error RoundUnavailable();

    event BeaconRegistered(uint8 indexed beaconId, bytes32 indexed identity, address indexed verifier, bytes32 chainHash, bytes publicKey,
        uint64 genesis, uint64 period);
    event BeaconScheduled(uint8 indexed beaconId, uint64 indexed fromTime);
    event BeaconScheduleCancelled(uint8 indexed beaconId, uint64 indexed fromTime);
    /// @notice A round verified on chain for the first time, with its signature: the evidence every request of the round replays.
    /// Emitted by whichever fulfillment first serves a request of the round; anyone may submit fulfillments, so it does not identify a keeper.
    event RoundVerified(uint8 indexed beaconId, uint64 indexed round, bytes32 randomness, bytes signature);

    // ---- views
    function beaconCount() external view returns (uint256) { return _book().beacons.length; }
    function getBeacon(uint8 beaconId) external view returns (Beacon memory b) {
        StoredBeacon storage s = _beacon(beaconId);
        b = Beacon(s.verifier, s.genesis, s.period, s.chainHash, s.publicKey);
    }
    /// @notice A registration's identity: keccak256(abi.encode(ROUND_BEACON_DOMAIN, verifier, chainHash, keccak256(publicKey), genesis, period)).
    /// No two registrations share one.
    function beaconIdentity(uint8 beaconId) public view returns (bytes32) {
        StoredBeacon storage s = _beacon(beaconId);
        return _identity(s.verifier, s.chainHash, s.publicKey, s.genesis, s.period);
    }
    /// @notice The beacon in force now and since when, and the change still pending, if any (nextFrom zero when none). A change whose
    /// time has come shows as the beacon in force.
    function beaconSchedule() external view returns (uint8 beaconId, uint64 since, uint8 nextBeaconId, uint64 nextFrom) {
        Schedule memory s = _settled(_book().schedule);
        return (s.beaconId, s.since, s.nextBeaconId, s.nextFrom);
    }
    /// @notice The beacon and round that a request sent in a block with this timestamp binds, for any timestamp since initialization.
    function roundAt(uint64 timestamp) external view returns (uint8 beaconId, uint64 round) {
        BeaconBookStorage storage $ = _book();
        Schedule memory s = $.schedule;
        if (s.nextFrom != 0 && timestamp >= s.nextFrom) return (s.nextBeaconId, _round(s.nextGenesis, s.nextPeriod, timestamp));
        uint256 i = $.past.length;
        if (timestamp >= s.since || i == 0) return (s.beaconId, _round(s.genesis, s.period, timestamp));
        // Before the era in force: the latest replaced era that had begun, or the first one.
        Era memory e;
        do e = $.past[--i]; while (i != 0 && timestamp < e.since);
        return (e.beaconId, _round(e.genesis, e.period, timestamp));
    }
    /// @notice When a beacon's round is scheduled: genesis + (round - 1) × period.
    function roundTime(uint8 beaconId, uint64 round) external view returns (uint256) {
        StoredBeacon storage s = _beacon(beaconId);
        if (round == 0) revert InvalidBeacon();
        return uint256(s.genesis) + uint256(round - 1) * s.period;
    }
    /// @notice A round's randomness, sha256 of its verified signature (drand's own randomness value); zero while not verified on chain.
    function roundRandomness(uint8 beaconId, uint64 round) external view returns (bytes32) {
        return _book().roundRandomness[beaconId][round];
    }
    /// @notice Whether signature is a registered beacon's valid signature of round. Reverts BeaconGasTooLow, never answers false, when the
    /// call's gas cannot give the verifier its whole allowance (an eth_call needs about 460,000 gas).
    function checkRoundSignature(uint8 beaconId, uint64 round, bytes calldata signature) external view returns (bool) {
        return _verify(_beacon(beaconId), round, signature);
    }

    // ---- owner operations, reached through the coordinator's onlyOwner wrappers
    /// @dev Append a beacon. Its verifier must accept the key and the sample round's signature under the gas a fulfillment gives it.
    /// Owner checklist, not checkable here: the verifier's scheme has unique signatures, and its code hash is pinned off chain.
    function _registerBeacon(BeaconRegistration memory r) internal returns (uint8 id) {
        BeaconBookStorage storage $ = _book();
        uint256 count = $.beacons.length;
        if (count == MAX_BEACONS || r.verifier.code.length == 0 || r.chainHash == bytes32(0) || r.genesis == 0 ||
            r.genesis > type(uint40).max || r.period == 0 || r.period > MAX_BEACON_PERIOD || r.sampleRound == 0 ||
            uint256(r.genesis) + uint256(r.sampleRound - 1) * r.period > block.timestamp) revert InvalidBeacon();
        if (!_yes(r.verifier, abi.encodeCall(IBeaconVerifier.isValidPublicKey, (r.publicKey))) ||
            !_yes(r.verifier, abi.encodeCall(IBeaconVerifier.verifyRound, (r.publicKey, r.sampleRound, r.sampleSignature)))) revert InvalidBeacon();
        bytes32 identity = _identity(r.verifier, r.chainHash, r.publicKey, r.genesis, r.period);
        if ($.registered[identity]) revert InvalidBeacon();
        $.registered[identity] = true;
        id = uint8(count);
        $.beacons.push(StoredBeacon(r.verifier, uint40(r.genesis), uint16(r.period), r.chainHash, r.publicKey));
        emit BeaconRegistered(id, identity, r.verifier, r.chainHash, r.publicKey, r.genesis, r.period);
    }
    /// @dev The first beacon, in force from initialization.
    function _startWith(uint8 beaconId) internal {
        StoredBeacon storage b = _beacon(beaconId);
        _book().schedule = Schedule(beaconId, uint40(block.timestamp), b.genesis, b.period, 0, 0, 0, 0);
        emit BeaconScheduled(beaconId, uint64(block.timestamp));
    }
    /// @dev Replace the beacon in force from fromTime on, at least MIN_SCHEDULE_LEAD ahead. A change that has taken effect is settled
    /// first, its predecessor kept for roundAt; one still pending is replaced.
    function _scheduleBeacon(uint8 beaconId, uint64 fromTime) internal {
        StoredBeacon storage b = _beacon(beaconId);
        if (fromTime < block.timestamp + MIN_SCHEDULE_LEAD || fromTime > type(uint40).max) revert InvalidSchedule();
        BeaconBookStorage storage $ = _book();
        Schedule memory s = $.schedule;
        if (s.nextFrom != 0 && block.timestamp >= s.nextFrom) {
            $.past.push(Era(s.beaconId, s.since, s.genesis, s.period));
            s = _settled(s);
        }
        (s.nextBeaconId, s.nextFrom, s.nextGenesis, s.nextPeriod) = (beaconId, uint40(fromTime), b.genesis, b.period);
        $.schedule = s;
        emit BeaconScheduled(beaconId, fromTime);
    }
    /// @dev Drop a change that has not taken effect yet.
    function _cancelSchedule() internal {
        Schedule storage s = _book().schedule;
        (uint8 beaconId, uint40 fromTime) = (s.nextBeaconId, s.nextFrom);
        if (fromTime == 0 || block.timestamp >= fromTime) revert InvalidSchedule();
        (s.nextBeaconId, s.nextFrom, s.nextGenesis, s.nextPeriod) = (0, 0, 0, 0);
        emit BeaconScheduleCancelled(beaconId, fromTime);
    }

    // ---- request and fulfillment paths
    /// @dev The beacon and round of a request in this block: one storage read, no write.
    function _assignRound() internal view returns (uint8 beaconId, uint64 round) {
        Schedule memory s = _book().schedule;
        if (s.nextFrom != 0 && block.timestamp >= s.nextFrom) return (s.nextBeaconId, _round(s.nextGenesis, s.nextPeriod, block.timestamp));
        return (s.beaconId, _round(s.genesis, s.period, block.timestamp));
    }
    /// @dev The cached randomness of a round; zero while it is not verified on chain.
    function _cachedRound(uint8 beaconId, uint64 round) internal view returns (bytes32) {
        return _book().roundRandomness[beaconId][round];
    }
    /// @dev Verify a round's signature now, cache its randomness and announce it with RoundVerified. The round must not be cached yet.
    function _verifyRound(uint8 beaconId, uint64 round, bytes calldata signature) internal returns (bytes32 value) {
        value = _checked(beaconId, round, signature);
        _book().roundRandomness[beaconId][round] = value;
        emit RoundVerified(beaconId, round, value, signature);
    }
    /// @dev A round's randomness for a view: cached, or computed from a signature the verifier accepts; nothing is written.
    function _roundRandomnessView(uint8 beaconId, uint64 round, bytes calldata signature) internal view returns (bytes32 value) {
        value = _cachedRound(beaconId, round);
        if (value == bytes32(0)) value = _checked(beaconId, round, signature);
    }

    // ---- internals
    function _checked(uint8 beaconId, uint64 round, bytes calldata signature) private view returns (bytes32) {
        if (!_verify(_book().beacons[beaconId], round, signature)) revert InvalidRoundSignature();
        return sha256(signature);
    }
    function _verify(StoredBeacon storage s, uint64 round, bytes calldata signature) private view returns (bool) {
        return _yes(s.verifier, abi.encodeCall(IBeaconVerifier.verifyRound, (s.publicKey, round, signature)));
    }
    /// @dev The first round r with genesis + (r - 1) * period >= timestamp + ROUND_LEAD.
    function _round(uint256 genesis, uint256 period, uint256 timestamp) private pure returns (uint64) {
        uint256 t = timestamp + ROUND_LEAD;
        return t <= genesis ? 1 : uint64((t - genesis + period - 1) / period + 1);
    }
    /// @dev The schedule with a change whose time has come made the era in force.
    function _settled(Schedule memory s) private view returns (Schedule memory) {
        if (s.nextFrom != 0 && block.timestamp >= s.nextFrom) return Schedule(s.nextBeaconId, s.nextFrom, s.nextGenesis, s.nextPeriod, 0, 0, 0, 0);
        return s;
    }
    function _beacon(uint8 beaconId) private view returns (StoredBeacon storage) {
        BeaconBookStorage storage $ = _book();
        if (beaconId >= $.beacons.length) revert InvalidBeacon();
        return $.beacons[beaconId];
    }
    function _identity(address verifier, bytes32 chainHash, bytes memory publicKey, uint64 genesis, uint64 period) private pure returns (bytes32) {
        return keccak256(abi.encode(ROUND_BEACON_DOMAIN, verifier, chainHash, keccak256(publicKey), genesis, period));
    }
    /// @dev A yes/no question to a verifier, under ROUND_VERIFY_GAS. One 32-byte word 1 is a yes and 0 a no; at most 32 bytes of the answer
    /// are read. Anything else, a revert included, is no answer: it reverts BeaconVerifierFailed and never reads as a no.
    function _yes(address verifier, bytes memory question) private view returns (bool yes) {
        if (gasleft() < ROUND_VERIFY_GAS_NEEDED) revert BeaconGasTooLow();
        bool answered;
        assembly ("memory-safe") {
            let ok := staticcall(ROUND_VERIFY_GAS, verifier, add(question, 32), mload(question), 0, 32)
            let word := mload(0)
            answered := and(ok, and(eq(returndatasize(), 32), lt(word, 2)))
            yes := and(answered, eq(word, 1))
        }
        if (!answered) revert BeaconVerifierFailed();
    }
    function _book() private pure returns (BeaconBookStorage storage $) {
        assembly ("memory-safe") { $.slot := BEACON_BOOK_LOCATION }
    }
}
