//! Round mode (COORDINATOR_KIND=round): the round coordinator's facts a keeper starts from, and the round lane that takes
//! the epoch lane's place.
//!
//! A round coordinator binds each request, when it is made, to the first round of the drand beacon in force that is
//! scheduled at least `ROUND_LEAD` seconds after the request block's time. A request is served once that round's
//! signature is public: the keeper fetches it from the drand relays (this lane, task K2), proves the request over the
//! round's randomness and sends the fulfillment with the signature (task K3, `worker::rounds`). Nothing of an epoch
//! coordinator is used: no epoch, no target block and no contract beside the round coordinator.
//!
//! Startup reads the coordinator's own facts and every beacon its book has registered (`observe`); discovery journals
//! every live request with its beacon, round and fingerprint (`journal::RoundAssignment`), and the journal answers round
//! demand: the rounds live requests wait on (`Journal::round_demand`). The lane keeps one `round_work` row per demanded
//! round and fetches each round's signature once the wall clock says it is due, through the chain-neutral drand client
//! (`drand.rs`), with the coordinator's `checkRoundSignature` as the verifier. A verified round keeps its signature and
//! its randomness, sha256(signature). A request whose round is verified is proved over the seed this module computes as
//! the coordinator does (`seed`), and journaled with the fingerprint it was proved for (`Prepared`); its fulfillment always
//! carries its round's signature (`single_call`, `batch_call`).
//!
//! The `round_work` states: a row is `pending` from the tick that first sees demand for its round. A fetch is started
//! when the row is due (`retry_at`) and the round is due by the wall clock; one that fails leaves the row `pending`,
//! retried RETRY_SECONDS later, and dates the run of failures it belongs to (`failing_since`, `failed_at`, `failed_rpc`).
//! A fetch that succeeds makes the row `verified`, with its signature and randomness, for good. A row whose round no live
//! job waits on any more is deleted, whatever its state; it is made again, and fetched again, if a job of its round comes
//! back to life.
pub use crate::events::Sighting;
use crate::{
    abi_round::{Beacon, RoundCoordinator as R, RoundProof, RoundRequest, RoundSignature},
    drand::{self, Breaker, Network},
    journal::{DemandedRound, Journal, RoundAssignment},
    rpc::{Head, Rpc},
};
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_sol_types::{SolEvent, SolType, SolValue, sol_data};
use anyhow::{Result, ensure};
use k256::sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::task::JoinHandle;

/// Whether a round-mode keeper may sign and send. True since task K3 brought the preparation and the fulfillments; the
/// worker keeps the switch (`ensure_lane_sends`) as the one place that says so.
pub const SENDS: bool = true;
/// The coordinator's response window, which a request's deadline is its block's time plus: `RESPONSE_TIMEOUT`.
const RESPONSE_SECONDS: u64 = crate::config::RESPONSE_TIMEOUT_SECONDS;
/// A request first seen this long after its block's time is logged: its block may have been withheld (design C, 1.5).
pub const SEALING_LAG_WARN_MS: i64 = 3_000;
/// At most this many fetches run at once: two beacons during a switch, or two consecutive rounds.
pub const MAX_FETCHES: usize = 2;
/// The gas the keeper's `eth_call` of `checkRoundSignature` may use. The call needs about 460,000 (the verifier's
/// ROUND_VERIFY_GAS and what the coordinator reserves beside it); the limit is written down so that the verdict does
/// not depend on what an endpoint gives a call without one.
pub const CHECK_GAS: u64 = 1_000_000;
/// The gas the keeper's `eth_call` of `getProofContext(id, signature)` may use: the coordinator verifies the round's
/// signature inside the view when it does not have the round yet, which needs about 500,000.
pub const PROOF_CONTEXT_GAS: u64 = 1_000_000;
/// How far the decision head's time may be ahead of this machine's clock before the clock counts as behind
/// (`Lane::observe_clock`): well above the sequencer's own drift and the age of a head when it is read.
pub const CLOCK_BEHIND_SECONDS: u64 = 15;
/// How long every new block must show this machine's clock right before `clock_behind` clears (`Lane::observe_clock`).
pub const CLOCK_RIGHT_SECONDS: u64 = 600;
/// The table of a round keeper's relay circuits.
pub const RELAY_BREAKER: Breaker = Breaker::new("drand_relay_breaker");
/// One row per round that live requests wait on: see the module documentation for its states.
pub const ROUND_WORK_DDL: &str = "CREATE TABLE IF NOT EXISTS round_work(beacon INTEGER NOT NULL,round INTEGER NOT NULL,state TEXT NOT NULL DEFAULT 'pending',signature TEXT,randomness TEXT,attempts INTEGER NOT NULL DEFAULT 0,retry_at INTEGER NOT NULL DEFAULT 0,last_error TEXT,failing_since INTEGER,failed_at INTEGER,failed_rpc INTEGER,PRIMARY KEY(beacon,round));";
/// The states of a job that make it live demand.
const LIVE: &str = "('pending','prepared','signed','submitted')";

/// The round lane's tables in a round coordinator's journal: its work and its relay circuits.
pub async fn install(pool: &SqlitePool) -> Result<()> {
    sqlx::raw_sql(ROUND_WORK_DDL).execute(pool).await?;
    RELAY_BREAKER.install(pool).await
}

/// The round coordinator's facts a keeper starts from, read once at startup and kept for the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Facts {
    /// `keyHash()`: the hash of the coordinator's VRF key, which is the keeper's own.
    pub key_hash: B256,
    /// `ROUND_LEAD()`: a request binds the first round scheduled at least this many seconds after its block's time.
    pub round_lead: u64,
    /// `pricing()`: the minimum fee, the base-fee multiplier and the gas overhead a request's fee is quoted with.
    pub min_fee: U256,
    pub fee_multiplier: u16,
    pub fulfill_gas_overhead: u32,
    /// `beaconSchedule()`: the beacon in force and since when, and the change still pending (`next_from` 0 when none).
    pub beacon: u8,
    pub beacon_since: u64,
    pub next_beacon: u8,
    pub next_from: u64,
}
/// The beacons a round coordinator's book has registered, by id. A registration never changes, so each is read once.
pub type Beacons = BTreeMap<u8, Beacon>;

/// The hash a round coordinator keeps of its VRF key: `keccak256(abi.encode(uint256[2] publicKey))`.
pub fn key_hash(public_key: [U256; 2]) -> B256 {
    keccak256(public_key.abi_encode())
}

