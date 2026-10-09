use alloy_primitives::{Address, B256};
use anyhow::{Context, Result, ensure};
use std::{env, ops::RangeInclusive, path::PathBuf};

/// Which transaction wallet authorization a keeper runs with. A primary is the registry's committer() and sends as
/// soon as work is ready, earliest deadline first. A follower is an allowed backup committer with its own wallet and
/// journal: it prepares the same work but joins the queue from its newest end only when the join rule fires, so the
/// two fronts never work the same requests until they meet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Primary,
    Follower(FollowerPlan),
}
/// How a follower decides to join, and which slice of the tail it takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FollowerPlan {
    /// Age of the oldest unserved request at which this follower joins the queue.
    pub delay: u64,
    /// Visible unserved queue length above which it joins whatever the ages are.
    pub queue_join: u64,
    /// Seconds of sendable work waiting without a committer nonce advance after which the primary counts as dead.
    pub liveness: u64,
    /// This follower's lane: with `lanes` follower lanes it takes the request ids where id % lanes == rank.
    pub rank: u64,
    pub lanes: u64,
}
/// The coordinator's response window: a request's deadline is its creation time plus this many seconds.
pub const RESPONSE_TIMEOUT_SECONDS: u64 = 60;
/// Bounds for FOLLOWER_DELAY_SECONDS: long enough for a healthy primary to be seen serving the oldest request,
/// short enough that at least 30 of a request's 60 seconds remain for a publication, the target block and the proof.
pub const FOLLOWER_DELAY_RANGE: std::ops::RangeInclusive<u64> = 5..=30;
/// Bounds for PRIMARY_LIVENESS_SECONDS, the time sendable work may wait without a committer nonce advance before the
/// primary counts as dead. It must exceed the primary's gap between transactions while it works through a burst
/// (measured at most 3 seconds between batches, about 6 seconds for an idle epoch's publish-then-serve pipeline).
pub const PRIMARY_LIVENESS_RANGE: std::ops::RangeInclusive<u64> = 5..=30;
/// Bounds for FOLLOWER_QUEUE_JOIN, the visible unserved queue above which a follower joins from the tail.
pub const FOLLOWER_QUEUE_JOIN_RANGE: std::ops::RangeInclusive<u64> = 1..=10_000;
/// The registry allows four backup committers, so at most four follower lanes can exist beside the primary.
pub const MAX_FOLLOWER_LANES: u64 = 4;
/// Every node, whatever its role or lane, sends a request this close to its deadline: the last line before a refund.
pub const SAFETY_AGE_SECONDS: u64 = 20;
/// Hysteresis: a follower that has joined leaves again only when the queue is this short and this young.
pub const FOLLOWER_LEAVE_PENDING: u64 = 32;
pub const FOLLOWER_LEAVE_AGE_SECONDS: u64 = 10;
/// Time a node keeps for one fulfillment round: target block, proof transaction and receipt.
pub const FULFILLMENT_ROUND_SECONDS: u64 = 10;
impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Follower { .. } => "follower",
        }
    }
    pub fn is_follower(self) -> bool {
        matches!(self, Self::Follower { .. })
    }
    /// `margin` is SEND_MARGIN_SECONDS: the join delay, the margin and one fulfillment round must fit the response
    /// window, and the margin and that round must fit inside the safety age every node honours.
    fn parse(role: Option<&str>, timing: FollowerSettings<'_>, margin: u64) -> Result<Self> {
        let number = |value: Option<&str>, default: &str, name: &str| -> Result<u64> {
            value
                .unwrap_or(default)
                .trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid {name}"))
        };
        ensure!(
            timing.busy_delay.is_none(),
            "FOLLOWER_BUSY_DELAY_SECONDS was replaced by the join rule: use FOLLOWER_DELAY_SECONDS, FOLLOWER_QUEUE_JOIN and PRIMARY_LIVENESS_SECONDS"
        );
        match role.map(str::trim).unwrap_or("primary") {
            "primary" => {
                ensure!(
                    timing.delay.is_none()
                        && timing.queue_join.is_none()
                        && timing.liveness.is_none()
                        && timing.rank.is_none()
                        && timing.lanes.is_none(),
                    "FOLLOWER_DELAY_SECONDS, FOLLOWER_QUEUE_JOIN, PRIMARY_LIVENESS_SECONDS, FOLLOWER_RANK and FOLLOWER_LANES apply only with KEEPER_ROLE=follower"
                );
                Ok(Self::Primary)
            }
            "follower" => {
                let delay = number(timing.delay, "20", "FOLLOWER_DELAY_SECONDS")?;
                let queue_join = number(timing.queue_join, "150", "FOLLOWER_QUEUE_JOIN")?;
                let liveness = number(timing.liveness, "10", "PRIMARY_LIVENESS_SECONDS")?;
                let rank = number(timing.rank, "0", "FOLLOWER_RANK")?;
                let lanes = number(timing.lanes, "1", "FOLLOWER_LANES")?;
                ensure!(
                    FOLLOWER_DELAY_RANGE.contains(&delay),
                    "FOLLOWER_DELAY_SECONDS must be between {} and {} so a takeover still fits the 60-second request deadline",
                    FOLLOWER_DELAY_RANGE.start(),
                    FOLLOWER_DELAY_RANGE.end()
                );
                ensure!(
                    delay + margin + FULFILLMENT_ROUND_SECONDS <= RESPONSE_TIMEOUT_SECONDS,
                    "FOLLOWER_DELAY_SECONDS, SEND_MARGIN_SECONDS and a {FULFILLMENT_ROUND_SECONDS}-second fulfillment round must fit the 60-second request deadline"
                );
                ensure!(
                    margin + FULFILLMENT_ROUND_SECONDS <= SAFETY_AGE_SECONDS,
                    "SEND_MARGIN_SECONDS must leave the {SAFETY_AGE_SECONDS}-second safety age room for a {FULFILLMENT_ROUND_SECONDS}-second fulfillment round"
                );
                ensure!(
                    FOLLOWER_QUEUE_JOIN_RANGE.contains(&queue_join),
                    "FOLLOWER_QUEUE_JOIN must be between {} and {}",
                    FOLLOWER_QUEUE_JOIN_RANGE.start(),
                    FOLLOWER_QUEUE_JOIN_RANGE.end()
                );
                ensure!(
                    PRIMARY_LIVENESS_RANGE.contains(&liveness),
                    "PRIMARY_LIVENESS_SECONDS must be between {} and {}",
                    PRIMARY_LIVENESS_RANGE.start(),
                    PRIMARY_LIVENESS_RANGE.end()
                );
                ensure!(
                    (1..=MAX_FOLLOWER_LANES).contains(&lanes) && rank < lanes,
                    "FOLLOWER_LANES must be 1 to {MAX_FOLLOWER_LANES} and FOLLOWER_RANK a lane below it: each follower takes the request ids where id % lanes == rank"
                );
                Ok(Self::Follower(FollowerPlan {
                    delay,
                    queue_join,
                    liveness,
                    rank,
                    lanes,
                }))
            }
            _ => anyhow::bail!("KEEPER_ROLE must be primary or follower"),
        }
    }
}
/// The raw follower settings, as read from the environment.
#[derive(Clone, Copy, Default)]
struct FollowerSettings<'a> {
    delay: Option<&'a str>,
    busy_delay: Option<&'a str>,
    queue_join: Option<&'a str>,
    liveness: Option<&'a str>,
    rank: Option<&'a str>,
    lanes: Option<&'a str>,
}
#[derive(Clone)]
pub struct Config {
    pub role: Role,
    pub telemetry: Option<crate::telemetry::Settings>,
    pub rpc_urls: Vec<String>,
    /// Optional WebSocket endpoints for pushed blocks and events; HTTP stays the source of truth.
    pub ws_urls: Vec<String>,
    pub chain_id: u64,
    pub coordinator: Address,
    pub db: PathBuf,
    pub lock_dir: PathBuf,
    pub tx_key_file: PathBuf,
    pub vrf_key_file: PathBuf,
    pub send: bool,
    pub once: bool,
    pub poll_ms: u64,
    /// Polling interval while nothing is open and no event subscription is live.
    pub idle_poll_ms: u64,
    /// The tick interval while nothing is open and the event subscription is live: IDLE_HEARTBEAT_SECONDS for a round
    /// keeper, `events::IDLE_HEARTBEAT` for an epoch keeper.
    pub idle_heartbeat: std::time::Duration,
    pub max_tick_failures: u64,
    pub tick_timeout_seconds: u64,
    pub margin: u64,
    pub max_gas: u64,
    pub max_fee: u128,
    pub cancel_max_fee: u128,
    pub max_cost: u128,
    /// Bounds for the tip taken from the recent median; the maximum also stays within max_fee.
    pub min_priority_fee: u128,
    pub max_priority_fee: u128,
    /// Escrowed fees must cover this share (bps) of a fulfillment's expected cost; 0 disables.
    pub fee_coverage_bps: u64,
    pub nonce_stuck_seconds: u64,
    pub progress_stuck_seconds: u64,
    /// Prepared requests one fulfillment transaction may carry; 1 keeps the single path only.
    pub fulfill_batch_max: usize,
    pub code_hash: Option<B256>,
    pub protocol_hash: Option<B256>,
    pub implementation_code_hash: Option<B256>,
    pub registry_implementation_code_hash: Option<B256>,
    /// Runtime code hashes of implementations approved ahead of an in-place upgrade. Startup accepts either the pin or
    /// this hash; a running keeper that sees its proxy move to this hash exits for a verified restart.
    pub approved_next_implementation_code_hash: Option<B256>,
    pub approved_next_registry_implementation_code_hash: Option<B256>,
    /// The drand relays beacon epochs read their rounds from.
    pub drand_relays: crate::drand::DrandRelays,
    /// The chain-specific behaviour settings; their defaults are the behaviour of 0.4.1.
    pub chain: ChainSettings,
}
fn required(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("Missing {name}"))
}
fn number<T: std::str::FromStr>(name: &str, default: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    env::var(name)
        .unwrap_or_else(|_| default.into())
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid {name}: {e}"))
}
/// Settings earlier releases read and this one does not, with what replaced them. A keeper environment that still sets
/// one starts normally and is told once, so that an operator can remove it.
const REMOVED_SETTINGS: [(&str, &str); 1] = [(
    "EPOCH_API_ENDPOINTS",
    "signed API recipes are not supported since 0.4.1, and epoch rounds are read from DRAND_RELAYS",
)];
/// The removed settings `is_set` finds in an environment, in the order of REMOVED_SETTINGS.
fn removed_settings(is_set: impl Fn(&str) -> bool) -> Vec<(&'static str, &'static str)> {
    REMOVED_SETTINGS
        .into_iter()
        .filter(|(name, _)| is_set(name))
        .collect()
}
/// Settings this release validates but does not act on for a keeper of `kind`, with what it does instead. A keeper
/// environment that sets one starts normally and is told once, so that nobody counts on it: the explorer index follows
/// the finalized head with its own window whatever `INDEX_*` say, and an epoch keeper keeps 0.4.1's subscription limits
/// and sweep reserve.
fn unused_settings(
    kind: CoordinatorKind,
    is_set: impl Fn(&str) -> bool,
) -> Vec<(&'static str, &'static str)> {
    const INDEX: &str = "the explorer index follows the finalized head and reads at most 128 new blocks a scan with a lookback of 12";
    const EPOCH: &str =
        "an epoch coordinator's keeper keeps its own subscription limits and sweep reserve";
    let mut unused = vec![
        ("INDEX_FINALITY", INDEX),
        ("INDEX_MAX_BLOCKS", INDEX),
        ("INDEX_LOOKBACK_BLOCKS", INDEX),
    ];
    if kind == CoordinatorKind::Epoch {
        unused.extend([
            ("WS_SILENCE_SECONDS", EPOCH),
            ("WS_BACKFILL_MAX_BLOCKS", EPOCH),
            ("WS_BACKFILL_RANGE", EPOCH),
            ("SWEEP_MIN_RESERVE_WEI", EPOCH),
        ]);
    }
    unused.retain(|(name, _)| is_set(name));
    unused
}

/// `urls` with each endpoint once, in the order it is first listed. A trailing `/` does not make another endpoint.
fn distinct_urls(urls: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    urls.into_iter()
        .filter(|url| seen.insert(url.trim_end_matches('/').to_owned()))
        .collect()
}
/// Which block the keeper decides on. `Finalized` is Arc's deterministic finality and the only mode 0.4.1 has; `Soft`
/// acts on the sequencer's latest block and audits it against L1 finality later (Robinhood Chain).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FinalityMode {
    #[default]
    Finalized,
    Soft,
}
/// How a transaction's gas limit is chosen. `Standard` is 0.4.1's; `Arbitrum` adds the L1 component that an Arbitrum
/// chain charges before execution to every gas floor and every cancellation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GasModel {
    #[default]
    Standard,
    Arbitrum,
}
/// Which coordinator the keeper serves. `Epoch` is the coordinator of 0.4.1 (Arc): a request waits for an epoch that a
/// registry publishes, and its proof input names a target block. `Round` is a round coordinator (Robinhood Chain): it
/// binds each request, when it is made, to a future round of a drand beacon, and has no registry, no epochs and no
/// target block. In round mode the keeper reads the round coordinator through its own ABI (`abi_round`) and never an
/// epoch coordinator's or a registry's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CoordinatorKind {
    #[default]
    Epoch,
    Round,
}
impl CoordinatorKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Epoch => "epoch",
            Self::Round => "round",
        }
    }
}
/// The words of COORDINATOR_KIND.
const COORDINATOR_KINDS: [(&str, CoordinatorKind); 2] = [
    ("epoch", CoordinatorKind::Epoch),
    ("round", CoordinatorKind::Round),
];
/// The settings of an epoch coordinator that a round coordinator has nothing to apply to: the pin and the approved next
/// implementation of a registry, and the settings of the epoch design that came before the round design (the registry
/// kind, the block nudge and the signed API endpoints). With COORDINATOR_KIND=round each is refused rather than ignored.
pub const EPOCH_ONLY_SETTINGS: [&str; 6] = [
    "EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH",
    "APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH",
    "REGISTRY_KIND",
    "BLOCK_NUDGE",
    "BLOCK_NUDGE_AFTER_MS",
    "EPOCH_API_ENDPOINTS",
];
/// Which block the public explorer index follows. `Soft` needs FINALITY_MODE=soft.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IndexFinality {
    #[default]
    Finalized,
    Soft,
}
/// Bounds of the chain settings that are not words.
pub const SOFT_DEPTH_RANGE: RangeInclusive<u64> = 0..=16;
pub const FINALITY_AUDIT_INTERVAL_RANGE: RangeInclusive<u64> = 5..=300;
pub const FINALITY_AUDIT_MAX_LAG_RANGE: RangeInclusive<u64> = 600..=7_200;
pub const L1_GAS_MARGIN_RANGE: RangeInclusive<u64> = 0..=20_000;
pub const SEQUENCER_DROP_RANGE: RangeInclusive<u64> = 2..=60;
pub const WS_SILENCE_RANGE: RangeInclusive<u64> = 5..=600;
pub const WS_BACKFILL_MAX_BLOCKS_RANGE: RangeInclusive<u64> = 1..=100_000;
pub const WS_BACKFILL_RANGE_BLOCKS: RangeInclusive<u64> = 1..=10_000;
pub const INDEX_MAX_BLOCKS_RANGE: RangeInclusive<u64> = 1..=10_000;
pub const INDEX_LOOKBACK_RANGE: RangeInclusive<u64> = 1..=10_000;

