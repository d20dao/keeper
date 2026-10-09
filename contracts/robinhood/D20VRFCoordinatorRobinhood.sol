// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";
import {Ownable2StepUpgradeable} from "@openzeppelin/contracts-upgradeable/access/Ownable2StepUpgradeable.sol";
import {UUPSUpgradeable} from "@openzeppelin/contracts/proxy/utils/UUPSUpgradeable.sol";
import {VRF} from "../vendor/VRF.sol";
import {ID20VRF, ID20VRFConsumer, ID20VRFRefundConsumer} from "../interfaces/ID20VRF.sol";
import {RandomnessMapping} from "../libraries/RandomnessMapping.sol";
import {ArbitrumBlocks} from "./ArbitrumBlocks.sol";
import {BeaconBook} from "./BeaconBook.sol";
import {D20VRFProofVerifier} from "./D20VRFProofVerifier.sol";
import {LinkedRandomnessMapping} from "./LinkedRandomnessMapping.sol";

/// @notice Single-operator secp256k1 VRF for Robinhood Chain whose seed binds a drand round published after the request. A request
///         binds, when it is made, the first round of the beacon in force scheduled at least ROUND_LEAD seconds after its block's
///         timestamp; its result is the operator's VRF output over the request's fixed fields and that round's randomness, which the
///         contract verifies from the round's BLS signature. Proof submission and callback retries are permissionless; the keeper share
///         of a request goes to the submitter when it is an allowed backup keeper, otherwise to keeper().
/// @dev Not audited. Operational setters cannot replace a fixed result; the upgrade owner is trusted, but the secret-key holder can
///      withhold a result, and the request is then refundable after RESPONSE_TIMEOUT. Expired requests refund; never reroll
///      automatically. Block numbers are the chain's own, read from ArbSys. VRF proofs are checked by proofVerifier and mapped results
///      computed by the linked LinkedRandomnessMapping, which keeps this contract under EIP-170's 24,576-byte limit.
contract D20VRFCoordinatorRobinhood is VRF, ReentrancyGuard, ID20VRF, Ownable2StepUpgradeable, UUPSUpgradeable, BeaconBook {
    uint32 public constant MIN_CALLBACK_GAS = 30_000;
    uint32 public constant MAX_CALLBACK_GAS = 1_000_000;
    uint64 public constant RESPONSE_TIMEOUT = 60 seconds;
    uint32 public constant REFUND_CALLBACK_GAS = 100_000;
    uint256 public constant MAX_EVIDENCE_PACKET_BYTES = 512;
    uint256 public constant MAX_FULFILL_BATCH = 16;
    /// @notice 0.01 ETH: a minimum fee above it is a typo, not a price.
    uint256 public constant MAX_MIN_FEE = 1e16;
    uint16 public constant MAX_FEE_MULTIPLIER = 20;
    uint32 public constant MIN_FULFILL_GAS_OVERHEAD = 100_000;
    uint32 public constant MAX_FULFILL_GAS_OVERHEAD = 2_000_000;
    uint16 public constant MIN_REFUND_BPS = 5000;
    uint256 public constant MAX_BACKUP_KEEPERS = 4;
    uint256 private constant CALLBACK_RESERVE = 140_000;
    // Budget per served request outside its callback and its round's verification (measured: ~0.16-0.19M); the rest is headroom.
    uint256 private constant BATCH_MEMBER_OVERHEAD = 400_000;
    bytes32 public constant SEED_DOMAIN = keccak256("D20_VRF_ROUND_SEED");
    bytes32 public constant TRANSCRIPT_DOMAIN = keccak256("D20_VRF_ROUND_TRANSCRIPT");
    bytes32 public constant CONFIG_DOMAIN = keccak256("D20_VRF_ROUND_CONFIG");
    /// @notice The stateless VRF proof check this implementation calls. Fixed in code, so the implementation's code hash pins it.
    /// @custom:oz-upgrades-unsafe-allow state-variable-immutable
    D20VRFProofVerifier public immutable proofVerifier;

    bytes32 public protocolConfigurationHash;
    uint256 public publicKeyX;
    uint256 public publicKeyY;
    bytes32 public keyHash;
    // Replay uses the initial value bound into the initialized configuration hash.
    address public initialFeeRecipient;
    address public feeRecipient;
    uint16 public keeperFeeBps;
    mapping(address => uint256) public keeperCredits;
    uint256 public totalKeeperCredits;
    uint256 public minFee;
    // fee = max(minFee, feeMultiplier * block.basefee * (fulfillGasOverhead + callbackGasLimit)); multiplier 0 is flat minFee.
    uint16 public feeMultiplier;
    uint32 public fulfillGasOverhead;
    // Share of feePaid returned on expiry, snapshotted into each new request; the rest is retained as earnedFees.
    uint16 public refundBps;
    uint256 public nextRequestId;
    uint256 public earnedFees;
    uint256 public lastServedRequestId;
    uint256 public lastServedIndex;
    mapping(uint256 => uint256) public servedRequestAt;
    uint256 public totalRefundCredits;
    mapping(address => uint256) public refundCredits;

    /// @dev getRoundRequest's tuple: every seed input, the result, the escrowed fee and the status of a request in one read.
    ///      roundRandomness is zero until the round is verified on chain; randomness, proofHash and transcriptHash until it is fulfilled.
    struct RoundRequest {
        address consumer;
        uint32 callbackGasLimit;
        uint64 requestBlock;
        uint64 deadline;
        address refundAddress;
        bytes32 clientSeed;
        bytes32 mappingHash;
        uint8 beaconId;
        uint64 round;
        bytes32 roundRandomness;
        bytes32 randomness;
        bytes32 proofHash;
        bytes32 transcriptHash;
        uint256 feePaid;
        bool fulfilled;
        bool delivered;
        bool refunded;
    }
    /// @dev A round's signature carried by a batch. It is verified only when a member of the round is served and the round is not
    ///      verified on chain yet.
    struct RoundSignature { uint8 beaconId; uint64 round; bytes signature; }
    struct InitParams {
        uint256[2] publicKey;
        address owner;
        address feeRecipient;
        address keeper;
        uint16 keeperBps;
        uint256 minFee;
        uint16 feeMultiplier;
        uint32 fulfillGasOverhead;
    }
    struct StoredRequest {
        address consumer;          // slot 0
        uint32 callbackGasLimit;
        uint64 requestBlock;
        uint40 deadline;           // slot 1
        uint64 round;
        uint8 beaconId;
        // Escrowed at request time; settlement never reads the live fee or refund ratio.
        uint96 feePaid;
        uint16 refundBps;
        bool fulfilled;
        bool delivered;
        bool refunded;
        address refundAddress;     // slot 2
        bytes32 clientSeed;        // slot 3
        bytes32 mappingHash;       // slot 4
        bytes32 randomness;        // slot 5
        bytes32 proofHash;         // slot 6
        bytes32 transcriptHash;    // slot 7
    }
    mapping(uint256 => StoredRequest) private requests;
    mapping(uint256 => RandomnessMapping.Spec) private mappingSpecs;
    mapping(uint256 => bool) public refundCallbackDelivered;
    // Replay uses the initial minimum fee bound into the initialized configuration hash.
    uint256 public initialMinFee;
    /// @notice The primary keeper wallet: paid the keeper share of requests served by any submitter that is not an allowed backup keeper.
    address public keeper;
    mapping(address => bool) private backupKeepers;
    uint256 public backupKeeperCount;
    // Preserve declared fields, packing and mapping value layouts across upgrades.
    uint256[40] private __gap;

    error InvalidConfig();
    error InvalidPublicKey();
    error ContractConsumerRequired();
    error IncorrectFee(uint256 expected, uint256 actual);
    error InvalidCallbackGas();
    error UnknownRequest();
    error AlreadyFulfilled();
    error NotFulfilled();
    error AlreadyDelivered();
    error WrongPublicKey();
    error WrongSeed();
    error InsufficientCallbackGas();
    error OnlyFeeRecipient();
    error TransferFailed();
    error InvalidRefundAddress();
    error RequestExpired();
    error RequestRefunded();
    error RefundNotAvailable();
    error NoRefundCredit();
    error NoKeeperCredit();
    error NotRefunded();
    error RefundCallbackAlreadyDelivered();
    error InvalidScan();
    error EvidencePacketTooLarge();
    error RenounceDisabled();
    error FeeOverflow();
    error InvalidBatch();

    event RandomnessRequested(
        uint256 indexed requestId, address indexed consumer, bytes32 indexed keyHash,
        bytes32 clientSeed, uint64 requestBlock, uint32 callbackGasLimit, uint256 feePaid,
        address refundAddress, uint64 deadline
    );
    /// @notice The drand round a request is bound to, fixed at request time.
    event RoundAssigned(uint256 indexed requestId, uint8 indexed beaconId, uint64 indexed round);
    event RandomnessFulfilled(uint256 indexed requestId, bytes32 randomness, address indexed submitter);
    event CallbackAttempted(uint256 indexed requestId, bool success, uint32 gasLimit);
    event FeesWithdrawn(address indexed recipient, uint256 amount);
    event FeeRecipientChanged(address indexed previousRecipient, address indexed newRecipient);
    event KeeperFeeBpsChanged(uint16 previousBps, uint16 newBps);
    event PricingChanged(uint256 minFee, uint16 feeMultiplier, uint32 fulfillGasOverhead);
    event RefundBpsChanged(uint16 previousBps, uint16 newBps);
    event FeeOverpaymentCredited(uint256 indexed requestId, address indexed refundAddress, uint256 amount);
    event KeeperFeePaid(uint256 indexed requestId, address indexed paidKeeper, uint256 amount, bool paid);
    event KeeperCreditWithdrawn(address indexed account, address indexed recipient, uint256 amount);
    event RequestRefundedTo(uint256 indexed requestId, address indexed refundAddress, uint256 amount, bool paid);
    event RefundCreditWithdrawn(address indexed owner, address indexed recipient, uint256 amount);
    event RefundCallbackAttempted(uint256 indexed requestId, address indexed consumer, bool success, uint32 gasLimit);
    event MappingRequested(uint256 indexed requestId, bytes32 indexed mappingHash, RandomnessMapping.Spec spec);
    event ProofVerified(uint256 indexed requestId, bytes32 indexed keyHash, uint256 seed, bytes32 proofHash);
    event RequestServed(uint256 indexed requestId, uint256 indexed serveIndex);
    /// @dev Batch member left untouched: reason 1 = already fulfilled, 2 = refunded, 3 = past its deadline.
    event FulfillmentSkipped(uint256 indexed requestId, uint8 reason);
    /// @dev Packet is NON-indexed so it is recoverable from logs, even through wrappers/multicalls.
    event FulfillmentEvidence(uint256 indexed requestId, bytes32 indexed transcriptHash, bytes packet);
    event KeeperChanged(address indexed previousKeeper, address indexed newKeeper);
    event BackupKeeperSet(address indexed account, bool allowed);

    /// @custom:oz-upgrades-unsafe-allow constructor
    constructor(D20VRFProofVerifier verifier) {
        if (address(verifier).code.length == 0) revert InvalidConfig();
        proofVerifier = verifier;
        _disableInitializers();
    }
    // OZ 5.6's namespaced ReentrancyGuard is constructor-independent: this modifier
    // sets the proxy's guard to NOT_ENTERED on completion without duplicating its slot.
    /// @notice Set the VRF key, fees, roles and pricing, and register `beacon` as beacon 0, in force from now on.
    function initialize(InitParams calldata p, BeaconRegistration calldata beacon) external initializer nonReentrant {
        __Ownable_init(p.owner);
        __Ownable2Step_init();
        nextRequestId = 1;
        if (p.feeRecipient == address(0) || p.keeper == address(0) || p.keeperBps > 10000) revert InvalidConfig();
        if (!_isOnCurve(p.publicKey)) revert InvalidPublicKey();
        publicKeyX = p.publicKey[0];
        publicKeyY = p.publicKey[1];
        keyHash = keccak256(abi.encode(p.publicKey));
        feeRecipient = p.feeRecipient;
        initialFeeRecipient = p.feeRecipient;
        keeperFeeBps = p.keeperBps;
        keeper = p.keeper;
        _setPricing(p.minFee, p.feeMultiplier, p.fulfillGasOverhead);
        initialMinFee = p.minFee;
        refundBps = 10000;
        uint8 id = _registerBeacon(beacon);
        _startWith(id);
        protocolConfigurationHash = keccak256(abi.encode(CONFIG_DOMAIN, p.publicKey, p.feeRecipient, p.minFee, beaconIdentity(id)));
    }

    /// @dev The owner's upgrade is also the only emergency path: there is no pause, and a beacon change takes effect no sooner than
    ///      MIN_SCHEDULE_LEAD after it is scheduled.
    function _authorizeUpgrade(address) internal override onlyOwner {}
    /// @notice Upgrade authority can only move through the two-step transfer; it can never be abandoned.
    function renounceOwnership() public view override onlyOwner { revert RenounceDisabled(); }

    // ---- owner: beacons and keepers
    /// @notice Append a beacon, vouched for by its signature of a past round. A registration never changes; scheduleBeacon puts it in force.
    function registerBeacon(BeaconRegistration calldata beacon) external onlyOwner returns (uint8 beaconId) {
        return _registerBeacon(beacon);
    }
    /// @notice Put a registered beacon in force for requests whose block timestamp is fromTime or later, at least MIN_SCHEDULE_LEAD
    ///         from now. Earlier requests keep the beacon they bound. A pending change is replaced. There is no faster path: in an
    ///         emergency, such as a compromised beacon, the owner upgrades the implementation (UUPS).
    function scheduleBeacon(uint8 beaconId, uint64 fromTime) external onlyOwner { _scheduleBeacon(beaconId, fromTime); }
    /// @notice Drop the pending beacon change; reverts InvalidSchedule once it has taken effect.
    function cancelBeaconSchedule() external onlyOwner { _cancelSchedule(); }
    function setKeeper(address next) external onlyOwner {
        if (next == address(0)) revert InvalidConfig();
        emit KeeperChanged(keeper, next); keeper = next;
    }
    /// @notice Allow or disallow a backup keeper wallet, such as a follower's, to earn the keeper share of the requests it serves.
    function setBackupKeeper(address account, bool allowed) external onlyOwner {
        if (account == address(0) || (allowed && account == keeper) || backupKeepers[account] == allowed) revert InvalidConfig();
        if (allowed) {
            if (backupKeeperCount == MAX_BACKUP_KEEPERS) revert InvalidConfig();
            ++backupKeeperCount;
        } else {
            --backupKeeperCount;
        }
        backupKeepers[account] = allowed;
        emit BackupKeeperSet(account, allowed);
    }
    function isBackupKeeper(address account) external view returns (bool) { return backupKeepers[account]; }
    function isAuthorizedKeeper(address account) external view returns (bool) { return account == keeper || backupKeepers[account]; }

    // ---- owner: fees
    function setFeeRecipient(address next) external onlyOwner {
        if(next == address(0)) revert InvalidConfig();
        emit FeeRecipientChanged(feeRecipient,next); feeRecipient=next;
    }
    function setKeeperFeeBps(uint16 next) external onlyOwner {
        if(next > 10000) revert InvalidConfig();
        emit KeeperFeeBpsChanged(keeperFeeBps,next); keeperFeeBps=next;
    }
    /// @notice Bounded pricing update. Open requests keep settling from the fee they escrowed.
    function setPricing(uint256 nextMinFee, uint16 multiplier, uint32 overhead) external onlyOwner {
        _setPricing(nextMinFee, multiplier, overhead);
    }
    function pricing() external view returns (uint256, uint16, uint32) {
        return (minFee, feeMultiplier, fulfillGasOverhead);
    }
    /// @notice Refund ratio snapshotted into future requests; never below 50%. Open requests keep the ratio they escrowed under.
    function setRefundBps(uint16 next) external onlyOwner {
        if (next < MIN_REFUND_BPS || next > 10000) revert InvalidConfig();
        emit RefundBpsChanged(refundBps, next); refundBps = next;
    }

    /// @notice Fee formula over the current parameters for a given base fee: max(minFee, feeMultiplier * baseFee * (fulfillGasOverhead + callbackGasLimit)).
    /// @dev Off-chain senders quote with the latest header baseFeePerGas plus a buffer; the excess is credited to the refund address.
    function quoteFeeAt(uint32 callbackGasLimit, uint256 baseFee) public view returns (uint256) {
        uint256 dynamic = uint256(feeMultiplier) * baseFee * (uint256(fulfillGasOverhead) + callbackGasLimit);
        // Bounded parameters exceed uint96 escrow only above ~1.3e21 wei base fee; refuse rather than truncate.
        if (dynamic > type(uint96).max) revert FeeOverflow();
        return dynamic > minFee ? dynamic : minFee;
    }
    /// @notice Exact fee for a request sent in this same transaction (block.basefee).
    /// @dev eth_call often reports block.basefee as 0, so this is NOT a reliable off-chain quote: use quoteFeeAt with the latest
    ///      header baseFeePerGas plus a buffer instead.
    function quoteFee(uint32 callbackGasLimit) external view returns (uint256) {
        return quoteFeeAt(callbackGasLimit, block.basefee);
    }

    // ---- requests
    function requestRandomness(bytes32 clientSeed, uint32 callbackGasLimit, address _refundAddress)
        external payable nonReentrant returns (uint256 requestId)
    {
        RandomnessMapping.Spec memory raw;
        return _createRequest(clientSeed, callbackGasLimit, _refundAddress, raw);
    }

    function requestMappedRandomness(
        bytes32 clientSeed, uint32 callbackGasLimit, address _refundAddress, RandomnessMapping.Spec calldata spec
    ) external payable nonReentrant returns (uint256 requestId) {
        return _createRequest(clientSeed, callbackGasLimit, _refundAddress, spec);
    }

    function _createRequest(
        bytes32 clientSeed, uint32 callbackGasLimit, address _refundAddress, RandomnessMapping.Spec memory spec
    ) private returns (uint256 requestId) {
        if (msg.sender.code.length == 0) revert ContractConsumerRequired();
        if (_refundAddress == address(0)) revert InvalidRefundAddress();
        _checkGasLimit(callbackGasLimit);
        uint256 fee = quoteFeeAt(callbackGasLimit, block.basefee);
        if (msg.value < fee) revert IncorrectFee(fee, msg.value);
        RandomnessMapping.validate(spec);
        requestId = nextRequestId++;
        StoredRequest storage r = requests[requestId];
        r.consumer = msg.sender;
        r.callbackGasLimit = callbackGasLimit;
        r.requestBlock = uint64(ArbitrumBlocks.number());
        r.deadline = uint40(block.timestamp + RESPONSE_TIMEOUT);
        (r.beaconId, r.round) = _assignRound();
        r.feePaid = uint96(fee);
        r.refundBps = refundBps;
        r.refundAddress = _refundAddress;
        r.clientSeed = clientSeed;
        r.mappingHash = RandomnessMapping.hash(spec);
        // A raw request's spec is all zeros, which a fresh slot already holds.
        if (spec.operation != RandomnessMapping.Operation.Raw) mappingSpecs[requestId] = spec;
        // Overpayment from off-chain quoting is pull-withdrawable refund credit, never treasury revenue.
        uint256 excess = msg.value - fee;
        if (excess != 0) {
            refundCredits[_refundAddress] += excess;
            totalRefundCredits += excess;
            emit FeeOverpaymentCredited(requestId, _refundAddress, excess);
        }
        emit RandomnessRequested(requestId, msg.sender, keyHash, clientSeed, r.requestBlock, callbackGasLimit,
            fee, _refundAddress, r.deadline);
        emit MappingRequested(requestId, r.mappingHash, spec);
        emit RoundAssigned(requestId, r.beaconId, r.round);
    }

    /// @notice Everything about a request in one read: its seed inputs, its result and its status.
    function getRoundRequest(uint256 requestId) external view returns (RoundRequest memory q) {
        StoredRequest storage r = _request(requestId);
        q.consumer = r.consumer;
        q.callbackGasLimit = r.callbackGasLimit;
        q.requestBlock = r.requestBlock;
        q.deadline = r.deadline;
        q.refundAddress = r.refundAddress;
        q.clientSeed = r.clientSeed;
        q.mappingHash = r.mappingHash;
        q.beaconId = r.beaconId;
        q.round = r.round;
        q.roundRandomness = _cachedRound(r.beaconId, r.round);
        q.randomness = r.randomness;
        q.proofHash = r.proofHash;
        q.transcriptHash = r.transcriptHash;
        q.feePaid = r.feePaid;
        q.fulfilled = r.fulfilled;
        q.delivered = r.delivered;
        q.refunded = r.refunded;
    }

    /// @notice Recovery scan over REQUEST IDs, not completion order. Never skips an older unserved gap.
    /// @dev Scans at most limit slots; expired/refunded/fulfilled jobs are excluded. Keeper must recheck
    ///      state/time before prover/send. Start at 1 after loss of local state; paginate until nextRequestId.
    function getPendingRequestIds(uint256 fromId, uint256 limit)
        external view returns (uint256[] memory ids, uint256 nextCursor)
    {
        if (fromId == 0 || limit == 0 || limit > 256) revert InvalidScan();
        uint256 end = nextRequestId;
        nextCursor = fromId > end ? end : fromId;
        ids = new uint256[](limit);
        uint256 count;
        uint256 scanned;
        while (nextCursor < end && scanned < limit) {
            StoredRequest storage r = requests[nextCursor];
            if (!r.fulfilled && !r.refunded && block.timestamp <= r.deadline) ids[count++] = nextCursor;
            ++nextCursor;
            ++scanned;
        }
        assembly ("memory-safe") { mstore(ids, count) }
    }

    /// @notice Fee escrowed by this request; the amount its keeper share, treasury share and refund settle from.
    function requestFeePaid(uint256 requestId) external view returns (uint256) {
        return _request(requestId).feePaid;
    }
    /// @notice Refund ratio this request escrowed under; a later setRefundBps does not change it.
    function requestRefundBps(uint256 requestId) external view returns (uint16) {
        return _request(requestId).refundBps;
    }

    function getMapping(uint256 requestId) external view returns (RandomnessMapping.Spec memory) {
        _request(requestId);
        return mappingSpecs[requestId];
    }

    function getMappedResult(uint256 requestId) external view returns (uint256[] memory) {
        StoredRequest storage r = _request(requestId);
        if (!r.fulfilled) revert NotFulfilled();
        return LinkedRandomnessMapping.map(r.randomness, mappingSpecs[requestId]);
    }

    /// @notice Public pure mapping for independent inspection; does not claim any request was fulfilled.
    function mapRandomness(bytes32 randomness, RandomnessMapping.Spec calldata spec)
        external pure returns (uint256[] memory)
    {
        return LinkedRandomnessMapping.map(randomness, spec);
    }

    // ---- proof input
    /// @notice Exact input the keeper must prove, once the request's round is verified on chain. Reverts RoundUnavailable before.
    function requestSeed(uint256 requestId) external view returns (uint256) {
        StoredRequest storage r = _request(requestId);
        return _seed(requestId, r, _verifiedRound(r));
    }
    /// @notice Verify the VRF math of a request whose round is verified on chain, without mutating state. A valid proof is NOT
    ///         evidence of timely acceptance: check getRoundRequest().fulfilled, proofHash and events for accepted-service status.
    function verifyRequestProof(uint256 requestId, Proof calldata proof) external view returns (bytes32) {
        StoredRequest storage r = _request(requestId);
        return _verifyProof(proof, _seed(requestId, r, _verifiedRound(r)));
    }
    /// @notice The seed to prove and the service status, given the round's signature: the cached randomness when the round is verified
    ///         on chain, else the signature, which must verify (an eth_call then needs about 500,000 gas).
    function getProofContext(uint256 requestId, bytes calldata roundSignature)
        external view returns (uint256 seed, uint64 deadline, bool fulfilled, bool refunded)
    {
        StoredRequest storage r = _request(requestId);
        bytes32 roundRand = _roundRandomnessView(r.beaconId, r.round, roundSignature);
        return (_seed(requestId, r, roundRand), r.deadline, r.fulfilled, r.refunded);
    }

    // ---- fulfillment
    /// @notice Fulfill one request. roundSignature is its round's signature; it may be empty once the round is verified on chain.
    /// @dev A round not verified yet is verified here, and the call must then carry gas for the check and the whole callback before
    ///      it starts, as a one-member batch does: 140000 + callbackGasLimit + callbackGasLimit / 63 + 400000 + 411349.
    function fulfillRandomness(uint256 requestId, Proof calldata proof, bytes calldata roundSignature)
        external nonReentrant
    {
        StoredRequest storage r = _request(requestId);
        if (r.fulfilled) revert AlreadyFulfilled();
        if (r.refunded) revert RequestRefunded();
        if (block.timestamp > r.deadline) revert RequestExpired();
        (uint8 beaconId, uint64 round) = (r.beaconId, r.round);
        bytes32 roundRand = _cachedRound(beaconId, round);
        if (roundRand == bytes32(0)) {
            if (gasleft() < _memberGas(r.callbackGasLimit) + CALLBACK_RESERVE + ROUND_VERIFY_GAS_NEEDED) revert InsufficientCallbackGas();
            roundRand = _verifyRound(beaconId, round, roundSignature);
        }
        _fulfill(requestId, r, proof, roundRand);
    }

    /// @notice Fulfill up to MAX_FULFILL_BATCH requests in one transaction. rounds carries the signatures of the members' rounds that
    ///         may not be verified on chain yet; a listed round is verified when its first served member is reached, and rounds of
    ///         members that are skipped are never verified. Members already fulfilled, refunded or past their deadline are skipped
    ///         with FulfillmentSkipped; every other member runs exactly like fulfillRandomness, so a wrong seed, an invalid proof, a
    ///         wrong round signature or a round neither verified nor listed reverts the whole batch.
    /// @dev Requires gas for every member that will be served, and for verifying each of their rounds not verified yet, before any
    ///      result is revealed: 140000 + Σ(callbackGasLimit + callbackGasLimit / 63 + 400000) + 411349 per such round, else
    ///      InsufficientCallbackGas.
    function fulfillRandomnessBatch(RoundSignature[] calldata rounds, uint256[] calldata ids, Proof[] calldata proofs) external nonReentrant {
        uint256 count = ids.length;
        if (count == 0 || count > MAX_FULFILL_BATCH || count != proofs.length || rounds.length > count) revert InvalidBatch();
        if (gasleft() < _batchGas(rounds, ids)) revert InsufficientCallbackGas();
        for (uint256 i; i < count; ++i) {
            StoredRequest storage r = _request(ids[i]);
            uint8 reason = r.fulfilled ? 1 : r.refunded ? 2 : block.timestamp > r.deadline ? 3 : 0;
            if (reason != 0) { emit FulfillmentSkipped(ids[i], reason); continue; }
            _fulfill(ids[i], r, proofs[i], _batchRound(rounds, r));
        }
    }

    /// @dev The gas a batch needs before it starts. A callback sees the results stored before it. Budget every served member's full
    ///      callback, and every round verification still ahead, up front, so no callback, however much of its own budget it burns, can
    ///      starve a later member or round and revert revealed results. Skipped members stay skipped for the whole transaction, so they
    ///      need no budget, and neither do their rounds.
    function _batchGas(RoundSignature[] calldata rounds, uint256[] calldata ids) private view returns (uint256 need) {
        need = CALLBACK_RESERVE;
        uint256 budgeted; // bit i: listed round i is budgeted for verification
        for (uint256 i; i < ids.length; ++i) {
            StoredRequest storage r = _request(ids[i]);
            if (r.fulfilled || r.refunded || block.timestamp > r.deadline) continue;
            need += _memberGas(r.callbackGasLimit);
            if (_cachedRound(r.beaconId, r.round) == bytes32(0)) {
                uint256 bit = 1 << _listed(rounds, r.beaconId, r.round);
                if (budgeted & bit == 0) { budgeted |= bit; need += ROUND_VERIFY_GAS_NEEDED; }
            }
        }
    }

    /// @dev A served batch member's round randomness: cached, or verified now from the round's listed signature.
    function _batchRound(RoundSignature[] calldata rounds, StoredRequest storage r) private returns (bytes32 value) {
        (uint8 beaconId, uint64 round) = (r.beaconId, r.round);
        value = _cachedRound(beaconId, round);
        if (value == bytes32(0)) value = _verifyRound(beaconId, round, rounds[_listed(rounds, beaconId, round)].signature);
    }

    function _fulfill(uint256 requestId, StoredRequest storage r, Proof calldata proof, bytes32 roundRand) private {
        bytes32 randomness = _verifyProof(proof, _seed(requestId, r, roundRand));
        r.randomness = randomness;
        r.proofHash = keccak256(abi.encode(proof));
        r.transcriptHash = _transcriptHash(requestId, r, roundRand);
        r.fulfilled = true;
        // Submission stays open to anyone. The keeper share goes to the submitter when it is an allowed backup keeper, so each
        // keeper earns what it serves; any other submitter's fulfillment pays keeper().
        address paidKeeper = keeper;
        if (msg.sender != paidKeeper && backupKeepers[msg.sender]) paidKeeper = msg.sender;
        uint256 fee = r.feePaid;
        uint256 keeperAmount = fee / 10000 * keeperFeeBps + fee % 10000 * keeperFeeBps / 10000;
        earnedFees += fee - keeperAmount;
        lastServedRequestId = requestId;
        servedRequestAt[++lastServedIndex] = requestId;
        emit RequestServed(requestId, lastServedIndex);
        emit ProofVerified(requestId, keyHash, proof.seed, r.proofHash);
        emit RandomnessFulfilled(requestId, randomness, msg.sender);
        _emitEvidence(requestId, r.transcriptHash, proof);
        _deliver(requestId, r, r.callbackGasLimit);
        if(keeperAmount != 0) {
            bool paid;
            assembly ("memory-safe") { paid := call(30000, paidKeeper, keeperAmount, 0, 0, 0, 0) }
            if(!paid) { keeperCredits[paidKeeper] += keeperAmount; totalKeeperCredits += keeperAmount; }
            emit KeeperFeePaid(requestId,paidKeeper,keeperAmount,paid);
        }
    }

    function _emitEvidence(uint256 id, bytes32 transcript, Proof calldata proof) private {
        bytes memory packet = abi.encode(proof);
        if (packet.length > MAX_EVIDENCE_PACKET_BYTES) revert EvidencePacketTooLarge();
        emit FulfillmentEvidence(id, transcript, packet);
    }

    // ---- retries and refunds
    /// @notice Retry delivery of the stored result, with more gas if necessary. Never rerolls.
    function retryCallback(uint256 requestId, uint32 gasLimit) external nonReentrant {
        StoredRequest storage r = _request(requestId);
        if (!r.fulfilled) revert NotFulfilled();
        if (r.delivered) revert AlreadyDelivered();
        _checkGasLimit(gasLimit);
        if (gasLimit < r.callbackGasLimit) revert InvalidCallbackGas();
        _deliver(requestId, r, gasLimit);
    }

    /// @notice After 60 seconds without a verified result, anyone can trigger the fixed-address refund.
    /// @dev Callback failure AFTER verification is not refundable. Failed transfers remain backed credits.
    ///      Pays feePaid * the ratio snapshotted at request time / 10000; the remainder is retained as earnedFees.
    function refundRequest(uint256 requestId) external nonReentrant {
        StoredRequest storage r = _request(requestId);
        if (r.fulfilled || r.refunded || block.timestamp <= r.deadline) revert RefundNotAvailable();
        r.refunded = true;
        uint256 fee = r.feePaid;
        uint256 amount = fee * r.refundBps / 10000;
        earnedFees += fee - amount;
        refundCredits[r.refundAddress] += amount;
        totalRefundCredits += amount;
        // Reserve enough gas to record a failed transfer; never copy arbitrary return data.
        // Include the notification budget so gas estimation cannot silently skip the hook.
        if (gasleft() < uint256(REFUND_CALLBACK_GAS) + REFUND_CALLBACK_GAS / 63 + 140_000) revert InsufficientCallbackGas();
        address recipient = r.refundAddress;
        bool success;
        assembly ("memory-safe") { success := call(30000, recipient, amount, 0, 0, 0, 0) }
        if (success) {
            refundCredits[recipient] -= amount;
            totalRefundCredits -= amount;
        }
        emit RequestRefundedTo(requestId, recipient, amount, success);
        _deliverRefund(requestId, r.consumer, REFUND_CALLBACK_GAS);
    }

    /// @notice Retry only a failed refund notification. Never transfers the fee a second time.
    function retryRefundCallback(uint256 requestId, uint32 gasLimit) external nonReentrant {
        StoredRequest storage r = _request(requestId);
        if (!r.refunded) revert NotRefunded();
        if (refundCallbackDelivered[requestId]) revert RefundCallbackAlreadyDelivered();
        _checkGasLimit(gasLimit);
        if (gasLimit < REFUND_CALLBACK_GAS) revert InvalidCallbackGas();
        _deliverRefund(requestId, r.consumer, gasLimit);
    }

    function _deliverRefund(uint256 requestId, address consumer, uint32 gasLimit) private {
        // Effects and refund transfer/credit have already settled under the shared guard.
        // Never allocate or copy consumer return data, including revert-data bombs.
        if (gasleft() < uint256(gasLimit) + gasLimit / 63 + 50_000) revert InsufficientCallbackGas();
        bytes memory payload = abi.encodeCall(ID20VRFRefundConsumer.onRefund, (requestId));
        bool success;
        assembly ("memory-safe") {
            success := call(gasLimit, consumer, 0, add(payload, 32), mload(payload), 0, 0)
        }
        if (success) refundCallbackDelivered[requestId] = true;
        emit RefundCallbackAttempted(requestId, consumer, success, gasLimit);
    }

    /// @notice Only the original refund recipient can redirect its own failed-transfer credit.
    function withdrawRefundCredit(address payable recipient) external nonReentrant {
        if (recipient == address(0)) revert InvalidRefundAddress();
        uint256 amount = refundCredits[msg.sender];
        if (amount == 0) revert NoRefundCredit();
        refundCredits[msg.sender] = 0;
        totalRefundCredits -= amount;
        (bool success,) = recipient.call{value: amount}("");
        if (!success) revert TransferFailed();
        emit RefundCreditWithdrawn(msg.sender, recipient, amount);
    }

    /// @notice Only verified requests earn fees. Pending request fees remain in escrow.
    function withdrawFees(address payable recipient) external nonReentrant {
        if (msg.sender != feeRecipient) revert OnlyFeeRecipient();
        if (recipient == address(0)) revert InvalidConfig();
        uint256 amount = earnedFees;
        earnedFees = 0;
        (bool success,) = recipient.call{value: amount}("");
        if (!success) revert TransferFailed();
        emit FeesWithdrawn(recipient, amount);
    }

    function withdrawKeeperCredit(address payable recipient) external nonReentrant {
        if(recipient == address(0)) revert InvalidConfig();
        uint256 amount = keeperCredits[msg.sender];
        if(amount == 0) revert NoKeeperCredit();
        keeperCredits[msg.sender] = 0;
        totalKeeperCredits -= amount;
        (bool success,) = recipient.call{value:amount}("");
        if(!success) revert TransferFailed();
        emit KeeperCreditWithdrawn(msg.sender,recipient,amount);
    }

    // ---- internals
    function _setPricing(uint256 nextMinFee, uint16 multiplier, uint32 overhead) private {
        if (nextMinFee > MAX_MIN_FEE || multiplier > MAX_FEE_MULTIPLIER || overhead < MIN_FULFILL_GAS_OVERHEAD || overhead > MAX_FULFILL_GAS_OVERHEAD) revert InvalidConfig();
        // Requests are never free: a zero minimum fee needs a base-fee multiplier.
        if (nextMinFee == 0 && multiplier == 0) revert InvalidConfig();
        minFee = nextMinFee; feeMultiplier = multiplier; fulfillGasOverhead = overhead;
        emit PricingChanged(nextMinFee, multiplier, overhead);
    }

    function _request(uint256 requestId) private view returns (StoredRequest storage r) {
        r = requests[requestId];
        if (r.consumer == address(0)) revert UnknownRequest();
    }

    function _verifiedRound(StoredRequest storage r) private view returns (bytes32 value) {
        value = _cachedRound(r.beaconId, r.round);
        if (value == bytes32(0)) revert RoundUnavailable();
    }

    /// @dev The index of a round in a batch's list; RoundUnavailable when it is not listed.
    function _listed(RoundSignature[] calldata rounds, uint8 beaconId, uint64 round) private pure returns (uint256 i) {
        for (; i < rounds.length; ++i) if (rounds[i].beaconId == beaconId && rounds[i].round == round) return i;
        revert RoundUnavailable();
    }

    function _memberGas(uint256 limit) private pure returns (uint256) {
        return limit + limit / 63 + BATCH_MEMBER_OVERHEAD;
    }

    function _seed(uint256 id, StoredRequest storage r, bytes32 roundRand) private view returns (uint256) {
        return uint256(keccak256(abi.encode(
            SEED_DOMAIN, block.chainid, address(this), keyHash, id,
            r.consumer, r.clientSeed, r.mappingHash, r.requestBlock, r.beaconId, r.round, roundRand
        )));
    }

    function _verifyProof(Proof calldata proof, uint256 seed)
        private view returns (bytes32)
    {
        if (proof.pk[0] != publicKeyX || proof.pk[1] != publicKeyY) revert WrongPublicKey();
        // Upstream ignores proof.seed in favor of its explicit seed argument: validate both here.
        if (proof.seed != seed) revert WrongSeed();
        return bytes32(proofVerifier.randomValue(proof, seed));
    }

    function _transcriptHash(uint256 id, StoredRequest storage r, bytes32 roundRand) private view returns (bytes32) {
        return keccak256(abi.encode(
            TRANSCRIPT_DOMAIN, block.chainid, address(this), id, protocolConfigurationHash,
            r.beaconId, r.round, roundRand, r.proofHash, r.randomness, r.mappingHash
        ));
    }

    function _checkGasLimit(uint32 gasLimit) private pure {
        if (gasLimit < MIN_CALLBACK_GAS || gasLimit > MAX_CALLBACK_GAS) revert InvalidCallbackGas();
    }

    function _deliver(uint256 id, StoredRequest storage r, uint32 gasLimit) private {
        bytes memory data = abi.encodeCall(ID20VRFConsumer.rawFulfillRandomness, (id, r.randomness));
        address consumer = r.consumer;
        // Warm account access before the EIP-150 budget check. Do not mark an empty address delivered.
        if (consumer.code.length == 0) {
            emit CallbackAttempted(id, false, gasLimit);
            return;
        }
        if (gasleft() < uint256(gasLimit) + uint256(gasLimit) / 63 + CALLBACK_RESERVE)
            revert InsufficientCallbackGas();
        bool success;
        // No return-data copy: an untrusted callback cannot allocate a return-data bomb here.
        assembly ("memory-safe") {
            success := call(gasLimit, consumer, 0, add(data, 32), mload(data), 0, 0)
        }
        r.delivered = success;
        emit CallbackAttempted(id, success, gasLimit);
    }
}