/// The round coordinator's facts and its beacons, read at the view tag. The VRF key the coordinator holds must be the
/// keeper's: its `keyHash` is pinned against the operator key. Only the round coordinator's own functions are read: a
/// coordinator that does not answer them is not a round coordinator, and the keeper does not start.
pub async fn observe(
    rpc: &Rpc,
    coordinator: Address,
    public_key: [U256; 2],
) -> Result<(Facts, Beacons)> {
    let (hash, lead, pricing, schedule, count) = tokio::try_join!(
        rpc.call(coordinator, R::keyHashCall {}),
        rpc.call(coordinator, R::ROUND_LEADCall {}),
        rpc.call(coordinator, R::pricingCall {}),
        rpc.call(coordinator, R::beaconScheduleCall {}),
        rpc.call(coordinator, R::beaconCountCall {}),
    )?;
    ensure!(
        hash == key_hash(public_key),
        "Round coordinator key hash does not match the VRF key"
    );
    // A round must be due well inside the request's response window, or every request would expire unserved.
    ensure!(
        lead > 0 && lead < RESPONSE_SECONDS,
        "Round coordinator answered ROUND_LEAD {lead}, which leaves no response window"
    );
    // Beacon ids are uint8: a book of more than 256 is not a beacon book.
    let count = u16::try_from(count)
        .ok()
        .filter(|count| *count <= 256)
        .ok_or_else(|| anyhow::anyhow!("Round coordinator answered a beacon count over 256"))?;
    let ids: Vec<u8> = (0..count).map(|id| id as u8).collect();
    let registrations = futures_util::future::try_join_all(
        ids.iter()
            .map(|id| rpc.call(coordinator, R::getBeaconCall { beaconId: *id })),
    )
    .await?;
    let beacons: Beacons = ids.into_iter().zip(registrations).collect();
    for (id, beacon) in &beacons {
        tracing::info!(beacon=id,genesis=beacon.genesis,period=beacon.period,chain_hash=%beacon.chainHash,
            verifier=%beacon.verifier,usable=network(beacon).is_ok(),"Round coordinator beacon");
    }
    if !beacons.contains_key(&schedule.beaconId) {
        tracing::warn!(
            beacon = schedule.beaconId,
            count,
            "The beacon in force is not one the round coordinator has registered"
        );
    }
    Ok((
        Facts {
            key_hash: hash,
            round_lead: lead,
            min_fee: pricing.minFee,
            fee_multiplier: pricing.feeMultiplier,
            fulfill_gas_overhead: pricing.fulfillGasOverhead,
            beacon: schedule.beaconId,
            beacon_since: schedule.since,
            next_beacon: schedule.nextBeaconId,
            next_from: schedule.nextFrom,
        },
        beacons,
    ))
}
/// The drand network a registration names, when its rounds can be fetched: it has a verifier and a schedule.
pub fn network(beacon: &Beacon) -> Result<Network> {
    ensure!(
        !beacon.verifier.is_zero() && beacon.genesis > 0 && beacon.period > 0,
        "it has no verifier or no schedule"
    );
    Ok(Network {
        chain_hash: beacon.chainHash,
        genesis: beacon.genesis,
        period: beacon.period,
    })
}

/// A request's fingerprint: every input of its seed that a reorg could change for the same id,
/// `keccak256(abi.encode(consumer, clientSeed, mappingHash, requestBlock, beaconId, round))`. A proof is valid only for
/// the fingerprint it was made for (task K3 compares them before signing).
pub fn fingerprint(request: &RoundRequest) -> B256 {
    type Inputs = (
        sol_data::Address,
        sol_data::FixedBytes<32>,
        sol_data::FixedBytes<32>,
        sol_data::Uint<64>,
        sol_data::Uint<8>,
        sol_data::Uint<64>,
    );
    keccak256(Inputs::abi_encode(&(
        request.consumer,
        request.clientSeed,
        request.mappingHash,
        request.requestBlock,
        request.beaconId,
        request.round,
    )))
}

/// The round coordinator's `SEED_DOMAIN`: keccak256("D20_VRF_ROUND_SEED").
pub fn seed_domain() -> B256 {
    keccak256("D20_VRF_ROUND_SEED")
}
/// The seed a request is proved over, computed here as the coordinator's `_seed` computes it (review N5):
/// `keccak256(abi.encode(SEED_DOMAIN, chainid, coordinator, keyHash, requestId, consumer, clientSeed, mappingHash,
/// requestBlock, beaconId, round, roundRandomness))`. Every input but the round's randomness is in `getRoundRequest`.
pub fn seed(
    chain_id: u64,
    coordinator: Address,
    key_hash: B256,
    id: U256,
    request: &RoundRequest,
    round_randomness: B256,
) -> U256 {
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
    U256::from_be_bytes(
        keccak256(Inputs::abi_encode(&(
            seed_domain(),
            U256::from(chain_id),
            coordinator,
            key_hash,
            id,
            request.consumer,
            request.clientSeed,
            request.mappingHash,
            request.requestBlock,
            request.beaconId,
            request.round,
            round_randomness,
        )))
        .0,
    )
}

/// What a prepared request's job keeps of its proof (`jobs.proof`): the proof, and the fingerprint of the request as it
/// was when the proof was made. The proof is valid for that fingerprint only: before a fulfillment is signed, every
/// request it serves is read again, and one whose fingerprint is another loses its proof and is proved again.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Prepared {
    pub fingerprint: B256,
    pub proof: RoundProof,
}
/// The calldata of a single fulfillment: `fulfillRandomness(id, proof, signature)`, with the signature of the request's
/// round whether or not the coordinator has verified the round already (design C, 3.5).
pub fn single_call(id: U256, proof: RoundProof, signature: Bytes) -> Bytes {
    use alloy_sol_types::SolCall;
    Bytes::from(
        R::fulfillRandomnessCall {
            requestId: id,
            proof,
            roundSignature: signature,
        }
        .abi_encode(),
    )
}
/// One request a batch serves: its id and proof, and its round with the round's signature.
#[derive(Clone, Debug)]
pub struct Member {
    pub id: U256,
    pub proof: RoundProof,
    pub beacon: u8,
    pub round: u64,
    pub signature: Bytes,
}
/// The calldata of a batch: `fulfillRandomnessBatch(rounds, ids, proofs)` with one `RoundSignature` for each distinct
/// round of the members, in the order the members first name it, cached on chain or not (design C, 3.5).
pub fn batch_call(members: &[Member]) -> Bytes {
    use alloy_sol_types::SolCall;
    let mut rounds: Vec<RoundSignature> = Vec::new();
    for member in members {
        if !rounds
            .iter()
            .any(|listed| listed.beaconId == member.beacon && listed.round == member.round)
        {
            rounds.push(RoundSignature {
                beaconId: member.beacon,
                round: member.round,
                signature: member.signature.clone(),
            });
        }
    }
    Bytes::from(
        R::fulfillRandomnessBatchCall {
            rounds,
            ids: members.iter().map(|member| member.id).collect(),
            proofs: members.iter().map(|member| member.proof.clone()).collect(),
        }
        .abi_encode(),
    )
}