/// The settings that select chain-specific behaviour: finality, gas model, coordinator kind, sequencer handling, units
/// and the chain clock. They are parsed and carried here; every default is the behaviour of 0.4.1, so a keeper that
/// sets none of them runs exactly as before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainSettings {
    pub finality_mode: FinalityMode,
    /// Soft mode decides at `latest - depth`; 0 keeps chains that make blocks only on demand from stalling.
    pub soft_depth_blocks: u64,
    pub finality_audit_interval_seconds: u64,
    pub finality_audit_max_lag_seconds: u64,
    pub gas_model: GasModel,
    /// The share (bps) added to every L1 gas component the arbitrum model reads.
    pub l1_gas_margin_bps: u64,
    pub coordinator_kind: CoordinatorKind,
    /// A sent transaction is treated as dropped by the sequencer after this long; unset keeps the fee-bump path only.
    pub sequencer_drop_seconds: Option<u64>,
    pub sweep_min_reserve_wei: u128,
    pub ws_silence_seconds: u64,
    pub ws_backfill_max_blocks: u64,
    pub ws_backfill_range: u64,
    pub index_finality: IndexFinality,
    pub index_max_blocks: u64,
    pub index_lookback_blocks: u64,
}
impl Default for ChainSettings {
    fn default() -> Self {
        Self {
            finality_mode: FinalityMode::Finalized,
            soft_depth_blocks: 0,
            finality_audit_interval_seconds: 30,
            finality_audit_max_lag_seconds: 2_700,
            gas_model: GasModel::Standard,
            l1_gas_margin_bps: 2_500,
            coordinator_kind: CoordinatorKind::Epoch,
            sequencer_drop_seconds: None,
            // events.rs, explorer.rs and sweep.rs carry these as constants until the settings are wired.
            sweep_min_reserve_wei: 1_000_000_000_000_000_000,
            ws_silence_seconds: 20,
            ws_backfill_max_blocks: 5_000,
            ws_backfill_range: 500,
            index_finality: IndexFinality::Finalized,
            index_max_blocks: 128,
            index_lookback_blocks: 12,
        }
    }
}
/// What a known chain requires of the three behaviour switches; `None` accepts either value. This stops a keeper from
/// silently running a chain with another chain's defaults.
struct ChainPolicy {
    name: &'static str,
    ids: &'static [u64],
    finality: Option<FinalityMode>,
    gas_model: Option<GasModel>,
    coordinator: Option<CoordinatorKind>,
    /// The settings an operator must write down on this chain, each with the default it would silently take instead.
    explicit: &'static [(&'static str, &'static str)],
    /// Whether the chain has no tip: its sequencer orders first come first served, so both priority fee bounds are 0.
    tipless: bool,
    /// The least L1_GAS_MARGIN_BPS the chain accepts, for a chain that charges an L1 component: with none, a quote
    /// of that component that the L1 price outgrows before the send leaves a gas limit short.
    min_l1_margin_bps: Option<u64>,
    /// The least FEE_COVERAGE_BPS the chain accepts: the share of a fulfillment's expected cost that the escrowed fees
    /// must cover before it is sent.
    min_fee_coverage_bps: Option<u64>,
}
/// The settings whose defaults are sized for Arc, where the gas is USDC: a fee cap of 100 gwei, a transaction cost of
/// 0.2, a tip between 1 and 50 gwei, a sweep reserve of 1, a stuck nonce after 120 seconds and 3,000,000 gas. On a
/// chain whose gas is ETH at a fraction of a gwei they are not a conservative choice but a wrong one, so a keeper on
/// such a chain, and every round-mode keeper whatever its chain, does not start until each is written down. Round mode
/// has no defaults of its own for them: what the gas of a chain costs is the operator's profile to write, not a number
/// this binary would have to keep in step with the market.
const ARC_SCALED: &[(&str, &str)] = &[
    ("MAX_FEE_PER_GAS_WEI", "100 gwei"),
    ("MAX_TX_COST_WEI", "0.2 of the native token"),
    ("MIN_PRIORITY_FEE_WEI", "1 gwei"),
    ("MAX_PRIORITY_FEE_WEI", "50 gwei"),
    ("SWEEP_MIN_RESERVE_WEI", "1 of the native token"),
    ("NONCE_STUCK_SECONDS", "120 seconds"),
    ("MAX_GAS", "3000000 gas"),
];
/// What Robinhood Chain requires written down: every Arc-scaled setting, and the fee coverage, which it also requires to
/// be at least `ROBINHOOD_MIN_FEE_COVERAGE_BPS`.
const ROBINHOOD_EXPLICIT: &[(&str, &str)] = &[
    ("MAX_FEE_PER_GAS_WEI", "100 gwei"),
    ("MAX_TX_COST_WEI", "0.2 of the native token"),
    ("MIN_PRIORITY_FEE_WEI", "1 gwei"),
    ("MAX_PRIORITY_FEE_WEI", "50 gwei"),
    ("SWEEP_MIN_RESERVE_WEI", "1 of the native token"),
    ("NONCE_STUCK_SECONDS", "120 seconds"),
    ("MAX_GAS", "3000000 gas"),
    ("FEE_COVERAGE_BPS", "10000 bps"),
];
/// The least fee coverage on Robinhood Chain: escrowed fees cover a quarter more than a fulfillment's expected cost, the
/// margin the round coordinator's pricing is sized for (design C, 3.3 and 3.6).
pub const ROBINHOOD_MIN_FEE_COVERAGE_BPS: u64 = 12_500;
/// Arc is finalized/standard/epoch and Robinhood Chain soft/arbitrum/round; the local test chain and nitro-testnode
/// take any value. A chain that is not listed behaves as 0.4.1 did: it reads finalized state, so `soft` is refused
/// there, and nothing else is constrained.
const CHAIN_POLICIES: [ChainPolicy; 4] = [
    ChainPolicy {
        name: "Arc",
        ids: &[5042, 5042002],
        finality: Some(FinalityMode::Finalized),
        gas_model: Some(GasModel::Standard),
        coordinator: Some(CoordinatorKind::Epoch),
        explicit: &[],
        tipless: false,
        min_l1_margin_bps: None,
        min_fee_coverage_bps: None,
    },
    ChainPolicy {
        name: "Robinhood Chain",
        ids: &[4663, 46630],
        finality: Some(FinalityMode::Soft),
        gas_model: Some(GasModel::Arbitrum),
        coordinator: Some(CoordinatorKind::Round),
        explicit: ROBINHOOD_EXPLICIT,
        tipless: true,
        min_l1_margin_bps: Some(1_000),
        min_fee_coverage_bps: Some(ROBINHOOD_MIN_FEE_COVERAGE_BPS),
    },
    ChainPolicy {
        name: "the local test chain",
        ids: &[31337],
        finality: None,
        gas_model: None,
        coordinator: None,
        explicit: &[],
        tipless: false,
        min_l1_margin_bps: None,
        min_fee_coverage_bps: None,
    },
    ChainPolicy {
        name: "nitro-testnode",
        ids: &[412346],
        finality: None,
        gas_model: None,
        coordinator: None,
        explicit: &[],
        tipless: false,
        min_l1_margin_bps: None,
        min_fee_coverage_bps: None,
    },
];
impl FinalityMode {
    fn name(self) -> &'static str {
        match self {
            Self::Finalized => "finalized",
            Self::Soft => "soft",
        }
    }
}
impl GasModel {
    fn name(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Arbitrum => "arbitrum",
        }
    }
}
/// One setting's text: trimmed, and unset when empty.
fn setting(get: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    get(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}
/// A word out of `options`, or `default` when the setting is unset.
fn choice<T: Copy>(
    get: &dyn Fn(&str) -> Option<String>,
    name: &str,
    default: T,
    options: &[(&str, T)],
) -> Result<T> {
    let Some(text) = setting(get, name) else {
        return Ok(default);
    };
    options
        .iter()
        .find(|(word, _)| *word == text)
        .map(|(_, value)| *value)
        .ok_or_else(|| {
            let words: Vec<&str> = options.iter().map(|(word, _)| *word).collect();
            anyhow::anyhow!("{name} must be {}", words.join(" or "))
        })
}
/// A whole number within `range`, or `None` when the setting is unset.
fn bounded(
    get: &dyn Fn(&str) -> Option<String>,
    name: &str,
    range: &RangeInclusive<u64>,
) -> Result<Option<u64>> {
    let Some(text) = setting(get, name) else {
        return Ok(None);
    };
    let value: u64 = text
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid {name}: {e}"))?;
    ensure!(
        range.contains(&value),
        "{name} must be between {} and {}",
        range.start(),
        range.end()
    );
    Ok(Some(value))
}
impl ChainSettings {
    /// The settings of an environment (`get` answers a variable's value, or `None` when it is unset; an empty value
    /// counts as unset) on chain `chain_id`: each within its bounds, the three behaviour switches within the chain's
    /// policy, and every setting that belongs to a mode only with that mode on. A keeper that cannot run as
    /// configured refuses to start instead of falling back to another chain's behaviour.
    pub fn resolve(chain_id: u64, get: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        let default = Self::default();
        let finality_mode = choice(
            get,
            "FINALITY_MODE",
            default.finality_mode,
            &[
                ("finalized", FinalityMode::Finalized),
                ("soft", FinalityMode::Soft),
            ],
        )?;
        let soft_depth = bounded(get, "SOFT_DEPTH_BLOCKS", &SOFT_DEPTH_RANGE)?;
        let audit_interval = bounded(
            get,
            "FINALITY_AUDIT_INTERVAL_SECONDS",
            &FINALITY_AUDIT_INTERVAL_RANGE,
        )?;
        let audit_max_lag = bounded(
            get,
            "FINALITY_AUDIT_MAX_LAG_SECONDS",
            &FINALITY_AUDIT_MAX_LAG_RANGE,
        )?;
        let gas_model = choice(
            get,
            "GAS_MODEL",
            default.gas_model,
            &[
                ("standard", GasModel::Standard),
                ("arbitrum", GasModel::Arbitrum),
            ],
        )?;
        let l1_margin = bounded(get, "L1_GAS_MARGIN_BPS", &L1_GAS_MARGIN_RANGE)?;
        let coordinator_kind = Self::coordinator_kind(get)?;
        let sequencer_drop = bounded(get, "SEQUENCER_DROP_SECONDS", &SEQUENCER_DROP_RANGE)?;
        let sweep_min_reserve_wei = match setting(get, "SWEEP_MIN_RESERVE_WEI") {
            Some(text) => text
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid SWEEP_MIN_RESERVE_WEI: {e}"))?,
            None => default.sweep_min_reserve_wei,
        };
        let ws_silence = bounded(get, "WS_SILENCE_SECONDS", &WS_SILENCE_RANGE)?;
        let ws_backfill_max =
            bounded(get, "WS_BACKFILL_MAX_BLOCKS", &WS_BACKFILL_MAX_BLOCKS_RANGE)?;
        let ws_backfill_range = bounded(get, "WS_BACKFILL_RANGE", &WS_BACKFILL_RANGE_BLOCKS)?;
        let index_finality = choice(
            get,
            "INDEX_FINALITY",
            default.index_finality,
            &[
                ("finalized", IndexFinality::Finalized),
                ("soft", IndexFinality::Soft),
            ],
        )?;
        let index_max_blocks = bounded(get, "INDEX_MAX_BLOCKS", &INDEX_MAX_BLOCKS_RANGE)?;
        let index_lookback = bounded(get, "INDEX_LOOKBACK_BLOCKS", &INDEX_LOOKBACK_RANGE)?;
        let result = Self {
            finality_mode,
            soft_depth_blocks: soft_depth.unwrap_or(default.soft_depth_blocks),
            finality_audit_interval_seconds: audit_interval
                .unwrap_or(default.finality_audit_interval_seconds),
            finality_audit_max_lag_seconds: audit_max_lag
                .unwrap_or(default.finality_audit_max_lag_seconds),
            gas_model,
            l1_gas_margin_bps: l1_margin.unwrap_or(default.l1_gas_margin_bps),
            coordinator_kind,
            sequencer_drop_seconds: sequencer_drop,
            sweep_min_reserve_wei,
            ws_silence_seconds: ws_silence.unwrap_or(default.ws_silence_seconds),
            ws_backfill_max_blocks: ws_backfill_max.unwrap_or(default.ws_backfill_max_blocks),
            ws_backfill_range: ws_backfill_range.unwrap_or(default.ws_backfill_range),
            index_finality,
            index_max_blocks: index_max_blocks.unwrap_or(default.index_max_blocks),
            index_lookback_blocks: index_lookback.unwrap_or(default.index_lookback_blocks),
        };
        ensure!(
            result.ws_backfill_range <= result.ws_backfill_max_blocks,
            "WS_BACKFILL_RANGE must not exceed WS_BACKFILL_MAX_BLOCKS"
        );
        result.check_chain_policy(chain_id, get)?;
        // A setting that belongs to one mode is a mistake without it, and says so rather than being ignored.
        for (name, present) in [
            ("SOFT_DEPTH_BLOCKS", soft_depth.is_some()),
            ("FINALITY_AUDIT_INTERVAL_SECONDS", audit_interval.is_some()),
            ("FINALITY_AUDIT_MAX_LAG_SECONDS", audit_max_lag.is_some()),
        ] {
            ensure!(
                !present || finality_mode == FinalityMode::Soft,
                "{name} applies only with FINALITY_MODE=soft"
            );
        }
        for (name, present) in [
            ("L1_GAS_MARGIN_BPS", l1_margin.is_some()),
            ("SEQUENCER_DROP_SECONDS", sequencer_drop.is_some()),
        ] {
            ensure!(
                !present || gas_model == GasModel::Arbitrum,
                "{name} applies only with GAS_MODEL=arbitrum"
            );
        }
        ensure!(
            index_finality == IndexFinality::Finalized || finality_mode == FinalityMode::Soft,
            "INDEX_FINALITY=soft requires FINALITY_MODE=soft"
        );
        // A round coordinator has no registry to pin, no epoch to publish and no block to nudge: a setting of the epoch
        // design is a mistake there, and says so rather than being ignored.
        if coordinator_kind == CoordinatorKind::Round {
            for name in EPOCH_ONLY_SETTINGS {
                ensure!(
                    setting(get, name).is_none(),
                    "{name} is refused with COORDINATOR_KIND=round: it belongs to an epoch coordinator, and a round coordinator has no registry and no epochs; remove it"
                );
            }
        }
        Ok(result)
    }
    /// COORDINATOR_KIND of an environment: `epoch` when it is unset.
    pub fn coordinator_kind(get: &dyn Fn(&str) -> Option<String>) -> Result<CoordinatorKind> {
        choice(
            get,
            "COORDINATOR_KIND",
            CoordinatorKind::default(),
            &COORDINATOR_KINDS,
        )
    }
    /// What the policy of a listed chain requires of the settings that `resolve` does not carry: each of its
    /// `explicit` settings is written down in the environment (an empty value is not), on a chain without a tip both
    /// priority fee bounds are 0, and the fee coverage is at least the chain's least. A keeper that leaves Arc's
    /// USDC-scaled defaults on such a chain refuses to start and names the setting. A round-mode keeper
    /// (COORDINATOR_KIND=round) writes the Arc-scaled settings down on every chain, since no default of them is a round
    /// coordinator's. Every other keeper takes its defaults as before.
    pub fn check_economics(chain_id: u64, get: &dyn Fn(&str) -> Option<String>) -> Result<()> {
        let kind = Self::coordinator_kind(get)?;
        if let Some(policy) = CHAIN_POLICIES
            .iter()
            .find(|policy| policy.ids.contains(&chain_id))
        {
            Self::check_policy_economics(policy, chain_id, get)?;
        }
        if kind == CoordinatorKind::Round {
            for (name, arc_default) in ARC_SCALED {
                ensure!(
                    setting(get, name).is_some(),
                    "{name} is not set with COORDINATOR_KIND=round, where its default ({arc_default}) is Arc's: set it explicitly"
                );
            }
        }
        Ok(())
    }
    fn check_policy_economics(
        policy: &ChainPolicy,
        chain_id: u64,
        get: &dyn Fn(&str) -> Option<String>,
    ) -> Result<()> {
        for (name, arc_default) in policy.explicit {
            ensure!(
                setting(get, name).is_some(),
                "{name} is not set on chain {chain_id} ({}), where its default ({arc_default}) is Arc's: set it explicitly",
                policy.name
            );
        }
        if policy.tipless {
            for name in ["MIN_PRIORITY_FEE_WEI", "MAX_PRIORITY_FEE_WEI"] {
                let tip: u128 = setting(get, name)
                    .with_context(|| {
                        format!(
                            "{name} must be set to 0 on chain {chain_id} ({})",
                            policy.name
                        )
                    })?
                    .parse()
                    .map_err(|e| anyhow::anyhow!("Invalid {name}: {e}"))?;
                ensure!(
                    tip == 0,
                    "{name}={tip} is not allowed on chain {chain_id} ({}), which has no tip: its sequencer orders first come first served, so it must be 0",
                    policy.name
                );
            }
        }
        if let Some(least) = policy.min_fee_coverage_bps {
            let text = setting(get, "FEE_COVERAGE_BPS").with_context(|| {
                format!(
                    "FEE_COVERAGE_BPS must be set on chain {chain_id} ({}), to at least {least}",
                    policy.name
                )
            })?;
            let coverage: u64 = text
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid FEE_COVERAGE_BPS: {e}"))?;
            ensure!(
                coverage >= least,
                "FEE_COVERAGE_BPS={coverage} is not allowed on chain {chain_id} ({}), which requires at least {least}: the escrowed fees must cover a quarter more than a fulfillment's expected cost",
                policy.name
            );
        }
        Ok(())
    }
    /// The three behaviour switches against the chain's policy: a listed chain accepts only its own values, and `soft`
    /// is accepted only on the chains that take it.
    fn check_chain_policy(
        &self,
        chain_id: u64,
        get: &dyn Fn(&str) -> Option<String>,
    ) -> Result<()> {
        let policy = CHAIN_POLICIES
            .iter()
            .find(|policy| policy.ids.contains(&chain_id));
        let refuse = |name: &str, value: &str, required: &str, chain: &str| {
            let how = if setting(get, name).is_some() {
                format!("{name}={value} is not allowed")
            } else {
                format!("{name} defaults to {value}, which is not allowed")
            };
            anyhow::anyhow!("{how} on chain {chain_id} ({chain}), which requires {name}={required}")
        };
        let Some(policy) = policy else {
            // A round coordinator's keeper fails closed: it runs only on a chain whose policy takes round mode, never on
            // a chain it knows nothing of with the defaults of another.
            let round: Vec<String> = CHAIN_POLICIES
                .iter()
                .filter(|policy| policy.coordinator != Some(CoordinatorKind::Epoch))
                .flat_map(|policy| policy.ids)
                .map(u64::to_string)
                .collect();
            ensure!(
                self.coordinator_kind == CoordinatorKind::Epoch,
                "COORDINATOR_KIND=round is allowed only on chains {} and {}, not on chain {chain_id}, which no chain policy lists",
                round[..round.len() - 1].join(", "),
                round[round.len() - 1]
            );
            let soft: Vec<String> = CHAIN_POLICIES
                .iter()
                .filter(|policy| policy.finality != Some(FinalityMode::Finalized))
                .flat_map(|policy| policy.ids)
                .map(u64::to_string)
                .collect();
            ensure!(
                self.finality_mode == FinalityMode::Finalized,
                "FINALITY_MODE=soft is allowed only on chains {} and {}, not on chain {chain_id}",
                soft[..soft.len() - 1].join(", "),
                soft[soft.len() - 1]
            );
            return Ok(());
        };
        if let Some(required) = policy.finality
            && self.finality_mode != required
        {
            return Err(refuse(
                "FINALITY_MODE",
                self.finality_mode.name(),
                required.name(),
                policy.name,
            ));
        }
        if let Some(required) = policy.gas_model
            && self.gas_model != required
        {
            return Err(refuse(
                "GAS_MODEL",
                self.gas_model.name(),
                required.name(),
                policy.name,
            ));
        }
        if let Some(required) = policy.coordinator
            && self.coordinator_kind != required
        {
            return Err(refuse(
                "COORDINATOR_KIND",
                self.coordinator_kind.name(),
                required.name(),
                policy.name,
            ));
        }
        if let Some(least) = policy.min_l1_margin_bps {
            ensure!(
                self.l1_gas_margin_bps >= least,
                "L1_GAS_MARGIN_BPS={} is not allowed on chain {chain_id} ({}), which requires at least {least}: a quote of the L1 component that the L1 price outgrows before the send would leave a gas limit short",
                self.l1_gas_margin_bps,
                policy.name
            );
        }
        Ok(())
    }
}
impl Config {
    pub fn load(once: bool) -> Result<Self> {
        for (name, replacement) in removed_settings(|name| env::var_os(name).is_some()) {
            tracing::warn!(
                setting = name,
                "{name} is ignored: {replacement}; remove it from the keeper environment"
            );
        }
        let chain_id = number("CHAIN_ID", "0")?;
        ensure!(chain_id > 0, "CHAIN_ID must be configured explicitly");
        let rpc_urls: Vec<String> = required("RPC_URLS")?
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        ensure!(!rpc_urls.is_empty(), "At least one RPC required");
        for url in &rpc_urls {
            ensure!(
                url.starts_with("https://")
                    || (chain_id == 31337 && url.starts_with("http://127.0.0.1:")),
                "RPC must use HTTPS (loopback HTTP is local-test only)"
            );
        }
        let ws_urls: Vec<String> = env::var("WS_URLS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        for url in &ws_urls {
            ensure!(
                url.starts_with("wss://")
                    || (chain_id == 31337 && url.starts_with("ws://127.0.0.1:")),
                "WS_URLS must use wss:// (loopback ws is local-test only)"
            );
        }
        let code_hash = env::var("EXPECTED_CODE_HASH")
            .ok()
            .map(|v| v.parse())
            .transpose()?;
        ensure!(
            env::var_os("EXPECTED_SOURCE_HASH").is_none(),
            "Use EXPECTED_PROTOCOL_HASH for the coordinator configuration"
        );
        let protocol_hash = env::var("EXPECTED_PROTOCOL_HASH")
            .ok()
            .map(|v| v.parse())
            .transpose()?;
        let implementation_code_hash = env::var("EXPECTED_IMPLEMENTATION_CODE_HASH")
            .ok()
            .map(|v| v.parse())
            .transpose()?;
        // A round coordinator has no registry: its keeper pins the coordinator alone, and `ChainSettings::resolve` refuses
        // the registry's settings with it.
        let kind = ChainSettings::coordinator_kind(&|name| env::var(name).ok())?;
        let registry_implementation_code_hash = match kind {
            CoordinatorKind::Epoch => env::var("EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH")
                .ok()
                .map(|v| v.parse())
                .transpose()?,
            CoordinatorKind::Round => None,
        };
        ensure!(
            chain_id == 31337
                || (code_hash.is_some()
                    && protocol_hash.is_some()
                    && implementation_code_hash.is_some()
                    && (kind == CoordinatorKind::Round
                        || registry_implementation_code_hash.is_some())),
            "{}",
            match kind {
                CoordinatorKind::Epoch =>
                    "Nonlocal networks require proxy, both implementation, and protocol configuration hashes",
                CoordinatorKind::Round =>
                    "Nonlocal networks require the round coordinator's proxy, implementation and protocol configuration hashes",
            }
        );
        let approved_next_implementation_code_hash = approved_next(
            "APPROVED_NEXT_IMPLEMENTATION_CODE_HASH",
            env::var("APPROVED_NEXT_IMPLEMENTATION_CODE_HASH")
                .ok()
                .as_deref(),
        )?;
        let approved_next_registry_implementation_code_hash = match kind {
            CoordinatorKind::Epoch => approved_next(
                "APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH",
                env::var("APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH")
                    .ok()
                    .as_deref(),
            )?,
            CoordinatorKind::Round => None,
        };
        let test_base = env::var("TEST_API_BASE").ok();
        if let Some(url) = &test_base {
            ensure!(
                chain_id == 31337 && url.starts_with("http://127.0.0.1:"),
                "API override is local-test only"
            );
        }
        // TEST_API_BASE replaces the relay list by that one base.
        let drand_relays = crate::drand::DrandRelays::configured(
            env::var("DRAND_RELAYS")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .as_deref(),
            test_base.as_deref(),
            chain_id == 31337,
        )?;
        let lock_dir = if let Ok(path) = env::var("TEST_LOCK_DIR") {
            ensure!(
                chain_id == 31337,
                "Lock directory override is local-test only"
            );
            PathBuf::from(path)
        } else {
            #[cfg(windows)]
            let path = PathBuf::from(required("ProgramData")?).join("D20DAO/locks");
            #[cfg(not(windows))]
            let path = PathBuf::from("/var/lib/d20dao/locks");
            path
        };
        let margin = number("SEND_MARGIN_SECONDS", "5")?;
        let follower = [
            "FOLLOWER_DELAY_SECONDS",
            "FOLLOWER_BUSY_DELAY_SECONDS",
            "FOLLOWER_QUEUE_JOIN",
            "PRIMARY_LIVENESS_SECONDS",
            "FOLLOWER_RANK",
            "FOLLOWER_LANES",
        ]
        .map(|name| env::var(name).ok());
        let role = Role::parse(
            env::var("KEEPER_ROLE").ok().as_deref(),
            FollowerSettings {
                delay: follower[0].as_deref(),
                busy_delay: follower[1].as_deref(),
                queue_join: follower[2].as_deref(),
                liveness: follower[3].as_deref(),
                rank: follower[4].as_deref(),
                lanes: follower[5].as_deref(),
            },
            margin,
        )?;
        let poll_ms: u64 = number("POLL_MS", "1000")?;
        ChainSettings::check_economics(chain_id, &|name| env::var(name).ok())?;
        let mut result = Self {
            role,
            telemetry: crate::telemetry::Settings::load(chain_id)?,
            rpc_urls,
            ws_urls,
            chain_id,
            coordinator: required("COORDINATOR_ADDRESS")?.parse()?,
            db: required("KEEPER_DB")?.into(),
            lock_dir,
            tx_key_file: required("TX_KEY_FILE")?.into(),
            vrf_key_file: required("VRF_KEY_FILE")?.into(),
            send: env::var("SEND_TRANSACTIONS").as_deref() == Ok("true"),
            once,
            poll_ms,
            idle_poll_ms: number("IDLE_POLL_MS", &poll_ms.max(1000).to_string())?,
            idle_heartbeat: crate::events::IDLE_HEARTBEAT,
            max_tick_failures: number("MAX_TICK_FAILURES", "5")?,
            tick_timeout_seconds: number("TICK_TIMEOUT_SECONDS", "20")?,
            margin,
            // A fulfillment reserves its callbacks' full gas limits (worker::fulfillment_gas_l1): one request
            // with the coordinator's largest callback limit needs about 2.5M, so the default carries it.
            max_gas: number("MAX_GAS", "3000000")?,
            max_fee: number("MAX_FEE_PER_GAS_WEI", "100000000000")?,
            cancel_max_fee: required("CANCEL_MAX_FEE_PER_GAS_WEI")?
                .parse()
                .context("Invalid CANCEL_MAX_FEE_PER_GAS_WEI")?,
            progress_stuck_seconds: number("PROGRESS_STUCK_SECONDS", "20")?,
            nonce_stuck_seconds: number("NONCE_STUCK_SECONDS", "120")?,
            max_cost: number("MAX_TX_COST_WEI", "200000000000000000")?,
            min_priority_fee: number("MIN_PRIORITY_FEE_WEI", "1000000000")?,
            max_priority_fee: number("MAX_PRIORITY_FEE_WEI", "50000000000")?,
            fee_coverage_bps: validate_fee_coverage_bps(number("FEE_COVERAGE_BPS", "10000")?)?,
            fulfill_batch_max: validate_fulfill_batch_max(number("FULFILL_BATCH_MAX", "8")?)?,
            code_hash,
            protocol_hash,
            implementation_code_hash,
            registry_implementation_code_hash,
            approved_next_implementation_code_hash,
            approved_next_registry_implementation_code_hash,
            drand_relays,
            chain: ChainSettings::resolve(chain_id, &|name| env::var(name).ok())?,
        };
        for (name, instead) in unused_settings(result.chain.coordinator_kind, |name| {
            env::var_os(name).is_some()
        }) {
            tracing::warn!(
                setting = name,
                "{name} has no effect in this release: {instead}"
            );
        }
        // Soft finality counts endpoints as witnesses of a block: one listed twice would be two that agree.
        if result.chain.finality_mode == FinalityMode::Soft {
            let listed = result.rpc_urls.len();
            result.rpc_urls = distinct_urls(std::mem::take(&mut result.rpc_urls));
            if result.rpc_urls.len() < listed {
                tracing::warn!(
                    listed,
                    distinct = result.rpc_urls.len(),
                    "RPC_URLS lists an endpoint more than once; it is asked once, and counts once when the endpoints are asked about a block"
                );
            }
        }
        ensure!(
            result.db.is_absolute() && result.lock_dir.is_absolute(),
            "KEEPER_DB and lock directory must be absolute paths"
        );
        ensure!(
            !result.coordinator.is_zero()
                && result.margin > 0
                && result.margin < 60
                && result.poll_ms >= 100
                && result.idle_poll_ms >= result.poll_ms
                && result.idle_poll_ms <= result.poll_ms.max(20_000)
                && result.max_tick_failures > 0
                && result.tick_timeout_seconds > 0
                && result.tick_timeout_seconds <= 20,
            "Invalid coordinator/timing configuration"
        );
        result.idle_heartbeat = idle_heartbeat(
            &|name| env::var(name).ok(),
            result.chain.coordinator_kind,
            result.margin,
        )?;
        match result.chain.gas_model {
            GasModel::Standard => {
                validate_recovery_budget(result.max_fee, result.cancel_max_fee, result.max_cost)?
            }
            GasModel::Arbitrum => validate_arbitrum_recovery_budget(
                result.max_fee,
                result.cancel_max_fee,
                result.max_cost,
            )?,
        }
        validate_priority_bounds(
            result.min_priority_fee,
            result.max_priority_fee,
            result.max_fee,
        )?;
        ensure!(
            result.nonce_stuck_seconds > 0 && result.progress_stuck_seconds > 0,
            "NONCE_STUCK_SECONDS and PROGRESS_STUCK_SECONDS must be positive"
        );
        Ok(result)
    }
    /// The implementations approved ahead of an in-place upgrade, per proxy.
    pub fn approved_next(&self) -> crate::proxy::ApprovedNext {
        crate::proxy::ApprovedNext {
            coordinator: self.approved_next_implementation_code_hash,
            registry: self.approved_next_registry_implementation_code_hash,
        }
    }
}

/// Bounds of IDLE_HEARTBEAT_SECONDS: how often a round keeper with nothing open ticks while its event subscription is live.
pub const IDLE_HEARTBEAT_RANGE: RangeInclusive<u64> = 2..=30;
/// IDLE_HEARTBEAT_SECONDS when it is unset.
pub const IDLE_HEARTBEAT_DEFAULT: u64 = 30;
/// A round keeper's idle heartbeat, IDLE_HEARTBEAT_SECONDS (2 to 30, default 30). A pushed request wakes the keeper at
/// once; the heartbeat finds one whose event the subscription missed, so it must leave the request time to be served:
/// the heartbeat, SEND_MARGIN_SECONDS and a fulfillment round fit the request's 60 seconds. The request's round is at
/// least ROUND_LEAD seconds after it, which the heartbeat overlaps; a follower's join delay is held to the same window
/// (`Role::parse`). An epoch keeper refuses the setting and keeps `events::IDLE_HEARTBEAT`.
fn idle_heartbeat(
    get: &dyn Fn(&str) -> Option<String>,
    kind: CoordinatorKind,
    margin: u64,
) -> Result<std::time::Duration> {
    let value = bounded(get, "IDLE_HEARTBEAT_SECONDS", &IDLE_HEARTBEAT_RANGE)?;
    if kind == CoordinatorKind::Epoch {
        ensure!(
            value.is_none(),
            "IDLE_HEARTBEAT_SECONDS applies only with COORDINATOR_KIND=round: an epoch keeper's idle heartbeat is {} seconds",
            crate::events::IDLE_HEARTBEAT.as_secs()
        );
        return Ok(crate::events::IDLE_HEARTBEAT);
    }
    let seconds = value.unwrap_or(IDLE_HEARTBEAT_DEFAULT);
    ensure!(
        seconds + margin + FULFILLMENT_ROUND_SECONDS <= RESPONSE_TIMEOUT_SECONDS,
        "IDLE_HEARTBEAT_SECONDS ({seconds}), SEND_MARGIN_SECONDS ({margin}) and a {FULFILLMENT_ROUND_SECONDS}-second fulfillment round must fit the {RESPONSE_TIMEOUT_SECONDS}-second request deadline: a request whose event the subscription missed is found at the next heartbeat"
    );
    Ok(std::time::Duration::from_secs(seconds))
}

/// An optional approved next implementation: the runtime code hash an operator reviewed before an in-place upgrade.
/// Unset or empty approves nothing. Every environment variable the keeper does not read is ignored, so a keeper
/// release without this setting runs unchanged beside it.
fn approved_next(name: &str, value: Option<&str>) -> Result<Option<B256>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let hash: B256 = value
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid {name}: expected a 32-byte runtime code hash"))?;
    ensure!(
        !hash.is_zero(),
        "{name} must be the approved implementation's runtime code hash, not a placeholder"
    );
    Ok(Some(hash))
}

