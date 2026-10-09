//! A scripted chain behind a JSON-RPC endpoint, for tests that drive the real `Worker` through whole scenarios: the
//! state and answers of a coordinator, an epoch registry and one funded transaction wallet, on a chain whose blocks the
//! test script mines. A script can make the coordinator a round coordinator instead (`Chain::round_coordinator`): the
//! chain then has no registry, and answers the round coordinator's own reads and none of an epoch coordinator's. Every call the keeper makes is recorded with everything it asked, in the order it asked it, so
//! that a test can compare the whole sequence (see `golden`):
//!
//! - the method, and the block a read is made at: a named tag as it was sent, a number as its distance from the latest
//!   head and from the finalized head, so that the trace says which of them it came from;
//! - for a call or an estimate its target and selector, who it is from, what it carries and its arguments;
//! - for a storage read the slot, for a fee history the blocks and percentiles;
//! - for a transaction it is sent, its type, chain, nonce, gas limit, fees and value, and the ids it serves.
//!
//! The chain is as small as the scenarios need. `finalized` and `safe` trail the latest block by a number of blocks
//! the script sets, and a read at one of them (or at a block number) sees the state that block left, so a keeper that
//! reads the wrong head gets another answer. A transaction is accepted at once and executed when the script mines,
//! and no signature is checked.
use crate::{
    abi::{Beacon, Coordinator as C, EpochRecord, EpochRegistry as E, EpochSelection, Request},
    beacon::fixture,
};
use alloy_consensus::{Transaction, TxEnvelope};
use alloy_eips::eip2718::{Decodable2718, Typed2718};
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_sol_types::{SolCall, SolEvent, sol};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