/// How long after its block's time this keeper first saw a request, in milliseconds: the wall clock at first sight less
/// the request block's time, which is its deadline less the response window. Informational, and coarse: block times are
/// whole seconds.
pub fn sealing_lag_ms(deadline: u64, now_ms: u64) -> i64 {
    let block_ms = i128::from(deadline.saturating_sub(RESPONSE_SECONDS)) * 1_000;
    i64::try_from(i128::from(now_ms) - block_ms).unwrap_or(i64::MAX)
}
/// Where a request's sealing lag was measured: when the header of its block first arrived (a pushed head, or a decision
/// head this keeper read), or, when no header of that block arrived, when discovery first saw the request, which is later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LagFrom {
    Header,
    Discovery,
}
impl LagFrom {
    pub fn name(self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::Discovery => "discovery",
        }
    }
}

/// What discovery journals of a round request first seen at `now_ms`, with its sealing lag measured at the first sight of
/// its block's header when there is one (design C, review L6), and at `now_ms` otherwise.
pub fn assignment(
    request: &RoundRequest,
    header: Option<Sighting>,
    now_ms: u64,
) -> (RoundAssignment, LagFrom) {
    let (lag, from) = match header {
        Some(seen) => (
            i64::try_from(i128::from(seen.seen_ms) - i128::from(seen.timestamp) * 1_000)
                .unwrap_or(i64::MAX),
            LagFrom::Header,
        ),
        None => (sealing_lag_ms(request.deadline, now_ms), LagFrom::Discovery),
    };
    (
        RoundAssignment {
            beacon: request.beaconId,
            round: request.round,
            fingerprint: fingerprint(request).to_string(),
            sealing_lag_ms: lag,
            seen_at: now_ms / 1_000,
        },
        from,
    )
}

/// The wall clock the round lane schedules its fetches by and dates its rows with. In production it is the system's;
/// a test sets it.
#[derive(Clone, Debug, Default)]
pub struct Clock(Arc<AtomicU64>);
impl Clock {
    /// Milliseconds since the Unix epoch.
    pub fn now_ms(&self) -> u64 {
        match self.0.load(Ordering::SeqCst) {
            0 => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| {
                    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
                }),
            set => set,
        }
    }
    /// Whether a test set the clock.
    fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst) != 0
    }
    /// From now on the clock reads `ms`, until it is set again.
    #[cfg(test)]
    pub fn set(&self, ms: u64) {
        self.0.store(ms.max(1), Ordering::SeqCst);
    }
}

/// One `round_work` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Work {
    pub beacon: u8,
    pub round: u64,
    pub state: String,
    /// `0x`-prefixed hex of the 64-byte signature, and of its sha256, once verified.
    pub signature: Option<String>,
    pub randomness: Option<String>,
    pub attempts: i64,
    pub retry_at: i64,
    pub last_error: Option<String>,
    pub failing_since: Option<i64>,
    pub failed_at: Option<i64>,
    pub failed_rpc: bool,
}
/// The row of a round, if it has one.
pub async fn work(pool: &SqlitePool, beacon: u8, round: u64) -> Result<Option<Work>> {
    type Row = (
        String,
        Option<String>,
        Option<String>,
        i64,
        i64,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    );
    let row: Option<Row> = sqlx::query_as("SELECT state,signature,randomness,attempts,retry_at,last_error,failing_since,failed_at,failed_rpc FROM round_work WHERE beacon=? AND round=?")
        .bind(i64::from(beacon))
        .bind(i64::try_from(round)?)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(
        |(state, signature, randomness, attempts, retry_at, last_error, since, at, rpc)| Work {
            beacon,
            round,
            state,
            signature,
            randomness,
            attempts,
            retry_at,
            last_error,
            failing_since: since,
            failed_at: at,
            failed_rpc: rpc == Some(1),
        },
    ))
}
/// The randomness of a round, sha256 of its signature, as drand and the round coordinator compute it.
pub fn randomness(signature: &[u8]) -> B256 {
    B256::from_slice(&Sha256::digest(signature))
}