/// The coordinator rejects more than MAX_FULFILL_BATCH members; 1 disables batching entirely.
fn validate_fulfill_batch_max(value: usize) -> Result<usize> {
    ensure!(
        (1..=crate::journal::MAX_BATCH_MEMBERS).contains(&value),
        "FULFILL_BATCH_MAX must be between 1 and {}",
        crate::journal::MAX_BATCH_MEMBERS
    );
    Ok(value)
}

fn validate_fee_coverage_bps(value: u64) -> Result<u64> {
    ensure!(
        value <= 100_000,
        "FEE_COVERAGE_BPS must be between 0 (disabled) and 100000"
    );
    Ok(value)
}
fn validate_priority_bounds(min: u128, max: u128, max_fee: u128) -> Result<()> {
    ensure!(
        min <= max && max <= max_fee,
        "MIN_PRIORITY_FEE_WEI must not exceed MAX_PRIORITY_FEE_WEI, which must not exceed MAX_FEE_PER_GAS_WEI"
    );
    Ok(())
}
/// The gas of a nonce cancellation, a zero-value transfer to the wallet itself.
pub const CANCEL_GAS: u64 = 21_000;
/// What the arbitrum gas model budgets a cancellation at: an Arbitrum chain charges the L1 component of the transfer on top
/// of its 21,000 gas (a self transfer needs 21,363 gas on Robinhood Chain mainnet and 27,559 on its testnet), and the
/// keeper adds a margin to that. This bound lies above both with room for the L1 price to move, so that
/// MAX_TX_COST_WEI is checked against what a cancellation can really cost.
pub const CANCEL_GAS_BOUND: u64 = 100_000;
fn validate_recovery_budget(fulfill: u128, cancel: u128, max_cost: u128) -> Result<()> {
    recovery_budget(fulfill, cancel, max_cost, CANCEL_GAS)
}
fn validate_arbitrum_recovery_budget(fulfill: u128, cancel: u128, max_cost: u128) -> Result<()> {
    recovery_budget(fulfill, cancel, max_cost, CANCEL_GAS_BOUND)
}
/// The cheapest nonce recovery the configuration allows: a replacement of a fulfillment at least 12.5% and 1 wei above
/// its fee, for `cancel_gas` gas, must fit MAX_TX_COST_WEI.
fn recovery_budget(fulfill: u128, cancel: u128, max_cost: u128, cancel_gas: u64) -> Result<()> {
    let minimum = fulfill
        .checked_mul(9)
        .map(|v| v / 8 + 1)
        .ok_or_else(|| anyhow::anyhow!("Recovery fee overflow"))?;
    ensure!(
        fulfill > 0 && cancel >= minimum,
        "CANCEL_MAX_FEE_PER_GAS_WEI must explicitly allow at least a 12.5% + 1 wei replacement above the fulfillment cap"
    );
    ensure!(
        minimum
            .checked_mul(u128::from(cancel_gas))
            .is_some_and(|v| v <= max_cost),
        "MAX_TX_COST_WEI cannot fund minimum nonce recovery{}",
        if cancel_gas == CANCEL_GAS {
            String::new()
        } else {
            format!(" (GAS_MODEL=arbitrum budgets a cancellation at {cancel_gas} gas)")
        }
    );
    Ok(())
}