/// The chain's time at block 0, far enough ahead of any wall clock that no request ever looks old to the journal.
pub const T0: u64 = 4_000_000_000;
pub const BLOCK_SECONDS: u64 = 1;
pub const EPOCH_LENGTH: u64 = 200;
pub const FIRST_EPOCH_START: u64 = 1_000;
/// The beacon recipe the registry's catalog selects, and the drand network it names.
pub const BEACON_RECIPE: u8 = 6;
pub const BEACON_PERIOD: u64 = 3;
pub const BEACON_GENESIS: u64 = T0 - 3_000_000;
/// How long the endpoint holds answers. A request opens a window of this length, and every request that arrives inside
/// the window is answered at its end: calls the keeper makes at the same time are one wave, and a call that waits for
/// an answer cannot arrive before the window ends, so it starts the next wave. A keeper that issues its calls in a
/// pipeline, such as four at a time, is read in the same steps whatever the jitter between the calls. They arrive
/// within a millisecond or two, so 20 ms tells them from dependent calls with room to spare; `D20_GOLDEN_HOLD_MS`
/// raises it on a machine too busy for that.
pub fn hold() -> Duration {
    static HOLD: OnceLock<Duration> = OnceLock::new();
    *HOLD.get_or_init(|| {
        let millis = std::env::var("D20_GOLDEN_HOLD_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(20);
        Duration::from_millis(millis)
    })
}

sol! {
    struct MapSpec {uint8 operation;uint256 lower;uint256 upper;uint32 count;uint32 population;}
    /// What the public explorer reads of the contracts and the events it indexes, as the scripted chain serves and
    /// emits them. Declared here for the same reason as the NodeInterface below.
    interface Indexed {
        function initialFeeRecipient() external view returns (address);
        function initialMinFee() external view returns (uint256);
        function keyHash() external view returns (bytes32);
        function firstEpochStart() external view returns (uint64);
        function hyperliquidSigner() external view returns (address);
        function ethereumBlockSigner() external view returns (address);
        function btcTradeSigner() external view returns (address);
        function ethTradeSigner() external view returns (address);
        function getMapping(uint256 requestId) external view returns (MapSpec);
    }
    event EpochCommitted(uint64 indexed epochId, bytes32 indexed epochHash, bytes packet);
    event FulfillmentEvidence(uint256 indexed requestId, bytes32 indexed transcriptHash, bytes packet);
    event RandomnessRequested(uint256 indexed requestId, address indexed consumer, bytes32 indexed keyHash, bytes32 clientSeed, uint64 requestBlock, uint32 callbackGasLimit, uint256 feePaid, address refundAddress, uint64 deadline);
    /// A round coordinator's request and the reads of it the chain serves, declared here for the same reason as the
    /// NodeInterface below: the chain needs nothing of the keeper that 0.4.1 lacks.
    #[derive(Default)]
    struct RoundRequest {
        address consumer; uint32 callbackGasLimit; uint64 requestBlock; uint64 deadline; address refundAddress;
        bytes32 clientSeed; bytes32 mappingHash; uint8 beaconId; uint64 round; bytes32 roundRandomness;
        bytes32 randomness; bytes32 proofHash; bytes32 transcriptHash; uint256 feePaid;
        bool fulfilled; bool delivered; bool refunded;
    }
    /// A beacon the round coordinator's book registers.
    struct RoundBeacon { address verifier; uint64 genesis; uint64 period; bytes32 chainHash; bytes publicKey; }
    /// The VRF proof a round fulfillment carries, and the signature of a round a batch lists.
    struct RoundVrfProof {
        uint256[2] pk; uint256[2] gamma; uint256 c; uint256 s; uint256 seed; address uWitness;
        uint256[2] cGammaWitness; uint256[2] sHashWitness; uint256 zInv;
    }
    struct RoundSig { uint8 beaconId; uint64 round; bytes signature; }
    interface Round {
        function getProofContext(uint256 requestId, bytes roundSignature) external view returns (uint256 seed, uint64 deadline, bool fulfilled, bool refunded);
        function fulfillRandomness(uint256 requestId, RoundVrfProof proof, bytes roundSignature) external;
        function fulfillRandomnessBatch(RoundSig[] rounds, uint256[] ids, RoundVrfProof[] proofs) external;
        function getRoundRequest(uint256 requestId) external view returns (RoundRequest request);
        function beaconCount() external view returns (uint256 count);
        function getBeacon(uint8 beaconId) external view returns (RoundBeacon beacon);
        function checkRoundSignature(uint8 beaconId, uint64 round, bytes signature) external view returns (bool valid);
        function roundRandomness(uint8 beaconId, uint64 round) external view returns (bytes32 randomness);
        event RoundVerified(uint8 indexed beaconId, uint64 indexed round, bytes32 randomness, bytes signature);
        function keeper() external view returns (address primary);
        function isBackupKeeper(address account) external view returns (bool allowed);
        function ROUND_LEAD() external view returns (uint64 lead);
        function pricing() external view returns (uint256 minFee, uint16 feeMultiplier, uint32 fulfillGasOverhead);
        function keeperFeeBps() external view returns (uint16 bps);
        function beaconSchedule() external view returns (uint8 beaconId, uint64 since, uint8 nextBeaconId, uint64 nextFrom);
        event RoundAssigned(uint256 indexed requestId, uint8 indexed beaconId, uint64 indexed round);
    }
    /// Arbitrum's NodeInterface as the scripted chain serves it. The chain declares its own copy instead of the
    /// keeper's, so that it needs nothing from the keeper but the interfaces it exists to answer, and the same
    /// scripted chain runs against every release of the keeper.
    interface NodeInterface {
        function gasEstimateL1Component(address to, bool contractCreation, bytes data) external payable returns (uint64 gasEstimateForL1, uint256 baseFee, uint256 l1BaseFeeEstimate);
    }
}
/// The NodeInterface precompile's address, 0xc8.
const NODE_INTERFACE: Address = Address::new([
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xc8,
]);

/// A JSON-RPC error: its code and message.
pub type Fault = (i64, String);

/// What one call is recorded as.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Call(pub String);

/// What the node was asked at one instant: one HTTP request, a single call or a batch.
#[derive(Clone, Debug)]
pub struct Seen {
    pub arrived: Instant,
    /// When the node answered: the end of the window the request arrived in.
    pub answered: Instant,
    pub calls: Vec<Call>,
    pub batch: bool,
    /// Which endpoint was asked, when the node has more than one: `[2] `.
    pub label: String,
}
impl Seen {
    /// The text of the request, which orders it among the requests of its wave.
    fn key(&self) -> String {
        let calls = self
            .calls
            .iter()
            .map(|call| call.0.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        format!("{}{calls}", self.label)
    }
}

/// A request of the scripted coordinator, with the part of its state the script sets.
#[derive(Clone, Debug)]
pub struct Stored {
    pub consumer: Address,
    pub callback_gas: u32,
    pub request_block: u64,
    pub epoch_id: u64,
    pub fee_paid: u128,
    pub fulfilled: bool,
    pub delivered: bool,
    pub refunded: bool,
    /// The proof packet that the serving transaction emitted as evidence.
    pub packet: Option<Vec<u8>>,
    /// The beacon and round a round coordinator bound the request to when it was made; `None` on an epoch coordinator.
    pub round: Option<(u8, u64)>,
    /// How many times a replaced block moved the request to other fields (`Chain::move_request`): a round coordinator's
    /// request then has another client seed under the same id.
    pub moved: u64,
}
/// A round coordinator in place of the epoch coordinator and its registry: what the chain answers of its beacon book and
/// pricing. A request is bound, when it is made, to the first round scheduled at least `lead` seconds after its block's
/// time, as `BeaconBook._round` binds it.
#[derive(Clone, Debug)]
pub struct RoundBook {
    pub lead: u64,
    pub beacon: u8,
    pub genesis: u64,
    pub period: u64,
    pub min_fee: U256,
    pub fee_multiplier: u16,
    pub fulfill_gas_overhead: u32,
    /// `keeperFeeBps()`: the keeper's share of a request's fee, in basis points.
    pub keeper_fee_bps: u16,
    /// What `keyHash()` answers instead of the hash of the chain's key: a coordinator that holds another key.
    pub key_hash: Option<B256>,
    /// The rounds verified on chain, by beacon and round: the block that verified each and its randomness.
    pub verified: BTreeMap<(u8, u64), (u64, B256)>,
    /// How many beacons the book has registered, from 0: the one in force and the ones registered before it.
    pub beacons: u8,
}
impl Default for RoundBook {
    fn default() -> Self {
        Self {
            lead: 3,
            beacon: 0,
            genesis: BEACON_GENESIS,
            period: BEACON_PERIOD,
            min_fee: U256::from(20_000_000_000_000u64),
            fee_multiplier: 4,
            fulfill_gas_overhead: 360_000,
            keeper_fee_bps: 8_000,
            key_hash: None,
            verified: BTreeMap::new(),
            beacons: 1,
        }
    }
}
/// The signature the chain's drand network has for a round: what its relays serve and its verifier accepts.
pub fn round_signature(round: u64) -> [u8; 64] {
    let mut signature = [0u8; 64];
    signature[..32].copy_from_slice(keccak256(round.to_be_bytes()).as_slice());
    signature[32..].copy_from_slice(keccak256(round.to_le_bytes()).as_slice());
    signature
}
/// A round's randomness: the sha256 of its signature, as drand and the round coordinator compute it.
pub fn round_randomness(signature: &[u8]) -> B256 {
    use k256::sha2::{Digest, Sha256};
    B256::from_slice(&Sha256::digest(signature))
}
impl RoundBook {
    /// The round a request made in a block of this time binds.
    pub fn round_at(&self, time: u64) -> u64 {
        let t = time + self.lead;
        if t <= self.genesis {
            1
        } else {
            (t - self.genesis).div_ceil(self.period) + 1
        }
    }
}
/// What a round coordinator's fulfillment does when it runs (`Chain::round_fulfillment`): the requests it serves, those it
/// skips with their reason, and the rounds it verifies with their randomness.
struct RoundPlan {
    serve: Vec<u64>,
    skip: Vec<(u64, u8)>,
    verify: Vec<(u8, u64, B256)>,
}
/// What `eth_estimateGas` answers.
#[derive(Clone, Debug)]
pub struct Estimates {
    pub fulfill: u64,
    pub batch_base: u64,
    pub batch_member: u64,
    pub epoch: u64,
    pub transfer: u64,
}
impl Default for Estimates {
    fn default() -> Self {
        Self {
            fulfill: 760_000,
            batch_base: 150_000,
            batch_member: 600_000,
            epoch: 500_000,
            transfer: 21_000,
        }
    }
}
/// The part of the chain's state that a read can ask about at an older block: what the script has set up and what the
/// transactions did. The chain keeps what every block it has mined past left, for the reads at `finalized`, `safe` and
/// a block number.
#[derive(Clone)]
struct Ledger {
    requests: BTreeMap<u64, Stored>,
    next_request: u64,
    epochs: BTreeMap<u64, EpochRecord>,
    balances: HashMap<Address, U256>,
    nonce: u64,
    primary_nonce: u64,
}
#[derive(Clone)]
struct Pending {
    nonce: u64,
    hash: B256,
    envelope: TxEnvelope,
}
struct Mined {
    block: u64,
    success: bool,
    logs: Vec<Value>,
}

pub struct Chain {
    pub chain_id: u64,
    pub head: u64,
    /// How many blocks `finalized` and `safe` trail the latest block; 0 for a chain that finalizes at once.
    pub finalized_lag: u64,
    pub safe_lag: u64,
    pub base_fee: u128,
    /// The base fee rises by this much for each block number modulo 8, so that a fee priced from the wrong block
    /// differs; 0 keeps it constant.
    pub base_fee_step: u128,
    /// The tips blocks paid, as a fee history reports them: the block at position `i` of a history paid
    /// `tips[i % tips.len()]`.
    pub tips: Vec<u128>,
    /// The transaction wallet of the keeper under test.
    pub keeper: Address,
    /// The registry's committer: the keeper itself, or another wallet when the keeper under test is a follower.
    pub committer: Address,
    /// The wallets the registry's owner has allowed to publish epochs as backups.
    pub backups: HashSet<Address>,
    pub coordinator: Address,
    pub registry: Address,
    pub fee_recipient: Address,
    pub proxy_code: Bytes,
    pub coordinator_implementation: Address,
    pub registry_implementation: Address,
    pub implementation_code: BTreeMap<Address, Bytes>,
    pub public_key: [U256; 2],
    pub protocol_hash: B256,
    pub catalog_hash: B256,
    pub confirmations: u16,
    pub beacon_chain_hash: B256,
    pub requests: BTreeMap<u64, Stored>,
    pub next_request: u64,
    pub epochs: BTreeMap<u64, EpochRecord>,
    pub balances: HashMap<Address, U256>,
    /// The wallet's confirmed nonce: transactions below it are mined.
    pub nonce: u64,
    /// The confirmed nonce of the committer when it is not the keeper's wallet: the primary's own transactions.
    pub primary_nonce: u64,
    /// The logs of the chain's contracts, as `eth_getLogs` returns them.
    pub logs: Vec<Value>,
    pub estimates: Estimates,
    /// The L1 gas component the NodeInterface answers for a payload of this many bytes.
    pub l1_gas: fn(usize) -> u64,
    /// While set, `eth_estimateGas` fails for a transaction without data.
    pub fail_transfer_estimate: bool,
    /// While set, a transaction without data whose gas limit is below what the node estimates for a transfer is
    /// rejected as an Arbitrum node rejects it: its intrinsic gas, with the L1 component, is more than the limit.
    pub enforce_transfer_gas: bool,
    /// While set, the NodeInterface is not served.
    pub fail_node_interface: bool,
    /// While set, no receipt is served for any transaction: an endpoint that has not caught up with the block that
    /// holds it.
    pub hide_receipts: bool,
    /// Set when the coordinator is a round coordinator (`round_coordinator`).
    pub round: Option<RoundBook>,
    /// While set, the round coordinator does not answer `checkRoundSignature`: a node error, not a verdict.
    pub fail_round_check: bool,
    /// While set, the round coordinator's `getProofContext` answers a seed that is not the request's.
    pub wrong_proof_context: bool,
    /// The block numbers from which the sequencer replaced the chain, oldest first (see `replace_blocks`).
    forks: Vec<u64>,
    /// How many times the script changed the seed of a request's proof input (see `reseed`), by request id.
    seeds: BTreeMap<u64, u64>,
    queue: Vec<Pending>,
    /// The transactions the chain has mined, with the block of each: what `drop_blocks` takes out and may put back.
    mined: Vec<(u64, Pending)>,
    receipts: HashMap<B256, Mined>,
    sent: HashMap<B256, String>,
    /// What every block the chain has mined past left behind, by block number.
    history: BTreeMap<u64, Ledger>,
    /// Transactions the keeper sent, in order: their hash, label, gas limit and nonce.
    pub sends: Vec<(B256, String, u64, u64)>,
}

/// Where `number` lies relative to `head`: nothing for the head itself, otherwise `-3` or `+3`.
fn distance(number: u64, head: u64) -> String {
    match number.cmp(&head) {
        std::cmp::Ordering::Equal => String::new(),
        std::cmp::Ordering::Less => format!("-{}", head - number),
        std::cmp::Ordering::Greater => format!("+{}", number - head),
    }
}
/// The first four bytes of the hash of `data`: enough for a trace to show that arguments differ.
fn fingerprint(data: &[u8]) -> String {
    hex::encode(&keccak256(data)[..4])
}
/// The EIP-1967 slot of a proxy's implementation, which the keeper reads to pin it.
const IMPLEMENTATION_SLOT: &str =
    "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";

fn quantity(value: u128) -> Value {
    json!(format!("0x{value:x}"))
}
/// The number in a JSON quantity such as `"0x3e8"`.
fn quantity_of(value: &Value) -> u64 {
    u64::from_str_radix(
        value.as_str().expect("a quantity").trim_start_matches("0x"),
        16,
    )
    .expect("a hexadecimal quantity")
}
fn address(value: &Value) -> Address {
    serde_json::from_value(value.clone()).expect("address parameter")
}
fn bytes(value: &Value) -> Vec<u8> {
    serde_json::from_value::<Bytes>(value.clone())
        .expect("data parameter")
        .to_vec()
}
fn word(value: B256) -> Value {
    json!(value)
}
fn revert(reason: &str) -> Fault {
    (3, format!("execution reverted: {reason}"))
}
fn returned(bytes: Vec<u8>) -> Result<Value, Fault> {
    Ok(json!(Bytes::from(bytes)))
}
/// Names of the selectors the keeper calls, for the trace.
const NAMES: &[([u8; 4], &str)] = &[
    (C::getRequestCall::SELECTOR, "getRequest"),
    (C::epochRegistryCall::SELECTOR, "epochRegistry"),
    (
        C::protocolConfigurationHashCall::SELECTOR,
        "protocolConfigurationHash",
    ),
    (C::confirmationBlocksCall::SELECTOR, "confirmationBlocks"),
    (C::getProofContextCall::SELECTOR, "getProofContext"),
    (C::fulfillRandomnessCall::SELECTOR, "fulfillRandomness"),
    (
        C::fulfillRandomnessBatchCall::SELECTOR,
        "fulfillRandomnessBatch",
    ),
    (C::requestFeePaidCall::SELECTOR, "requestFeePaid"),
    (C::feeRecipientCall::SELECTOR, "feeRecipient"),
    (
        C::getPendingRequestIdsCall::SELECTOR,
        "getPendingRequestIds",
    ),
    (C::nextRequestIdCall::SELECTOR, "nextRequestId"),
    (C::publicKeyXCall::SELECTOR, "publicKeyX"),
    (C::publicKeyYCall::SELECTOR, "publicKeyY"),
    (E::committerCall::SELECTOR, "committer"),
    (E::isBackupCommitterCall::SELECTOR, "isBackupCommitter"),
    (E::catalogHashCall::SELECTOR, "catalogHash"),
    (E::nextEpochToPrepareCall::SELECTOR, "nextEpochToPrepare"),
    (E::epochStartCall::SELECTOR, "epochStart"),
    (E::sourceCountAtCall::SELECTOR, "sourceCountAt"),
    (E::catalogAtCall::SELECTOR, "catalogAt"),
    (E::getEpochCall::SELECTOR, "getEpoch"),
    (
        E::getEpochFallbackSelectionCall::SELECTOR,
        "getEpochFallbackSelection",
    ),
    (E::getRecipeCall::SELECTOR, "getRecipe"),
    (E::beaconOfCall::SELECTOR, "beaconOf"),
    (E::verifyBeaconCall::SELECTOR, "verifyBeacon"),
    (E::commitEpochCall::SELECTOR, "commitEpoch"),
    (E::commitEpochFallbackCall::SELECTOR, "commitEpochFallback"),
    (
        NodeInterface::gasEstimateL1ComponentCall::SELECTOR,
        "gasEstimateL1Component",
    ),
    (
        Indexed::initialFeeRecipientCall::SELECTOR,
        "initialFeeRecipient",
    ),
    (Indexed::initialMinFeeCall::SELECTOR, "initialMinFee"),
    (Indexed::keyHashCall::SELECTOR, "keyHash"),
    (Indexed::firstEpochStartCall::SELECTOR, "firstEpochStart"),
    (
        Indexed::hyperliquidSignerCall::SELECTOR,
        "hyperliquidSigner",
    ),
    (
        Indexed::ethereumBlockSignerCall::SELECTOR,
        "ethereumBlockSigner",
    ),
    (Indexed::btcTradeSignerCall::SELECTOR, "btcTradeSigner"),
    (Indexed::ethTradeSignerCall::SELECTOR, "ethTradeSigner"),
    (Indexed::getMappingCall::SELECTOR, "getMapping"),
    (Round::getRoundRequestCall::SELECTOR, "getRoundRequest"),
    (Round::keeperCall::SELECTOR, "keeper"),
    (Round::isBackupKeeperCall::SELECTOR, "isBackupKeeper"),
    (Round::ROUND_LEADCall::SELECTOR, "ROUND_LEAD"),
    (Round::pricingCall::SELECTOR, "pricing"),
    (Round::keeperFeeBpsCall::SELECTOR, "keeperFeeBps"),
    (Round::beaconScheduleCall::SELECTOR, "beaconSchedule"),
    (Round::beaconCountCall::SELECTOR, "beaconCount"),
    (Round::getBeaconCall::SELECTOR, "getBeacon"),
    (
        Round::checkRoundSignatureCall::SELECTOR,
        "checkRoundSignature",
    ),
    (Round::roundRandomnessCall::SELECTOR, "roundRandomness"),
    (Round::getProofContextCall::SELECTOR, "getProofContext"),
    (Round::fulfillRandomnessCall::SELECTOR, "fulfillRandomness"),
    (
        Round::fulfillRandomnessBatchCall::SELECTOR,
        "fulfillRandomnessBatch",
    ),
];
fn selector_text(data: &[u8]) -> String {
    match data.get(..4) {
        None => "no-data".into(),
        Some(selector) => {
            let name = NAMES
                .iter()
                .find(|(known, _)| known == selector)
                .map_or("unknown", |(_, name)| name);
            format!("0x{} {name}", hex::encode(selector))
        }
    }
}

impl Chain {
    /// A chain at the start of epoch 1 with no request, an epoch registry with a beacon catalog, and a funded wallet.
    pub fn new(chain_id: u64, keeper: Address, public_key: [U256; 2]) -> Self {
        let coordinator = Address::repeat_byte(0xc0);
        let registry = Address::repeat_byte(0xe0);
        let mut balances = HashMap::new();
        balances.insert(
            keeper,
            U256::from(10u64) * U256::from(10u64).pow(U256::from(18)),
        );
        Self {
            chain_id,
            head: FIRST_EPOCH_START,
            finalized_lag: 0,
            safe_lag: 0,
            base_fee: 1_000_000_000,
            base_fee_step: 0,
            tips: vec![1_000_000_000],
            keeper,
            committer: keeper,
            backups: HashSet::new(),
            coordinator,
            registry,
            fee_recipient: Address::repeat_byte(0xfe),
            proxy_code: Bytes::from_static(&[
                0x36, 0x3d, 0x3d, 0x37, 0x3d, 0x3d, 0x3d, 0x36, 0x3d, 0x73, 0xf3,
            ]),
            coordinator_implementation: Address::repeat_byte(0xc1),
            registry_implementation: Address::repeat_byte(0xe1),
            implementation_code: BTreeMap::from([
                (
                    Address::repeat_byte(0xc1),
                    Bytes::from_static(b"coordinator implementation"),
                ),
                (
                    Address::repeat_byte(0xe1),
                    Bytes::from_static(b"registry implementation"),
                ),
            ]),
            public_key,
            protocol_hash: keccak256(b"protocol configuration"),
            catalog_hash: keccak256(b"catalog"),
            confirmations: 1,
            beacon_chain_hash: keccak256(b"drand test network"),
            requests: BTreeMap::new(),
            next_request: 1,
            epochs: BTreeMap::new(),
            balances,
            nonce: 0,
            primary_nonce: 0,
            logs: Vec::new(),
            estimates: Estimates::default(),
            l1_gas: |_| 0,
            fail_transfer_estimate: false,
            enforce_transfer_gas: false,
            fail_node_interface: false,
            hide_receipts: false,
            round: None,
            fail_round_check: false,
            wrong_proof_context: false,
            forks: Vec::new(),
            seeds: BTreeMap::new(),
            queue: Vec::new(),
            mined: Vec::new(),
            receipts: HashMap::new(),
            sent: HashMap::new(),
            history: BTreeMap::new(),
            sends: Vec::new(),
        }
    }
    pub fn time(&self, block: u64) -> u64 {
        T0 + block * BLOCK_SECONDS
    }
    pub fn block_hash(&self, block: u64) -> B256 {
        self.block_hash_after(block, self.forks.len())
    }
    /// The hash a block had when the sequencer had made only its first `replacements` replacements.
    fn block_hash_after(&self, block: u64, replacements: usize) -> B256 {
        let mut name = format!("block {} {block}", self.chain_id);
        // Every replacement gives the blocks at or above its number a hash of their own; without one the hash is what
        // it has always been.
        for (replacement, from) in self.forks.iter().take(replacements).enumerate() {
            if *from <= block {
                name.push_str(&format!(" replaced {replacement}"));
            }
        }
        keccak256(name)
    }
    /// The sequencer replaces the blocks from number `from` up to the latest one, and every block it mines after, with
    /// others: they have other hashes, and the state they hold is the state the replaced ones left. What a keeper
    /// recorded of the replaced blocks is no longer the chain's.
    pub fn replace_blocks(&mut self, from: u64) {
        assert!(from <= self.head, "only mined blocks can be replaced");
        self.forks.push(from);
    }
    /// The sequencer replaces the blocks from number `from` up to the latest one, and every block it mines after, with
    /// blocks that hold none of the transactions the replaced ones held: it rebuilt its history and lost what came after
    /// the last batch it had posted. The blocks have other hashes, as with `replace_blocks`; the receipts of the lost
    /// transactions are gone, and so are the logs of everything the blocks held, and the state is what block `from - 1`
    /// left: the wallet's nonce, the requests, the epochs, the balances. With `requeue` the lost transactions are back
    /// in the sequencer's queue, to be included again when the script says so, as a sequencer that sequences them again
    /// does; without it they are gone for good and the keeper must send them again.
    pub fn drop_blocks(&mut self, from: u64, requeue: bool) {
        assert!(
            from >= 1 && from <= self.head,
            "only mined blocks can be dropped"
        );
        let restored = self
            .history
            .get(&(from - 1))
            .cloned()
            .expect("the chain mined past the block before");
        let lost: Vec<Pending> = self
            .mined
            .iter()
            .filter(|(block, _)| *block >= from)
            .map(|(_, pending)| pending.clone())
            .collect();
        self.mined.retain(|(block, _)| *block < from);
        self.receipts.retain(|_, mined| mined.block < from);
        // A round verified in a block that is gone is not verified any more.
        if let Some(book) = self.round.as_mut() {
            book.verified.retain(|_, (block, _)| *block < from);
        }
        self.logs
            .retain(|log| quantity_of(&log["blockNumber"]) < from);
        self.exchange(restored.clone());
        for block in from..self.head {
            self.history.insert(block, restored.clone());
        }
        self.forks.push(from);
        if requeue {
            self.queue.extend(lost);
        }
    }
    /// The seed of request `id`'s proof input is another one from now on, as it is when the block it binds was
    /// replaced.
    pub fn reseed(&mut self, id: u64) {
        *self.seeds.entry(id).or_default() += 1;
    }
    /// The seed the coordinator's proof context gives request `id`.
    pub fn proof_seed(&self, id: u64) -> U256 {
        U256::from_be_bytes(
            match self.seeds.get(&id) {
                None | Some(0) => keccak256(format!("proof seed {id}")),
                Some(times) => keccak256(format!("proof seed {id} {times}")),
            }
            .0,
        )
    }
    /// The code hash of the implementation behind the coordinator proxy and the registry proxy.
    pub fn implementation_hash(&self, implementation: Address) -> B256 {
        keccak256(&self.implementation_code[&implementation])
    }
    pub fn finalized_head(&self) -> u64 {
        self.head.saturating_sub(self.finalized_lag)
    }
    pub fn safe_head(&self) -> u64 {
        self.head.saturating_sub(self.safe_lag)
    }
    /// The base fee of a block.
    pub fn base_fee_at(&self, block: u64) -> u128 {
        self.base_fee + u128::from(block % 8) * self.base_fee_step
    }
    fn ledger(&self) -> Ledger {
        Ledger {
            requests: self.requests.clone(),
            next_request: self.next_request,
            epochs: self.epochs.clone(),
            balances: self.balances.clone(),
            nonce: self.nonce,
            primary_nonce: self.primary_nonce,
        }
    }
    /// Put `ledger` in place of the chain's state, and return the state that was there.
    fn exchange(&mut self, ledger: Ledger) -> Ledger {
        Ledger {
            requests: std::mem::replace(&mut self.requests, ledger.requests),
            next_request: std::mem::replace(&mut self.next_request, ledger.next_request),
            epochs: std::mem::replace(&mut self.epochs, ledger.epochs),
            balances: std::mem::replace(&mut self.balances, ledger.balances),
            nonce: std::mem::replace(&mut self.nonce, ledger.nonce),
            primary_nonce: std::mem::replace(&mut self.primary_nonce, ledger.primary_nonce),
        }
    }
    /// Mine empty blocks: each leaves the state as it is now.
    pub fn mine(&mut self, blocks: u64) {
        for block in self.head..self.head + blocks {
            self.history.insert(block, self.ledger());
        }
        self.head += blocks;
    }
    /// Mine up to block `number`, which is the head from then on.
    pub fn mine_to(&mut self, number: u64) {
        assert!(number >= self.head, "the chain does not go back");
        self.mine(number - self.head);
    }
    /// Run a read as of block `block` when it is behind the head: the state that block left, with its number as the
    /// head. A read of the head, or of a block the chain did not mine past, sees the state as it is.
    fn read_at<T>(&mut self, block: Option<u64>, read: impl FnOnce(&Self) -> T) -> T {
        let past = block
            .filter(|block| *block < self.head)
            .and_then(|block| Some((block, self.history.get(&block)?.clone())));
        let Some((block, ledger)) = past else {
            return read(self);
        };
        let live = self.exchange(ledger);
        let head = std::mem::replace(&mut self.head, block);
        let answer = read(self);
        self.head = head;
        self.exchange(live);
        answer
    }
    pub fn epoch_start(&self, epoch: u64) -> u64 {
        FIRST_EPOCH_START + (epoch - 1) * EPOCH_LENGTH
    }
    fn epoch_of(&self, block: u64) -> u64 {
        if block < FIRST_EPOCH_START {
            0
        } else {
            (block - FIRST_EPOCH_START) / EPOCH_LENGTH + 1
        }
    }
    fn canonical_request(&self) -> String {
        format!(r#"["drand","0x{}"]"#, hex::encode(self.beacon_chain_hash))
    }
    /// A request from `consumer` in the epoch current at the head, paying `fee_paid`.
    pub fn request(&mut self, consumer: Address, callback_gas: u32, fee_paid: u128) -> u64 {
        let id = self.next_request;
        self.next_request += 1;
        self.requests.insert(
            id,
            Stored {
                consumer,
                callback_gas,
                request_block: self.head,
                epoch_id: self.epoch_of(self.head),
                fee_paid,
                fulfilled: false,
                delivered: false,
                refunded: false,
                packet: None,
                round: self
                    .round
                    .as_ref()
                    .map(|book| (book.beacon, book.round_at(self.time(self.head)))),
                moved: 0,
            },
        );
        let log = RandomnessRequested {
            requestId: U256::from(id),
            consumer,
            keyHash: if self.round.is_some() {
                self.round_key_hash()
            } else {
                self.key_hash()
            },
            clientSeed: keccak256(format!("seed {id}")),
            requestBlock: self.head,
            callbackGasLimit: callback_gas,
            feePaid: U256::from(fee_paid),
            refundAddress: consumer,
            deadline: self.time(self.head) + crate::config::RESPONSE_TIMEOUT_SECONDS,
        }
        .encode_log_data();
        self.emit(
            self.coordinator,
            self.head,
            log.topics().to_vec(),
            log.data.to_vec(),
        );
        if let Some((beacon, round)) = self.requests[&id].round {
            let assigned = Round::RoundAssigned {
                requestId: U256::from(id),
                beaconId: beacon,
                round,
            }
            .encode_log_data();
            self.emit(
                self.coordinator,
                self.head,
                assigned.topics().to_vec(),
                assigned.data.to_vec(),
            );
        }
        id
    }
    /// The hash of the coordinator's VRF key.
    fn key_hash(&self) -> B256 {
        keccak256(format!("key {} {}", self.public_key[0], self.public_key[1]))
    }
    /// The hash a round coordinator keeps of its VRF key: `keccak256(abi.encode(uint256[2] publicKey))`.
    pub fn round_key_hash(&self) -> B256 {
        let mut words = self.public_key[0].to_be_bytes::<32>().to_vec();
        words.extend_from_slice(&self.public_key[1].to_be_bytes::<32>());
        keccak256(words)
    }
    /// From now on the coordinator is a round coordinator with the beacon book `RoundBook::default()`: the chain has no
    /// registry, binds every new request to a drand round, and answers the round coordinator's reads alone.
    pub fn round_coordinator(&mut self) {
        self.round = Some(RoundBook::default());
    }
    /// Another keeper's fulfillment verifies `round` of `beacon` in block `block`: the coordinator keeps its randomness
    /// and emits `RoundVerified` with the signature.
    pub fn verify_round(&mut self, beacon: u8, round: u64, block: u64) {
        let signature = round_signature(round);
        let randomness = round_randomness(&signature);
        self.round
            .as_mut()
            .expect("a round coordinator")
            .verified
            .insert((beacon, round), (block, randomness));
        let verified = Round::RoundVerified {
            beaconId: beacon,
            round,
            randomness,
            signature: Bytes::copy_from_slice(&signature),
        }
        .encode_log_data();
        self.emit(
            self.coordinator,
            block,
            verified.topics().to_vec(),
            verified.data.to_vec(),
        );
    }
    /// A replaced block moves request `id`: the same id now names a request with another client seed, as when the
    /// sequencer put another request of the same consumer at its place. Its fingerprint and seed change.
    pub fn move_request(&mut self, id: u64) {
        self.requests
            .get_mut(&id)
            .expect("a request of the chain")
            .moved += 1;
    }
    /// The randomness of `round` of `beacon` that the round coordinator has verified at the head, if it has.
    fn cached_round(&self, beacon: u8, round: u64) -> Option<B256> {
        self.round
            .as_ref()?
            .verified
            .get(&(beacon, round))
            .filter(|(block, _)| *block <= self.head)
            .map(|(_, randomness)| *randomness)
    }
    /// A round coordinator's request as `getRoundRequest` answers it; `None` for an id the coordinator does not have, for
    /// which it reverts `UnknownRequest`.
    pub fn round_view(&self, id: u64) -> Option<RoundRequest> {
        let stored = self.requests.get(&id)?;
        let (beacon, round) = stored.round.unwrap_or_default();
        Some(RoundRequest {
            consumer: stored.consumer,
            callbackGasLimit: stored.callback_gas,
            requestBlock: stored.request_block,
            deadline: self.time(stored.request_block) + crate::config::RESPONSE_TIMEOUT_SECONDS,
            refundAddress: stored.consumer,
            clientSeed: match stored.moved {
                0 => keccak256(format!("seed {id}")),
                moved => keccak256(format!("seed {id} moved {moved}")),
            },
            mappingHash: B256::ZERO,
            beaconId: beacon,
            round,
            roundRandomness: self.cached_round(beacon, round).unwrap_or_default(),
            randomness: B256::ZERO,
            proofHash: stored.packet.as_ref().map_or(B256::ZERO, keccak256),
            transcriptHash: B256::ZERO,
            feePaid: U256::from(stored.fee_paid),
            fulfilled: stored.fulfilled,
            delivered: stored.delivered,
            refunded: stored.refunded,
        })
    }
    /// A round coordinator's answer to a call with `selector`: its own reads, or a revert for the functions only an epoch
    /// coordinator has. `None` for the functions both have, which the epoch coordinator's branches answer.
    fn round_call(
        &self,
        book: &RoundBook,
        selector: [u8; 4],
        data: &[u8],
    ) -> Option<Result<Value, Fault>> {
        let failed = |e: alloy_sol_types::Error| revert(&e.to_string());
        Some(if selector == Round::getRoundRequestCall::SELECTOR {
            let call = match Round::getRoundRequestCall::abi_decode(data) {
                Ok(call) => call,
                Err(error) => return Some(Err(failed(error))),
            };
            match u64::try_from(call.requestId)
                .ok()
                .and_then(|id| self.round_view(id))
            {
                Some(request) => returned(Round::getRoundRequestCall::abi_encode_returns(&request)),
                None => Err(revert("UnknownRequest")),
            }
        } else if selector == Round::keeperCall::SELECTOR {
            returned(Round::keeperCall::abi_encode_returns(&self.committer))
        } else if selector == Round::isBackupKeeperCall::SELECTOR {
            match Round::isBackupKeeperCall::abi_decode(data) {
                Ok(call) => returned(Round::isBackupKeeperCall::abi_encode_returns(
                    &self.backups.contains(&call.account),
                )),
                Err(error) => Err(failed(error)),
            }
        } else if selector == Indexed::keyHashCall::SELECTOR {
            returned(Indexed::keyHashCall::abi_encode_returns(
                &book.key_hash.unwrap_or_else(|| self.round_key_hash()),
            ))
        } else if selector == Round::ROUND_LEADCall::SELECTOR {
            returned(Round::ROUND_LEADCall::abi_encode_returns(&book.lead))
        } else if selector == Round::pricingCall::SELECTOR {
            returned(Round::pricingCall::abi_encode_returns(
                &Round::pricingReturn {
                    minFee: book.min_fee,
                    feeMultiplier: book.fee_multiplier,
                    fulfillGasOverhead: book.fulfill_gas_overhead,
                },
            ))
        } else if selector == Round::keeperFeeBpsCall::SELECTOR {
            returned(Round::keeperFeeBpsCall::abi_encode_returns(
                &book.keeper_fee_bps,
            ))
        } else if selector == Round::beaconCountCall::SELECTOR {
            returned(Round::beaconCountCall::abi_encode_returns(&U256::from(
                book.beacons,
            )))
        } else if selector == Round::getBeaconCall::SELECTOR {
            match Round::getBeaconCall::abi_decode(data) {
                // Every beacon of the book is the chain's drand network, with a verifier of its own.
                Ok(call) if call.beaconId < book.beacons => {
                    returned(Round::getBeaconCall::abi_encode_returns(&RoundBeacon {
                        verifier: Address::repeat_byte(0xb0 + call.beaconId),
                        genesis: book.genesis,
                        period: book.period,
                        chainHash: self.beacon_chain_hash,
                        publicKey: Bytes::from(vec![7u8; 128]),
                    }))
                }
                Ok(_) => Err(revert("UnknownBeacon")),
                Err(error) => Err(failed(error)),
            }
        } else if selector == Round::getProofContextCall::SELECTOR {
            let call = match Round::getProofContextCall::abi_decode(data) {
                Ok(call) => call,
                Err(error) => return Some(Err(failed(error))),
            };
            let id = u64::try_from(call.requestId).unwrap_or(0);
            let Some(request) = self.round_view(id) else {
                return Some(Err(revert("UnknownRequest")));
            };
            let randomness = match self.round_randomness(
                request.beaconId,
                request.round,
                &call.roundSignature,
            ) {
                Ok((randomness, _)) => randomness,
                Err(fault) => return Some(Err(fault)),
            };
            let mut seed = self.round_seed(id, randomness).expect("the request exists");
            if self.wrong_proof_context {
                seed ^= U256::from(1);
            }
            returned(Round::getProofContextCall::abi_encode_returns(
                &Round::getProofContextReturn {
                    seed,
                    deadline: request.deadline,
                    fulfilled: request.fulfilled,
                    refunded: request.refunded,
                },
            ))
        } else if selector == Round::checkRoundSignatureCall::SELECTOR && self.fail_round_check {
            Err((-32000, "the node failed the call".into()))
        } else if selector == Round::checkRoundSignatureCall::SELECTOR {
            match Round::checkRoundSignatureCall::abi_decode(data) {
                Ok(call) => returned(Round::checkRoundSignatureCall::abi_encode_returns(
                    &(call.beaconId < book.beacons
                        && call.signature[..] == round_signature(call.round)),
                )),
                Err(error) => Err(failed(error)),
            }
        } else if selector == Round::roundRandomnessCall::SELECTOR {
            match Round::roundRandomnessCall::abi_decode(data) {
                Ok(call) => returned(Round::roundRandomnessCall::abi_encode_returns(
                    &book
                        .verified
                        .get(&(call.beaconId, call.round))
                        .filter(|(block, _)| *block <= self.head)
                        .map_or(B256::ZERO, |(_, randomness)| *randomness),
                )),
                Err(error) => Err(failed(error)),
            }
        } else if selector == Round::beaconScheduleCall::SELECTOR {
            returned(Round::beaconScheduleCall::abi_encode_returns(
                &Round::beaconScheduleReturn {
                    beaconId: book.beacon,
                    since: T0,
                    nextBeaconId: 0,
                    nextFrom: 0,
                },
            ))
        } else if [
            C::getRequestCall::SELECTOR,
            C::epochRegistryCall::SELECTOR,
            C::confirmationBlocksCall::SELECTOR,
            C::getProofContextCall::SELECTOR,
            C::fulfillRandomnessCall::SELECTOR,
            C::fulfillRandomnessBatchCall::SELECTOR,
        ]
        .contains(&selector)
        {
            Err(revert("a round coordinator has no such function"))
        } else {
            return None;
        })
    }
    /// The randomness of `round` of `beacon` for a fulfillment that carries `signature`: the round's cached randomness
    /// when the coordinator has verified the round (the signature is not read then), or the signature's, when it is the
    /// round's (`round_signature`) of a registered beacon, with whether this verifies the round now.
    fn round_randomness(
        &self,
        beacon: u8,
        round: u64,
        signature: &[u8],
    ) -> Result<(B256, bool), Fault> {
        if let Some(cached) = self.cached_round(beacon, round) {
            return Ok((cached, false));
        }
        let book = self.round.as_ref().expect("a round coordinator");
        if beacon < book.beacons && signature == round_signature(round) {
            Ok((round_randomness(signature), true))
        } else {
            Err(revert("InvalidRoundSignature"))
        }
    }
    /// The seed of request `id` over its round's `randomness`, as the round coordinator's `_seed` computes it:
    /// `keccak256(abi.encode(SEED_DOMAIN, chainid, coordinator, keyHash, id, consumer, clientSeed, mappingHash,
    /// requestBlock, beaconId, round, randomness))`.
    pub fn round_seed(&self, id: u64, randomness: B256) -> Option<U256> {
        use alloy_sol_types::{SolType, sol_data};
        let request = self.round_view(id)?;
        type Inputs = (
            sol_data::FixedBytes<32>,
            sol_data::Uint<256>,
            sol_data::Address,
            sol_data::FixedBytes<32>,
            sol_data::Uint<256>,
            sol_data::Address,
            sol_data::FixedBytes<32>,
            sol_data::FixedBytes<32>,
            sol_data::Uint<64>,
            sol_data::Uint<8>,
            sol_data::Uint<64>,
            sol_data::FixedBytes<32>,
        );
        Some(U256::from_be_bytes(
            keccak256(Inputs::abi_encode(&(
                keccak256("D20_VRF_ROUND_SEED"),
                U256::from(self.chain_id),
                self.coordinator,
                self.round_key_hash(),
                U256::from(id),
                request.consumer,
                request.clientSeed,
                request.mappingHash,
                request.requestBlock,
                request.beaconId,
                request.round,
                randomness,
            )))
            .0,
        ))
    }
    /// What a round coordinator's fulfillment would do in a block of time `now`, or why it reverts, checked as the
    /// contract checks it before it changes anything: each member's state, its round (cached, or verified now from the
    /// signature the transaction carries) and its proof's seed, and with `gas` (the transaction's gas limit and its L1
    /// component) the guard the coordinator runs behind its proxy: what is left after the L1 component, less the 64th the
    /// proxy keeps, must cover the callback reserve, each served member's callback and overhead, and each round it
    /// verifies. `None` for calldata that is no round fulfillment.
    fn round_fulfillment(
        &self,
        data: &[u8],
        gas: Option<(u64, u64)>,
        now: u64,
    ) -> Option<Result<RoundPlan, Fault>> {
        const CALLBACK_RESERVE: u64 = 140_000;
        const VERIFICATION: u64 = 400_000 + 400_000 / 63 + 5_000;
        let member_gas = |limit: u32| u64::from(limit) + u64::from(limit) / 63 + 400_000;
        let guard = |need: u64| match gas {
            Some((limit, l1)) if limit.saturating_sub(l1) * 63 / 64 < need => {
                Err(revert("InsufficientCallbackGas"))
            }
            _ => Ok(()),
        };
        let open = |id: U256| -> Result<(u64, RoundRequest, Option<u8>), Fault> {
            let id = u64::try_from(id).unwrap_or(0);
            let request = self
                .round_view(id)
                .ok_or_else(|| revert("UnknownRequest"))?;
            let skipped = if request.fulfilled {
                Some(1)
            } else if request.refunded {
                Some(2)
            } else if now > request.deadline {
                Some(3)
            } else {
                None
            };
            Ok((id, request, skipped))
        };
        if let Ok(call) = Round::fulfillRandomnessCall::abi_decode(data) {
            return Some((|| {
                let (id, request, skipped) = open(call.requestId)?;
                match skipped {
                    Some(1) => return Err(revert("AlreadyFulfilled")),
                    Some(2) => return Err(revert("RequestRefunded")),
                    Some(_) => return Err(revert("RequestExpired")),
                    None => {}
                }
                if self.cached_round(request.beaconId, request.round).is_none() {
                    guard(member_gas(request.callbackGasLimit) + CALLBACK_RESERVE + VERIFICATION)?;
                }
                let (randomness, verified) =
                    self.round_randomness(request.beaconId, request.round, &call.roundSignature)?;
                if Some(call.proof.seed) != self.round_seed(id, randomness) {
                    return Err(revert("WrongSeed"));
                }
                Ok(RoundPlan {
                    serve: vec![id],
                    skip: Vec::new(),
                    verify: if verified {
                        vec![(request.beaconId, request.round, randomness)]
                    } else {
                        Vec::new()
                    },
                })
            })());
        }
        let call = Round::fulfillRandomnessBatchCall::abi_decode(data).ok()?;
        Some((|| {
            let count = call.ids.len();
            if count == 0 || count > 16 || count != call.proofs.len() || call.rounds.len() > count {
                return Err(revert("InvalidBatch"));
            }
            let listed = |beacon: u8, round: u64| {
                call.rounds
                    .iter()
                    .position(|listed| listed.beaconId == beacon && listed.round == round)
                    .ok_or_else(|| revert("RoundUnavailable"))
            };
            let mut plan = RoundPlan {
                serve: Vec::new(),
                skip: Vec::new(),
                verify: Vec::new(),
            };
            let mut need = CALLBACK_RESERVE;
            let mut budgeted = Vec::new();
            let mut served = Vec::new();
            for (id, proof) in call.ids.iter().zip(&call.proofs) {
                let (id, request, skipped) = open(*id)?;
                if let Some(reason) = skipped {
                    plan.skip.push((id, reason));
                    continue;
                }
                need += member_gas(request.callbackGasLimit);
                if self.cached_round(request.beaconId, request.round).is_none() {
                    let index = listed(request.beaconId, request.round)?;
                    if !budgeted.contains(&index) {
                        budgeted.push(index);
                        need += VERIFICATION;
                    }
                }
                served.push((id, request, proof.seed));
            }
            guard(need)?;
            for (id, request, seed) in served {
                let randomness = match self.cached_round(request.beaconId, request.round) {
                    Some(cached) => cached,
                    None => {
                        if let Some((_, _, randomness)) = plan
                            .verify
                            .iter()
                            .find(|(b, r, _)| *b == request.beaconId && *r == request.round)
                        {
                            *randomness
                        } else {
                            let index = listed(request.beaconId, request.round)?;
                            let (randomness, _) = self.round_randomness(
                                request.beaconId,
                                request.round,
                                &call.rounds[index].signature,
                            )?;
                            plan.verify
                                .push((request.beaconId, request.round, randomness));
                            randomness
                        }
                    }
                };
                if Some(seed) != self.round_seed(id, randomness) {
                    return Err(revert("WrongSeed"));
                }
                plan.serve.push(id);
            }
            Ok(plan)
        })())
    }
    /// The committer, which is not the keeper under test, serves request `id` in the current block: one transaction of
    /// its own, which the keeper sees only as the request settled and the committer's nonce one higher.
    pub fn primary_serves(&mut self, id: u64) {
        let (committer, block) = (self.committer, self.head);
        self.fulfil(id, block, committer);
        self.primary_nonce += 1;
    }
    /// Request `id` is fulfilled in block `block` by `submitter`: its state, and the logs of the coordinator.
    pub fn fulfil(&mut self, id: u64, block: u64, submitter: Address) -> Vec<Value> {
        let packet = vec![(id % 251) as u8; 416];
        let stored = self.requests.get_mut(&id).expect("a request of the chain");
        stored.fulfilled = true;
        stored.delivered = true;
        stored.packet = Some(packet.clone());
        let randomness = keccak256(U256::from(id).to_be_bytes::<32>());
        let fulfilled = C::RandomnessFulfilled {
            requestId: U256::from(id),
            randomness,
            submitter,
        }
        .encode_log_data();
        let evidence = FulfillmentEvidence {
            requestId: U256::from(id),
            transcriptHash: keccak256(&packet),
            packet: Bytes::from(packet),
        }
        .encode_log_data();
        for log in [&fulfilled, &evidence] {
            self.emit(
                self.coordinator,
                block,
                log.topics().to_vec(),
                log.data.to_vec(),
            );
        }
        vec![json!({
            "address": self.coordinator,
            "topics": fulfilled.topics(),
            "data": fulfilled.data,
        })]
    }
    /// A log of `address` in block `block`, with `topics` (the first is the event) and `data`.
    pub fn emit(&mut self, address: Address, block: u64, topics: Vec<B256>, data: Vec<u8>) {
        let index = self
            .logs
            .iter()
            .filter(|log| log["blockNumber"] == quantity(block.into()))
            .count();
        let transaction = keccak256(format!("transaction {block} {index}"));
        self.logs.push(json!({
            "address": address,
            "topics": topics,
            "data": Bytes::from(data),
            "blockNumber": quantity(block.into()),
            "blockHash": self.block_hash(block),
            "transactionHash": transaction,
            "transactionIndex": quantity(index as u128),
            "logIndex": quantity(index as u128),
            "removed": false,
        }));
    }
    /// The registry's record of an epoch published at block `block`.
    pub fn publish_epoch(&mut self, epoch: u64, block: u64) {
        let hash = keccak256(format!("epoch {epoch} {block}"));
        self.epochs.insert(
            epoch,
            EpochRecord {
                epochHash: hash,
                catalogHash: self.catalog_hash,
                anchorHash: keccak256(b"anchor"),
                source: 0,
                queryHash: keccak256(self.canonical_request().as_bytes()),
                dataHash: keccak256(b"data"),
                attestationHash: keccak256(b"attestation"),
                signedAt: U256::from(self.time(block)),
                committedBlock: block,
            },
        );
        let log = EpochCommitted {
            epochId: epoch,
            epochHash: hash,
            packet: Bytes::from(vec![(epoch % 251) as u8; 448]),
        }
        .encode_log_data();
        self.emit(
            self.registry,
            block,
            log.topics().to_vec(),
            log.data.to_vec(),
        );
    }
    /// The request as the coordinator reads it: the epoch and target resolve when the epoch is published.
    pub fn view(&self, id: u64) -> Option<Request> {
        let stored = self.requests.get(&id)?;
        let epoch = self.epochs.get(&stored.epoch_id);
        Some(Request {
            consumer: stored.consumer,
            callbackGasLimit: stored.callback_gas,
            requestBlock: stored.request_block,
            targetBlock: epoch.map_or(0, |record| {
                stored.request_block.max(record.committedBlock + 1)
            }),
            deadline: self.time(stored.request_block) + crate::config::RESPONSE_TIMEOUT_SECONDS,
            refundAddress: stored.consumer,
            clientSeed: keccak256(format!("seed {id}")),
            mappingHash: B256::ZERO,
            blockHash: B256::ZERO,
            randomness: B256::ZERO,
            proofHash: stored.packet.as_ref().map_or(B256::ZERO, keccak256),
            transcriptHash: B256::ZERO,
            fulfilled: stored.fulfilled,
            delivered: stored.delivered,
            refunded: stored.refunded,
            epochId: stored.epoch_id,
            epochHash: epoch.map_or(B256::ZERO, |record| record.epochHash),
        })
    }
    /// Drop what the wallet has sent and not had mined, as a sequencer that never includes it does.
    pub fn drop_queue(&mut self) {
        self.queue.clear();
    }
    /// Whether the mined transaction `hash` succeeded, when it is mined.
    pub fn receipt_status(&self, hash: &B256) -> Option<bool> {
        self.receipts.get(hash).map(|mined| mined.success)
    }
    /// The up-front cost of each transaction waiting in the sequencer's queue, gas limit times max fee per gas plus value,
    /// in nonce order: what the wallet had to hold to send it.
    pub fn queued_up_front(&self) -> Vec<u128> {
        let mut queued: Vec<&Pending> = self.queue.iter().collect();
        queued.sort_by_key(|pending| pending.nonce);
        queued
            .into_iter()
            .map(|pending| {
                u128::from(pending.envelope.gas_limit()) * pending.envelope.max_fee_per_gas()
                    + u128::try_from(pending.envelope.value()).unwrap()
            })
            .collect()
    }
    /// Mine one block with every transaction the wallet has sent, in nonce order.
    pub fn include(&mut self) {
        self.history.insert(self.head, self.ledger());
        self.head += 1;
        let block = self.head;
        let mut queue = std::mem::take(&mut self.queue);
        queue.sort_by_key(|pending| pending.nonce);
        for pending in queue {
            let (success, logs) = self.execute(&pending.envelope, block);
            self.receipts.insert(
                pending.hash,
                Mined {
                    block,
                    success,
                    logs,
                },
            );
            self.nonce = pending.nonce + 1;
            self.mined.push((block, pending));
        }
    }
    fn log(&self, event: B256, topics: &[B256], data: Vec<u8>) -> Value {
        let mut all = vec![event];
        all.extend_from_slice(topics);
        json!({"address": self.coordinator, "topics": all, "data": Bytes::from(data)})
    }
    fn serve(&mut self, id: U256, block: u64) -> Option<Vec<Value>> {
        let id = u64::try_from(id).ok()?;
        let stored = self.requests.get(&id)?;
        if stored.fulfilled || stored.refunded {
            return None;
        }
        let keeper = self.keeper;
        Some(self.fulfil(id, block, keeper))
    }
    /// What a mined transaction does: whether it succeeds and the logs it emits.
    fn execute(&mut self, envelope: &TxEnvelope, block: u64) -> (bool, Vec<Value>) {
        let input = envelope.input().to_vec();
        let to = envelope.to().unwrap_or_default();
        if to == self.coordinator
            && self.round.is_some()
            && let Some(outcome) = self.round_fulfillment(
                &input,
                Some((envelope.gas_limit(), (self.l1_gas)(input.len()))),
                self.time(block),
            )
        {
            let Ok(plan) = outcome else {
                return (false, vec![]);
            };
            let mut logs = Vec::new();
            for (beacon, round, _) in plan.verify {
                self.verify_round(beacon, round, block);
            }
            for (id, reason) in plan.skip {
                logs.push(self.log(
                    C::FulfillmentSkipped::SIGNATURE_HASH,
                    &[B256::from(U256::from(id).to_be_bytes::<32>())],
                    U256::from(reason).to_be_bytes::<32>().to_vec(),
                ));
            }
            for id in plan.serve {
                let keeper = self.keeper;
                logs.extend(self.fulfil(id, block, keeper));
            }
            return (true, logs);
        }
        if to == self.coordinator {
            if let Ok(call) = C::fulfillRandomnessCall::abi_decode(&input) {
                return match self.serve(call.id, block) {
                    Some(logs) => (true, logs),
                    None => (false, vec![]),
                };
            }
            if let Ok(call) = C::fulfillRandomnessBatchCall::abi_decode(&input) {
                let mut logs = Vec::new();
                for id in call.ids {
                    let skipped = match self.requests.get(&u64::try_from(id).unwrap_or(0)) {
                        Some(stored) if stored.fulfilled => Some(1u8),
                        Some(stored) if stored.refunded => Some(2),
                        _ => None,
                    };
                    match skipped {
                        Some(reason) => logs.push(self.log(
                            C::FulfillmentSkipped::SIGNATURE_HASH,
                            &[B256::from(id.to_be_bytes::<32>())],
                            U256::from(reason).to_be_bytes::<32>().to_vec(),
                        )),
                        None => logs.extend(self.serve(id, block).into_iter().flatten()),
                    }
                }
                return (true, logs);
            }
        } else if to == self.registry {
            if let Ok(call) = E::commitEpochCall::abi_decode(&input) {
                if self.epochs.contains_key(&call.epochId) {
                    return (false, vec![]);
                }
                self.publish_epoch(call.epochId, block);
                return (true, vec![]);
            }
        } else if envelope.value() > U256::ZERO {
            let value = envelope.value();
            let balance = self.balances.entry(self.keeper).or_default();
            *balance = balance.saturating_sub(value);
            *self.balances.entry(to).or_default() += value;
        }
        (true, vec![])
    }
    pub fn role(&self, address: Address) -> String {
        if address == self.coordinator {
            "coordinator".into()
        } else if address == self.registry {
            "registry".into()
        } else if address == self.keeper {
            "keeper".into()
        } else if address == self.committer {
            "primary".into()
        } else if address == self.fee_recipient {
            "fee_recipient".into()
        } else if address == NODE_INTERFACE {
            "node_interface".into()
        } else if address == self.coordinator_implementation {
            "coordinator_implementation".into()
        } else if address == self.registry_implementation {
            "registry_implementation".into()
        } else {
            format!("{address:#x}")
        }
    }
    /// A block tag as the trace names it. A named tag is as the keeper sent it. A number is its distance from the
    /// latest head and from the finalized head (`#latest-4/finalized-1`), so that the trace does not depend on how far
    /// the script mined before the call, and says which head the keeper took the number from.
    fn tag(&self, tag: &Value) -> String {
        let text = tag.as_str().unwrap_or("?");
        match text
            .strip_prefix("0x")
            .map(|digits| u64::from_str_radix(digits, 16))
        {
            Some(Ok(number)) => self.number(number),
            _ => text.to_owned(),
        }
    }
    /// A block number as the trace names it: its distance from both heads.
    fn number(&self, number: u64) -> String {
        format!(
            "#latest{}/finalized{}",
            distance(number, self.head),
            distance(number, self.finalized_head())
        )
    }
    /// The block a tag names, or `None` for a block this chain has not mined.
    fn block_of(&self, tag: &Value) -> Option<u64> {
        match tag.as_str()? {
            "latest" | "pending" => Some(self.head),
            "finalized" => Some(self.finalized_head()),
            "safe" => Some(self.safe_head()),
            "earliest" => Some(0),
            text => {
                let number = u64::from_str_radix(text.strip_prefix("0x")?, 16).ok()?;
                (number <= self.head).then_some(number)
            }
        }
    }
    fn block_json(&self, number: u64) -> Value {
        json!({
            "number": quantity(number.into()),
            "hash": self.block_hash(number),
            "parentHash": self.block_hash(number.saturating_sub(1)),
            "timestamp": quantity(self.time(number).into()),
            "baseFeePerGas": quantity(self.base_fee_at(number)),
        })
    }
    fn code_of(&self, target: Address) -> Option<Bytes> {
        if self.round.is_some() && target == self.registry {
            None
        } else if target == self.coordinator || target == self.registry {
            Some(self.proxy_code.clone())
        } else {
            self.implementation_code.get(&target).cloned()
        }
    }
    fn implementation_of(&self, proxy: Address) -> Option<Address> {
        if proxy == self.coordinator {
            Some(self.coordinator_implementation)
        } else if proxy == self.registry {
            Some(self.registry_implementation)
        } else {
            None
        }
    }
    fn coordinator_call(&self, data: &[u8]) -> Result<Value, Fault> {
        let selector: [u8; 4] = data
            .get(..4)
            .and_then(|s| s.try_into().ok())
            .unwrap_or_default();
        if let Some(book) = &self.round
            && let Some(answer) = self.round_call(book, selector, data)
        {
            return answer;
        }
        if selector == C::getRequestCall::SELECTOR {
            let call = C::getRequestCall::abi_decode(data).map_err(|e| revert(&e.to_string()))?;
            let request = u64::try_from(call.id)
                .ok()
                .and_then(|id| self.view(id))
                .unwrap_or_default();
            returned(C::getRequestCall::abi_encode_returns(&request))
        } else if selector == C::epochRegistryCall::SELECTOR {
            returned(C::epochRegistryCall::abi_encode_returns(&self.registry))
        } else if selector == C::protocolConfigurationHashCall::SELECTOR {
            returned(C::protocolConfigurationHashCall::abi_encode_returns(
                &self.protocol_hash,
            ))
        } else if selector == C::confirmationBlocksCall::SELECTOR {
            returned(C::confirmationBlocksCall::abi_encode_returns(
                &self.confirmations,
            ))
        } else if selector == C::publicKeyXCall::SELECTOR {
            returned(C::publicKeyXCall::abi_encode_returns(&self.public_key[0]))
        } else if selector == C::publicKeyYCall::SELECTOR {
            returned(C::publicKeyYCall::abi_encode_returns(&self.public_key[1]))
        } else if selector == C::feeRecipientCall::SELECTOR {
            returned(C::feeRecipientCall::abi_encode_returns(&self.fee_recipient))
        } else if selector == C::nextRequestIdCall::SELECTOR {
            returned(C::nextRequestIdCall::abi_encode_returns(&U256::from(
                self.next_request,
            )))
        } else if selector == C::requestFeePaidCall::SELECTOR {
            let call =
                C::requestFeePaidCall::abi_decode(data).map_err(|e| revert(&e.to_string()))?;
            let paid = u64::try_from(call.requestId)
                .ok()
                .and_then(|id| self.requests.get(&id))
                .map_or(0, |stored| stored.fee_paid);
            returned(C::requestFeePaidCall::abi_encode_returns(&U256::from(paid)))
        } else if selector == Indexed::initialFeeRecipientCall::SELECTOR {
            returned(Indexed::initialFeeRecipientCall::abi_encode_returns(
                &self.fee_recipient,
            ))
        } else if selector == Indexed::initialMinFeeCall::SELECTOR {
            returned(Indexed::initialMinFeeCall::abi_encode_returns(&U256::from(
                1_000_000_000_000_000u64,
            )))
        } else if selector == Indexed::keyHashCall::SELECTOR {
            returned(Indexed::keyHashCall::abi_encode_returns(&self.key_hash()))
        } else if selector == Indexed::getMappingCall::SELECTOR {
            returned(Indexed::getMappingCall::abi_encode_returns(&MapSpec {
                operation: 0,
                lower: U256::ZERO,
                upper: U256::ZERO,
                count: 0,
                population: 0,
            }))
        } else if selector == C::getProofContextCall::SELECTOR {
            let call =
                C::getProofContextCall::abi_decode(data).map_err(|e| revert(&e.to_string()))?;
            let id = u64::try_from(call.id).unwrap_or(0);
            let request = self.view(id).ok_or_else(|| revert("UnknownRequest"))?;
            // The proof input exists from the target block plus the confirmations on.
            if request.epochHash == B256::ZERO
                || self.head < request.targetBlock + u64::from(self.confirmations)
            {
                return Err(revert("NotReady"));
            }
            returned(C::getProofContextCall::abi_encode_returns(
                &C::getProofContextReturn {
                    seed: self.proof_seed(id),
                    deadline: request.deadline,
                    fulfilled: request.fulfilled,
                    refunded: request.refunded,
                },
            ))
        } else if selector == C::getPendingRequestIdsCall::SELECTOR {
            let call = C::getPendingRequestIdsCall::abi_decode(data)
                .map_err(|e| revert(&e.to_string()))?;
            let from = u64::try_from(call.fromId).unwrap_or(0);
            let limit = u64::try_from(call.limit).unwrap_or(0).min(256);
            let end = (from + limit).min(self.next_request);
            let ids: Vec<U256> = (from..end)
                .filter(|id| {
                    self.requests
                        .get(id)
                        .is_some_and(|stored| !stored.fulfilled && !stored.refunded)
                })
                .map(U256::from)
                .collect();
            returned(C::getPendingRequestIdsCall::abi_encode_returns(
                &C::getPendingRequestIdsReturn {
                    ids,
                    nextCursor: U256::from(end),
                },
            ))
        } else {
            Err(revert("unscripted coordinator call"))
        }
    }
    fn registry_call(&self, data: &[u8]) -> Result<Value, Fault> {
        let selector: [u8; 4] = data
            .get(..4)
            .and_then(|s| s.try_into().ok())
            .unwrap_or_default();
        let failed = |e: alloy_sol_types::Error| revert(&e.to_string());
        if selector == E::committerCall::SELECTOR {
            returned(E::committerCall::abi_encode_returns(&self.committer))
        } else if selector == E::isBackupCommitterCall::SELECTOR {
            let call = E::isBackupCommitterCall::abi_decode(data).map_err(failed)?;
            returned(E::isBackupCommitterCall::abi_encode_returns(
                &self.backups.contains(&call.account),
            ))
        } else if selector == E::catalogHashCall::SELECTOR {
            returned(E::catalogHashCall::abi_encode_returns(&self.catalog_hash))
        } else if selector == E::nextEpochToPrepareCall::SELECTOR {
            let call = E::nextEpochToPrepareCall::abi_decode(data).map_err(failed)?;
            let number = u64::try_from(call.number).unwrap_or(u64::MAX);
            returned(E::nextEpochToPrepareCall::abi_encode_returns(
                &self.epoch_of(number),
            ))
        } else if selector == E::epochStartCall::SELECTOR {
            let call = E::epochStartCall::abi_decode(data).map_err(failed)?;
            returned(E::epochStartCall::abi_encode_returns(
                &self.epoch_start(call.epochId),
            ))
        } else if selector == Indexed::firstEpochStartCall::SELECTOR {
            returned(Indexed::firstEpochStartCall::abi_encode_returns(
                &FIRST_EPOCH_START,
            ))
        } else if let Some(index) = [
            Indexed::hyperliquidSignerCall::SELECTOR,
            Indexed::ethereumBlockSignerCall::SELECTOR,
            Indexed::btcTradeSignerCall::SELECTOR,
            Indexed::ethTradeSignerCall::SELECTOR,
        ]
        .iter()
        .position(|signer| *signer == selector)
        {
            // Each catalog signer is an address of its own.
            returned(Indexed::hyperliquidSignerCall::abi_encode_returns(
                &Address::repeat_byte(0x51 + index as u8),
            ))
        } else if selector == E::catalogAtCall::SELECTOR {
            returned(E::catalogAtCall::abi_encode_returns(&E::catalogAtReturn {
                hash: self.catalog_hash,
                recipes: vec![BEACON_RECIPE],
                signers: vec![Address::repeat_byte(0x51)],
            }))
        } else if selector == E::sourceCountAtCall::SELECTOR {
            returned(E::sourceCountAtCall::abi_encode_returns(&U256::from(1)))
        } else if selector == E::getEpochCall::SELECTOR {
            let call = E::getEpochCall::abi_decode(data).map_err(failed)?;
            let record = self
                .epochs
                .get(&call.epochId)
                .cloned()
                .unwrap_or(EpochRecord {
                    epochHash: B256::ZERO,
                    catalogHash: B256::ZERO,
                    anchorHash: B256::ZERO,
                    source: 0,
                    queryHash: B256::ZERO,
                    dataHash: B256::ZERO,
                    attestationHash: B256::ZERO,
                    signedAt: U256::ZERO,
                    committedBlock: 0,
                });
            returned(E::getEpochCall::abi_encode_returns(&record))
        } else if selector == E::getEpochFallbackSelectionCall::SELECTOR {
            let call = E::getEpochFallbackSelectionCall::abi_decode(data).map_err(failed)?;
            let request = self.canonical_request();
            returned(E::getEpochFallbackSelectionCall::abi_encode_returns(
                &EpochSelection {
                    source: 0,
                    recipe: BEACON_RECIPE,
                    airnode: Address::ZERO,
                    selector: keccak256(format!("selector {}", call.epochId)),
                    queryHash: keccak256(request.as_bytes()),
                    canonicalRequest: request,
                },
            ))
        } else if selector == E::getRecipeCall::SELECTOR {
            let request = self.canonical_request();
            returned(E::getRecipeCall::abi_encode_returns(&E::getRecipeReturn {
                queryHash: keccak256(request.as_bytes()),
                canonicalRequest: request.clone(),
                template: Bytes::from_static(&[0x04, 0x01, 0x13]),
                body: request,
            }))
        } else if selector == E::beaconOfCall::SELECTOR {
            returned(E::beaconOfCall::abi_encode_returns(&Beacon {
                verifier: Address::repeat_byte(0xbb),
                genesis: BEACON_GENESIS,
                period: BEACON_PERIOD,
                chainHash: self.beacon_chain_hash,
                publicKey: Bytes::from(vec![7u8; 128]),
            }))
        } else if selector == E::verifyBeaconCall::SELECTOR {
            returned(E::verifyBeaconCall::abi_encode_returns(&true))
        } else {
            Err(revert("unscripted registry call"))
        }
    }
    fn node_interface_call(&self, data: &[u8]) -> Result<Value, Fault> {
        if self.fail_node_interface {
            return Err((-32000, "the NodeInterface is not served here".into()));
        }
        let call = NodeInterface::gasEstimateL1ComponentCall::abi_decode(data)
            .map_err(|e| revert(&e.to_string()))?;
        returned(
            NodeInterface::gasEstimateL1ComponentCall::abi_encode_returns(
                &NodeInterface::gasEstimateL1ComponentReturn {
                    gasEstimateForL1: (self.l1_gas)(call.data.len()),
                    baseFee: U256::from(self.base_fee_at(self.head)),
                    l1BaseFeeEstimate: U256::from(1_638_971u64),
                },
            ),
        )
    }
    /// One word of a call's arguments as a trace shows it: a small integer in decimal, or an address as its role.
    fn argument(&self, word: &[u8]) -> Option<String> {
        if word[..24].iter().all(|byte| *byte == 0) {
            Some(u64::from_be_bytes(word[24..].try_into().ok()?).to_string())
        } else if word[..12].iter().all(|byte| *byte == 0) {
            Some(self.role(Address::from_slice(&word[12..])))
        } else {
            None
        }
    }
    /// The arguments of a call: its words when each is a small integer or an address, otherwise the fingerprint of
    /// them all. Nothing for a call without arguments.
    fn arguments(&self, data: &[u8]) -> String {
        let Some(arguments) = data.get(4..).filter(|arguments| !arguments.is_empty()) else {
            return String::new();
        };
        // The one call whose argument is a block, which the trace places like a tag: where the keeper took it from.
        if let Ok(call) = E::nextEpochToPrepareCall::abi_decode(data)
            && let Ok(number) = u64::try_from(call.number)
        {
            return format!(" block={}", self.number(number));
        }
        let words: Option<Vec<String>> = (arguments.len() % 32 == 0 && arguments.len() <= 128)
            .then(|| {
                arguments
                    .chunks(32)
                    .map(|word| self.argument(word))
                    .collect::<Option<_>>()
            })
            .flatten();
        match words {
            Some(words) => format!(" args=[{}]", words.join(",")),
            None => format!(" args={}", fingerprint(arguments)),
        }
    }
    /// What a call or an estimate carries besides its target and data, by key: who it is from, how much it sends.
    fn fields(&self, call: &Value) -> String {
        let Some(object) = call.as_object() else {
            return String::new();
        };
        let mut keys: Vec<&String> = object
            .keys()
            .filter(|key| !["to", "data"].contains(&key.as_str()))
            .collect();
        keys.sort();
        keys.into_iter()
            .map(|key| {
                let value = &object[key];
                let shown = match (key.as_str(), value.as_str()) {
                    ("from", Some(_)) => self.role(address(value)),
                    (_, Some(text)) => text.to_owned(),
                    _ => value.to_string(),
                };
                format!(" {key}={shown}")
            })
            .collect()
    }
    /// What a transaction's data asks for, for an estimate and a send. A proof is random, so a fulfillment is named by
    /// the ids it serves and the seeds it proves, and an epoch commit by its epoch and the packet it carries.
    fn payload(&self, data: &[u8]) -> String {
        let name = selector_text(data);
        let seed = |proof: &crate::abi::VrfProof| fingerprint(&proof.seed.to_be_bytes::<32>());
        if let Ok(call) = C::fulfillRandomnessCall::abi_decode(data) {
            format!("{name} id={} seed={}", call.id, seed(&call.proof))
        } else if let Ok(call) = C::fulfillRandomnessBatchCall::abi_decode(data) {
            let ids: Vec<String> = call.ids.iter().map(U256::to_string).collect();
            let seeds: Vec<String> = call.proofs.iter().map(seed).collect();
            format!("{name} ids=[{}] seeds=[{}]", ids.join(","), seeds.join(","))
        } else if let Ok(call) = Round::fulfillRandomnessCall::abi_decode(data) {
            format!(
                "{name} id={} seed={} signature={}",
                call.requestId,
                fingerprint(&call.proof.seed.to_be_bytes::<32>()),
                fingerprint(&call.roundSignature)
            )
        } else if let Ok(call) = Round::fulfillRandomnessBatchCall::abi_decode(data) {
            let rounds: Vec<String> = call
                .rounds
                .iter()
                .map(|listed| {
                    format!(
                        "{}:{}:{}",
                        listed.beaconId,
                        listed.round,
                        fingerprint(&listed.signature)
                    )
                })
                .collect();
            let ids: Vec<String> = call.ids.iter().map(U256::to_string).collect();
            let seeds: Vec<String> = call
                .proofs
                .iter()
                .map(|proof| fingerprint(&proof.seed.to_be_bytes::<32>()))
                .collect();
            format!(
                "{name} rounds=[{}] ids=[{}] seeds=[{}]",
                rounds.join(","),
                ids.join(","),
                seeds.join(",")
            )
        } else if let Ok(call) = E::commitEpochCall::abi_decode(data) {
            let packet = call.attestation;
            format!(
                "{name} epoch={} timestamp={} data={} signature={}",
                call.epochId,
                packet.timestamp,
                String::from_utf8_lossy(&packet.data),
                fingerprint(&packet.signature)
            )
        } else if let Ok(call) = E::commitEpochFallbackCall::abi_decode(data) {
            let packet = call.attestation;
            format!(
                "{name} epoch={} attempt={} timestamp={} data={} signature={}",
                call.epochId,
                call.attempt,
                packet.timestamp,
                String::from_utf8_lossy(&packet.data),
                fingerprint(&packet.signature)
            )
        } else {
            format!("{name}{}", self.arguments(data))
        }
    }
    /// The addresses a log filter names, as one address or a list.
    fn log_addresses(&self, filter: &Value) -> Vec<Address> {
        match &filter["address"] {
            Value::Array(list) => list.iter().map(address).collect(),
            Value::Null => Vec::new(),
            one => vec![address(one)],
        }
    }
    /// How one call is recorded: everything it asks, and nothing it changes or reads from the chain's state.
    pub fn record(&self, method: &str, params: &Value) -> Call {
        let plain = |extra: String| Call(format!("{method} {extra}").trim_end().to_owned());
        match method {
            "eth_chainId" => plain(String::new()),
            "eth_getCode" => plain(format!(
                "{} {}",
                self.tag(&params[1]),
                self.role(address(&params[0]))
            )),
            "eth_getStorageAt" => {
                let at = params[1].as_str().unwrap_or("?").to_ascii_lowercase();
                let slot_name = if at == IMPLEMENTATION_SLOT {
                    "eip1967.implementation".to_owned()
                } else {
                    at
                };
                plain(format!(
                    "{} {} slot={slot_name}",
                    self.tag(&params[2]),
                    self.role(address(&params[0]))
                ))
            }
            "eth_getBlockByNumber" => {
                let full = params.get(1).and_then(Value::as_bool);
                plain(format!(
                    "{} full={}",
                    self.tag(&params[0]),
                    full.map_or("?".to_owned(), |full| full.to_string())
                ))
            }
            "eth_getBalance" | "eth_getTransactionCount" => plain(format!(
                "{} {}",
                self.tag(&params[1]),
                self.role(address(&params[0]))
            )),
            "eth_feeHistory" => {
                let blocks: usize = usize::from_str_radix(
                    params[0].as_str().unwrap_or("0x1").trim_start_matches("0x"),
                    16,
                )
                .unwrap_or(1);
                let percentiles = params.get(2).cloned().unwrap_or(Value::Null);
                plain(format!(
                    "{} blocks={blocks} percentiles={percentiles}",
                    self.tag(&params[1])
                ))
            }
            "eth_getLogs" => {
                let filter = &params[0];
                let names: Vec<String> = self
                    .log_addresses(filter)
                    .into_iter()
                    .map(|log| self.role(log))
                    .collect();
                let topics = match filter.get("topics") {
                    Some(topics) => format!(" topics={topics}"),
                    None => String::new(),
                };
                plain(format!(
                    "from={} to={} addresses=[{}]{topics}",
                    self.tag(&filter["fromBlock"]),
                    self.tag(&filter["toBlock"]),
                    names.join(",")
                ))
            }
            "eth_call" => {
                let data = bytes(&params[0]["data"]);
                plain(format!(
                    "{} {} {}{}{}",
                    self.tag(&params[1]),
                    self.role(address(&params[0]["to"])),
                    selector_text(&data),
                    self.fields(&params[0]),
                    self.arguments(&data)
                ))
            }
            "eth_estimateGas" => {
                let to = params[0]["to"].as_str().map(|_| address(&params[0]["to"]));
                let data = params[0].get("data").map(bytes).unwrap_or_default();
                let target = to.map_or("create".to_owned(), |to| self.role(to));
                let what = if data.is_empty() {
                    "transfer".to_owned()
                } else {
                    format!("{} bytes={}", self.payload(&data), data.len())
                };
                plain(format!("- {target} {what}{}", self.fields(&params[0])))
            }
            "eth_sendRawTransaction" => {
                let raw = bytes(&params[0]);
                let envelope = TxEnvelope::decode_2718(&mut raw.as_slice())
                    .expect("the keeper signs well-formed transactions");
                let to = envelope.to().unwrap_or_default();
                let carries = if envelope.input().is_empty() {
                    Self::label(&envelope)
                } else {
                    format!(
                        "{} bytes={}",
                        self.payload(envelope.input()),
                        envelope.input().len()
                    )
                };
                // A legacy or access-list transaction has one gas price; the others a fee cap and a tip.
                let price = match envelope.gas_price() {
                    Some(price) => format!("gasPrice={price}"),
                    None => format!(
                        "maxFee={} maxPriority={}",
                        envelope.max_fee_per_gas(),
                        envelope.max_priority_fee_per_gas().unwrap_or_default()
                    ),
                };
                plain(format!(
                    "- {} {carries} type={} chain={} nonce={} gas={} {price} value={}",
                    self.role(to),
                    envelope.ty(),
                    envelope
                        .chain_id()
                        .map_or("none".to_owned(), |id| id.to_string()),
                    envelope.nonce(),
                    envelope.gas_limit(),
                    envelope.value()
                ))
            }
            "eth_getTransactionReceipt" => {
                let hash: B256 =
                    serde_json::from_value(params[0].clone()).expect("transaction hash");
                plain(
                    self.sent
                        .get(&hash)
                        .map_or("unknown", String::as_str)
                        .to_owned(),
                )
            }
            _ => plain(format!("UNSCRIPTED {params}")),
        }
    }
    /// What a transaction without data is called: a sweep when it carries value, a cancellation when it does not; with
    /// data, the call it makes.
    fn label(envelope: &TxEnvelope) -> String {
        if envelope.input().is_empty() {
            if envelope.value() > U256::ZERO {
                "sweep".to_owned()
            } else {
                "cancel".to_owned()
            }
        } else {
            selector_text(envelope.input())
        }
    }
    /// Answer one call, and say how it is recorded.
    pub fn handle(&mut self, method: &str, params: &Value) -> (Call, Result<Value, Fault>) {
        let call = self.record(method, params);
        let answer = self.answer(method, params);
        (call, answer)
    }
    /// Answer one call.
    fn answer(&mut self, method: &str, params: &Value) -> Result<Value, Fault> {
        match method {
            "eth_chainId" => Ok(quantity(self.chain_id.into())),
            "eth_getCode" => Ok(json!(self.code_of(address(&params[0])).unwrap_or_default())),
            "eth_getStorageAt" => {
                let slot = self
                    .implementation_of(address(&params[0]))
                    .map_or(B256::ZERO, |implementation| {
                        B256::left_padding_from(implementation.as_slice())
                    });
                Ok(word(slot))
            }
            "eth_getBlockByNumber" => Ok(self
                .block_of(&params[0])
                .map_or(Value::Null, |n| self.block_json(n))),
            "eth_getBalance" => {
                let who = address(&params[0]);
                let block = self.block_of(&params[1]);
                let balance = self.read_at(block, |chain| {
                    chain.balances.get(&who).copied().unwrap_or_default()
                });
                Ok(json!(format!("0x{balance:x}")))
            }
            "eth_getTransactionCount" => {
                let who = address(&params[0]);
                let block = self.block_of(&params[1]);
                let nonce = if who == self.keeper {
                    if params[1] == "pending" {
                        self.nonce + self.queue.len() as u64
                    } else {
                        self.read_at(block, |chain| chain.nonce)
                    }
                } else if who == self.committer {
                    self.read_at(block, |chain| chain.primary_nonce)
                } else {
                    0
                };
                Ok(quantity(nonce.into()))
            }
            "eth_feeHistory" => {
                let blocks: usize = usize::from_str_radix(
                    params[0].as_str().unwrap_or("0x1").trim_start_matches("0x"),
                    16,
                )
                .unwrap_or(1);
                let asked = params.get(2).and_then(Value::as_array).map_or(0, Vec::len);
                let reward: Vec<Value> = (0..blocks)
                    .map(|block| {
                        let tip = self.tips[block % self.tips.len()];
                        json!((0..asked).map(|_| quantity(tip)).collect::<Vec<_>>())
                    })
                    .collect();
                Ok(json!({"oldestBlock": quantity(0), "reward": reward}))
            }
            "eth_getLogs" => {
                let filter = &params[0];
                let addresses = self.log_addresses(filter);
                let from = self.block_of(&filter["fromBlock"]).unwrap_or(u64::MAX);
                let to = self.block_of(&filter["toBlock"]).unwrap_or(self.head);
                let wanted = |log: &&Value| {
                    let block = quantity_of(&log["blockNumber"]);
                    (from..=to).contains(&block)
                        && (addresses.is_empty() || addresses.contains(&address(&log["address"])))
                };
                Ok(json!(self.logs.iter().filter(wanted).collect::<Vec<_>>()))
            }
            "eth_call" => {
                let target = address(&params[0]["to"]);
                let data = bytes(&params[0]["data"]);
                let block = self.block_of(&params[1]);
                self.read_at(block, |chain| {
                    if target == chain.coordinator {
                        chain.coordinator_call(&data)
                    } else if target == chain.registry && chain.round.is_none() {
                        chain.registry_call(&data)
                    } else if target == NODE_INTERFACE {
                        chain.node_interface_call(&data)
                    } else {
                        Err(revert("no contract"))
                    }
                })
            }
            "eth_estimateGas" => {
                let to = params[0]["to"].as_str().map(|_| address(&params[0]["to"]));
                let data = params[0].get("data").map(bytes).unwrap_or_default();
                if data.is_empty() && self.fail_transfer_estimate {
                    return Err(revert("scripted estimate failure"));
                }
                if to == Some(self.coordinator) && self.round.is_some() {
                    match self.round_fulfillment(&data, None, self.time(self.head)) {
                        Some(Err(fault)) => return Err(fault),
                        Some(Ok(plan)) => {
                            let members = plan.serve.len() + plan.skip.len();
                            let gas = if Round::fulfillRandomnessCall::abi_decode(&data).is_ok() {
                                self.estimates.fulfill
                            } else {
                                self.estimates.batch_base
                                    + self.estimates.batch_member * members as u64
                            };
                            return Ok(quantity(gas.into()));
                        }
                        None => {}
                    }
                }
                let gas = if to == Some(self.coordinator) {
                    match C::fulfillRandomnessBatchCall::abi_decode(&data) {
                        Ok(batch) => {
                            self.estimates.batch_base
                                + self.estimates.batch_member * batch.ids.len() as u64
                        }
                        Err(_) => self.estimates.fulfill,
                    }
                } else if to == Some(self.registry) {
                    self.estimates.epoch
                } else {
                    self.estimates.transfer
                };
                Ok(quantity(gas.into()))
            }
            "eth_sendRawTransaction" => {
                let raw = bytes(&params[0]);
                let hash = keccak256(&raw);
                let envelope = TxEnvelope::decode_2718(&mut raw.as_slice())
                    .expect("the keeper signs well-formed transactions");
                let what = Self::label(&envelope);
                let gas = envelope.gas_limit();
                let nonce = envelope.nonce();
                let needed = self.estimates.transfer;
                if self.enforce_transfer_gas && envelope.input().is_empty() && gas < needed {
                    return Err((
                        -32000,
                        format!("intrinsic gas too low: have {gas}, want {needed}"),
                    ));
                }
                // As a node does: the wallet must hold the gas limit at the max fee per gas, and the value, up front.
                let up_front =
                    U256::from(gas) * U256::from(envelope.max_fee_per_gas()) + envelope.value();
                let balance = self.balances.get(&self.keeper).copied().unwrap_or_default();
                if up_front > balance {
                    return Err((
                        -32000,
                        format!(
                            "insufficient funds for gas * price + value: have {balance} want {up_front}"
                        ),
                    ));
                }
                self.sent.insert(hash, what.clone());
                self.sends.push((hash, what, gas, nonce));
                self.queue.retain(|pending| pending.nonce != nonce);
                self.queue.push(Pending {
                    nonce,
                    hash,
                    envelope,
                });
                Ok(word(hash))
            }
            "eth_getTransactionReceipt" => {
                let hash: B256 =
                    serde_json::from_value(params[0].clone()).expect("transaction hash");
                if self.hide_receipts {
                    return Ok(Value::Null);
                }
                Ok(self.receipts.get(&hash).map_or(Value::Null, |mined| {
                    json!({
                        "transactionHash": hash,
                        "blockHash": self.block_hash(mined.block),
                        "blockNumber": quantity(mined.block.into()),
                        "status": if mined.success { "0x1" } else { "0x0" },
                        "logs": mined.logs,
                    })
                }))
            }
            other => Err((-32601, format!("scripted chain: {other} is not scripted"))),
        }
    }
}

/// How an endpoint answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// As the chain does.
    Up,
    /// With an HTTP error and nothing else, to everything it is asked.
    Down,
    /// With HTTP 429 and nothing else, to everything it is asked: a provider's rate limit.
    RateLimited,
    /// As the chain does, except that a JSON-RPC batch of more than this many calls is refused with HTTP 500 and an error
    /// that names batches, as a provider's free plan refuses it; nothing of the refused batch is answered or done.
    BatchLimit(usize),
    /// As the chain does, except that the node refuses one method with this JSON-RPC error message.
    Refusing(&'static str, &'static str),
    /// As the chain does, except that the blocks from this number on have the hashes of another fork, in a block and
    /// in a receipt: a node that follows another chain than the sequencer's.
    Fork(u64),
    /// As the chain does, except that the hashes of the blocks are those the chain had after its first so many
    /// replacements: a node that has not seen the sequencer's later replacements yet.
    Unreplaced(usize),
    /// A drand relay that answers every round it has with a well-formed signature that is not the round's. An endpoint
    /// of the node in this mode answers as the chain does.
    Forging,
    /// As the chain answers, except that every `getRoundRequest` reverts (`UnknownRequest`): a load-balanced backend
    /// behind the others answers so at its own head for a request it has not seen yet.
    RevertingRoundRequests,
    /// As the chain answers, except that a read of the `finalized` block tag is refused with a JSON-RPC error: a
    /// provider that does not serve the tag.
    NoFinalized,
    /// As the chain answers, except that the `finalized` block tag is answered with the latest block: a provider that
    /// aliases the tag to `latest`.
    FinalizedLatest,
}
impl Chain {
    /// `result`, the chain's answer to `method`, as an endpoint in `mode` gives it.
    fn as_seen(&self, mode: Mode, method: &str, mut result: Value) -> Value {
        let seen = |number: u64| match mode {
            Mode::Fork(from) if number >= from => {
                keccak256(format!("fork {}", self.block_hash(number)))
            }
            Mode::Unreplaced(replacements) => self.block_hash_after(number, replacements),
            _ => self.block_hash(number),
        };
        if !matches!(mode, Mode::Fork(_) | Mode::Unreplaced(_)) || !result.is_object() {
            return result;
        }
        match method {
            "eth_getBlockByNumber" => {
                let number = quantity_of(&result["number"]);
                result["hash"] = json!(seen(number));
                result["parentHash"] = json!(seen(number.saturating_sub(1)));
            }
            "eth_getTransactionReceipt" => {
                result["blockHash"] = json!(seen(quantity_of(&result["blockNumber"])));
            }
            _ => {}
        }
        result
    }
}
/// One HTTP endpoint of the node, or one drand relay, and the way it answers.
#[derive(Clone)]
pub struct Endpoint {
    pub url: String,
    mode: Arc<Mutex<Mode>>,
}
impl Endpoint {
    pub fn set(&self, mode: Mode) {
        *self.mode.lock().expect("mode") = mode;
    }
}

/// The node: the scripted chain behind an HTTP endpoint, and everything it was asked, in the order it was asked.
/// More endpoints and drand relays can serve the same chain; once there is more than one of either, the trace names
/// the one that was asked (`[2] ` before a node call, `relay[2]` for a relay).
pub struct Node {
    pub url: String,
    pub chain: Arc<Mutex<Chain>>,
    seen: Arc<Mutex<Vec<Seen>>>,
    hold: Arc<AtomicU64>,
    /// Milliseconds by which a request that reads the header of the finalized block is answered later than the window
    /// says; see `late_finalized`.
    late: Arc<AtomicU64>,
    window: Window,
    endpoints: Arc<AtomicUsize>,
    relays: Arc<AtomicUsize>,
    first: Endpoint,
}
/// The end of the window that is open, if any: shared by every endpoint and relay of a node.
type Window = Arc<Mutex<Option<Instant>>>;
/// When a request that arrived at `now` is answered: at the end of the window it arrived in, or of a new one that it
/// opens, `hold` long.
fn answer_time(window: &Window, now: Instant, hold: Duration) -> Instant {
    let mut open = window.lock().expect("window");
    match *open {
        Some(until) if now < until => until,
        _ => {
            *open = Some(now + hold);
            now + hold
        }
    }
}
/// The requests of one wave, in the order the trace prints them.
pub type Wave = Vec<Seen>;
impl Node {
    pub async fn start(chain: Chain) -> Self {
        let chain = Arc::new(Mutex::new(chain));
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
        let hold = Arc::new(AtomicU64::new(hold().as_millis() as u64));
        let late = Arc::new(AtomicU64::new(0));
        let window: Window = Arc::new(Mutex::new(None));
        let endpoints = Arc::new(AtomicUsize::new(1));
        let first = Self::serve(&chain, &seen, (&hold, &late), &window, &endpoints, 0).await;
        Self {
            url: first.url.clone(),
            chain,
            seen,
            hold,
            late,
            window,
            endpoints,
            relays: Arc::new(AtomicUsize::new(0)),
            first,
        }
    }
    /// The first endpoint, whose mode a script can change.
    pub fn endpoint(&self) -> &Endpoint {
        &self.first
    }
    /// Another HTTP endpoint for the same chain.
    pub async fn add_endpoint(&self) -> Endpoint {
        let index = self.endpoints.fetch_add(1, Ordering::SeqCst);
        Self::serve(
            &self.chain,
            &self.seen,
            (&self.hold, &self.late),
            &self.window,
            &self.endpoints,
            index,
        )
        .await
    }
    async fn serve(
        chain: &Arc<Mutex<Chain>>,
        seen: &Arc<Mutex<Vec<Seen>>>,
        (hold, late): (&Arc<AtomicU64>, &Arc<AtomicU64>),
        window: &Window,
        endpoints: &Arc<AtomicUsize>,
        index: usize,
    ) -> Endpoint {
        let mode = Arc::new(Mutex::new(Mode::Up));
        let (state, log, held, late, window, count, how) = (
            chain.clone(),
            seen.clone(),
            hold.clone(),
            late.clone(),
            window.clone(),
            endpoints.clone(),
            mode.clone(),
        );
        let (url, _) = fixture::serve(move |_, body| {
            let arrived = Instant::now();
            let held = Duration::from_millis(held.load(Ordering::Relaxed));
            let mut answered = answer_time(&window, arrived, held);
            let mode = *how.lock().expect("mode");
            let label = if count.load(Ordering::SeqCst) > 1 {
                format!("[{}] ", index + 1)
            } else {
                String::new()
            };
            let request: Value = serde_json::from_slice(body).expect("a JSON-RPC body");
            let batch = request.is_array();
            let items: Vec<Value> = match request {
                Value::Array(items) => items,
                single => vec![single],
            };
            let refused_batch = match mode {
                Mode::BatchLimit(limit) if batch && items.len() > limit => Some(limit),
                _ => None,
            };
            let silent = matches!(mode, Mode::Down | Mode::RateLimited) || refused_batch.is_some();
            let late = Duration::from_millis(late.load(Ordering::Relaxed));
            if !late.is_zero()
                && items.iter().any(|item| {
                    item["method"] == "eth_getBlockByNumber" && item["params"][0] == "finalized"
                })
            {
                answered = answered.max(arrived + late);
            }
            let mut calls = Vec::new();
            let mut answers = Vec::new();
            {
                let mut chain = state.lock().expect("chain");
                for item in &items {
                    let method = item["method"].as_str().expect("a method");
                    if mode == Mode::RevertingRoundRequests
                        && method == "eth_call"
                        && item["params"][0]["data"].as_str().is_some_and(|data| {
                            data.starts_with(&format!(
                                "0x{}",
                                hex::encode(Round::getRoundRequestCall::SELECTOR)
                            ))
                        })
                    {
                        calls.push(chain.record(method, &item["params"]));
                        answers.push(json!({"jsonrpc":"2.0","id":item["id"],"error":{"code":3,"message":"execution reverted"}}));
                        continue;
                    }
                    let refused = match mode {
                        Mode::Refusing(refused, message) if refused == method => Some(message),
                        Mode::NoFinalized
                            if method == "eth_getBlockByNumber"
                                && item["params"][0] == "finalized" =>
                        {
                            Some("finalized block tag not supported")
                        }
                        _ => None,
                    };
                    if silent || refused.is_some() {
                        calls.push(chain.record(method, &item["params"]));
                        if let Some(message) = refused {
                            answers.push(json!({"jsonrpc":"2.0","id":item["id"],"error":{"code":-32000,"message":message}}));
                        }
                        continue;
                    }
                    let mut params = item["params"].clone();
                    if mode == Mode::FinalizedLatest
                        && method == "eth_getBlockByNumber"
                        && params[0] == "finalized"
                    {
                        params[0] = json!("latest");
                    }
                    let (call, outcome) = chain.handle(method, &params);
                    let outcome = outcome.map(|result| chain.as_seen(mode, method, result));
                    calls.push(call);
                    answers.push(match outcome {
                        Ok(result) => json!({"jsonrpc":"2.0","id":item["id"],"result":result}),
                        Err((code, message)) => json!({"jsonrpc":"2.0","id":item["id"],"error":{"code":code,"message":message}}),
                    });
                }
            }
            log.lock().expect("log").push(Seen {
                arrived,
                answered,
                calls,
                batch,
                label,
            });
            let mut answer = if mode == Mode::Down {
                fixture::answer(503, "{}")
            } else if mode == Mode::RateLimited {
                fixture::answer(429, "{}")
            } else if let Some(limit) = refused_batch {
                fixture::answer(
                    500,
                    json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":format!("Batch of more than {limit} requests are not allowed on free plan")}})
                        .to_string(),
                )
            } else if batch {
                fixture::answer(200, Value::Array(answers).to_string())
            } else {
                fixture::answer(200, answers.remove(0).to_string())
            };
            answer.delay = answered.saturating_duration_since(Instant::now());
            answer
        })
        .await;
        Endpoint { url, mode }
    }
    /// Hold answers for `hold` from now on. Without a hold the trace of a run is not reliable; a test sets none only
    /// for a run whose trace it does not read.
    pub fn hold(&self, hold: Duration) {
        self.hold.store(hold.as_millis() as u64, Ordering::Relaxed);
    }
    /// From now on a request that reads the header of the `finalized` block is answered `late` after it arrived, whatever
    /// the window says; every other request is answered as it was. A soft keeper reads that header and nothing else at
    /// the tag, so this slows down its finality audit alone.
    pub fn late_finalized(&self, late: Duration) {
        self.late.store(late.as_millis() as u64, Ordering::Relaxed);
    }
    /// A drand relay for this chain's beacon: a round is served once its scheduled time has passed. Its requests are
    /// recorded with the node's, in the one sequence, and a script can take it down.
    pub async fn add_relay(&self) -> Endpoint {
        let index = self.relays.fetch_add(1, Ordering::SeqCst);
        let mode = Arc::new(Mutex::new(Mode::Up));
        let (state, log, held, window, count, how) = (
            self.chain.clone(),
            self.seen.clone(),
            self.hold.clone(),
            self.window.clone(),
            self.relays.clone(),
            mode.clone(),
        );
        let (url, _) = fixture::serve(move |line, _| {
            let arrived = Instant::now();
            let held = Duration::from_millis(held.load(Ordering::Relaxed));
            let answered = answer_time(&window, arrived, held);
            let mode = *how.lock().expect("mode");
            let path = line.split_whitespace().nth(1).unwrap_or_default();
            let (hash, round) = path
                .trim_start_matches('/')
                .split_once("/public/")
                .unwrap_or_default();
            let round: u64 = round.parse().unwrap_or(0);
            let name = if count.load(Ordering::SeqCst) > 1 {
                format!("relay[{}]", index + 1)
            } else {
                "relay".to_owned()
            };
            log.lock().expect("log").push(Seen {
                arrived,
                answered,
                calls: vec![Call(format!("{name} GET /<chain hash>/public/{round}"))],
                batch: false,
                label: String::new(),
            });
            let chain = state.lock().expect("chain");
            let due = BEACON_GENESIS + (round.saturating_sub(1)) * BEACON_PERIOD;
            let mut answer = if mode == Mode::Down {
                fixture::answer(500, "{}")
            } else if hash != hex::encode(chain.beacon_chain_hash) || due > chain.time(chain.head) {
                fixture::answer(425, "{}")
            } else {
                // A relay that forges answers gives a well-formed signature of another round.
                let signature = if mode == Mode::Forging {
                    round_signature(round.wrapping_add(1))
                } else {
                    round_signature(round)
                };
                fixture::answer(
                    200,
                    json!({"round": round, "signature": hex::encode(signature)}).to_string(),
                )
            };
            answer.delay = answered.saturating_duration_since(Instant::now());
            answer
        })
        .await;
        Endpoint { url, mode }
    }
    /// What the node was asked since the last call, grouped into waves. Calls made at the same time (they arrive
    /// before the first of them is answered) are one wave and print in a fixed order; a call that waited for an
    /// answer starts the next wave, so the order of the waves is the order the keeper made its calls in.
    pub fn take(&self) -> Vec<Wave> {
        let mut seen = std::mem::take(&mut *self.seen.lock().expect("log"));
        seen.sort_by_key(|request| request.arrived);
        let mut waves: Vec<(Instant, Wave)> = Vec::new();
        for request in seen {
            match waves.last_mut() {
                Some((until, wave)) if request.arrived < *until => wave.push(request),
                _ => waves.push((request.answered, vec![request])),
            }
        }
        waves
            .into_iter()
            .map(|(_, mut wave)| {
                wave.sort_by_key(Seen::key);
                wave
            })
            .collect()
    }
    /// How many requests the node has been asked since the last `take`.
    pub fn asked(&self) -> usize {
        self.seen.lock().expect("log").len()
    }
    /// Run `script` on the chain.
    pub fn with<T>(&self, script: impl FnOnce(&mut Chain) -> T) -> T {
        script(&mut self.chain.lock().expect("chain"))
    }
}
/// The waves as lines: one line per call, a batch in brackets, and the requests of a wave that has more than one
/// inside braces. A request to one of several endpoints starts with its label.
pub fn render(waves: &[Wave]) -> Vec<String> {
    let mut lines = Vec::new();
    for wave in waves {
        let nested = wave.len() > 1;
        if nested {
            lines.push("{".to_owned());
        }
        for request in wave {
            let indent = if nested { "  " } else { "" };
            let label = &request.label;
            if request.batch {
                lines.push(format!("{indent}{label}batch ["));
                lines.extend(
                    request
                        .calls
                        .iter()
                        .map(|call| format!("{indent}  {}", call.0)),
                );
                lines.push(format!("{indent}]"));
            } else {
                lines.extend(
                    request
                        .calls
                        .iter()
                        .map(|call| format!("{indent}{label}{}", call.0)),
                );
            }
        }
        if nested {
            lines.push("}".to_owned());
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{SignableTransaction, Signed, TxEip1559, TxEip2930, TxLegacy};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Signature, TxKind};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;

    const GWEI: u128 = 1_000_000_000;

    fn wallet() -> PrivateKeySigner {
        PrivateKeySigner::from_bytes(&B256::repeat_byte(0x24)).unwrap()
    }
    fn chain() -> Chain {
        Chain::new(5_042_002, wallet().address(), [U256::ZERO; 2])
    }
    /// How the trace records a call.
    fn recorded(chain: &mut Chain, method: &str, params: Value) -> String {
        chain.handle(method, &params).0.0
    }
    fn call_object(to: Address, data: Vec<u8>) -> Value {
        json!({"to": to, "data": Bytes::from(data)})
    }
    /// The parameters of an `eth_sendRawTransaction` for `transaction`, signed by the keeper's wallet.
    fn raw<T>(transaction: T) -> Value
    where
        T: SignableTransaction<Signature>,
        Signed<T>: Encodable2718,
    {
        let signature = wallet()
            .sign_hash_sync(&transaction.signature_hash())
            .unwrap();
        let encoded = transaction.into_signed(signature).encoded_2718();
        json!([format!("0x{}", hex::encode(encoded))])
    }

    #[test]
    fn a_number_is_named_by_its_distance_from_both_heads_and_a_tag_as_it_was_sent() {
        let mut chain = chain();
        chain.head = 1_100;
        chain.finalized_lag = 3;
        let mut read =
            |tag: &str| recorded(&mut chain, "eth_getBlockByNumber", json!([tag, false]));
        assert_eq!(
            read("0x44c"),
            "eth_getBlockByNumber #latest/finalized+3 full=false"
        );
        assert_eq!(
            read("0x449"),
            "eth_getBlockByNumber #latest-3/finalized full=false"
        );
        assert_eq!(
            read("0x400"),
            "eth_getBlockByNumber #latest-76/finalized-73 full=false"
        );
        assert_eq!(
            read("0x450"),
            "eth_getBlockByNumber #latest+4/finalized+7 full=false"
        );
        for name in ["latest", "finalized", "safe", "pending", "earliest"] {
            assert_eq!(
                read(name),
                format!("eth_getBlockByNumber {name} full=false")
            );
        }
        // Whether the block is the full one is part of the question.
        assert_eq!(
            recorded(&mut chain, "eth_getBlockByNumber", json!(["latest", true])),
            "eth_getBlockByNumber latest full=true"
        );
    }

    #[test]
    fn a_storage_read_names_its_slot() {
        let mut chain = chain();
        let proxy = chain.coordinator;
        let mut read = |slot: &str| {
            recorded(
                &mut chain,
                "eth_getStorageAt",
                json!([proxy, slot, "latest"]),
            )
        };
        assert_eq!(
            read(IMPLEMENTATION_SLOT),
            "eth_getStorageAt latest coordinator slot=eip1967.implementation"
        );
        // The admin slot is another read, and so is any other.
        let admin = "0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103";
        assert_eq!(
            read(admin),
            format!("eth_getStorageAt latest coordinator slot={admin}")
        );
        assert_eq!(read("0x0"), "eth_getStorageAt latest coordinator slot=0x0");
    }

    #[test]
    fn a_fee_history_names_its_blocks_and_percentiles_and_answers_one_row_per_block() {
        let mut chain = chain();
        chain.tips = vec![GWEI, 2 * GWEI, 50 * GWEI];
        let (call, answer) = chain.handle("eth_feeHistory", &json!(["0x5", "latest", [50]]));
        assert_eq!(call.0, "eth_feeHistory latest blocks=5 percentiles=[50]");
        assert_eq!(
            answer.unwrap()["reward"],
            json!([
                ["0x3b9aca00"],
                ["0x77359400"],
                ["0xba43b7400"],
                ["0x3b9aca00"],
                ["0x77359400"]
            ])
        );
        // Each percentile asked for has its own entry, and the call says which.
        let (call, answer) = chain.handle("eth_feeHistory", &json!(["0x2", "0x3e8", [10, 50, 90]]));
        assert_eq!(
            call.0,
            "eth_feeHistory #latest/finalized blocks=2 percentiles=[10,50,90]"
        );
        assert_eq!(answer.unwrap()["reward"][0].as_array().unwrap().len(), 3);
    }

    #[test]
    fn a_call_names_who_it_is_from_what_it_sends_and_its_arguments() {
        let mut chain = chain();
        let (keeper, coordinator, registry) = (chain.keeper, chain.coordinator, chain.registry);
        let request = |id: u64| C::getRequestCall { id: U256::from(id) }.abi_encode();
        let object = call_object(coordinator, request(7));
        assert_eq!(
            recorded(&mut chain, "eth_call", json!([object, "latest"])),
            "eth_call latest coordinator 0xc58343ef getRequest args=[7]"
        );
        let mut object = call_object(coordinator, request(7));
        object["from"] = json!(keeper);
        object["value"] = json!("0x5");
        assert_eq!(
            recorded(&mut chain, "eth_call", json!([object, "finalized"])),
            "eth_call finalized coordinator 0xc58343ef getRequest from=keeper value=0x5 args=[7]"
        );
        // Another request is another call.
        let object = call_object(coordinator, request(8));
        assert_eq!(
            recorded(&mut chain, "eth_call", json!([object, "latest"])),
            "eth_call latest coordinator 0xc58343ef getRequest args=[8]"
        );
        // An address is a role, and several words are listed.
        let account = E::isBackupCommitterCall { account: keeper }.abi_encode();
        let named = selector_text(&account);
        let object = call_object(registry, account);
        assert_eq!(
            recorded(&mut chain, "eth_call", json!([object, "latest"])),
            format!("eth_call latest registry {named} args=[keeper]")
        );
        let page = C::getPendingRequestIdsCall {
            fromId: U256::from(1),
            limit: U256::from(256),
        }
        .abi_encode();
        let object = call_object(coordinator, page);
        assert!(
            recorded(&mut chain, "eth_call", json!([object, "latest"]))
                .ends_with("getPendingRequestIds args=[1,256]")
        );
        // Anything else is a fingerprint of all the bytes, which another byte changes.
        let verify = |last: u8| {
            let mut signature = vec![1u8; 64];
            signature[63] = last;
            E::verifyBeaconCall {
                recipe: 6,
                round: 5,
                signature: Bytes::from(signature),
            }
            .abi_encode()
        };
        let (first, second) = (verify(1), verify(2));
        let line = |chain: &mut Chain, data: &Vec<u8>| {
            let object = call_object(registry, data.clone());
            recorded(chain, "eth_call", json!([object, "latest"]))
        };
        assert_eq!(
            line(&mut chain, &first),
            format!(
                "eth_call latest registry {} args={}",
                selector_text(&first),
                fingerprint(&first[4..])
            )
        );
        assert_ne!(line(&mut chain, &first), line(&mut chain, &second));
        // The one argument that is a block is placed like a tag.
        chain.head = 1_100;
        chain.finalized_lag = 3;
        let prepare = E::nextEpochToPrepareCall {
            number: U256::from(1_097u64),
        }
        .abi_encode();
        let object = call_object(registry, prepare);
        assert!(
            recorded(&mut chain, "eth_call", json!([object, "finalized"]))
                .ends_with("nextEpochToPrepare block=#latest-3/finalized")
        );
    }

    #[test]
    fn an_estimate_names_its_target_sender_value_and_what_it_asks_for() {
        let mut chain = chain();
        let keeper = chain.keeper;
        let recipient = chain.fee_recipient;
        let mut estimate = |tx: Value| recorded(&mut chain, "eth_estimateGas", json!([tx]));
        assert_eq!(
            estimate(json!({"from": keeper, "to": keeper, "value": "0x0"})),
            "eth_estimateGas - keeper transfer from=keeper value=0x0"
        );
        assert_eq!(
            estimate(json!({"from": keeper, "to": recipient, "value": "0x1"})),
            "eth_estimateGas - fee_recipient transfer from=keeper value=0x1"
        );
        // A value that is not sent is not recorded.
        assert_eq!(
            estimate(json!({"from": keeper, "to": recipient})),
            "eth_estimateGas - fee_recipient transfer from=keeper"
        );
    }

    #[test]
    fn a_sent_transaction_is_recorded_with_its_type_chain_fees_and_value() {
        let mut chain = chain();
        let (keeper, recipient) = (chain.keeper, chain.fee_recipient);
        let transfer = |value: u64, to: Address| TxEip1559 {
            chain_id: 5_042_002,
            nonce: 3,
            gas_limit: 21_000,
            max_fee_per_gas: 4_020_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Call(to),
            value: U256::from(value),
            access_list: Default::default(),
            input: Bytes::new(),
        };
        assert_eq!(
            recorded(
                &mut chain,
                "eth_sendRawTransaction",
                raw(transfer(0, keeper))
            ),
            "eth_sendRawTransaction - keeper cancel type=2 chain=5042002 nonce=3 gas=21000 maxFee=4020000000 maxPriority=2000000000 value=0"
        );
        assert_eq!(
            recorded(
                &mut chain,
                "eth_sendRawTransaction",
                raw(transfer(9, recipient))
            ),
            "eth_sendRawTransaction - fee_recipient sweep type=2 chain=5042002 nonce=3 gas=21000 maxFee=4020000000 maxPriority=2000000000 value=9"
        );
        // A legacy and an access-list transaction have one gas price and no tip of their own.
        let legacy = TxLegacy {
            chain_id: Some(5_042_002),
            nonce: 4,
            gas_price: 7,
            gas_limit: 21_000,
            to: TxKind::Call(keeper),
            value: U256::ZERO,
            input: Bytes::new(),
        };
        assert_eq!(
            recorded(&mut chain, "eth_sendRawTransaction", raw(legacy)),
            "eth_sendRawTransaction - keeper cancel type=0 chain=5042002 nonce=4 gas=21000 gasPrice=7 value=0"
        );
        let listed = TxEip2930 {
            chain_id: 5_042_002,
            nonce: 5,
            gas_price: 8,
            gas_limit: 21_000,
            to: TxKind::Call(keeper),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::new(),
        };
        assert_eq!(
            recorded(&mut chain, "eth_sendRawTransaction", raw(listed)),
            "eth_sendRawTransaction - keeper cancel type=1 chain=5042002 nonce=5 gas=21000 gasPrice=8 value=0"
        );
        // Another chain and another tip are other records.
        let mut other = transfer(0, keeper);
        other.chain_id = 1;
        other.max_priority_fee_per_gas = 50 * GWEI;
        assert_eq!(
            recorded(&mut chain, "eth_sendRawTransaction", raw(other)),
            "eth_sendRawTransaction - keeper cancel type=2 chain=1 nonce=3 gas=21000 maxFee=4020000000 maxPriority=50000000000 value=0"
        );
    }

    #[test]
    fn a_fulfillment_is_recorded_by_the_ids_and_seeds_it_serves_and_by_nothing_random() {
        use crate::abi::VrfProof;
        let mut chain = chain();
        let proof = |seed: u64, random: u64| VrfProof {
            pk: [U256::from(1), U256::from(2)],
            gamma: [U256::from(random); 2],
            c: U256::from(random),
            s: U256::from(random),
            seed: U256::from(seed),
            uWitness: Address::repeat_byte(3),
            cGammaWitness: [U256::from(random); 2],
            sHashWitness: [U256::from(random); 2],
            zInv: U256::from(random),
        };
        let estimate = |chain: &mut Chain, seed: u64, random: u64| {
            let data = C::fulfillRandomnessCall {
                id: U256::from(4),
                proof: proof(seed, random),
            }
            .abi_encode();
            let mut object = call_object(chain.coordinator, data);
            object["from"] = json!(chain.keeper);
            recorded(chain, "eth_estimateGas", json!([object]))
        };
        let first = estimate(&mut chain, 77, 1);
        assert!(first.contains(" id=4 seed="), "{first}");
        assert!(first.contains(" bytes=452 "), "{first}");
        // The proof's own randomness is not in the record: the same request and seed read the same.
        assert_eq!(first, estimate(&mut chain, 77, 2));
        // Another seed is another record.
        assert_ne!(first, estimate(&mut chain, 78, 1));
    }

    /// What `getRequest` answers for request `id` when read at `tag`.
    fn request_at(chain: &mut Chain, id: u64, tag: &str) -> crate::abi::Request {
        let data = C::getRequestCall { id: U256::from(id) }.abi_encode();
        let object = call_object(chain.coordinator, data);
        let (_, answer) = chain.handle("eth_call", &json!([object, tag]));
        let bytes: Bytes = serde_json::from_value(answer.unwrap()).unwrap();
        C::getRequestCall::abi_decode_returns(&bytes).unwrap()
    }
    fn nonce_at(chain: &mut Chain, tag: &str) -> u64 {
        let keeper = chain.keeper;
        let (_, answer) = chain.handle("eth_getTransactionCount", &json!([keeper, tag]));
        u64::from_str_radix(
            answer.unwrap().as_str().unwrap().trim_start_matches("0x"),
            16,
        )
        .unwrap()
    }
    fn number_of(chain: &mut Chain, tag: &str) -> u64 {
        let (_, block) = chain.handle("eth_getBlockByNumber", &json!([tag, false]));
        u64::from_str_radix(
            block.unwrap()["number"]
                .as_str()
                .unwrap()
                .trim_start_matches("0x"),
            16,
        )
        .unwrap()
    }

    #[test]
    fn finalized_and_safe_trail_the_latest_block_by_the_lags_the_script_sets() {
        let mut chain = chain();
        chain.head = 1_100;
        // A chain that finalizes at once reads one head under every tag.
        for tag in ["latest", "finalized", "safe", "pending"] {
            assert_eq!(number_of(&mut chain, tag), 1_100, "{tag}");
        }
        chain.finalized_lag = 3;
        chain.safe_lag = 1;
        assert_eq!(number_of(&mut chain, "latest"), 1_100);
        assert_eq!(number_of(&mut chain, "pending"), 1_100);
        assert_eq!(number_of(&mut chain, "safe"), 1_099);
        assert_eq!(number_of(&mut chain, "finalized"), 1_097);
        assert_eq!(number_of(&mut chain, "earliest"), 0);
        // A lag longer than the chain is its start.
        chain.head = 2;
        assert_eq!(number_of(&mut chain, "finalized"), 0);
    }

    #[test]
    fn a_read_at_a_tag_or_a_number_sees_the_state_that_block_left() {
        let mut chain = chain();
        chain.finalized_lag = 3;
        chain.safe_lag = 1;
        let consumer = Address::repeat_byte(0xa1);
        // Block 1000: a request. Block 1002: its refund. The head is 1004.
        let id = chain.request(consumer, 100_000, 1);
        chain.mine(2);
        chain.requests.get_mut(&id).unwrap().refunded = true;
        chain.mine(2);
        assert_eq!(chain.head, 1_004);
        // Finalized is block 1001, which had no refund; safe is 1003 and latest 1004, which have it.
        assert!(!request_at(&mut chain, id, "finalized").refunded);
        assert!(request_at(&mut chain, id, "safe").refunded);
        assert!(request_at(&mut chain, id, "latest").refunded);
        // A number is read as the block it names, not as the head.
        assert!(!request_at(&mut chain, id, "0x3e8").refunded);
        assert!(!request_at(&mut chain, id, "0x3e9").refunded);
        assert!(request_at(&mut chain, id, "0x3ea").refunded);
        assert!(request_at(&mut chain, id, "0x3ec").refunded);
        // A request made after a block is not in it.
        let later = chain.request(consumer, 100_000, 1);
        chain.mine(1);
        assert_eq!(
            request_at(&mut chain, later, "finalized").consumer,
            Address::ZERO
        );
        assert_eq!(request_at(&mut chain, later, "latest").consumer, consumer);
        // And the state of the chain is what it was after every read.
        assert_eq!(chain.head, 1_005);
        assert!(chain.requests[&id].refunded);
    }

    #[test]
    fn the_wallets_nonce_is_the_one_the_block_left_but_a_pending_one_counts_what_is_queued() {
        let mut chain = chain();
        chain.finalized_lag = 2;
        chain.mine(1);
        chain.nonce = 1;
        chain.mine(1);
        chain.nonce = 2;
        chain.mine(1);
        // Head 1003: the nonce was 0 until block 1000's end, 1 at the end of 1001 and 2 now.
        assert_eq!(chain.head, 1_003);
        assert_eq!(nonce_at(&mut chain, "latest"), 2);
        assert_eq!(nonce_at(&mut chain, "finalized"), 1);
        assert_eq!(nonce_at(&mut chain, "0x3e8"), 0);
        assert_eq!(nonce_at(&mut chain, "0x3e9"), 1);
        assert_eq!(nonce_at(&mut chain, "pending"), 2);
    }

    #[test]
    fn a_proof_is_ready_at_a_block_only_when_that_block_is_past_the_target_by_the_confirmations() {
        let mut chain = chain();
        chain.finalized_lag = 3;
        chain.publish_epoch(1, chain.head);
        let id = chain.request(Address::repeat_byte(0xa1), 100_000, 1);
        // The epoch was committed in block 1000, so the target is 1001 and the proof needs block 1002.
        let ready = |chain: &mut Chain, tag: &str| {
            let data = C::getProofContextCall { id: U256::from(id) }.abi_encode();
            let object = call_object(chain.coordinator, data);
            chain.handle("eth_call", &json!([object, tag])).1.is_ok()
        };
        chain.mine(4);
        assert_eq!(chain.head, 1_004);
        assert!(ready(&mut chain, "latest"));
        assert!(
            !ready(&mut chain, "finalized"),
            "block 1001 is not past the target"
        );
        chain.mine(1);
        assert!(ready(&mut chain, "finalized"), "block 1002 is");
    }

    #[test]
    fn the_chain_does_not_go_back() {
        let mut chain = chain();
        chain.mine_to(1_010);
        assert_eq!(chain.head, 1_010);
        assert!(chain.history.contains_key(&1_000) && chain.history.contains_key(&1_009));
        assert!(!chain.history.contains_key(&1_010));
        let result = std::panic::catch_unwind(move || {
            let mut chain = chain;
            chain.mine_to(1_009);
        });
        assert!(result.is_err());
    }
}