/// A fetch in flight: its round, and the task that writes its outcome to the round's row.
struct Fetch {
    beacon: u8,
    round: u64,
    task: JoinHandle<Result<()>>,
}
/// The round lane: in round mode it takes the epoch lane's place in the tick.
pub struct Lane {
    pub facts: Facts,
    /// The beacons read so far: every one registered at startup, and any a request names later.
    beacons: Mutex<Beacons>,
    /// The drand relays, their circuits in RELAY_BREAKER, with a server error excused while a round is recent.
    relays: drand::Client,
    fetches: Mutex<Vec<Fetch>>,
    clock: Clock,
    /// Whether a seed computed here has matched the coordinator's `getProofContext` in this process (see
    /// `seed_checked`).
    seed_checked: AtomicBool,
    /// While `clock_behind` stands: since when (wall-clock ms) every new block has shown this machine's clock right, and
    /// the last block seen (`observe_clock`).
    clock_right: Mutex<Option<(u64, u64)>>,
}
/// What a fetch task needs, moved into it.
struct Launch {
    rpc: Rpc,
    pool: SqlitePool,
    relays: drand::Client,
    coordinator: Address,
    beacon: u8,
    round: u64,
    network: Network,
    /// The block tag of the decision head, where the coordinator is asked whether it has the round already, and the
    /// oldest live request of the round, whose block bounds the search for its RoundVerified event.
    tag: String,
    first_job: U256,
    attempt: i64,
    clock: Clock,
}
impl Lane {
    pub fn new(
        facts: Facts,
        beacons: Beacons,
        relays: &drand::DrandRelays,
        pool: SqlitePool,
    ) -> Result<Self> {
        Ok(Self {
            facts,
            beacons: Mutex::new(beacons),
            relays: drand::Client {
                http: drand::relay_client()?,
                pool,
                breaker: RELAY_BREAKER,
                relays: relays.urls().to_vec(),
                stragglers: drand::Stragglers::default(),
                server_errors: true,
                verifier: "the coordinator",
            },
            fetches: Mutex::new(Vec::new()),
            clock: Clock::default(),
            seed_checked: AtomicBool::new(false),
            clock_right: Mutex::new(None),
        })
    }
    /// Whether a seed this keeper computed (`seed`) has matched the coordinator's own in this process. Until one has, every
    /// preparation asks the coordinator for the request's proof context too, and refuses to prove when the two differ: a
    /// process checks one request this way, and computes the others' seeds alone.
    pub fn seed_checked(&self) -> bool {
        self.seed_checked.load(Ordering::SeqCst)
    }
    /// A seed computed here matched the coordinator's.
    pub fn seed_matched(&self) {
        self.seed_checked.store(true, Ordering::SeqCst);
    }
    /// The clock the lane schedules and dates by.
    pub fn clock(&self) -> &Clock {
        &self.clock
    }
    /// One tick of the lane, at the decision head `head`, for the rounds live requests wait on with a deadline beyond
    /// `after` (the decision head's time plus the send margin): a row for each such round, rows no live job waits on
    /// deleted, the fetches of rows that are due started (at most MAX_FETCHES at once), and the health fault
    /// `round_unavailable` raised or cleared. Returns the demand. A fetch runs in the background and writes its outcome
    /// to its row; a fetch that ended since the last tick is collected first.
    pub async fn poll(
        &self,
        rpc: &Rpc,
        journal: &Journal,
        coordinator: Address,
        head: &Head,
        after: u64,
    ) -> Result<Vec<DemandedRound>> {
        self.collect(false).await?;
        let pool = &journal.pool;
        let demand = journal.round_demand(after).await?;
        for round in &demand {
            sqlx::query("INSERT OR IGNORE INTO round_work(beacon,round) VALUES(?,?)")
                .bind(i64::from(round.beacon))
                .bind(i64::try_from(round.round)?)
                .execute(pool)
                .await?;
        }
        sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM round_work WHERE rowid IN (SELECT round_work.rowid FROM round_work WHERE NOT EXISTS(SELECT 1 FROM round_demand JOIN jobs ON jobs.id=round_demand.job WHERE round_demand.beacon=round_work.beacon AND round_demand.round=round_work.round AND jobs.state IN {LIVE}) LIMIT 128)")))
            .execute(pool)
            .await?;
        let now_ms = self.clock.now_ms();
        let now = now_ms / 1_000;
        let due_ms = self.observe_clock(journal, head, now_ms).await?;
        for round in &demand {
            if self.in_flight() >= MAX_FETCHES {
                break;
            }
            if self.fetching(round.beacon, round.round) {
                continue;
            }
            let Some(row) = work(pool, round.beacon, round.round).await? else {
                continue;
            };
            if row.state != "pending" || row.retry_at > i64::try_from(now)? {
                continue;
            }
            let network = match self.network(rpc, coordinator, round.beacon).await {
                Ok(network) => network,
                Err(error) => {
                    let message = format!("Beacon {} cannot be fetched: {error:#}", round.beacon);
                    let chain_read = drand::is_chain_read(&error);
                    record_failure(pool, round.beacon, round.round, &message, chain_read, now)
                        .await?;
                    tracing::warn!(beacon = round.beacon, round = round.round, error = %message, "Round fetch deferred");
                    continue;
                }
            };
            // Never before the round's scheduled time by the wall clock: a relay that holds requests answers as soon as
            // the round exists, and the chain's clock stops on an idle chain. A wall clock behind the chain's is not
            // waited for (`observe_clock`).
            if u128::from(due_ms) < u128::from(network.round_time(round.round)) * 1_000 {
                continue;
            }
            let Some(first_job) = first_live_job(pool, round.beacon, round.round).await? else {
                continue;
            };
            let attempt = sqlx::query_scalar::<_, i64>("UPDATE round_work SET attempts=attempts+1 WHERE beacon=? AND round=? AND state='pending' RETURNING attempts")
                .bind(i64::from(round.beacon))
                .bind(i64::try_from(round.round)?)
                .fetch_one(pool)
                .await?;
            let launch = Launch {
                rpc: rpc.clone(),
                pool: pool.clone(),
                relays: self.relays.clone(),
                coordinator,
                beacon: round.beacon,
                round: round.round,
                network,
                tag: rpc.decision_tag(head),
                first_job,
                attempt,
                clock: self.clock.clone(),
            };
            tracing::debug!(
                beacon = round.beacon,
                round = round.round,
                attempt,
                "Fetching a due round"
            );
            self.fetches
                .lock()
                .expect("round fetches mutex")
                .push(Fetch {
                    beacon: round.beacon,
                    round: round.round,
                    task: tokio::spawn(launch.run()),
                });
        }
        self.observe_health(journal, &demand, now).await?;
        Ok(demand)
    }
    /// The time the lane takes a round to be due by, in milliseconds: the wall clock `now_ms`, or the decision head's time
    /// when that is later while the health fault `clock_behind` stands. The sequencer stamps its blocks with its own clock,
    /// so a head more than `CLOCK_BEHIND_SECONDS` ahead of the wall clock says this machine's clock is behind: the fault is
    /// raised, for the owner to set the clock right (`health::CLOCK_BEHIND`), and the lane keeps serving by the chain's
    /// time. A head behind the wall clock may only be an idle chain's, so it says nothing either way: the fault clears only
    /// once every new block for `CLOCK_RIGHT_SECONDS` has been within half the threshold of the wall clock.
    async fn observe_clock(&self, journal: &Journal, head: &Head, now_ms: u64) -> Result<u64> {
        // The scripted chain's time is far ahead of any wall clock (`scripted::T0`): a test's lane goes by the wall clock
        // unless the test sets the clock it schedules by.
        if cfg!(test) && !self.clock.is_set() {
            return Ok(now_ms);
        }
        let chain_ms = head.timestamp.saturating_mul(1_000);
        let ahead = chain_ms.saturating_sub(now_ms);
        let behind = ahead > CLOCK_BEHIND_SECONDS * 1_000;
        let raised = journal.meta(crate::health::CLOCK_BEHIND).await?.is_some();
        if behind {
            *self.clock_right.lock().expect("clock mutex") = None;
            if !raised {
                tracing::error!(
                    behind_seconds = ahead / 1_000,
                    block = head.number,
                    "This machine's clock is behind the chain's: rounds are taken due by the chain's time meanwhile. Set the system clock right (NTP)"
                );
                journal
                    .set_meta(crate::health::CLOCK_BEHIND, &ahead.to_string())
                    .await?;
            }
            return Ok(chain_ms);
        }
        if !raised {
            return Ok(now_ms);
        }
        // The fault stands: a new block within half the threshold is evidence that the clock is right.
        let cleared = {
            let mut right = self.clock_right.lock().expect("clock mutex");
            match *right {
                Some((_, last)) if head.number <= last => {}
                _ if ahead * 2 > CLOCK_BEHIND_SECONDS * 1_000 => *right = None,
                Some((since, _)) => *right = Some((since, head.number)),
                None => *right = Some((now_ms, head.number)),
            }
            let cleared = right.is_some_and(|(since, _)| {
                now_ms.saturating_sub(since) >= CLOCK_RIGHT_SECONDS * 1_000
            });
            if cleared {
                *right = None;
            }
            cleared
        };
        if cleared {
            tracing::info!("This machine's clock agrees with the chain's again");
            sqlx::query("DELETE FROM meta WHERE key=?")
                .bind(crate::health::CLOCK_BEHIND)
                .execute(&journal.pool)
                .await?;
            return Ok(now_ms);
        }
        Ok(now_ms.max(chain_ms))
    }
    /// Let the fetches in flight write their outcome. A worker that stops waits for them, as for an epoch fetch.
    pub async fn finish_fetches(&self) -> Result<()> {
        self.collect(true).await
    }
    /// Wait until the relays that lost a fetch's race have been credited.
    #[cfg(test)]
    pub(crate) async fn settled(&self) {
        self.relays.stragglers.settled().await;
    }
    /// Collect the fetches that have ended, or, with `all`, every fetch once it ends. A fetch whose outcome could not be
    /// written fails the tick.
    async fn collect(&self, all: bool) -> Result<()> {
        let ended: Vec<Fetch> = {
            let mut fetches = self.fetches.lock().expect("round fetches mutex");
            let (ended, running) = std::mem::take(&mut *fetches)
                .into_iter()
                .partition(|fetch| all || fetch.task.is_finished());
            *fetches = running;
            ended
        };
        let mut failed = None;
        for fetch in ended {
            match fetch.task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(beacon=fetch.beacon,round=fetch.round,error=%error,"Round fetch outcome not recorded");
                    failed.get_or_insert(error);
                }
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    failed.get_or_insert(anyhow::anyhow!("Round fetch task failed: {error}"));
                }
            }
        }
        failed.map_or(Ok(()), Err)
    }
    fn in_flight(&self) -> usize {
        self.fetches.lock().expect("round fetches mutex").len()
    }
    fn fetching(&self, beacon: u8, round: u64) -> bool {
        self.fetches
            .lock()
            .expect("round fetches mutex")
            .iter()
            .any(|fetch| fetch.beacon == beacon && fetch.round == round)
    }
    /// The network of a beacon, read from the coordinator the first time a request names a beacon startup did not read.
    async fn network(&self, rpc: &Rpc, coordinator: Address, beacon: u8) -> Result<Network> {
        let known = self
            .beacons
            .lock()
            .expect("round beacons mutex")
            .get(&beacon)
            .cloned();
        let registration = match known {
            Some(registration) => registration,
            None => {
                let registration = rpc
                    .call(coordinator, R::getBeaconCall { beaconId: beacon })
                    .await
                    .map_err(|error| {
                        drand::chain_read(format!("its registration could not be read: {error:#}"))
                    })?;
                tracing::info!(beacon,genesis=registration.genesis,period=registration.period,
                    chain_hash=%registration.chainHash,verifier=%registration.verifier,"Round coordinator beacon read for a request");
                self.beacons
                    .lock()
                    .expect("round beacons mutex")
                    .insert(beacon, registration.clone());
                registration
            }
        };
        network(&registration)
    }
    /// Health: live demand that has waited more than drand::UNAVAILABLE_SECONDS on a due round whose fetches keep
    /// failing raises the fault `round_unavailable`, or `round_rpc_error` when the last failure was the keeper's own read
    /// of the chain and not what the relays did. The fault clears as soon as no such demand remains, and at most one of
    /// the two stands.
    async fn observe_health(
        &self,
        journal: &Journal,
        demand: &[DemandedRound],
        now: u64,
    ) -> Result<()> {
        let mut unavailable = None;
        for round in demand {
            let Some(row) = work(&journal.pool, round.beacon, round.round).await? else {
                continue;
            };
            let (Some(since), Some(at)) = (row.failing_since, row.failed_at) else {
                continue;
            };
            let (since, at) = (u64::try_from(since)?, u64::try_from(at)?);
            if row.state == "pending"
                && now.saturating_sub(since) >= drand::UNAVAILABLE_SECONDS
                && at.saturating_add(drand::RUN_GAP_SECONDS) >= now
            {
                unavailable = Some((row, since));
                break;
            }
        }
        let keys = [
            crate::health::ROUND_UNAVAILABLE,
            crate::health::ROUND_RPC_ERROR,
        ];
        let mut raised = None;
        for key in keys {
            if journal.meta(key).await?.is_some() {
                raised = Some(key);
            }
        }
        match unavailable {
            Some((row, since)) => {
                let key = if row.failed_rpc {
                    crate::health::ROUND_RPC_ERROR
                } else {
                    crate::health::ROUND_UNAVAILABLE
                };
                if raised != Some(key) {
                    let message = if row.failed_rpc {
                        "Round unavailable: live requests wait on a due round that this keeper's own reads of the chain fail to check; inspect the RPC endpoints"
                    } else {
                        "Round unavailable: live requests wait on a due round that no relay serves"
                    };
                    tracing::warn!(
                        beacon = row.beacon,
                        round = row.round,
                        failing_seconds = now.saturating_sub(since),
                        chain_read = row.failed_rpc,
                        error = row.last_error.as_deref().unwrap_or_default(),
                        "{message}"
                    );
                }
                let mut tx = journal.pool.begin().await?;
                sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
                    .bind(key)
                    .bind(format!("{}:{}", row.beacon, row.round))
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM meta WHERE key IN (?,?) AND key!=?")
                    .bind(keys[0])
                    .bind(keys[1])
                    .bind(key)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
            }
            None if raised.is_some() => {
                tracing::info!(
                    "Round available again: no live request waits on a round that cannot be fetched"
                );
                sqlx::query("DELETE FROM meta WHERE key IN (?,?)")
                    .bind(keys[0])
                    .bind(keys[1])
                    .execute(&journal.pool)
                    .await?;
            }
            None => {}
        }
        Ok(())
    }
}
impl Drop for Lane {
    fn drop(&mut self) {
        if let Ok(fetches) = self.fetches.get_mut() {
            for fetch in fetches.drain(..) {
                fetch.task.abort();
            }
        }
        self.relays.stragglers.abort();
    }
}