/// A configured ceiling that a required transaction can exceed. Each names the
/// environment variable an operator raises; none is bypassed to meet a deadline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeeCap {
    MaxGas,
    MaxFeePerGas,
    CancelMaxFeePerGas,
    MaxTxCost,
    /// Not an operator ceiling: the escrowed fees of the served requests do not cover the expected cost.
    FeeCoverage,
}
impl FeeCap {
    pub fn variable(self) -> &'static str {
        match self {
            Self::MaxGas => "MAX_GAS",
            Self::MaxFeePerGas => "MAX_FEE_PER_GAS_WEI",
            Self::CancelMaxFeePerGas => "CANCEL_MAX_FEE_PER_GAS_WEI",
            Self::MaxTxCost => "MAX_TX_COST_WEI",
            Self::FeeCoverage => "FEE_COVERAGE_BPS",
        }
    }
}
/// Public, operator-facing budget observation: only configured limits and required amounts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeeBudget {
    pub cap: FeeCap,
    pub required: u128,
    pub limit: u128,
}
impl std::fmt::Display for FeeBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.cap == FeeCap::FeeCoverage {
            return write!(
                f,
                "expected cost share {} exceeds escrowed fees {} under FEE_COVERAGE_BPS",
                self.required, self.limit
            );
        }
        write!(
            f,
            "required {} exceeds {}={}",
            self.required,
            self.cap.variable(),
            self.limit
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roles_default_to_primary_and_bound_the_follower_join_rule() {
        let settings = |delay, queue_join, liveness| FollowerSettings {
            delay,
            queue_join,
            liveness,
            ..FollowerSettings::default()
        };
        let lane = |rank, lanes| FollowerSettings {
            rank,
            lanes,
            ..FollowerSettings::default()
        };
        let follower = |delay, queue_join, liveness, rank, lanes| {
            Role::Follower(FollowerPlan {
                delay,
                queue_join,
                liveness,
                rank,
                lanes,
            })
        };
        let none = FollowerSettings::default();
        assert_eq!(Role::parse(None, none, 5).unwrap(), Role::Primary);
        assert_eq!(
            Role::parse(Some("primary"), none, 5).unwrap(),
            Role::Primary
        );
        assert_eq!(
            Role::parse(Some("follower"), none, 5).unwrap(),
            follower(20, 150, 10, 0, 1)
        );
        assert_eq!(
            Role::parse(
                Some("follower"),
                settings(Some("5"), Some("1"), Some("5")),
                5
            )
            .unwrap(),
            follower(5, 1, 5, 0, 1)
        );
        // Three lanes beside the primary: each follower takes one residue class of the request ids.
        assert_eq!(
            Role::parse(Some("follower"), lane(Some("2"), Some("3")), 5).unwrap(),
            follower(20, 150, 10, 2, 3)
        );
        // The longest join delay still fits the deadline with the largest send margin the safety age allows.
        assert_eq!(
            Role::parse(Some("follower"), settings(Some("30"), None, None), 10).unwrap(),
            follower(30, 150, 10, 0, 1)
        );
        for (role, timing) in [
            (Some("follower"), settings(Some("4"), None, None)),
            (Some("follower"), settings(Some("31"), None, None)),
            (Some("follower"), settings(Some("twenty"), None, None)),
            (Some("follower"), settings(None, Some("0"), None)),
            (Some("follower"), settings(None, None, Some("4"))),
            (Some("follower"), settings(None, None, Some("31"))),
            (Some("follower"), lane(Some("1"), Some("1"))),
            (Some("follower"), lane(Some("0"), Some("5"))),
            (Some("follower"), lane(Some("0"), Some("0"))),
            (
                Some("follower"),
                FollowerSettings {
                    busy_delay: Some("40"),
                    ..FollowerSettings::default()
                },
            ),
            (Some("primary"), settings(Some("20"), None, None)),
            (Some("primary"), settings(None, Some("150"), None)),
            (Some("primary"), lane(None, Some("2"))),
            (Some("backup"), none),
        ] {
            assert!(Role::parse(role, timing, 5).is_err(), "{role:?}");
        }
        // A send margin that leaves no room for a fulfillment round inside the safety age is refused.
        assert!(Role::parse(Some("follower"), none, 11).is_err());
        assert_eq!(follower(20, 150, 10, 0, 1).name(), "follower");
    }
    #[test]
    fn an_endpoint_listed_twice_is_one_endpoint() {
        let urls = |list: &[&str]| list.iter().map(|url| (*url).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            distinct_urls(urls(&[
                "https://a/x",
                "https://b",
                "https://a/x/",
                "https://b",
                "https://c"
            ])),
            urls(&["https://a/x", "https://b", "https://c"])
        );
        assert_eq!(distinct_urls(urls(&["https://a"])), urls(&["https://a"]));
    }
    #[test]
    fn settings_without_an_effect_are_named_by_coordinator_kind() {
        let environment =
            |present: &'static [&'static str]| move |name: &str| present.contains(&name);
        let named = |kind, present| -> Vec<&str> {
            unused_settings(kind, environment(present))
                .into_iter()
                .map(|(name, _)| name)
                .collect()
        };
        let all = &[
            "INDEX_FINALITY",
            "INDEX_MAX_BLOCKS",
            "INDEX_LOOKBACK_BLOCKS",
            "WS_SILENCE_SECONDS",
            "WS_BACKFILL_MAX_BLOCKS",
            "WS_BACKFILL_RANGE",
            "SWEEP_MIN_RESERVE_WEI",
            "RPC_URLS",
        ];
        assert!(named(CoordinatorKind::Round, &[]).is_empty());
        // A round keeper acts on its subscription limits and its sweep reserve; nobody acts on INDEX_* yet.
        assert_eq!(
            named(CoordinatorKind::Round, all),
            [
                "INDEX_FINALITY",
                "INDEX_MAX_BLOCKS",
                "INDEX_LOOKBACK_BLOCKS"
            ]
        );
        assert_eq!(named(CoordinatorKind::Epoch, all), all[..7]);
    }
    #[test]
    fn removed_settings_are_found_by_name_and_never_refuse_a_start() {
        let environment =
            |present: &'static [&'static str]| move |name: &str| present.contains(&name);
        assert!(removed_settings(environment(&[])).is_empty());
        // Whatever else is set, only a removed setting is named, and a value that 0.4.0 would have refused is not read.
        assert!(
            removed_settings(environment(&["DRAND_RELAYS", "TEST_API_BASE", "RPC_URLS"]))
                .is_empty()
        );
        let found = removed_settings(environment(&["EPOCH_API_ENDPOINTS", "DRAND_RELAYS"]));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "EPOCH_API_ENDPOINTS");
        assert!(
            found[0]
                .1
                .contains("signed API recipes are not supported since 0.4.1")
        );
    }
    #[test]
    fn approved_next_implementations_are_optional_exact_code_hashes() {
        let name = "APPROVED_NEXT_IMPLEMENTATION_CODE_HASH";
        let hash = "0x3dda400d8360d7e03b8dacd8ba1ffad7ad672e07628bee7de4542d754dd5348c";
        // Unset or empty approves nothing, so the setting can be cleared in place after an upgrade.
        assert_eq!(approved_next(name, None).unwrap(), None);
        assert_eq!(approved_next(name, Some("")).unwrap(), None);
        assert_eq!(approved_next(name, Some("  ")).unwrap(), None);
        assert_eq!(
            approved_next(name, Some(hash)).unwrap(),
            Some(hash.parse().unwrap())
        );
        assert_eq!(
            approved_next(name, Some(&format!(" {hash}\t"))).unwrap(),
            Some(hash.parse().unwrap())
        );
        // A malformed value or a placeholder is refused, naming the setting.
        for invalid in [
            "0x1234",
            "not-a-hash",
            &hash[..65],
            &format!("{hash}00"),
            "0x0000000000000000000000000000000000000000000000000000000000000000",
        ] {
            let error = approved_next(name, Some(invalid)).unwrap_err().to_string();
            assert!(error.contains(name), "{invalid}: {error}");
        }
    }
    #[test]
    fn recovery_requires_explicit_affordable_premium() {
        assert!(validate_recovery_budget(100, 100, 3_000_000).is_err());
        assert!(validate_recovery_budget(100, 113, 113 * 21000).is_ok());
        assert!(validate_recovery_budget(100, 113, 113 * 21000 - 1).is_err());
        assert!(validate_recovery_budget(u128::MAX, u128::MAX, u128::MAX).is_err());
    }
    #[test]
    fn fulfill_batch_max_is_bounded_by_the_coordinator_limit() {
        // Priority bounds: min <= max <= MAX_FEE_PER_GAS_WEI, so a clamped tip is always affordable.
        assert!(validate_priority_bounds(1, 50, 100).is_ok());
        assert!(validate_priority_bounds(50, 50, 50).is_ok());
        assert!(validate_priority_bounds(51, 50, 100).is_err());
        assert!(validate_priority_bounds(1, 101, 100).is_err());
        assert_eq!(validate_fee_coverage_bps(0).unwrap(), 0);
        assert_eq!(validate_fee_coverage_bps(10_000).unwrap(), 10_000);
        assert!(validate_fee_coverage_bps(100_001).is_err());
        assert!(validate_fulfill_batch_max(0).is_err());
        assert_eq!(validate_fulfill_batch_max(1).unwrap(), 1);
        assert_eq!(validate_fulfill_batch_max(8).unwrap(), 8);
        assert_eq!(validate_fulfill_batch_max(16).unwrap(), 16);
        assert!(validate_fulfill_batch_max(17).is_err());
    }
    #[test]
    fn documented_mainnet_budgets_validate_and_cover_observed_base_fees() {
        // keeper/.env.example mainnet guidance: 2000 gwei, 2500 gwei, 4 USDC (native 18).
        let (fulfill, cancel, cost) = (
            2_000_000_000_000u128,
            2_500_000_000_000u128,
            4_000_000_000_000_000_000u128,
        );
        validate_recovery_budget(fulfill, cancel, cost).unwrap();
        assert!(validate_recovery_budget(fulfill, 2_250_000_000_000, cost).is_err());
        // 2 * base fee + 1 gwei at the highest observed Arc base fee (251 gwei) fits the cap.
        let required = 251_000_000_000u128 * 2 + 1_000_000_000;
        assert!(required <= fulfill);
        assert!(required * 600_000 <= cost);
        assert_eq!(
            FeeBudget {
                cap: FeeCap::MaxFeePerGas,
                required,
                limit: 100_000_000_000,
            }
            .to_string(),
            "required 503000000000 exceeds MAX_FEE_PER_GAS_WEI=100000000000"
        );
    }
}
#[cfg(test)]
mod chain_settings_tests {
    use super::*;
    use std::collections::HashMap;

