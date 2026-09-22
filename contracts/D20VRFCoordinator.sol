// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";
import {Ownable2StepUpgradeable} from "@openzeppelin/contracts-upgradeable/access/Ownable2StepUpgradeable.sol";
import {UUPSUpgradeable} from "@openzeppelin/contracts/proxy/utils/UUPSUpgradeable.sol";
import {EpochEntropy} from "./EpochEntropy.sol";
import {VRF} from "./vendor/VRF.sol";
import {ID20VRF, ID20VRFConsumer, ID20VRFRefundConsumer} from "./interfaces/ID20VRF.sol";
import {RandomnessMapping} from "./libraries/RandomnessMapping.sol";

/// @notice Single-operator secp256k1 VRF. Proof submission and callback retries are permissionless; the keeper share
///         of a request goes to the submitter when the registry authorizes it to publish epochs, otherwise to committer().
/// @dev Prototype: not audited or validated on Arc. Operational setters cannot replace a fixed result; the upgrade owner is trusted,
///      but the secret-key holder can withhold it. Expired requests refund; never reroll automatically.
contract D20VRFCoordinator is VRF, ReentrancyGuard, ID20VRF, Ownable2StepUpgradeable, UUPSUpgradeable {
    uint32 public constant MIN_CALLBACK_GAS = 30_000;
    uint32 public constant MAX_CALLBACK_GAS = 1_000_000;
    uint64 public constant RESPONSE_TIMEOUT = 60 seconds;
    uint32 public constant REFUND_CALLBACK_GAS = 100_000;
    uint256 public constant MAX_EVIDENCE_PACKET_BYTES = 512;
    uint256 public constant MAX_FULFILL_BATCH = 16;
    uint256 public constant MAX_MIN_FEE = 10e18;
    uint16 public constant MAX_FEE_MULTIPLIER = 20;
    uint32 public constant MIN_FULFILL_GAS_OVERHEAD = 100_000;
    uint32 public constant MAX_FULFILL_GAS_OVERHEAD = 2_000_000;
    uint16 public constant MIN_REFUND_BPS = 5000;
    uint256 private constant CALLBACK_RESERVE = 140_000;
    // Budget per batch member outside its callback (measured: ~0.2M typical, ~0.27M worst case); the rest is headroom.
    uint256 private constant BATCH_MEMBER_OVERHEAD = 400_000;
    bytes32 public constant SEED_DOMAIN = keccak256("D20_VRF_SEED");
    bytes32 public constant TRANSCRIPT_DOMAIN = keccak256("D20_VRF_TRANSCRIPT");
    bytes32 public constant CONFIG_DOMAIN = keccak256("D20_VRF_CONFIG");
    bytes32 public protocolConfigurationHash;
    EpochEntropy public epochRegistry;

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
    uint16 public confirmationBlocks;
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

    struct Request {
        address consumer;
        uint32 callbackGasLimit;
        uint64 requestBlock;
        uint64 targetBlock;
        uint64 deadline;
        address refundAddress;
        bytes32 clientSeed;
        bytes32 mappingHash;
        bytes32 blockHash;
        bytes32 randomness;
        bytes32 proofHash;
        bytes32 transcriptHash;
        bool fulfilled;
        bool delivered;
        bool refunded;
        uint64 epochId;
        bytes32 epochHash;
    }
    /// @dev Same public Request ABI, but pack three status flags into the existing deadline/refund slot.
    struct StoredRequest {
        address consumer;
        uint32 callbackGasLimit;
        uint64 requestBlock;
        uint64 targetBlock;
        uint64 deadline;
        address refundAddress;
        bool fulfilled;
        bool delivered;
        bool refunded;
        bytes32 clientSeed;
        bytes32 mappingHash;
        bytes32 blockHash;
        bytes32 randomness;
        bytes32 proofHash;
        bytes32 transcriptHash;
        uint64 epochId;
        // Escrowed at request time; settlement never reads the live fee or refund ratio.
        uint96 feePaid;
        uint16 refundBps;
        bytes32 epochHash;
    }
    mapping(uint256 => StoredRequest) private requests;
    mapping(uint256 => RandomnessMapping.Spec) private mappingSpecs;
    mapping(uint256 => bool) public refundCallbackDelivered;
    // Replay uses the initial minimum fee bound into the initialized configuration hash.
    uint256 public initialMinFee;
    // Preserve declared fields, packing and mapping value layouts across upgrades.
    uint256[38] private __gap;
    error InvalidConfig();
    error EpochUnavailable();
    error InvalidPublicKey();
    error ContractConsumerRequired();
    error IncorrectFee(uint256 expected, uint256 actual);
    error InvalidCallbackGas();
    error UnknownRequest();
    error NotReady();
    error BlockHashUnavailable();
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
    event BlockHashStored(uint256 indexed requestId, uint64 targetBlock, bytes32 blockHash);
    event RandomnessFulfilled(uint256 indexed requestId, bytes32 randomness, address indexed submitter);
    event CallbackAttempted(uint256 indexed requestId, bool success, uint32 gasLimit);
    event FeesWithdrawn(address indexed recipient, uint256 amount);
    event FeeRecipientChanged(address indexed previousRecipient, address indexed newRecipient);
    event KeeperFeeBpsChanged(uint16 previousBps, uint16 newBps);
    event PricingChanged(uint256 minFee, uint16 feeMultiplier, uint32 fulfillGasOverhead);
    event RefundBpsChanged(uint16 previousBps, uint16 newBps);
    event FeeOverpaymentCredited(uint256 indexed requestId, address indexed refundAddress, uint256 amount);
    event KeeperFeePaid(uint256 indexed requestId, address indexed keeper, uint256 amount, bool paid);
    event KeeperCreditWithdrawn(address indexed keeper, address indexed recipient, uint256 amount);
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

    /// @custom:oz-upgrades-unsafe-allow constructor
    constructor() { _disableInitializers(); }
    // OZ 5.6's namespaced ReentrancyGuard is constructor-independent: this modifier
    // sets the proxy's guard to NOT_ENTERED on completion without duplicating its slot.
    function initialize(uint256[2] memory publicKey, address initialOwner, address recipient, uint256 fee, uint16 confirmations, EpochEntropy registry, uint16 keeperBps) external initializer nonReentrant {
        __Ownable_init(initialOwner);
        __Ownable2Step_init();
        nextRequestId=1;
        if (recipient == address(0) || confirmations == 0 || confirmations > 64 || keeperBps > 10000 || fee > MAX_MIN_FEE) revert InvalidConfig();
        if (address(registry).code.length == 0) revert InvalidConfig();
        epochRegistry = registry;
        if (!_isOnCurve(publicKey)) revert InvalidPublicKey();
        publicKeyX = publicKey[0];
        publicKeyY = publicKey[1];
        keyHash = keccak256(abi.encode(publicKey));
        feeRecipient = recipient;
        initialFeeRecipient = recipient;
        keeperFeeBps = keeperBps;
        minFee = fee;
        initialMinFee = fee;
        feeMultiplier = 5;
        fulfillGasOverhead = 300_000;
        refundBps = 10000;
        confirmationBlocks = confirmations;
        protocolConfigurationHash = keccak256(abi.encode(CONFIG_DOMAIN, publicKey, recipient, fee, confirmations, address(registry), registry.catalogHash(), registry.firstEpochStart(), uint64(200)));
    }

    function _authorizeUpgrade(address) internal override onlyOwner {}
    /// @notice Upgrade authority can only move through the two-step transfer; it can never be abandoned.
    function renounceOwnership() public view override onlyOwner { revert RenounceDisabled(); }

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
        if (nextMinFee > MAX_MIN_FEE || multiplier > MAX_FEE_MULTIPLIER || overhead < MIN_FULFILL_GAS_OVERHEAD || overhead > MAX_FULFILL_GAS_OVERHEAD) revert InvalidConfig();
        // Requests are never free: a zero minimum fee needs a base-fee multiplier.
        if (nextMinFee == 0 && multiplier == 0) revert InvalidConfig();
        minFee = nextMinFee; feeMultiplier = multiplier; fulfillGasOverhead = overhead;
        emit PricingChanged(nextMinFee, multiplier, overhead);
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
    /// @dev eth_call commonly reports block.basefee as 0 (observed on Arc), so this is NOT a reliable off-chain quote:
    ///      use quoteFeeAt with the latest header baseFeePerGas plus a buffer instead.
    function quoteFee(uint32 callbackGasLimit) external view returns (uint256) {
        return quoteFeeAt(callbackGasLimit, block.basefee);
    }

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
        uint64 epochId = epochRegistry.epochForBlock(block.number);
        EpochEntropy.Epoch memory epoch = epochRegistry.getEpoch(epochId);
        if (epochId == 0) revert EpochUnavailable();
        epochRegistry.checkpointEpoch(epochId);
        requestId = nextRequestId++;
        // An unpublished epoch cannot choose data after the randomness block is known.
        StoredRequest storage r = requests[requestId];
        r.epochId = epochId;
        r.epochHash = epoch.epochHash;
        r.consumer = msg.sender;
        r.callbackGasLimit = callbackGasLimit;
        r.requestBlock = uint64(block.number);
        r.targetBlock = epoch.epochHash == bytes32(0) ? 0 : _target(r.requestBlock,epoch.committedBlock);
        r.deadline = uint64(block.timestamp + RESPONSE_TIMEOUT);
        r.feePaid = uint96(fee);
        r.refundBps = refundBps;
        r.refundAddress = _refundAddress;
        r.clientSeed = clientSeed;
        r.mappingHash = RandomnessMapping.hash(spec);
        mappingSpecs[requestId] = spec;
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
    }

    function getRequest(uint256 requestId) external view returns (Request memory result) {
        StoredRequest storage r = _request(requestId);
        result.consumer = r.consumer;
        result.callbackGasLimit = r.callbackGasLimit;
        result.requestBlock = r.requestBlock;
        (result.targetBlock,result.epochHash)=_resolvedEpoch(r);
        result.deadline = r.deadline;
        result.refundAddress = r.refundAddress;
        result.clientSeed = r.clientSeed;
        result.mappingHash = r.mappingHash;
        result.blockHash = r.blockHash;
        result.randomness = r.randomness;
        result.proofHash = r.proofHash;
        result.transcriptHash = r.transcriptHash;
        result.fulfilled = r.fulfilled;
        result.delivered = r.delivered;
        result.refunded = r.refunded;
        result.epochId = r.epochId;

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
        return RandomnessMapping.map(r.randomness, mappingSpecs[requestId]);
    }

    /// @notice Public pure mapping for independent inspection; does not claim any request was fulfilled.
    function mapRandomness(bytes32 randomness, RandomnessMapping.Spec calldata spec)
        external pure returns (uint256[] memory)
    {
        return RandomnessMapping.map(randomness, spec);
    }

    /// @notice Verify the VRF math without mutating state. A valid proof is NOT evidence of timely acceptance.
    /// @dev Check getRequest().fulfilled, proofHash and events separately for accepted-service status.
    function verifyRequestProof(uint256 requestId, Proof calldata proof)
        external view returns (bytes32)
    {
        StoredRequest storage r = _request(requestId);
        bytes32 anchor = _resolvedBlockHash(r);
        return _verifyProof(proof, _seed(requestId, r, anchor));
    }

    /// @notice Cache a canonical block hash before BLOCKHASH's 256-block window expires.
    /// @dev Anyone can checkpoint; an uncheckpointed expired request cannot be fulfilled.
    function storeBlockHash(uint256 requestId) external nonReentrant returns (bytes32) {
        return _storeBlockHash(requestId, _request(requestId));
    }

    /// @notice Exact input the keeper must prove. Never accept an RPC-supplied hash as authority.
    function requestSeed(uint256 requestId) external view returns (uint256) {
        StoredRequest storage r = _request(requestId);
        bytes32 anchor = _resolvedBlockHash(r);
        return _seed(requestId, r, anchor);
    }

    /// @notice Canonical proof input and service status in a single read; verification itself remains timeless.
    function getProofContext(uint256 requestId)
        external view returns (uint256 seed, uint64 deadline, bool fulfilled, bool refunded)
    {
        StoredRequest storage r = _request(requestId);
        return (_seed(requestId, r, _resolvedBlockHash(r)), r.deadline, r.fulfilled, r.refunded);
    }

    function fulfillRandomness(uint256 requestId, Proof calldata proof)
        external nonReentrant
    {
        StoredRequest storage r = _request(requestId);
        if (r.fulfilled) revert AlreadyFulfilled();
        if (r.refunded) revert RequestRefunded();
        if (block.timestamp > r.deadline) revert RequestExpired();
        _fulfill(requestId, r, proof);
    }

    /// @notice Fulfill up to MAX_FULFILL_BATCH prepared requests in one transaction. Members that are already fulfilled,
    ///         refunded or past their deadline are skipped with FulfillmentSkipped; every other member runs exactly like
    ///         fulfillRandomness, so a wrong seed, invalid proof or unready request reverts the whole batch.
    /// @dev Requires gas for every member that will be served before any result is revealed:
    ///      140000 + Σ(callbackGasLimit + callbackGasLimit / 63 + 400000), else InsufficientCallbackGas.
    function fulfillRandomnessBatch(uint256[] calldata ids, Proof[] calldata proofs) external nonReentrant {
        uint256 count = ids.length;
        if (count == 0 || count > MAX_FULFILL_BATCH || count != proofs.length) revert InvalidBatch();
        // A callback sees the results stored before it. Budget every served member's full callback up front, so no
        // callback, however much of its own budget it burns, can starve a later member and revert revealed results.
        // Skipped members stay skipped for the whole transaction, so they need no budget.
        uint256 need = CALLBACK_RESERVE;
        for (uint256 i; i < count; ++i) {
            StoredRequest storage r = _request(ids[i]);
            if (r.fulfilled || r.refunded || block.timestamp > r.deadline) continue;
            uint256 limit = r.callbackGasLimit;
            need += limit + limit / 63 + BATCH_MEMBER_OVERHEAD;
        }
        if (gasleft() < need) revert InsufficientCallbackGas();
        for (uint256 i; i < count; ++i) {
            StoredRequest storage r = _request(ids[i]);
            uint8 reason = r.fulfilled ? 1 : r.refunded ? 2 : block.timestamp > r.deadline ? 3 : 0;
            if (reason != 0) { emit FulfillmentSkipped(ids[i], reason); continue; }
            _fulfill(ids[i], r, proofs[i]);
        }
    }

    function _fulfill(uint256 requestId, StoredRequest storage r, Proof calldata proof) private {
        bytes32 anchor = _storeBlockHash(requestId, r);
        bytes32 randomness = _verifyProof(proof, _seed(requestId, r, anchor));
        r.randomness = randomness;
        r.proofHash = keccak256(abi.encode(proof));
        r.transcriptHash = _transcriptHash(requestId, r, anchor);
        r.fulfilled = true;
        // Submission stays open to anyone. The keeper share goes to the submitter when the registry authorizes it to
        // publish epochs (the committer or an allowed backup committer, such as a follower keeper), so each keeper
        // earns what it serves; any other submitter's fulfillment pays committer(), as before. A registry that reverts
        // the call, including one whose implementation has no such function, pays committer() instead of skipping the
        // share. Return data that does not decode as a bool is not caught and would revert the fulfillment; the
        // registry is the same owner-upgraded contract this request's epoch came from.
        address keeper = epochRegistry.committer();
        if (msg.sender != keeper) {
            try epochRegistry.isAuthorizedCommitter(msg.sender) returns (bool authorized) {
                if (authorized) keeper = msg.sender;
            } catch {}
        }
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
            assembly ("memory-safe") { paid := call(30000, keeper, keeperAmount, 0, 0, 0, 0) }
            if(!paid) { keeperCredits[keeper] += keeperAmount; totalKeeperCredits += keeperAmount; }
            emit KeeperFeePaid(requestId,keeper,keeperAmount,paid);
        }
    }

    function _emitEvidence(uint256 id, bytes32 transcript, Proof calldata proof) private {
        bytes memory packet = abi.encode(proof);
        if (packet.length > MAX_EVIDENCE_PACKET_BYTES) revert EvidencePacketTooLarge();
        emit FulfillmentEvidence(id, transcript, packet);
    }

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

    function _request(uint256 requestId) private view returns (StoredRequest storage r) {
        r = requests[requestId];
        if (r.consumer == address(0)) revert UnknownRequest();
    }

    function _target(uint64 requestBlock,uint64 committedBlock) private pure returns(uint64) {
        uint64 future=committedBlock+1;
        return requestBlock>future?requestBlock:future;
    }
    function _resolvedEpoch(StoredRequest storage r) private view returns(uint64 target,bytes32 epochHash) {
        if(r.epochHash!=bytes32(0)) return (r.targetBlock,r.epochHash);
        EpochEntropy.Epoch memory epoch=epochRegistry.getEpoch(r.epochId);
        if(epoch.epochHash==bytes32(0)) return (0,bytes32(0));
        return (_target(r.requestBlock,epoch.committedBlock),epoch.epochHash);
    }
    function _resolvedBlockHash(StoredRequest storage r) private view returns (bytes32 value) {
        (uint64 target,bytes32 epochHash)=_resolvedEpoch(r);
        if (epochHash==bytes32(0)||block.number < uint256(target) + confirmationBlocks) revert NotReady();
        value = r.blockHash;
        if (value == bytes32(0)) value = blockhash(target);
        if (value == bytes32(0)) revert BlockHashUnavailable();
    }

    function _storeBlockHash(uint256 id, StoredRequest storage r) private returns (bytes32 value) {
        value = _resolvedBlockHash(r);
        (r.targetBlock,r.epochHash)=_resolvedEpoch(r);
        if (r.blockHash == bytes32(0)) {
            r.blockHash = value;
            emit BlockHashStored(id, r.targetBlock, value);
        }
    }

    function _seed(uint256 id, StoredRequest storage r, bytes32 blockHash_)
        private view returns (uint256)
    {
        (uint64 target,bytes32 epochHash)=_resolvedEpoch(r);
        return uint256(keccak256(abi.encode(
            SEED_DOMAIN, block.chainid, address(this), keyHash, id,
            r.consumer, r.clientSeed, r.mappingHash, r.requestBlock, target, blockHash_, r.epochId, epochHash
        )));
    }

    function _verifyProof(Proof calldata proof, uint256 seed)
        private view returns (bytes32)
    {
        if (proof.pk[0] != publicKeyX || proof.pk[1] != publicKeyY) revert WrongPublicKey();
        // Upstream ignores proof.seed in favor of its explicit seed argument: validate both here.
        if (proof.seed != seed) revert WrongSeed();
        return bytes32(_randomValueFromVRFProof(proof, seed));
    }

    function _transcriptHash(uint256 id, StoredRequest storage r, bytes32 anchor)
        private view returns (bytes32)
    {
        return keccak256(abi.encode(
            TRANSCRIPT_DOMAIN, block.chainid, address(this), id, protocolConfigurationHash,
            anchor, r.proofHash, r.randomness, r.mappingHash, r.epochId, r.epochHash
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