/// The oldest live job of a round, by id: its request's block is the earliest of the round's.
async fn first_live_job(pool: &SqlitePool, beacon: u8, round: u64) -> Result<Option<U256>> {
    let id: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT round_demand.job FROM round_demand JOIN jobs ON jobs.id=round_demand.job WHERE round_demand.beacon=? AND round_demand.round=? AND jobs.state IN {LIVE} ORDER BY CAST(round_demand.job AS INTEGER) LIMIT 1")))
        .bind(i64::from(beacon))
        .bind(i64::try_from(round)?)
        .fetch_optional(pool)
        .await?;
    id.map(|id| id.parse().map_err(Into::into)).transpose()
}
/// A failed fetch of a round: retried RETRY_SECONDS from now, and dated in the run of failures it belongs to, which it
/// starts when there is none or the last failure was more than drand::RUN_GAP_SECONDS ago. `chain_read`: the keeper's
/// own read of the chain failed, not the relays. Returns whether it was the first failure of its run.
async fn record_failure(
    pool: &SqlitePool,
    beacon: u8,
    round: u64,
    message: &str,
    chain_read: bool,
    now: u64,
) -> Result<bool> {
    let now = i64::try_from(now)?;
    let since: Option<i64> = sqlx::query_scalar("UPDATE round_work SET retry_at=?1,last_error=?2,failing_since=CASE WHEN failing_since IS NULL OR COALESCE(failed_at,0)<?3 THEN ?4 ELSE failing_since END,failed_at=?4,failed_rpc=?5 WHERE beacon=?6 AND round=?7 AND state='pending' RETURNING failing_since")
        .bind(now.saturating_add(i64::try_from(drand::RETRY_SECONDS)?))
        .bind(message)
        .bind(now.saturating_sub(i64::try_from(drand::RUN_GAP_SECONDS)?))
        .bind(now)
        .bind(i64::from(chain_read))
        .bind(i64::from(beacon))
        .bind(i64::try_from(round)?)
        .fetch_optional(pool)
        .await?;
    Ok(since == Some(now))
}
impl Launch {
    /// Fetch the round and write the outcome to its row. Only a write to the journal that fails is an error.
    async fn run(self) -> Result<()> {
        let previous = work(&self.pool, self.beacon, self.round)
            .await?
            .and_then(|row| row.failed_at)
            .and_then(|at| u64::try_from(at).ok());
        let started = self.clock.now_ms();
        let outcome = match self.on_chain().await {
            Ok(OnChain::Signature(signature)) => Ok((signature, "chain")),
            Ok(OnChain::Missing) => self
                .relays
                .fetch_round(&self.network, self.round, started / 1_000, |signature| {
                    self.check(signature)
                })
                .await
                .map(|signature| (signature, "relays")),
            // The coordinator holds the round's randomness, and its event was not found from the oldest live request's
            // block on (review L1): a relay's signature is the round's when its sha256 is that randomness.
            Ok(OnChain::Randomness(verified)) => self
                .relays
                .fetch_round(&self.network, self.round, started / 1_000, |signature| {
                    std::future::ready(Ok(randomness(&signature) == verified))
                })
                .await
                .map(|signature| (signature, "relays")),
            Err(error) => Err(drand::chain_read(format!(
                "The coordinator's verified round could not be read: {error:#}"
            ))),
        };
        let now_ms = self.clock.now_ms();
        match outcome {
            Ok((signature, source)) => {
                let randomness = randomness(&signature);
                sqlx::query("UPDATE round_work SET state='verified',signature=?,randomness=?,retry_at=0,last_error=NULL,failing_since=NULL,failed_at=NULL,failed_rpc=NULL WHERE beacon=? AND round=? AND state='pending'")
                    .bind(Bytes::copy_from_slice(&signature).to_string())
                    .bind(randomness.to_string())
                    .bind(i64::from(self.beacon))
                    .bind(i64::try_from(self.round)?)
                    .execute(&self.pool)
                    .await?;
                // How long after its scheduled time the round was verified: the latency budget of design C, 3.10.
                let after_ms =
                    i128::from(now_ms) - i128::from(self.network.round_time(self.round)) * 1_000;
                tracing::info!(beacon=self.beacon,round=self.round,source,randomness=%randomness,after_scheduled_ms=%after_ms,
                    attempt=self.attempt,"Round verified");
            }
            Err(error) => {
                let now = now_ms / 1_000;
                let chain_read = drand::is_chain_read(&error);
                let message = error.to_string();
                record_failure(
                    &self.pool,
                    self.beacon,
                    self.round,
                    &message,
                    chain_read,
                    now,
                )
                .await?;
                if drand::loud(self.attempt, previous, now) {
                    tracing::warn!(beacon=self.beacon,round=self.round,attempt=self.attempt,chain_read,error=%message,"Round fetch failed; retrying while requests wait on it");
                } else {
                    tracing::debug!(beacon=self.beacon,round=self.round,attempt=self.attempt,chain_read,error=%message,"Round fetch failed; retrying while requests wait on it");
                }
            }
        }
        Ok(())
    }
    /// Whether the coordinator verifies this signature of the round: `checkRoundSignature`, with the gas written down.
    async fn check(&self, signature: [u8; 64]) -> Result<bool> {
        self.rpc
            .call_with_gas(
                self.coordinator,
                R::checkRoundSignatureCall {
                    beaconId: self.beacon,
                    round: self.round,
                    signature: Bytes::copy_from_slice(&signature),
                },
                CHECK_GAS,
            )
            .await
    }
    /// The round's signature from the chain, when the coordinator has verified the round already at the decision head
    /// (another keeper's fulfillment did): `roundRandomness` is set, and the round's `RoundVerified` event, emitted after
    /// the oldest live request of the round was made, carries the signature whose sha256 it is. `Missing` while the
    /// coordinator does not have the round. When it has the round but no such event is found from that request's block
    /// on (another submitter verified the round serving an earlier request), its randomness, which the relays' signature
    /// must hash to.
    async fn on_chain(&self) -> Result<OnChain> {
        let call = R::roundRandomnessCall {
            beaconId: self.beacon,
            round: self.round,
        };
        let verified = self.rpc.call_tag(self.coordinator, call, &self.tag).await?;
        if verified == B256::ZERO {
            return Ok(OnChain::Missing);
        }
        let request = self
            .rpc
            .call_tag(
                self.coordinator,
                R::getRoundRequestCall {
                    requestId: self.first_job,
                },
                &self.tag,
            )
            .await?;
        let topics = [
            R::RoundVerified::SIGNATURE_HASH,
            B256::from(U256::from(self.beacon)),
            B256::from(U256::from(self.round)),
        ];
        let logs = self
            .rpc
            .request(
                "eth_getLogs",
                serde_json::json!([{"address":self.coordinator,"topics":topics,
                    "fromBlock":format!("0x{:x}",request.requestBlock),"toBlock":self.tag}]),
            )
            .await?;
        // The endpoint is not trusted to have filtered: the event is the coordinator's own and names the round.
        for log in logs.as_array().into_iter().flatten() {
            let address: Option<Address> = serde_json::from_value(log["address"].clone()).ok();
            let logged: Option<Vec<B256>> = serde_json::from_value(log["topics"].clone()).ok();
            let data: Option<Bytes> = serde_json::from_value(log["data"].clone()).ok();
            let (Some(address), Some(logged), Some(data)) = (address, logged, data) else {
                continue;
            };
            if address != self.coordinator || logged[..] != topics[..] {
                continue;
            }
            let Ok(event) = R::RoundVerified::decode_raw_log(logged, &data) else {
                continue;
            };
            if let Ok(signature) = <[u8; 64]>::try_from(event.signature.as_ref())
                && event.randomness == verified
                && randomness(&signature) == verified
            {
                return Ok(OnChain::Signature(signature));
            }
        }
        tracing::info!(
            beacon = self.beacon,
            round = self.round,
            from_block = request.requestBlock,
            "The coordinator has the round, but no RoundVerified event of it from the oldest live request's block on; it is fetched from the relays and held to the coordinator's randomness"
        );
        Ok(OnChain::Randomness(verified))
    }
}
/// What the chain says of a round (`Launch::on_chain`).
enum OnChain {
    /// The coordinator does not have the round.
    Missing,
    /// The signature its `RoundVerified` event carries.
    Signature([u8; 64]),
    /// The coordinator has the round, with this randomness, and no event of it was found.
    Randomness(B256),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> RoundRequest {
        RoundRequest {
            consumer: Address::repeat_byte(0xa1),
            callbackGasLimit: 100_000,
            requestBlock: 1_000,
            deadline: 4_000_001_060,
            clientSeed: B256::repeat_byte(1),
            mappingHash: B256::repeat_byte(2),
            beaconId: 0,
            round: 7,
            ..RoundRequest::default()
        }
    }