    const ARC: [u64; 2] = [5042, 5042002];
    const ROBINHOOD: [u64; 2] = [4663, 46630];
    /// The local test chain and nitro-testnode accept any value of the three behaviour switches.
    const OPEN: [u64; 2] = [31337, 412346];
    /// Chains the policy table does not list.
    const OTHER: [u64; 3] = [1, 8453, 421614];
    const ROBINHOOD_SWITCHES: [(&str, &str); 3] = [
        ("FINALITY_MODE", "soft"),
        ("GAS_MODEL", "arbitrum"),
        ("COORDINATOR_KIND", "round"),
    ];

    fn resolve(chain_id: u64, pairs: &[(&str, &str)]) -> Result<ChainSettings> {
        let environment: HashMap<&str, &str> = pairs.iter().copied().collect();
        ChainSettings::resolve(chain_id, &|name| {
            environment.get(name).map(|value| (*value).to_owned())
        })
    }
    fn refused(chain_id: u64, pairs: &[(&str, &str)]) -> String {
        format!("{:#}", resolve(chain_id, pairs).unwrap_err())
    }
    /// `base` with the settings of `more` added or replaced.
    fn with<'a>(
        base: &[(&'a str, &'a str)],
        more: &[(&'a str, &'a str)],
    ) -> Vec<(&'a str, &'a str)> {
        let mut pairs: Vec<_> = base
            .iter()
            .filter(|(name, _)| !more.iter().any(|(other, _)| other == name))
            .copied()
            .collect();
        pairs.extend_from_slice(more);
        pairs
    }

    #[test]
    fn every_default_is_the_behaviour_of_0_4_1() {
        for chain in ARC.into_iter().chain(OPEN).chain(OTHER) {
            assert_eq!(
                resolve(chain, &[]).unwrap(),
                ChainSettings::default(),
                "{chain}"
            );
        }
        let d = ChainSettings::default();
        assert_eq!(
            (
                d.finality_mode,
                d.gas_model,
                d.coordinator_kind,
                d.index_finality
            ),
            (
                FinalityMode::Finalized,
                GasModel::Standard,
                CoordinatorKind::Epoch,
                IndexFinality::Finalized
            )
        );
        assert_eq!(
            (
                d.soft_depth_blocks,
                d.finality_audit_interval_seconds,
                d.finality_audit_max_lag_seconds
            ),
            (0, 30, 2_700)
        );
        assert_eq!(
            (d.l1_gas_margin_bps, d.sequencer_drop_seconds),
            (2_500, None)
        );
        // The constants sweep.rs, events.rs and explorer.rs carry until these settings are wired.
        assert_eq!(d.sweep_min_reserve_wei, crate::sweep::MIN_RESERVE_WEI);
        assert_eq!(
            (
                d.ws_silence_seconds,
                d.ws_backfill_max_blocks,
                d.ws_backfill_range
            ),
            (20, 5_000, 500)
        );
        assert_eq!((d.index_max_blocks, d.index_lookback_blocks), (128, 12));
        // Empty values count as unset, so an environment file may list a setting without a value.
        let blank = [
            ("FINALITY_MODE", ""),
            ("SOFT_DEPTH_BLOCKS", "  "),
            ("GAS_MODEL", " "),
            ("SEQUENCER_DROP_SECONDS", ""),
            ("COORDINATOR_KIND", ""),
            ("SWEEP_MIN_RESERVE_WEI", ""),
        ];
        for chain in ARC {
            assert_eq!(resolve(chain, &blank).unwrap(), d);
        }
    }

    /// A numeric setting: what it needs on the local test chain, its bounds and where it lands.
    type Numeric<'a> = (
        &'a str,
        &'a [(&'a str, &'a str)],
        u64,
        u64,
        fn(&ChainSettings) -> u64,
    );

    #[test]
    fn every_numeric_setting_is_bounded_on_both_edges() {
        let soft = [("FINALITY_MODE", "soft")];
        let arbitrum = [("GAS_MODEL", "arbitrum")];
        let settings: [Numeric; 10] = [
            ("SOFT_DEPTH_BLOCKS", &soft, 0, 16, |s| s.soft_depth_blocks),
            ("FINALITY_AUDIT_INTERVAL_SECONDS", &soft, 5, 300, |s| {
                s.finality_audit_interval_seconds
            }),
            ("FINALITY_AUDIT_MAX_LAG_SECONDS", &soft, 600, 7_200, |s| {
                s.finality_audit_max_lag_seconds
            }),
            ("L1_GAS_MARGIN_BPS", &arbitrum, 0, 20_000, |s| {
                s.l1_gas_margin_bps
            }),
            ("SEQUENCER_DROP_SECONDS", &arbitrum, 2, 60, |s| {
                s.sequencer_drop_seconds.unwrap()
            }),
            ("WS_SILENCE_SECONDS", &[], 5, 600, |s| s.ws_silence_seconds),
            (
                "WS_BACKFILL_MAX_BLOCKS",
                &[("WS_BACKFILL_RANGE", "1")],
                1,
                100_000,
                |s| s.ws_backfill_max_blocks,
            ),
            (
                "WS_BACKFILL_RANGE",
                &[("WS_BACKFILL_MAX_BLOCKS", "100000")],
                1,
                10_000,
                |s| s.ws_backfill_range,
            ),
            ("INDEX_MAX_BLOCKS", &[], 1, 10_000, |s| s.index_max_blocks),
            ("INDEX_LOOKBACK_BLOCKS", &[], 1, 10_000, |s| {
                s.index_lookback_blocks
            }),
        ];
        for (name, needs, low, high, read) in settings {
            for edge in [low, high] {
                let value = edge.to_string();
                let parsed = resolve(31337, &with(needs, &[(name, &value)])).unwrap();
                assert_eq!(read(&parsed), edge, "{name}");
                // Whitespace around a value is not part of it.
                let padded = format!(" {edge}\t");
                let parsed = resolve(31337, &with(needs, &[(name, &padded)])).unwrap();
                assert_eq!(read(&parsed), edge, "{name}");
            }
            let range = format!("{name} must be between {low} and {high}");
            for outside in [low.checked_sub(1), Some(high + 1)].into_iter().flatten() {
                let value = outside.to_string();
                let error = refused(31337, &with(needs, &[(name, &value)]));
                assert!(error.contains(&range), "{name}={value}: {error}");
            }
            for malformed in ["abc", "-1", "1.5", "0x10", "1 2", "1e3"] {
                let error = refused(31337, &with(needs, &[(name, malformed)]));
                assert!(
                    error.contains(&format!("Invalid {name}")),
                    "{name}={malformed}: {error}"
                );
            }
        }
        // The bounds the design lists.
        assert_eq!(SOFT_DEPTH_RANGE, 0..=16);
        assert_eq!(FINALITY_AUDIT_INTERVAL_RANGE, 5..=300);
        assert_eq!(FINALITY_AUDIT_MAX_LAG_RANGE, 600..=7_200);
        assert_eq!(L1_GAS_MARGIN_RANGE, 0..=20_000);
        assert_eq!(SEQUENCER_DROP_RANGE, 2..=60);
        assert_eq!(WS_SILENCE_RANGE, 5..=600);
    }

    #[test]
    fn the_wei_setting_is_a_plain_amount_and_the_backfill_range_fits_its_cap() {
        let amount = |value: &str| {
            resolve(5042, &[("SWEEP_MIN_RESERVE_WEI", value)]).map(|s| s.sweep_min_reserve_wei)
        };
        assert_eq!(amount("5000000000000000").unwrap(), 5_000_000_000_000_000);
        assert_eq!(amount("0").unwrap(), 0);
        assert_eq!(amount(&u128::MAX.to_string()).unwrap(), u128::MAX);
        for malformed in [
            "1e18",
            "-1",
            "1.5",
            "five",
            "0x10",
            "340282366920938463463374607431768211456",
        ] {
            let error = amount(malformed).unwrap_err().to_string();
            assert!(
                error.contains("Invalid SWEEP_MIN_RESERVE_WEI"),
                "{malformed}: {error}"
            );
        }
        // A range wider than the whole backfill cap is a typo, not a configuration.
        assert!(
            refused(
                31337,
                &[
                    ("WS_BACKFILL_RANGE", "6000"),
                    ("WS_BACKFILL_MAX_BLOCKS", "5000")
                ]
            )
            .contains("WS_BACKFILL_RANGE must not exceed WS_BACKFILL_MAX_BLOCKS")
        );
        resolve(
            31337,
            &[
                ("WS_BACKFILL_RANGE", "5000"),
                ("WS_BACKFILL_MAX_BLOCKS", "5000"),
            ],
        )
        .unwrap();
    }

    #[test]
    fn the_words_are_exact_and_lowercase() {
        // The setting, what it needs on the local test chain, its two words.
        type Words<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a str, &'a str);
        let switches: [Words; 4] = [
            ("FINALITY_MODE", &[], "finalized", "soft"),
            ("GAS_MODEL", &[], "standard", "arbitrum"),
            ("COORDINATOR_KIND", &[], "epoch", "round"),
            (
                "INDEX_FINALITY",
                &[("FINALITY_MODE", "soft")],
                "finalized",
                "soft",
            ),
        ];
        for (name, needs, first, second) in switches {
            for word in [first, second] {
                resolve(31337, &with(needs, &[(name, word)])).unwrap();
                resolve(31337, &with(needs, &[(name, &format!(" {word}\n"))])).unwrap();
            }
            let expected = format!("{name} must be {first} or {second}");
            for wrong in [
                "fast".to_owned(),
                first.to_uppercase(),
                format!("{first},{second}"),
                format!("{second}!"),
                "1".to_owned(),
            ] {
                let error = refused(31337, &with(needs, &[(name, &wrong)]));
                assert!(error.contains(&expected), "{name}={wrong}: {error}");
            }
        }
        let carried = resolve(
            31337,
            &[
                ("FINALITY_MODE", "soft"),
                ("GAS_MODEL", "arbitrum"),
                ("COORDINATOR_KIND", "round"),
                ("INDEX_FINALITY", "soft"),
            ],
        )
        .unwrap();
        assert_eq!(
            (
                carried.finality_mode,
                carried.gas_model,
                carried.coordinator_kind,
                carried.index_finality,
            ),
            (
                FinalityMode::Soft,
                GasModel::Arbitrum,
                CoordinatorKind::Round,
                IndexFinality::Soft,
            )
        );
    }

    #[test]
    fn arc_requires_finalized_standard_and_epoch() {
        let arc = [
            ("FINALITY_MODE", "finalized"),
            ("GAS_MODEL", "standard"),
            ("COORDINATOR_KIND", "epoch"),
        ];
        for chain in ARC {
            // Unset and explicit are the same keeper.
            assert_eq!(resolve(chain, &arc).unwrap(), ChainSettings::default());
            for (name, value) in ROBINHOOD_SWITCHES {
                let error = refused(chain, &[(name, value)]);
                assert!(
                    error.contains(&format!(
                        "{name}={value} is not allowed on chain {chain} (Arc), which requires"
                    )),
                    "{error}"
                );
                // Each switch is judged on its own: the other two stay Arc's.
                let error = refused(chain, &with(&arc, &[(name, value)]));
                assert!(
                    error.contains(&format!("{name}={value} is not allowed")),
                    "{error}"
                );
            }
            let all = refused(chain, &ROBINHOOD_SWITCHES);
            assert!(
                all.contains("FINALITY_MODE=soft is not allowed on chain"),
                "{all}"
            );
        }
    }

    #[test]
    fn robinhood_requires_soft_arbitrum_and_round() {
        for chain in ROBINHOOD {
            let accepted = resolve(chain, &ROBINHOOD_SWITCHES).unwrap();
            assert_eq!(
                (
                    accepted.finality_mode,
                    accepted.gas_model,
                    accepted.coordinator_kind
                ),
                (
                    FinalityMode::Soft,
                    GasModel::Arbitrum,
                    CoordinatorKind::Round
                )
            );
            for (name, wrong, required) in [
                ("FINALITY_MODE", "finalized", "soft"),
                ("GAS_MODEL", "standard", "arbitrum"),
                ("COORDINATOR_KIND", "epoch", "round"),
            ] {
                // A keeper that leaves the switch out would run on Arc's default: it is refused, and told so.
                let rest: Vec<_> = ROBINHOOD_SWITCHES
                    .iter()
                    .copied()
                    .filter(|(other, _)| *other != name)
                    .collect();
                let error = refused(chain, &rest);
                assert!(
                    error.contains(&format!(
                        "{name} defaults to {wrong}, which is not allowed on chain {chain} (Robinhood Chain), which requires {name}={required}"
                    )),
                    "{error}"
                );
                let error = refused(chain, &with(&ROBINHOOD_SWITCHES, &[(name, wrong)]));
                assert!(
                    error.contains(&format!(
                        "{name}={wrong} is not allowed on chain {chain} (Robinhood Chain), which requires {name}={required}"
                    )),
                    "{error}"
                );
            }
            // Nothing at all is Arc's configuration, and refused.
            assert!(refused(chain, &[]).contains("FINALITY_MODE defaults to finalized"));
        }
    }

    #[test]
    fn the_local_test_chain_and_nitro_testnode_accept_every_combination() {
        for chain in OPEN {
            for finality in ["finalized", "soft"] {
                for gas in ["standard", "arbitrum"] {
                    for kind in ["epoch", "round"] {
                        let parsed = resolve(
                            chain,
                            &[
                                ("FINALITY_MODE", finality),
                                ("GAS_MODEL", gas),
                                ("COORDINATOR_KIND", kind),
                            ],
                        )
                        .unwrap();
                        assert_eq!(parsed.finality_mode.name(), finality);
                        assert_eq!(parsed.gas_model.name(), gas);
                        assert_eq!(parsed.coordinator_kind.name(), kind);
                    }
                }
            }
        }
    }

    #[test]
    fn other_chains_behave_as_before_and_take_soft_nowhere() {
        for chain in OTHER {
            assert_eq!(resolve(chain, &[]).unwrap(), ChainSettings::default());
            let error = refused(chain, &[("FINALITY_MODE", "soft")]);
            assert!(
                error.contains(&format!(
                    "FINALITY_MODE=soft is allowed only on chains 4663, 46630, 31337 and 412346, not on chain {chain}"
                )),
                "{error}"
            );
            // An Arbitrum chain may use the gas model there, but a round coordinator's keeper fails closed on a chain no
            // policy lists.
            let parsed = resolve(chain, &[("GAS_MODEL", "arbitrum")]).unwrap();
            assert_eq!(
                (parsed.gas_model, parsed.coordinator_kind),
                (GasModel::Arbitrum, CoordinatorKind::Epoch)
            );
            let error = refused(
                chain,
                &[("GAS_MODEL", "arbitrum"), ("COORDINATOR_KIND", "round")],
            );
            assert!(
                error.contains("COORDINATOR_KIND=round is allowed only on chains"),
                "{error}"
            );
        }
    }

    #[test]
    fn a_setting_of_a_mode_is_refused_without_the_mode() {
        let soft_only = [
            ("SOFT_DEPTH_BLOCKS", "0"),
            ("FINALITY_AUDIT_INTERVAL_SECONDS", "30"),
            ("FINALITY_AUDIT_MAX_LAG_SECONDS", "2700"),
        ];
        let arbitrum_only = [
            ("L1_GAS_MARGIN_BPS", "2500"),
            ("SEQUENCER_DROP_SECONDS", "6"),
        ];
        for chain in ARC.into_iter().chain(OPEN).chain(OTHER) {
            for setting in soft_only {
                let error = refused(chain, &[setting]);
                assert!(
                    error.contains(&format!(
                        "{} applies only with FINALITY_MODE=soft",
                        setting.0
                    )),
                    "{chain}: {error}"
                );
            }
            for setting in arbitrum_only {
                let error = refused(chain, &[setting]);
                assert!(
                    error.contains(&format!(
                        "{} applies only with GAS_MODEL=arbitrum",
                        setting.0
                    )),
                    "{chain}: {error}"
                );
            }
        }
        for chain in OPEN {
            resolve(chain, &with(&soft_only, &[("FINALITY_MODE", "soft")])).unwrap();
            resolve(chain, &with(&arbitrum_only, &[("GAS_MODEL", "arbitrum")])).unwrap();
        }
        for chain in ROBINHOOD {
            let env = [&ROBINHOOD_SWITCHES[..], &soft_only, &arbitrum_only].concat();
            resolve(chain, &env).unwrap();
        }
        // The explorer index follows soft blocks only where the keeper does.
        for chain in ARC.into_iter().chain(OPEN).chain(OTHER) {
            let error = refused(chain, &[("INDEX_FINALITY", "soft")]);
            assert!(
                error.contains("INDEX_FINALITY=soft requires FINALITY_MODE=soft"),
                "{chain}: {error}"
            );
            resolve(chain, &[("INDEX_FINALITY", "finalized")]).unwrap();
        }
        for chain in OPEN {
            resolve(
                chain,
                &[("FINALITY_MODE", "soft"), ("INDEX_FINALITY", "soft")],
            )
            .unwrap();
        }
        // Robinhood may index finalized blocks instead, if the owner decides so.
        let env = with(&ROBINHOOD_SWITCHES, &[("INDEX_FINALITY", "finalized")]);
        assert_eq!(
            resolve(4663, &env).unwrap().index_finality,
            IndexFinality::Finalized
        );
    }

    #[test]
    fn the_robinhood_values_of_the_design_are_accepted() {
        // Design 6.2: mainnet, then testnet, which differs in WS_SILENCE_SECONDS alone.
        let mainnet = with(
            &ROBINHOOD_SWITCHES,
            &[
                ("SOFT_DEPTH_BLOCKS", "0"),
                ("FINALITY_AUDIT_INTERVAL_SECONDS", "30"),
                ("FINALITY_AUDIT_MAX_LAG_SECONDS", "2700"),
                ("L1_GAS_MARGIN_BPS", "2500"),
                ("SEQUENCER_DROP_SECONDS", "6"),
                ("SWEEP_MIN_RESERVE_WEI", "5000000000000000"),
                ("WS_SILENCE_SECONDS", "20"),
                ("WS_BACKFILL_MAX_BLOCKS", "6000"),
                ("WS_BACKFILL_RANGE", "2000"),
                ("INDEX_FINALITY", "soft"),
                ("INDEX_MAX_BLOCKS", "2000"),
                ("INDEX_LOOKBACK_BLOCKS", "1200"),
            ],
        );
        let expected = ChainSettings {
            finality_mode: FinalityMode::Soft,
            soft_depth_blocks: 0,
            finality_audit_interval_seconds: 30,
            finality_audit_max_lag_seconds: 2_700,
            gas_model: GasModel::Arbitrum,
            l1_gas_margin_bps: 2_500,
            coordinator_kind: CoordinatorKind::Round,
            sequencer_drop_seconds: Some(6),
            sweep_min_reserve_wei: 5_000_000_000_000_000,
            ws_silence_seconds: 20,
            ws_backfill_max_blocks: 6_000,
            ws_backfill_range: 2_000,
            index_finality: IndexFinality::Soft,
            index_max_blocks: 2_000,
            index_lookback_blocks: 1_200,
        };
        assert_eq!(resolve(4663, &mainnet).unwrap(), expected);
        let testnet = with(&mainnet, &[("WS_SILENCE_SECONDS", "60")]);
        assert_eq!(
            resolve(46630, &testnet).unwrap(),
            ChainSettings {
                ws_silence_seconds: 60,
                ..expected
            }
        );
        // Settings with no mode of their own are fine on Arc too, and carried.
        let arc = resolve(
            5042002,
            &[("WS_SILENCE_SECONDS", "60"), ("SWEEP_MIN_RESERVE_WEI", "7")],
        )
        .unwrap();
        assert_eq!((arc.ws_silence_seconds, arc.sweep_min_reserve_wei), (60, 7));
    }

    #[test]
    fn the_arbitrum_model_budgets_a_cancellation_at_its_bound_and_not_at_21000_gas() {
        assert_eq!((CANCEL_GAS, CANCEL_GAS_BOUND), (21_000, 100_000));
        // The standard model is as before.
        assert!(validate_recovery_budget(100, 113, 113 * 21_000).is_ok());
        assert!(validate_recovery_budget(100, 113, 113 * 21_000 - 1).is_err());
        // The Robinhood Chain values of the design: a 3 gwei cap, a 3.5 gwei cancellation cap and 0.005 ETH.
        let (fulfill, cancel, cost) = (3_000_000_000u128, 3_500_000_000u128, 5_000_000_000_000_000);
        validate_arbitrum_recovery_budget(fulfill, cancel, cost).unwrap();
        // The cheapest recovery is 12.5% and a wei above the cap, for 100,000 gas.
        let minimum = fulfill * 9 / 8 + 1;
        assert_eq!(minimum, 3_375_000_001);
        validate_arbitrum_recovery_budget(fulfill, cancel, minimum * 100_000).unwrap();
        assert!(validate_arbitrum_recovery_budget(fulfill, cancel, minimum * 100_000 - 1).is_err());
        // A budget that carries 21,000 gas and not 100,000 is refused under the arbitrum model, and says why.
        assert!(validate_recovery_budget(fulfill, cancel, minimum * 21_000).is_ok());
        let error = validate_arbitrum_recovery_budget(fulfill, cancel, minimum * 21_000)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "MAX_TX_COST_WEI cannot fund minimum nonce recovery (GAS_MODEL=arbitrum budgets a cancellation at 100000 gas)"
        );
        // The standard message is as before, and the cancellation cap rule and overflow are the same in both.
        assert_eq!(
            validate_recovery_budget(fulfill, cancel, 0)
                .unwrap_err()
                .to_string(),
            "MAX_TX_COST_WEI cannot fund minimum nonce recovery"
        );
        assert!(validate_arbitrum_recovery_budget(100, 100, u128::MAX).is_err());
        assert!(validate_arbitrum_recovery_budget(u128::MAX, u128::MAX, u128::MAX).is_err());
        assert!(validate_arbitrum_recovery_budget(0, 0, u128::MAX).is_err());
    }
}

#[cfg(test)]
mod economics_tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};

    const ARC: [u64; 2] = [5042, 5042002];
    const ROBINHOOD: [u64; 2] = [4663, 46630];
    /// The local test chain, nitro-testnode, and chains the policy table does not list.
    const OTHER: [u64; 5] = [31337, 412346, 1, 8453, 421614];
    /// The settings whose defaults are Arc's, with the values the design gives Robinhood Chain.
    const WRITTEN_DOWN: [(&str, &str); 8] = [
        ("MAX_FEE_PER_GAS_WEI", "3000000000"),
        ("MAX_TX_COST_WEI", "5000000000000000"),
        ("MIN_PRIORITY_FEE_WEI", "0"),
        ("MAX_PRIORITY_FEE_WEI", "0"),
        ("SWEEP_MIN_RESERVE_WEI", "5000000000000000"),
        ("NONCE_STUCK_SECONDS", "60"),
        ("MAX_GAS", "13000000"),
        ("FEE_COVERAGE_BPS", "12500"),
    ];

    fn check(chain_id: u64, pairs: &[(&str, &str)]) -> Result<()> {
        let environment: HashMap<&str, &str> = pairs.iter().copied().collect();
        ChainSettings::check_economics(chain_id, &|name| {
            environment.get(name).map(|value| (*value).to_owned())
        })
    }
    fn refused(chain_id: u64, pairs: &[(&str, &str)]) -> String {
        format!("{:#}", check(chain_id, pairs).unwrap_err())
    }
    /// `WRITTEN_DOWN` without `name`, or with `name` set to `value`.
    fn written_down<'a>(name: &str, value: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
        let mut pairs: Vec<(&str, &str)> = WRITTEN_DOWN
            .iter()
            .copied()
            .filter(|(other, _)| *other != name)
            .collect();
        if let Some(value) = value {
            pairs.push((
                WRITTEN_DOWN
                    .iter()
                    .find(|(other, _)| *other == name)
                    .unwrap()
                    .0,
                value,
            ));
        }
        pairs
    }

    #[test]
    fn robinhood_chain_does_not_start_until_every_setting_with_arcs_default_is_written_down() {
        for chain in ROBINHOOD {
            check(chain, &WRITTEN_DOWN).unwrap();
            // Nothing written down is Arc's configuration on a chain it is wrong for, and the first setting is named.
            let none = refused(chain, &[]);
            assert!(
                none.contains(&format!(
                    "MAX_FEE_PER_GAS_WEI is not set on chain {chain} (Robinhood Chain), where its default (100 gwei) is Arc's: set it explicitly"
                )),
                "{none}"
            );
            // Each one alone is missed and named, with the default it would have taken.
            for (name, default) in [
                ("MAX_FEE_PER_GAS_WEI", "100 gwei"),
                ("MAX_TX_COST_WEI", "0.2 of the native token"),
                ("MIN_PRIORITY_FEE_WEI", "1 gwei"),
                ("MAX_PRIORITY_FEE_WEI", "50 gwei"),
                ("SWEEP_MIN_RESERVE_WEI", "1 of the native token"),
                ("NONCE_STUCK_SECONDS", "120 seconds"),
                ("MAX_GAS", "3000000 gas"),
                ("FEE_COVERAGE_BPS", "10000 bps"),
            ] {
                let error = refused(chain, &written_down(name, None));
                assert!(
                    error.contains(&format!(
                        "{name} is not set on chain {chain} (Robinhood Chain), where its default ({default}) is Arc's"
                    )),
                    "{name}: {error}"
                );
            }
        }
    }

    #[test]
    fn a_blank_value_is_not_written_down() {
        for blank in ["", "  ", "\t"] {
            for (name, _) in WRITTEN_DOWN {
                let error = refused(4663, &written_down(name, Some(blank)));
                assert!(
                    error.contains(&format!("{name} is not set on chain 4663")),
                    "{name}: {error}"
                );
            }
        }
    }

    #[test]
    fn robinhood_chain_has_no_tip() {
        for chain in ROBINHOOD {
            for name in ["MIN_PRIORITY_FEE_WEI", "MAX_PRIORITY_FEE_WEI"] {
                for tip in ["1", "1000000000", "50000000000"] {
                    let error = refused(chain, &written_down(name, Some(tip)));
                    assert!(
                        error.contains(&format!(
                            "{name}={tip} is not allowed on chain {chain} (Robinhood Chain), which has no tip"
                        )),
                        "{error}"
                    );
                }
                let error = refused(chain, &written_down(name, Some("zero")));
                assert!(error.contains(&format!("Invalid {name}")), "{error}");
                // Zero in a longer spelling is still zero.
                check(chain, &written_down(name, Some("000"))).unwrap();
            }
        }
    }

    #[test]
    fn arc_and_every_other_chain_take_their_defaults_as_before() {
        for chain in ARC.into_iter().chain(OTHER) {
            check(chain, &[]).unwrap();
            // Whatever they write down is theirs, tips included.
            check(
                chain,
                &[
                    ("MIN_PRIORITY_FEE_WEI", "1000000000"),
                    ("MAX_PRIORITY_FEE_WEI", "50000000000"),
                    ("MAX_GAS", "13000000"),
                ],
            )
            .unwrap();
        }
    }

    /// `environment` of an epoch coordinator's keeper: no COORDINATOR_KIND, and the registry's pin the loader requires
    /// off the local test chain.
    pub(super) fn epoch_environment(chain_id: u64) -> BTreeMap<String, String> {
        let mut settings = environment(chain_id);
        settings.remove("COORDINATOR_KIND");
        settings.insert(
            "EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH".into(),
            format!("0x{}04", "00".repeat(31)),
        );
        settings
    }
    /// The environment of a keeper on `chain_id` as the loader requires it, with the settings of the design for
    /// Robinhood Chain: a round coordinator's keeper, which pins no registry.
    pub(super) fn environment(chain_id: u64) -> BTreeMap<String, String> {
        let database = std::env::temp_dir().join("economics-keeper.sqlite");
        let key = |name: &str| {
            std::env::temp_dir()
                .join(name)
                .to_string_lossy()
                .into_owned()
        };
        let hash = |last: u8| format!("0x{}{last:02x}", "00".repeat(31));
        let mut pairs: Vec<(&str, String)> = vec![
            ("CHAIN_ID", chain_id.to_string()),
            ("RPC_URLS", "https://rpc.invalid".into()),
            (
                "COORDINATOR_ADDRESS",
                Address::repeat_byte(0xc0).to_string(),
            ),
            ("KEEPER_DB", database.to_string_lossy().into_owned()),
            ("TX_KEY_FILE", key("transaction.key")),
            ("VRF_KEY_FILE", key("vrf.key")),
            ("EXPECTED_CODE_HASH", hash(1)),
            ("EXPECTED_PROTOCOL_HASH", hash(2)),
            ("EXPECTED_IMPLEMENTATION_CODE_HASH", hash(3)),
            ("CANCEL_MAX_FEE_PER_GAS_WEI", "3500000000".into()),
            ("FINALITY_MODE", "soft".into()),
            ("GAS_MODEL", "arbitrum".into()),
            ("COORDINATOR_KIND", "round".into()),
            ("FULFILL_BATCH_MAX", "16".into()),
        ];
        pairs.extend(WRITTEN_DOWN.map(|(name, value)| (name, value.to_owned())));
        pairs
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect()
    }

    #[test]
    fn the_loader_refuses_a_robinhood_keeper_that_leaves_arcs_defaults_and_loads_the_design_values()
    {
        // The design's values load on the local test chain, which takes any of them, and on both Robinhood chains,
        // where they are what the policy asks for: soft finality is allowed there now that its auditor and its halt
        // exist.
        for chain in [31337, ROBINHOOD[0], ROBINHOOD[1]] {
            let config = crate::rig::load(&environment(chain)).unwrap();
            assert_eq!(
                (
                    config.max_fee,
                    config.cancel_max_fee,
                    config.max_cost,
                    config.max_gas
                ),
                (
                    3_000_000_000,
                    3_500_000_000,
                    5_000_000_000_000_000,
                    13_000_000
                ),
                "chain {chain}"
            );
            assert_eq!(
                (
                    config.min_priority_fee,
                    config.max_priority_fee,
                    config.nonce_stuck_seconds
                ),
                (0, 0, 60)
            );
            assert_eq!(config.chain.sweep_min_reserve_wei, 5_000_000_000_000_000);
            assert_eq!(config.chain.finality_mode, FinalityMode::Soft);
            assert_eq!(config.chain.coordinator_kind, CoordinatorKind::Round);
            assert_eq!(config.fee_coverage_bps, 12_500);
            // A round coordinator's keeper pins no registry.
            assert_eq!(
                (
                    config.registry_implementation_code_hash,
                    config.approved_next_registry_implementation_code_hash
                ),
                (None, None)
            );
        }
        for chain in ROBINHOOD {
            for (name, _) in WRITTEN_DOWN {
                let mut missing = environment(chain);
                missing.remove(name);
                let error = format!("{:#}", crate::rig::load(&missing).err().unwrap());
                assert!(
                    error.contains(&format!("{name} is not set on chain {chain}")),
                    "{name}: {error}"
                );
            }
            // A tip is refused by the loader too.
            let mut tipped = environment(chain);
            tipped.insert("MAX_PRIORITY_FEE_WEI".into(), "1000000000".into());
            let error = format!("{:#}", crate::rig::load(&tipped).err().unwrap());
            assert!(
                error.contains("MAX_PRIORITY_FEE_WEI=1000000000 is not allowed"),
                "{error}"
            );
        }
    }

    #[test]
    fn the_loader_still_starts_an_arc_keeper_on_its_defaults() {
        for chain in ARC {
            let mut arc = epoch_environment(chain);
            for name in [
                "FINALITY_MODE",
                "GAS_MODEL",
                "FEE_COVERAGE_BPS",
                "MAX_FEE_PER_GAS_WEI",
                "MAX_TX_COST_WEI",
                "MIN_PRIORITY_FEE_WEI",
                "MAX_PRIORITY_FEE_WEI",
                "SWEEP_MIN_RESERVE_WEI",
                "NONCE_STUCK_SECONDS",
                "MAX_GAS",
                "FULFILL_BATCH_MAX",
            ] {
                arc.remove(name);
            }
            arc.insert("CANCEL_MAX_FEE_PER_GAS_WEI".into(), "150000000000".into());
            let config = crate::rig::load(&arc).unwrap();
            assert_eq!(
                (
                    config.max_fee,
                    config.max_cost,
                    config.max_gas,
                    config.nonce_stuck_seconds
                ),
                (100_000_000_000, 200_000_000_000_000_000, 3_000_000, 120)
            );
            assert_eq!(
                (config.min_priority_fee, config.max_priority_fee),
                (1_000_000_000, 50_000_000_000)
            );
            assert_eq!(config.fee_coverage_bps, 10_000);
            assert_eq!(config.chain, ChainSettings::default());
            assert!(config.registry_implementation_code_hash.is_some());
        }
    }
    #[test]
    fn robinhood_chain_requires_a_margin_of_at_least_1000_bps_on_the_l1_component() {
        const SWITCHES: [(&str, &str); 3] = [
            ("FINALITY_MODE", "soft"),
            ("GAS_MODEL", "arbitrum"),
            ("COORDINATOR_KIND", "round"),
        ];
        let resolve = |chain_id: u64, margin: Option<&str>| {
            let mut environment: HashMap<&str, &str> = SWITCHES.into_iter().collect();
            if let Some(margin) = margin {
                environment.insert("L1_GAS_MARGIN_BPS", margin);
            }
            ChainSettings::resolve(chain_id, &|name| {
                environment.get(name).map(|value| (*value).to_owned())
            })
        };
        for chain in ROBINHOOD {
            // Unset is the default, a quarter; the least is a tenth, and more is the operator's.
            assert_eq!(resolve(chain, None).unwrap().l1_gas_margin_bps, 2_500);
            for margin in ["1000", "1001", "2500", "20000"] {
                resolve(chain, Some(margin)).unwrap();
            }
            for margin in ["0", "1", "999"] {
                let error = format!("{:#}", resolve(chain, Some(margin)).unwrap_err());
                assert!(
                    error.contains(&format!(
                        "L1_GAS_MARGIN_BPS={margin} is not allowed on chain {chain} (Robinhood Chain), which requires at least 1000"
                    )),
                    "{margin}: {error}"
                );
            }
            // The loader refuses it too, before anything else of the keeper starts.
            let mut zero = environment(chain);
            zero.insert("L1_GAS_MARGIN_BPS".into(), "0".into());
            let error = format!("{:#}", crate::rig::load(&zero).err().unwrap());
            assert!(
                error.contains("L1_GAS_MARGIN_BPS=0 is not allowed"),
                "{error}"
            );
            // The least margin is past that refusal, and the keeper loads.
            zero.insert("L1_GAS_MARGIN_BPS".into(), "1000".into());
            assert_eq!(
                crate::rig::load(&zero).unwrap().chain.l1_gas_margin_bps,
                1_000
            );
        }
        // The chains that take any combination take any margin.
        for chain in [31337, 412346] {
            assert_eq!(
                ChainSettings::resolve(chain, &|name| {
                    (name == "GAS_MODEL")
                        .then(|| "arbitrum".to_owned())
                        .or_else(|| (name == "L1_GAS_MARGIN_BPS").then(|| "0".to_owned()))
                })
                .unwrap()
                .l1_gas_margin_bps,
                0
            );
        }
    }
}