    #[test]
    fn the_key_hash_is_the_coordinators_hash_of_the_two_words_of_the_key() {
        let key = [U256::from(3), U256::from(5)];
        let mut words = [0u8; 64];
        words[31] = 3;
        words[63] = 5;
        assert_eq!(key_hash(key), keccak256(words));
    }

    #[test]
    fn the_fingerprint_binds_every_seed_input_a_reorg_could_change_and_nothing_else() {
        let base = request();
        let first = fingerprint(&base);
        // The encoding is abi.encode of six words.
        let mut words = Vec::new();
        words.extend_from_slice(&[0u8; 12]);
        words.extend_from_slice(base.consumer.as_slice());
        words.extend_from_slice(base.clientSeed.as_slice());
        words.extend_from_slice(base.mappingHash.as_slice());
        words.extend_from_slice(&U256::from(base.requestBlock).to_be_bytes::<32>());
        words.extend_from_slice(&U256::from(base.beaconId).to_be_bytes::<32>());
        words.extend_from_slice(&U256::from(base.round).to_be_bytes::<32>());
        assert_eq!(first, keccak256(&words));
        let changed: [fn(&mut RoundRequest); 6] = [
            |r| r.consumer = Address::repeat_byte(0xa2),
            |r| r.clientSeed = B256::repeat_byte(9),
            |r| r.mappingHash = B256::repeat_byte(9),
            |r| r.requestBlock += 1,
            |r| r.beaconId = 1,
            |r| r.round += 1,
        ];
        for change in changed {
            let mut other = request();
            change(&mut other);
            assert_ne!(fingerprint(&other), first);
        }
        // What the chain fills in later, or the request's status, is not a seed input.
        let mut served = request();
        served.fulfilled = true;
        served.roundRandomness = B256::repeat_byte(4);
        served.deadline += 1;
        served.feePaid = U256::from(5);
        assert_eq!(fingerprint(&served), first);
    }