/// Where soft finality may start: the chains the policy table lets take it, now that the auditor and the halt exist.
#[cfg(test)]
mod soft_finality_policy_tests {
    use super::economics_tests::{environment, epoch_environment};
    use super::*;

    const ROBINHOOD: [u64; 2] = [4663, 46630];
    const ARC: [u64; 2] = [5042, 5042002];
    const LOCAL: [u64; 2] = [31337, 412346];

    fn loaded(
        chain: u64,
        edit: impl Fn(&mut std::collections::BTreeMap<String, String>),
    ) -> String {
        let mut settings = environment(chain);
        edit(&mut settings);
        match crate::rig::load(&settings) {
            Ok(_) => "loaded".to_owned(),
            Err(error) => format!("{error:#}"),
        }
    }

    #[test]
    fn soft_mode_starts_on_the_robinhood_chains_and_the_two_local_test_chains() {
        for chain in ROBINHOOD.into_iter().chain(LOCAL) {
            let config = crate::rig::load(&environment(chain)).unwrap();
            assert_eq!(config.chain.finality_mode, FinalityMode::Soft, "{chain}");
        }
    }

    #[test]
    fn soft_mode_is_still_refused_on_arc_and_on_every_chain_the_policy_does_not_list() {
        // Arc is refused by its policy, in its own words.
        for chain in ARC {
            let error = loaded(chain, |_| {});
            assert!(
                error.contains(&format!(
                    "FINALITY_MODE=soft is not allowed on chain {chain} (Arc)"
                )),
                "{error}"
            );
        }
        // A chain no table lists reads finalized state, as 0.4.1 did: soft is for the four that take it.
        for chain in [1, 8453, 421614, u64::MAX] {
            let mut unlisted = epoch_environment(chain);
            unlisted.remove("GAS_MODEL");
            let error = match crate::rig::load(&unlisted) {
                Ok(_) => "loaded".to_owned(),
                Err(error) => format!("{error:#}"),
            };
            assert!(
                error.contains(&format!(
                    "FINALITY_MODE=soft is allowed only on chains 4663, 46630, 31337 and 412346, not on chain {chain}"
                )),
                "{chain}: {error}"
            );
        }
        // And the sentence of the C2 gate is gone from everything.
        assert!(!loaded(ARC[0], |_| {}).contains("finality auditor"));
    }

    #[test]
    fn finalized_mode_is_still_refused_on_robinhood_where_it_cannot_work_and_loads_elsewhere() {
        for chain in ROBINHOOD {
            let finalized = loaded(chain, |settings| {
                settings.insert("FINALITY_MODE".into(), "finalized".into());
            });
            assert!(
                finalized.contains(&format!(
                    "FINALITY_MODE=finalized is not allowed on chain {chain} (Robinhood Chain), which requires FINALITY_MODE=soft"
                )),
                "{finalized}"
            );
        }
        for chain in LOCAL {
            let mut settings = epoch_environment(chain);
            settings.remove("FINALITY_MODE");
            settings.remove("GAS_MODEL");
            let config = crate::rig::load(&settings).unwrap();
            assert_eq!(config.chain.finality_mode, FinalityMode::Finalized);
        }
    }
}

/// COORDINATOR_KIND (keeper task C1c): what each chain requires of it, what a round coordinator's keeper refuses, and
/// what it must write down because no default of it is a round coordinator's.
#[cfg(test)]
mod coordinator_kind_tests {
    use super::economics_tests::{environment, epoch_environment};
    use super::*;
    use std::collections::{BTreeMap, HashMap};

    const ARC: [u64; 2] = [5042, 5042002];
    const ROBINHOOD: [u64; 2] = [4663, 46630];
    const LOCAL: [u64; 2] = [31337, 412346];
    const UNLISTED: [u64; 3] = [1, 8453, 421614];
    /// The Arc-scaled settings with the values the design gives Robinhood Chain.
    const WRITTEN: [(&str, &str); 7] = [
        ("MAX_FEE_PER_GAS_WEI", "3000000000"),
        ("MAX_TX_COST_WEI", "5000000000000000"),
        ("MIN_PRIORITY_FEE_WEI", "0"),
        ("MAX_PRIORITY_FEE_WEI", "0"),
        ("SWEEP_MIN_RESERVE_WEI", "5000000000000000"),
        ("NONCE_STUCK_SECONDS", "60"),
        ("MAX_GAS", "13000000"),
    ];

    fn resolve(chain_id: u64, pairs: &[(&str, &str)]) -> Result<ChainSettings> {
        let environment: HashMap<&str, &str> = pairs.iter().copied().collect();
        ChainSettings::resolve(chain_id, &|name| {
            environment.get(name).map(|value| (*value).to_owned())
        })
    }
    fn economics(chain_id: u64, pairs: &[(&str, &str)]) -> Result<()> {
        let environment: HashMap<&str, &str> = pairs.iter().copied().collect();
        ChainSettings::check_economics(chain_id, &|name| {
            environment.get(name).map(|value| (*value).to_owned())
        })
    }
    fn loaded(settings: &BTreeMap<String, String>) -> String {
        match crate::rig::load(settings) {
            Ok(_) => "loaded".to_owned(),
            Err(error) => format!("{error:#}"),
        }
    }