    #[test]
    fn the_sealing_lag_is_first_sight_less_the_block_time_in_milliseconds() {
        // The block's time is the deadline less 60 seconds.
        let deadline = 4_000_001_060;
        assert_eq!(sealing_lag_ms(deadline, 4_000_001_000_000), 0);
        assert_eq!(sealing_lag_ms(deadline, 4_000_001_001_250), 1_250);
        // A keeper clock behind the chain's reads as a negative lag, not as a large one.
        assert_eq!(sealing_lag_ms(deadline, 4_000_000_999_500), -500);
        let (assigned, from) = assignment(&request(), None, 4_000_001_003_500);
        assert_eq!(
            (
                assigned.beacon,
                assigned.round,
                assigned.sealing_lag_ms,
                assigned.seen_at
            ),
            (0, 7, 3_500, 4_000_001_003)
        );
        assert_eq!(from, LagFrom::Discovery);
        assert_eq!(assigned.fingerprint, fingerprint(&request()).to_string());
    }

    #[test]
    fn the_sealing_lag_is_measured_when_the_header_of_the_request_block_first_arrived() {
        // The header arrived 800 ms after its time; discovery saw the request 2.7 seconds later still.
        let header = Sighting {
            timestamp: 4_000_001_000,
            seen_ms: 4_000_001_000_800,
        };
        let (assigned, from) = assignment(&request(), Some(header), 4_000_001_003_500);
        assert_eq!(from, LagFrom::Header);
        assert_eq!(assigned.sealing_lag_ms, 800);
        // First sight is still discovery's: it is when the keeper first knew of the request.
        assert_eq!(assigned.seen_at, 4_000_001_003);
        // A header that arrived before its own time, by the keeper's clock, is a negative lag.
        let early = Sighting {
            seen_ms: 4_000_000_999_900,
            ..header
        };
        assert_eq!(
            assignment(&request(), Some(early), 4_000_001_003_500)
                .0
                .sealing_lag_ms,
            -100
        );
        assert_eq!(
            (LagFrom::Header.name(), LagFrom::Discovery.name()),
            ("header", "discovery")
        );
    }

    #[test]
    fn a_registration_is_fetched_only_with_a_verifier_and_a_schedule() {
        let registration = Beacon {
            verifier: Address::repeat_byte(0xbb),
            genesis: 1_727_521_075,
            period: 3,
            chainHash: B256::repeat_byte(0x11),
            publicKey: Bytes::from(vec![7u8; 128]),
        };
        assert_eq!(
            network(&registration).unwrap(),
            Network {
                chain_hash: B256::repeat_byte(0x11),
                genesis: 1_727_521_075,
                period: 3,
            }
        );
        for broken in [
            Beacon {
                verifier: Address::ZERO,
                ..registration.clone()
            },
            Beacon {
                genesis: 0,
                ..registration.clone()
            },
            Beacon {
                period: 0,
                ..registration.clone()
            },
        ] {
            assert!(network(&broken).is_err(), "{broken:?}");
        }
    }

    /// The round contracts' replay vectors (`test/fixtures/robinhood/round-replay-vectors.json` of rh/round-contracts,
    /// copied as it is): requests a local round coordinator bound to real drand evmnet rounds and served.
    fn vectors() -> serde_json::Value {
        serde_json::from_str(include_str!("../tests/fixtures/round-replay-vectors.json")).unwrap()
    }
    fn number(value: &serde_json::Value) -> U256 {
        value.as_str().unwrap().parse().unwrap()
    }