    #[test]
    fn unset_is_epoch_and_the_policy_names_the_kind_each_chain_requires() {
        assert_eq!(CoordinatorKind::default(), CoordinatorKind::Epoch);
        for chain in ARC.into_iter().chain(LOCAL).chain(UNLISTED) {
            assert_eq!(
                resolve(chain, &[]).unwrap().coordinator_kind,
                CoordinatorKind::Epoch
            );
        }
        // Arc requires epoch, and says so when round is asked for.
        for chain in ARC {
            let error = format!(
                "{:#}",
                resolve(chain, &[("COORDINATOR_KIND", "round")]).unwrap_err()
            );
            assert!(
                error.contains(&format!(
                    "COORDINATOR_KIND=round is not allowed on chain {chain} (Arc), which requires COORDINATOR_KIND=epoch"
                )),
                "{error}"
            );
        }
        // Robinhood Chain requires round: left unset it would run an epoch coordinator's keeper, and is refused.
        let soft_arbitrum = [("FINALITY_MODE", "soft"), ("GAS_MODEL", "arbitrum")];
        for chain in ROBINHOOD {
            let error = format!("{:#}", resolve(chain, &soft_arbitrum).unwrap_err());
            assert!(
                error.contains(&format!(
                    "COORDINATOR_KIND defaults to epoch, which is not allowed on chain {chain} (Robinhood Chain), which requires COORDINATOR_KIND=round"
                )),
                "{error}"
            );
            let mut round = soft_arbitrum.to_vec();
            round.push(("COORDINATOR_KIND", "round"));
            assert_eq!(
                resolve(chain, &round).unwrap().coordinator_kind,
                CoordinatorKind::Round
            );
        }
        // The two local chains take both.
        for chain in LOCAL {
            for (word, kind) in COORDINATOR_KINDS {
                assert_eq!(
                    resolve(chain, &[("COORDINATOR_KIND", word)])
                        .unwrap()
                        .coordinator_kind,
                    kind
                );
            }
        }
    }

    #[test]
    fn round_mode_is_refused_on_a_chain_no_policy_lists() {
        for chain in UNLISTED {
            for pairs in [
                vec![("COORDINATOR_KIND", "round")],
                vec![
                    ("COORDINATOR_KIND", "round"),
                    ("FINALITY_MODE", "soft"),
                    ("GAS_MODEL", "arbitrum"),
                ],
            ] {
                let error = format!("{:#}", resolve(chain, &pairs).unwrap_err());
                assert!(
                    error.contains(&format!(
                        "COORDINATOR_KIND=round is allowed only on chains 4663, 46630, 31337 and 412346, not on chain {chain}, which no chain policy lists"
                    )),
                    "{chain}: {error}"
                );
            }
            // An epoch coordinator's keeper runs there as 0.4.1 did.
            resolve(chain, &[("COORDINATOR_KIND", "epoch")]).unwrap();
            resolve(chain, &[]).unwrap();
        }
        // The loader refuses it before anything else is read of the chain.
        let mut unlisted = environment(31337);
        unlisted.insert("CHAIN_ID".into(), "8453".into());
        let error = loaded(&unlisted);
        assert!(
            error.contains("COORDINATOR_KIND=round is allowed only on chains"),
            "{error}"
        );
    }

    #[test]
    fn a_round_coordinators_keeper_refuses_every_registry_and_epoch_setting() {
        let value = format!("0x{}07", "00".repeat(31));
        for chain in LOCAL.into_iter().chain(ROBINHOOD) {
            let mut base = vec![("COORDINATOR_KIND", "round")];
            if ROBINHOOD.contains(&chain) {
                base.extend([("FINALITY_MODE", "soft"), ("GAS_MODEL", "arbitrum")]);
            }
            resolve(chain, &base).unwrap();
            for name in EPOCH_ONLY_SETTINGS {
                let mut pairs = base.clone();
                pairs.push((name, &value));
                let error = format!("{:#}", resolve(chain, &pairs).unwrap_err());
                assert!(
                    error.contains(&format!("{name} is refused with COORDINATOR_KIND=round")),
                    "{chain} {name}: {error}"
                );
                // An empty value is unset, as for every other setting.
                let mut blank = base.clone();
                blank.push((name, ""));
                resolve(chain, &blank).unwrap();
            }
        }
        // The same names are an epoch coordinator's business: its keeper reads the registry's two and ignores the rest,
        // as 0.4.1 did.
        for chain in ARC.into_iter().chain(LOCAL) {
            for name in EPOCH_ONLY_SETTINGS {
                resolve(chain, &[(name, &value)]).unwrap();
            }
        }
        assert_eq!(
            EPOCH_ONLY_SETTINGS,
            [
                "EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH",
                "APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH",
                "REGISTRY_KIND",
                "BLOCK_NUDGE",
                "BLOCK_NUDGE_AFTER_MS",
                "EPOCH_API_ENDPOINTS",
            ]
        );
    }

    #[test]
    fn the_loader_pins_no_registry_in_round_mode_and_still_requires_its_pin_in_epoch_mode() {
        for chain in ROBINHOOD {
            // The design's round keeper loads without a registry pin.
            assert_eq!(loaded(&environment(chain)), "loaded");
            // And refuses one, or the registry's approved next implementation, by name.
            for name in [
                "EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH",
                "APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH",
                "BLOCK_NUDGE",
                "REGISTRY_KIND",
            ] {
                let mut settings = environment(chain);
                settings.insert(name.into(), format!("0x{}04", "00".repeat(31)));
                let error = loaded(&settings);
                assert!(
                    error.contains(&format!("{name} is refused with COORDINATOR_KIND=round")),
                    "{name}: {error}"
                );
            }
            // Its own three pins stay required off the local chain.
            let mut unpinned = environment(chain);
            unpinned.remove("EXPECTED_IMPLEMENTATION_CODE_HASH");
            let error = loaded(&unpinned);
            assert!(
                error.contains(
                    "Nonlocal networks require the round coordinator's proxy, implementation and protocol configuration hashes"
                ),
                "{error}"
            );
        }
        // An epoch keeper off the local chain still requires the registry's pin, in the words of 0.4.1.
        let mut arc = epoch_environment(5_042_002);
        for name in ["FINALITY_MODE", "GAS_MODEL"] {
            arc.remove(name);
        }
        arc.insert("CANCEL_MAX_FEE_PER_GAS_WEI".into(), "150000000000".into());
        assert_eq!(loaded(&arc), "loaded");
        arc.remove("EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH");
        let error = loaded(&arc);
        assert!(
            error.contains(
                "Nonlocal networks require proxy, both implementation, and protocol configuration hashes"
            ),
            "{error}"
        );
    }

    #[test]
    fn robinhood_requires_the_fee_coverage_written_down_and_at_least_12500() {
        assert_eq!(ROBINHOOD_MIN_FEE_COVERAGE_BPS, 12_500);
        for chain in ROBINHOOD {
            // Unset would be Arc's 10000, and is refused before anything else of the coverage.
            let error = format!("{:#}", economics(chain, &WRITTEN).unwrap_err());
            assert!(
                error.contains(&format!(
                    "FEE_COVERAGE_BPS is not set on chain {chain} (Robinhood Chain), where its default (10000 bps) is Arc's"
                )),
                "{error}"
            );
            for low in ["0", "10000", "12499"] {
                let mut pairs = WRITTEN.to_vec();
                pairs.push(("FEE_COVERAGE_BPS", low));
                let error = format!("{:#}", economics(chain, &pairs).unwrap_err());
                assert!(
                    error.contains(&format!(
                        "FEE_COVERAGE_BPS={low} is not allowed on chain {chain} (Robinhood Chain), which requires at least 12500"
                    )),
                    "{low}: {error}"
                );
            }
            for enough in ["12500", "12501", "20000", "100000"] {
                let mut pairs = WRITTEN.to_vec();
                pairs.push(("FEE_COVERAGE_BPS", enough));
                economics(chain, &pairs).unwrap();
            }
            let mut pairs = WRITTEN.to_vec();
            pairs.push(("FEE_COVERAGE_BPS", "a lot"));
            assert!(
                format!("{:#}", economics(chain, &pairs).unwrap_err())
                    .contains("Invalid FEE_COVERAGE_BPS")
            );
            // The loader refuses it too.
            let mut low = environment(chain);
            low.insert("FEE_COVERAGE_BPS".into(), "12499".into());
            assert!(loaded(&low).contains("FEE_COVERAGE_BPS=12499 is not allowed"));
        }
        // Every other chain takes any coverage, its default included.
        for chain in ARC.into_iter().chain(LOCAL).chain(UNLISTED) {
            economics(chain, &[]).unwrap();
            economics(chain, &[("FEE_COVERAGE_BPS", "0")]).unwrap();
        }
    }

    #[test]
    fn round_mode_takes_no_arc_scaled_default_on_any_chain() {
        for chain in LOCAL.into_iter().chain(UNLISTED) {
            let mut round = WRITTEN.to_vec();
            round.push(("COORDINATOR_KIND", "round"));
            economics(chain, &round).unwrap();
            for (name, default) in ARC_SCALED {
                let missing: Vec<(&str, &str)> = round
                    .iter()
                    .copied()
                    .filter(|(other, _)| other != name)
                    .collect();
                let error = format!("{:#}", economics(chain, &missing).unwrap_err());
                assert!(
                    error.contains(&format!(
                        "{name} is not set with COORDINATOR_KIND=round, where its default ({default}) is Arc's: set it explicitly"
                    )),
                    "{chain} {name}: {error}"
                );
            }
            // An epoch coordinator's keeper on the same chain takes the defaults, as before.
            economics(chain, &[]).unwrap();
        }
        // The loader: a round keeper on the local test chain that leaves one of them out is refused.
        let mut local = environment(31337);
        local.remove("MAX_GAS");
        let error = loaded(&local);
        assert!(
            error.contains("MAX_GAS is not set with COORDINATOR_KIND=round"),
            "{error}"
        );
    }

    #[test]
    fn a_round_keepers_idle_heartbeat_is_2_to_30_seconds_inside_the_deadline_and_an_epoch_keeper_keeps_its_own()
     {
        let seconds = std::time::Duration::from_secs;
        for chain in ROBINHOOD.into_iter().chain([31337]) {
            assert_eq!(
                crate::rig::load(&environment(chain))
                    .unwrap()
                    .idle_heartbeat,
                seconds(IDLE_HEARTBEAT_DEFAULT)
            );
            assert_eq!(IDLE_HEARTBEAT_DEFAULT, 30);
            for value in [2, 15, 30] {
                let mut settings = environment(chain);
                settings.insert("IDLE_HEARTBEAT_SECONDS".into(), value.to_string());
                assert_eq!(
                    crate::rig::load(&settings).unwrap().idle_heartbeat,
                    seconds(value)
                );
            }
            for value in ["0", "1", "31", "600"] {
                let mut settings = environment(chain);
                settings.insert("IDLE_HEARTBEAT_SECONDS".into(), value.into());
                let error = loaded(&settings);
                assert!(
                    error.contains("IDLE_HEARTBEAT_SECONDS must be between 2 and 30"),
                    "{value}: {error}"
                );
            }
            let mut settings = environment(chain);
            settings.insert("IDLE_HEARTBEAT_SECONDS".into(), "ten".into());
            assert!(loaded(&settings).contains("Invalid IDLE_HEARTBEAT_SECONDS"));
            // A request whose event was missed is found at the next heartbeat, and must still be served: the heartbeat, the
            // send margin and a fulfillment round fit its 60 seconds. 30 + 20 + 10 does; 30 + 21 + 10 does not.
            let mut settings = environment(chain);
            settings.insert("IDLE_HEARTBEAT_SECONDS".into(), "30".into());
            settings.insert("SEND_MARGIN_SECONDS".into(), "20".into());
            assert_eq!(loaded(&settings), "loaded");
            settings.insert("SEND_MARGIN_SECONDS".into(), "21".into());
            let error = loaded(&settings);
            assert!(
                error.contains(
                    "IDLE_HEARTBEAT_SECONDS (30), SEND_MARGIN_SECONDS (21) and a 10-second fulfillment round must fit the 60-second request deadline"
                ),
                "{error}"
            );
            // The default heartbeat is the longest, and holds the margin to 20 seconds; a shorter one allows more.
            let mut settings = environment(chain);
            settings.insert("SEND_MARGIN_SECONDS".into(), "20".into());
            assert_eq!(loaded(&settings), "loaded");
            settings.insert("SEND_MARGIN_SECONDS".into(), "21".into());
            assert!(loaded(&settings).contains("must fit the 60-second request deadline"));
            settings.insert("IDLE_HEARTBEAT_SECONDS".into(), "10".into());
            settings.insert("SEND_MARGIN_SECONDS".into(), "40".into());
            assert_eq!(loaded(&settings), "loaded");
            settings.insert("SEND_MARGIN_SECONDS".into(), "41".into());
            assert!(loaded(&settings).contains("must fit the 60-second request deadline"));
        }
        // An epoch keeper keeps its 5-second heartbeat and refuses the setting by name.
        let mut arc = epoch_environment(5_042_002);
        for name in ["FINALITY_MODE", "GAS_MODEL"] {
            arc.remove(name);
        }
        arc.insert("CANCEL_MAX_FEE_PER_GAS_WEI".into(), "150000000000".into());
        assert_eq!(
            crate::rig::load(&arc).unwrap().idle_heartbeat,
            crate::events::IDLE_HEARTBEAT
        );
        arc.insert("IDLE_HEARTBEAT_SECONDS".into(), "10".into());
        let error = loaded(&arc);
        assert!(
            error.contains("IDLE_HEARTBEAT_SECONDS applies only with COORDINATOR_KIND=round"),
            "{error}"
        );
    }

    #[test]
    fn the_block_nudge_and_the_registry_kind_are_not_settings_any_more() {
        // An epoch keeper does not read them: whatever they say, the keeper is the one it is without them.
        for chain in ARC {
            let mut arc = epoch_environment(chain);
            for name in ["FINALITY_MODE", "GAS_MODEL"] {
                arc.remove(name);
            }
            arc.insert("CANCEL_MAX_FEE_PER_GAS_WEI".into(), "150000000000".into());
            let plain = crate::rig::load(&arc).unwrap();
            for (name, value) in [
                ("BLOCK_NUDGE", "true"),
                ("BLOCK_NUDGE_AFTER_MS", "1"),
                ("REGISTRY_KIND", "beacon"),
            ] {
                let mut with = arc.clone();
                with.insert(name.into(), value.into());
                let config = crate::rig::load(&with).unwrap();
                assert_eq!(config.chain, plain.chain, "{name}");
            }
        }
    }
}