    #[test]
    fn the_seed_is_the_coordinators_for_every_request_of_the_replay_vectors() {
        let fixture = vectors();
        assert_eq!(
            seed_domain(),
            fixture["domains"]["seed"]
                .as_str()
                .unwrap()
                .parse::<B256>()
                .unwrap()
        );
        let chain_id: u64 = fixture["chainId"].as_str().unwrap().parse().unwrap();
        let coordinator: Address = fixture["coordinator"].as_str().unwrap().parse().unwrap();
        let public_key = [
            number(&fixture["protocolConfiguration"]["publicKey"][0]),
            number(&fixture["protocolConfiguration"]["publicKey"][1]),
        ];
        let hash = key_hash(public_key);
        assert_eq!(
            hash,
            fixture["keyHash"]
                .as_str()
                .unwrap()
                .parse::<B256>()
                .unwrap()
        );
        // The prover's fixture key is the key the vectors were proved with.
        let key =
            k256::SecretKey::from_slice(&U256::from(123_456_789u64).to_be_bytes::<32>()).unwrap();
        assert_eq!(crate::prover::public_key(&key), public_key);
        let vectors = fixture["vectors"].as_array().unwrap();
        assert_eq!(vectors.len(), 7);
        for vector in vectors {
            let name = vector["name"].as_str().unwrap();
            let request = RoundRequest {
                consumer: vector["consumer"].as_str().unwrap().parse().unwrap(),
                callbackGasLimit: vector["callbackGasLimit"].as_u64().unwrap() as u32,
                requestBlock: vector["requestBlock"].as_str().unwrap().parse().unwrap(),
                deadline: vector["deadline"].as_str().unwrap().parse().unwrap(),
                refundAddress: vector["refundAddress"].as_str().unwrap().parse().unwrap(),
                clientSeed: vector["clientSeed"].as_str().unwrap().parse().unwrap(),
                mappingHash: vector["mappingHash"].as_str().unwrap().parse().unwrap(),
                beaconId: vector["beaconId"].as_u64().unwrap() as u8,
                round: vector["round"].as_str().unwrap().parse().unwrap(),
                ..RoundRequest::default()
            };
            let signature: Bytes = vector["roundSignature"].as_str().unwrap().parse().unwrap();
            let round_randomness = randomness(&signature);
            assert_eq!(
                round_randomness,
                vector["roundRandomness"]
                    .as_str()
                    .unwrap()
                    .parse::<B256>()
                    .unwrap(),
                "{name}"
            );
            let id = number(&vector["requestId"]);
            let computed = seed(chain_id, coordinator, hash, id, &request, round_randomness);
            assert_eq!(computed, number(&vector["seed"]), "{name}");
            assert_eq!(computed, number(&vector["proof"]["seed"]), "{name}");
            // Any other input is another seed: the chain, the coordinator, the key, the id and the round's randomness.
            for other in [
                seed(
                    chain_id + 1,
                    coordinator,
                    hash,
                    id,
                    &request,
                    round_randomness,
                ),
                seed(
                    chain_id,
                    Address::repeat_byte(1),
                    hash,
                    id,
                    &request,
                    round_randomness,
                ),
                seed(
                    chain_id,
                    coordinator,
                    B256::repeat_byte(1),
                    id,
                    &request,
                    round_randomness,
                ),
                seed(
                    chain_id,
                    coordinator,
                    hash,
                    id + U256::from(1),
                    &request,
                    round_randomness,
                ),
                seed(
                    chain_id,
                    coordinator,
                    hash,
                    id,
                    &request,
                    B256::repeat_byte(1),
                ),
            ] {
                assert_ne!(other, computed, "{name}");
            }
            // The unchanged prover proves the seed with the point the coordinator accepted: the VRF output is a function
            // of the key and the seed alone.
            let proof = crate::prover::prove(computed, &key).unwrap();
            assert_eq!(proof.seed, computed, "{name}");
            assert_eq!(
                proof.gamma,
                [
                    number(&vector["proof"]["gamma"][0]),
                    number(&vector["proof"]["gamma"][1])
                ],
                "{name}"
            );
        }
    }

    #[test]
    fn a_fulfillment_always_carries_its_rounds_signature_and_a_batch_lists_each_round_once() {
        use alloy_sol_types::SolCall;
        let proof = |seed: u64| RoundProof {
            seed: U256::from(seed),
            ..RoundProof::default()
        };
        let signature = |round: u64| Bytes::from(vec![round as u8; 64]);
        let single = single_call(U256::from(7), proof(1), signature(9));
        let decoded = R::fulfillRandomnessCall::abi_decode(&single).unwrap();
        assert_eq!(
            (
                decoded.requestId,
                decoded.proof.seed,
                decoded.roundSignature
            ),
            (U256::from(7), U256::from(1), signature(9))
        );
        let member = |id: u64, beacon: u8, round: u64| Member {
            id: U256::from(id),
            proof: proof(id),
            beacon,
            round,
            signature: signature(round),
        };
        let batch = batch_call(&[
            member(3, 0, 9),
            member(4, 0, 10),
            member(5, 0, 9),
            member(6, 1, 9),
        ]);
        let decoded = R::fulfillRandomnessBatchCall::abi_decode(&batch).unwrap();
        let rounds: Vec<(u8, u64, Bytes)> = decoded
            .rounds
            .into_iter()
            .map(|listed| (listed.beaconId, listed.round, listed.signature))
            .collect();
        assert_eq!(
            rounds,
            [
                (0, 9, signature(9)),
                (0, 10, signature(10)),
                (1, 9, signature(9))
            ]
        );
        assert_eq!(decoded.ids, [3, 4, 5, 6].map(U256::from).to_vec());
        assert_eq!(
            decoded.proofs.iter().map(|p| p.seed).collect::<Vec<_>>(),
            [3, 4, 5, 6].map(U256::from).to_vec()
        );
        // What a prepared job keeps reads back as it was written.
        let prepared = Prepared {
            fingerprint: B256::repeat_byte(5),
            proof: proof(11),
        };
        let back: Prepared =
            serde_json::from_str(&serde_json::to_string(&prepared).unwrap()).unwrap();
        assert_eq!(
            (back.fingerprint, back.proof.seed),
            (B256::repeat_byte(5), U256::from(11))
        );
    }

    #[test]
    fn a_rounds_randomness_is_the_sha256_of_its_signature() {
        // drand's randomness of a round is sha256 of its signature: of the empty string, a known digest.
        assert_eq!(
            randomness(b""),
            "0xe3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .parse::<B256>()
                .unwrap()
        );
        assert_eq!(
            randomness(b"abc"),
            "0xba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
                .parse::<B256>()
                .unwrap()
        );
    }

    #[test]
    fn the_clock_is_the_systems_until_a_test_sets_it() {
        let clock = Clock::default();
        let system = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(clock.now_ms().abs_diff(system) < 60_000);
        let shared = clock.clone();
        shared.set(4_000_000_000_123);
        assert_eq!(clock.now_ms(), 4_000_000_000_123);
    }

    /// The round lane uses the chain-neutral drand client alone: none of the epoch lane's recipe, registry, epoch or
    /// template types, and nothing of `beacon.rs` or `epoch.rs`. And the drand client knows none of them either.
    #[test]
    fn the_round_lane_and_the_drand_client_use_nothing_of_the_epoch_lane() {
        let production =
            |source: &'static str| source.split("#[cfg(test)]\nmod tests").next().unwrap();
        for (file, source) in [
            ("round.rs", production(include_str!("round.rs"))),
            ("drand.rs", production(include_str!("drand.rs"))),
        ] {
            for word in [
                "crate::beacon",
                "crate::epoch",
                "crate::abi::",
                "crate::template",
                "abi::{",
                "EpochRegistry",
                "RegisteredRecipe",
                "ApiProof",
                "EpochSelection",
                "verifyBeacon",
                "beaconOf",
                "epoch_",
                "recipe",
                "Recipe",
                "registry",
                "Registry",
            ] {
                assert!(!source.contains(word), "{file} names `{word}`");
            }
        }
        // The drand client names no contract of either coordinator.
        let drand = production(include_str!("drand.rs"));
        for word in [
            "abi_round",
            "RoundCoordinator",
            "Coordinator",
            "crate::round",
        ] {
            assert!(!drand.contains(word), "drand.rs names `{word}`");
        }
    }
}
