use crate::{
    abi::{ApiProof, Coordinator as C, EpochRegistry as ER, Request, VrfProof},
    config::{
        Config, FOLLOWER_LEAVE_AGE_SECONDS, FOLLOWER_LEAVE_PENDING, FeeBudget, FeeCap,
        RESPONSE_TIMEOUT_SECONDS, Role, SAFETY_AGE_SECONDS,
    },
    journal::{Attempt, Job, Journal, is_batch_job},
    prover,
    rpc::{Head, Rpc, quantity},
};
use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, keccak256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolEvent};
use anyhow::{Result, bail, ensure};
use futures_util::{StreamExt, stream};
use k256::SecretKey;
use serde_json::json;
use std::future::Future;

pub struct Worker {
    pub cfg: Config,
    pub rpc: Rpc,
    pub journal: Journal,
    vrf_key: SecretKey,
    tx_key: PrivateKeySigner,
    _scope_locks: Vec<std::fs::File>,
    epoch: crate::epoch::Publisher,
    telegram: Option<crate::telegram::TelegramNotifier>,
    discord: Option<crate::discord::Notifier>,
    proof_slots: std::sync::Arc<tokio::sync::Semaphore>,
    runtime_pins: crate::proxy::RuntimePins,
    /// Whether the transaction wallet may currently publish in its role. False disables sending only.
    authorized: std::sync::atomic::AtomicBool,
    /// A follower's observation of committer()'s confirmed nonce; kept in memory, so a restart observes afresh.
    primary: std::sync::Mutex<crate::liveness::PrimaryLiveness>,
    /// This tick's follower join decision, and when each epoch attempt first had live paid demand for this node.
    policy: std::sync::Mutex<SendPolicy>,
    epoch_demand_since: std::sync::Mutex<std::collections::BTreeMap<String, u64>>,
    /// Pushed chain events (upgrades, role changes, blocks). Without a subscription they never fire.
    signals: std::sync::Arc<crate::events::Signals>,
    /// The last successful runtime pin verification and the upgrade-event generation it covered.
    runtime_verified: std::sync::Mutex<(tokio::time::Instant, u64)>,
    /// Set once a proxy is seen on its approved next implementation. From then on this process signs and sends
    /// nothing, raises no error notices, and the run loop exits so that a restart verifies the new code.
    approved_upgrade: std::sync::OnceLock<crate::proxy::ApprovedUpgrade>,
    /// The operator notice of a first start on an approved implementation, sent once Telegram is attached.
    upgrade_notice: Option<String>,
    /// The last publishing-right check and the role-event generation it covered.
    authorization_checked: std::sync::Mutex<(tokio::time::Instant, u64)>,
    /// The finalized block the last tick read. Work events above it keep the loop in its busy cadence.
    last_finalized: std::sync::atomic::AtomicU64,
}
/// Without any send, the runtime pins are still re-verified this often, so an idle keeper without an event
/// subscription notices a proxy upgrade within a minute. Every signature and broadcast verifies them immediately
/// before it regardless, and an Upgraded event forces the check at the next tick.
const PIN_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(60);
/// How old the publishing-right check may be when work is open, and when it is not. A role event forces it.
const AUTHORIZATION_BUSY: std::time::Duration = std::time::Duration::from_secs(2);
const AUTHORIZATION_IDLE: std::time::Duration = std::time::Duration::from_secs(30);
/// Request reads per JSON-RPC batch: a fulfillment batch's members, or one slice of a discovery page.
const REQUEST_BATCH: usize = 16;
struct TxPlan {
    nonce: u64,
    gas: u64,
    fee: u128,
    priority: u128,
    payload: String,
    kind: String,
}
#[derive(Debug)]
struct SendDeferred {
    error: anyhow::Error,
    /// Set when a configured cap, not chain or RPC state, prevents the send.
    budget: Option<FeeBudget>,
}
impl SendDeferred {
    fn new(error: anyhow::Error) -> Self {
        Self {
            error,
            budget: None,
        }
    }
    fn budget(exceeded: FeeBudget) -> Self {
        Self {
            error: anyhow::anyhow!("Transaction exceeds fee/cost budget: {exceeded}"),
            budget: Some(exceeded),
        }
    }
}
impl std::fmt::Display for SendDeferred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}
impl std::error::Error for SendDeferred {}
/// Recent blocks whose median tip prices new sends and replacements.
const PRIORITY_FEE_BLOCKS: u64 = 20;
/// Verified endpoints can briefly disagree: one reports a consumed nonce before another serves
/// its receipt or the resulting contract state. Within this window the lane simply stays busy.
const RECEIPT_VISIBILITY_GRACE_SECONDS: u64 = 30;
/// Whether a consumed nonce may still be waiting for its receipt to become visible.
fn awaiting_receipt_visibility(now: u64, attempt: &Attempt) -> bool {
    let observed = attempt.broadcast.max(attempt.created).max(0) as u64;
    now.saturating_sub(observed) < RECEIPT_VISIBILITY_GRACE_SECONDS
}
/// Sends price gas at twice the observed base fee plus the bounded priority fee.
fn required_fee(base_fee: u128, priority: u128) -> Result<u128> {
    base_fee
        .checked_mul(2)
        .and_then(|fee| fee.checked_add(priority))
        .ok_or_else(|| anyhow::anyhow!("Fee overflow"))
}
/// The recent median tip, kept within the configured bounds. An unavailable fee history
/// falls back to the minimum: a missing observation must never raise what the keeper pays.
fn bounded_priority(observed: Option<u128>, min: u128, max: u128) -> u128 {
    observed.unwrap_or(min).clamp(min, max)
}
/// A replacement must outbid the previous attempt by 12.5% and never tip below the current market.
fn replacement_fees(
    previous_priority: u128,
    previous_fee: u128,
    base_fee: u128,
    tip: u128,
) -> Result<(u128, u128)> {
    let priority = bump(previous_priority)?.max(tip);
    let fee = bump(previous_fee)?.max(base_fee.saturating_mul(2).saturating_add(priority));
    Ok((fee, priority))
}
/// Advisory check: does the fulfillment cap cover a send at the observed base fee?
fn fee_headroom(base_fee: u128, priority: u128, max_fee: u128) -> Option<FeeBudget> {
    let required = base_fee.saturating_mul(2).saturating_add(priority);
    (required > max_fee).then_some(FeeBudget {
        cap: FeeCap::MaxFeePerGas,
        required,
        limit: max_fee,
    })
}
/// Cost at the observed base fee and tip for the estimated (unpadded) gas.
fn expected_cost(base_fee: u128, priority: u128, gas: u64) -> u128 {
    base_fee
        .saturating_add(priority)
        .saturating_mul(u128::from(gas))
}
/// Fulfillments pay for themselves: escrowed fees must cover coverage_bps of the expected cost.
/// Uncovered requests stay prepared; if they expire their fee is refunded, so the keeper never
/// spends beyond what users paid. 0 disables the rule, leaving only the operator caps.
fn uncovered(cost: u128, fees: u128, coverage_bps: u64) -> Option<FeeBudget> {
    let required = cost.saturating_mul(u128::from(coverage_bps)) / 10_000;
    (coverage_bps != 0 && required > fees).then_some(FeeBudget {
        cap: FeeCap::FeeCoverage,
        required,
        limit: fees,
    })
}
/// The first configured cap a plan exceeds. Cancellations use their own recovery cap.
fn over_budget(
    plan: &TxPlan,
    fulfill_cap: u128,
    cancel_cap: u128,
    max_cost: u128,
) -> Option<FeeBudget> {
    let (cap, limit) = if plan.kind == "cancel" || plan.kind == "epoch_cancel" {
        (FeeCap::CancelMaxFeePerGas, cancel_cap)
    } else {
        (FeeCap::MaxFeePerGas, fulfill_cap)
    };
    if plan.fee > limit {
        return Some(FeeBudget {
            cap,
            required: plan.fee,
            limit,
        });
    }
    match plan.fee.checked_mul(u128::from(plan.gas)) {
        Some(cost) if cost <= max_cost => None,
        cost => Some(FeeBudget {
            cap: FeeCap::MaxTxCost,
            required: cost.unwrap_or(u128::MAX),
            limit: max_cost,
        }),
    }
}
pub fn terminal(r: &Request, now: u64) -> Option<&'static str> {
    if r.fulfilled {
        Some("served")
    } else if r.refunded {
        Some("refunded")
    } else if now > r.deadline {
        Some("expired")
    } else {
        None
    }
}
/// A request that may still be sent: neither terminal nor inside the send margin.
fn timely(r: &Request, now: u64, margin: u64) -> bool {
    terminal(r, now).is_none() && now.saturating_add(margin) < r.deadline
}
/// Journal state a request receives when its own nonce resolves: chain state wins; otherwise a
/// cancelled or reverted attempt blocks the job and a successful attempt that left no terminal
/// state is inconsistent. A single attempt that reverts failed on its own.
fn resolved_state(r: &Request, now: u64, kind: &str, status: u64) -> &'static str {
    terminal(r, now).unwrap_or(if kind == "cancel" || status == 0 {
        "blocked"
    } else {
        "inconsistent"
    })
}
/// Journal state a batch member receives when the batch nonce resolves. Chain state wins, as for a
/// single attempt. A member still live after its batch reverted has not failed on its own: any
/// member, or the batch as a whole, can revert the shared transaction. It returns to `prepared`,
/// is left out of later batches and is resent one at a time; the proof is already public and the
/// result fixed by the VRF, so the resend reveals nothing new. Only its own single attempt
/// reverting blocks it.
fn batch_member_state(r: &Request, now: u64, kind: &str, status: u64) -> &'static str {
    if kind == "fulfill_batch" && status == 0 {
        terminal(r, now).unwrap_or("prepared")
    } else {
        resolved_state(r, now, kind, status)
    }
}
/// The coordinator's CALLBACK_RESERVE: before each callback `_deliver` requires
/// `gasleft() >= callbackGasLimit + callbackGasLimit / 63 + 140_000` and otherwise reverts the
/// whole transaction with InsufficientCallbackGas.
const CALLBACK_RESERVE_GAS: u64 = 140_000;
/// The coordinator's own per-member requirement: the whole callback gas limit, the 1/63 the caller
/// keeps under EIP-150 and CALLBACK_RESERVE.
fn callback_budget(limit: u32) -> u64 {
    let limit = u64::from(limit);
    limit + limit / 63 + CALLBACK_RESERVE_GAS
}
/// Gas limit for a fulfillment whose members have these callback gas limits.
///
/// eth_estimateGas sees each callback only as expensive as it chooses to be in the simulation
/// (Arc simulates with tx.gasprice == 0), and a callback can also read the results of earlier
/// members, already stored in the same transaction. On chain it may then burn its whole limit,
/// leaving a later member short of its own gas check, which reverts the whole batch. The limit
/// therefore adds every member's full callback budget to the estimate, and never falls below the
/// usual padding of the estimate. A single fulfillment follows the same rule.
fn fulfillment_gas(estimate: u64, limits: &[u32]) -> Result<u64> {
    let padded = estimate
        .checked_mul(12)
        .map(|gas| gas / 10)
        .and_then(|gas| gas.checked_add(50_000));
    let reserved = limits.iter().try_fold(estimate, |gas, &limit| {
        gas.checked_add(callback_budget(limit))
    });
    match (padded, reserved) {
        (Some(padded), Some(reserved)) => Ok(padded.max(reserved)),
        _ => bail!("Gas overflow"),
    }
}
/// The largest gas limit the configured caps allow at this price per gas: MAX_GAS, and
/// MAX_TX_COST_WEI divided by the price.
fn gas_cap(max_gas: u64, max_cost: u128, fee: u128) -> u64 {
    match max_cost.checked_div(fee) {
        Some(by_cost) => max_gas.min(u64::try_from(by_cost).unwrap_or(u64::MAX)),
        None => max_gas,
    }
}
/// How many members, from the front of a batch, fit `cap` with every member's full callback
/// budget, predicted from the estimate of the whole batch at an even share per member. The caller
/// re-estimates the shorter batch and shrinks it again if the prediction was short.
fn members_within(estimate: u64, limits: &[u32], cap: u64) -> usize {
    let Some(share) = u64::try_from(limits.len())
        .ok()
        .filter(|&count| count > 0)
        .map(|count| estimate.div_ceil(count))
    else {
        return 0;
    };
    (1..=limits.len())
        .take_while(|&count| {
            u64::try_from(count)
                .ok()
                .and_then(|count| share.checked_mul(count))
                .and_then(|estimate| fulfillment_gas(estimate, &limits[..count]).ok())
                .is_some_and(|gas| gas <= cap)
        })
        .last()
        .unwrap_or(0)
}
/// A batch over a cap is shrunk to the members that fit at most this many times before the single
/// path takes over.
const MAX_BATCH_SHRINKS: u32 = 4;
/// Journal state epoch work receives when its nonce resolves: the registry's record or the packet's
/// own freshness wins; otherwise a successful nonce cancellation leaves the saved packet publishable,
/// because paid demand can arrive after the cancellation was signed, a reverted attempt blocks the
/// work, and a successful publication that left no epoch record is inconsistent.
fn epoch_resolved_state(terminal: Option<&'static str>, kind: &str, status: u64) -> &'static str {
    terminal.unwrap_or(if kind == "epoch_cancel" && status == 1 {
        "prepared"
    } else if kind == "epoch_cancel" || status == 0 {
        "blocked"
    } else {
        "inconsistent"
    })
}
/// Request IDs a finalized receipt actually served, from the coordinator's own
/// RandomnessFulfilled logs. A member skipped on chain has no such log.
fn fulfilled_in_receipt(
    receipt: &serde_json::Value,
    coordinator: alloy_primitives::Address,
) -> Result<std::collections::BTreeSet<U256>> {
    let mut served = std::collections::BTreeSet::new();
    for log in receipt["logs"].as_array().into_iter().flatten() {
        let address: alloy_primitives::Address = serde_json::from_value(log["address"].clone())?;
        let topics: Vec<B256> = serde_json::from_value(log["topics"].clone())?;
        if address == coordinator
            && topics.first() == Some(&C::RandomnessFulfilled::SIGNATURE_HASH)
            && let Some(id) = topics.get(1)
        {
            served.insert(U256::from_be_bytes(id.0));
        }
    }
    Ok(served)
}
/// A prepared request selected for one batch, with the proof its journaled calldata carries.
struct Member {
    id: String,
    request_id: U256,
    proof: VrfProof,
    deadline: u64,
    /// The request's callbackGasLimit: the gas its callback may burn on chain, reserved in full.
    callback_gas: u32,
    /// Escrowed fee; read only when the fee coverage rule is enabled.
    fee_paid: u128,
}
fn batch_payload(members: &[Member]) -> String {
    let call = C::fulfillRandomnessBatchCall {
        ids: members.iter().map(|m| m.request_id).collect(),
        proofs: members.iter().map(|m| m.proof.clone()).collect(),
    }
    .abi_encode();
    format!("0x{}", hex::encode(call))
}
enum BatchOutcome {
    /// A batch was signed, journaled and handed to broadcast.
    Sent,
    /// Fewer than two members qualified, the node rejected the batch preflight, or fewer than
    /// two members fit the caps with their full callback budgets: the single path serves this tick.
    Single,
}
pub async fn live_lower_bound<F, Fut>(end: u64, now: u64, mut deadline: F) -> Result<u64>
where
    F: FnMut(u64) -> Fut,
    Fut: Future<Output = Result<u64>>,
{
    live_lower_bound_from(1, end, now, &mut deadline).await
}
async fn live_lower_bound_from<F, Fut>(
    start: u64,
    end: u64,
    now: u64,
    mut deadline: F,
) -> Result<u64>
where
    F: FnMut(u64) -> Fut,
    Fut: Future<Output = Result<u64>>,
{
    let (mut lo, mut hi) = (start, end);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if deadline(mid).await? < now {
            lo = mid + 1
        } else {
            hi = mid
        }
    }
    Ok(lo)
}
// A lagging endpoint can report an empty page ending before our durable cursor.
// Only that exact shape is deferrable; malformed pages must still fail closed.
fn discovery_page_advances(cursor: u64, next: u64, ids: &[U256]) -> Result<bool> {
    if ids.is_empty() && next <= cursor {
        return Ok(false);
    }
    ensure!(
        next > cursor && next - cursor <= 256,
        "Invalid discovery cursor"
    );
    let mut previous = None;
    for id in ids {
        let id: u64 = (*id).try_into()?;
        ensure!(
            id >= cursor && id < next && previous.is_none_or(|previous| id > previous),
            "Invalid discovery page"
        );
        previous = Some(id);
    }
    Ok(true)
}
/// Prepared jobs in send order: earliest deadline first for the head of the queue, latest first for its tail.
fn prepared_in_order(jobs: Vec<Job>, tail_first: bool) -> Result<Vec<Job>> {
    let mut jobs = jobs
        .into_iter()
        .filter(|job| job.state == "prepared" && job.call.is_some())
        .map(|job| Ok((job.deadline, job.id.parse::<U256>()?, job)))
        .collect::<Result<Vec<_>>>()?;
    jobs.sort_by_key(|(deadline, id, _)| (*deadline, *id));
    if tail_first {
        jobs.reverse();
    }
    Ok(jobs.into_iter().map(|(_, _, job)| job).collect())
}
fn preparation_candidate(job: &Job, now: u64, margin: u64) -> bool {
    job.state == "pending" && job.call.is_none() && now + margin < job.deadline as u64
}
fn bump(value: u128) -> Result<u128> {
    Ok(value
        .checked_mul(9)
        .ok_or_else(|| anyhow::anyhow!("Fee overflow"))?
        / 8
        + 1)
}
/// What this node may send this tick. A primary works the head of the queue and sends everything. A follower works
/// the tail, and only once the join rule has fired; with several follower lanes each takes one residue class of the
/// request ids, so the lanes never meet on the same request without any coordination between them. Whatever the
/// role or lane, every node sends a request that has reached the safety age: that is the last line before a refund.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SendPolicy {
    joined: bool,
    tail_first: bool,
    rank: u64,
    lanes: u64,
}
impl SendPolicy {
    const PRIMARY: Self = Self {
        joined: true,
        tail_first: false,
        rank: 0,
        lanes: 1,
    };
    fn allows(&self, id: &str, deadline: u64, now: u64) -> bool {
        if now.saturating_add(SAFETY_AGE_SECONDS) >= deadline {
            return true;
        }
        self.joined && (self.lanes <= 1 || lane_of(id, self.lanes) == Some(self.rank))
    }
}
/// This keeper is about to try to prepare a request: record the wait if the request is work this keeper is responsible
/// for, which is everything for a primary and, for a follower, its lane once it has joined the queue (which a dead
/// primary also causes) and any request that has reached the safety age. A follower preparing the primary's work ahead
/// of a possible takeover is readiness, not a responsibility, and a request another keeper serves resolves its wait.
async fn note_preparation_attempt(
    journal: &Journal,
    send: bool,
    policy: &SendPolicy,
    id: &str,
    deadline: u64,
    chain_now: u64,
    wall_now: u64,
) -> Result<()> {
    if send && policy.allows(id, deadline, chain_now) {
        crate::health::preparation_waiting(journal, id, wall_now).await?;
    }
    Ok(())
}
/// Whether any of these jobs is work this keeper is responsible for sending under `policy` at `now`.
fn owns_any(policy: &SendPolicy, jobs: &[Job], now: u64) -> bool {
    jobs.iter()
        .any(|job| policy.allows(&job.id, u64::try_from(job.deadline).unwrap_or(0), now))
}
/// Send order for one pass, from the queue order of this node. Requests left out of batches that
/// this node is responsible for come first, in queue order, to be sent one at a time: the live
/// members of a batch that reverted, and requests whose own preflight the node rejected. Without
/// this, a steady stream of batchable requests would keep them waiting until they expire. Returns
/// the order and how many requests lead it this way.
fn resend_first(
    jobs: Vec<Job>,
    excluded: &std::collections::HashSet<String>,
    policy: &SendPolicy,
    now: u64,
) -> (Vec<Job>, usize) {
    let (mut order, rest): (Vec<Job>, Vec<Job>) = jobs.into_iter().partition(|job| {
        excluded.contains(&job.id) && owns_any(policy, std::slice::from_ref(job), now)
    });
    let leading = order.len();
    order.extend(rest);
    (order, leading)
}
/// A follower's settlement observation lasts only while it is responsible for sending something: a prepared, open
/// request of its joined lane or at the safety age, or a transaction of its own still unresolved. Once none is left,
/// for instance after it left the queue to the primary, the observation is removed. A primary's is unchanged: it
/// always owns the queue and clears it with its next broadcast.
async fn retire_follower_settlement(
    journal: &Journal,
    role: Role,
    policy: &SendPolicy,
    now: u64,
) -> Result<()> {
    if !role.is_follower() || !journal.unresolved().await?.is_empty() {
        return Ok(());
    }
    let prepared: Vec<Job> = sqlx::query("SELECT * FROM jobs WHERE state='prepared' AND deadline>? ORDER BY deadline DESC LIMIT 1024")
        .bind(i64::try_from(now)?)
        .fetch_all(&journal.pool)
        .await?
        .into_iter()
        .map(crate::journal::job_from_row)
        .collect();
    if !owns_any(policy, &prepared, now) {
        crate::health::recovered(journal, "settlement").await?;
    }
    Ok(())
}
/// At most this many of the oldest open jobs are re-read from chain in one follower tick.
const LIVENESS_REFRESH_JOBS: u32 = 8;
/// The lane a request id belongs to: id % lanes, from the decimal id as the coordinator emits it.
fn lane_of(id: &str, lanes: u64) -> Option<u64> {
    let lanes = U256::from(lanes);
    id.parse::<U256>().ok().map(|id| (id % lanes).to::<u64>())
}
/// Whether the transaction wallet may publish in its role: as committer() for a primary, or as an allowed backup
/// committer other than committer() for a follower.
pub(crate) enum WalletStatus {
    Authorized,
    /// The owner has not (or no longer) granted this wallet its role's right to publish. The keeper keeps running
    /// with sending disabled, so every transaction it already signed still reconciles.
    Unauthorized(String),
    /// The wallet holds the other role's key. No owner action makes this run correctly, so it is refused.
    Misconfigured(String),
}
pub(crate) async fn wallet_status(
    rpc: &Rpc,
    registry: Address,
    wallet: Address,
    role: Role,
) -> Result<WalletStatus> {
    let committer = rpc.call(registry, ER::committerCall {}).await?;
    Ok(match role {
        Role::Primary if committer == wallet => WalletStatus::Authorized,
        Role::Primary => {
            if rpc
                .call(registry, ER::isBackupCommitterCall { account: wallet })
                .await
                .unwrap_or(false)
            {
                WalletStatus::Misconfigured(
                    "The primary transaction wallet is an allowed backup committer, not the registry committer; run it with KEEPER_ROLE=follower".into(),
                )
            } else {
                WalletStatus::Unauthorized(
                    "Epoch committer does not match the transaction wallet".into(),
                )
            }
        }
        Role::Follower { .. } if committer == wallet => WalletStatus::Misconfigured(
            "The follower transaction wallet is the registry committer; run it with KEEPER_ROLE=primary".into(),
        ),
        Role::Follower { .. } => {
            if rpc
                .call(registry, ER::isBackupCommitterCall { account: wallet })
                .await?
            {
                WalletStatus::Authorized
            } else {
                WalletStatus::Unauthorized(
                    "The follower transaction wallet is not an allowed backup committer; the registry owner must call setBackupCommitter(wallet, true)".into(),
                )
            }
        }
    })
}
/// The same check as a plain result, for the status snapshot and for callers that only need the verdict.
pub(crate) async fn authorize_wallet(
    rpc: &Rpc,
    registry: Address,
    wallet: Address,
    role: Role,
) -> Result<()> {
    match wallet_status(rpc, registry, wallet, role).await? {
        WalletStatus::Authorized => Ok(()),
        WalletStatus::Unauthorized(reason) | WalletStatus::Misconfigured(reason) => {
            Err(anyhow::anyhow!(reason))
        }
    }
}
fn epoch_terminal(work: &crate::epoch::Work, head: &Head) -> Result<Option<&'static str>> {
    if let Some(json) = &work.api {
        let api: ApiProof = serde_json::from_str(json)?;
        if U256::from(head.timestamp) > api.timestamp.saturating_add(U256::from(240)) {
            return Ok(Some("blocked"));
        }
    }
    Ok(None)
}

/// Live paid demand that cannot be published: its epoch work is blocked with no source left to try
/// (or with a packet that cannot be sent), or its saved packet is already older than the attestation
/// freshness bound. Transient fetch retries and a blocked source awaiting its fallback are not stalls.
async fn stalled_epoch_demand(
    pool: &sqlx::SqlitePool,
    registry: alloy_primitives::Address,
    catalog: B256,
    head: &Head,
    margin: u64,
) -> Result<Option<(String, &'static str)>> {
    let keys: Vec<String> = sqlx::query_scalar("SELECT DISTINCT epoch_work.key FROM epoch_demand JOIN jobs ON jobs.id=epoch_demand.job JOIN epoch_work ON epoch_work.epoch=epoch_demand.epoch AND epoch_work.registry=? AND epoch_work.catalog=? WHERE jobs.state IN ('pending','prepared','signed','submitted') AND jobs.deadline>? AND ((epoch_work.state='blocked' AND (epoch_work.api IS NOT NULL OR epoch_work.fallback>=epoch_work.sources-1)) OR (epoch_work.api IS NOT NULL AND epoch_work.state IN ('prepared','signed','submitted'))) ORDER BY epoch_work.epoch LIMIT 16")
        .bind(registry.to_string()).bind(catalog.to_string()).bind(i64::try_from(head.timestamp.saturating_add(margin))?).fetch_all(pool).await?;
    for key in keys {
        let work = crate::epoch::work(pool, &key).await?;
        if work.state == "blocked" {
            return Ok(Some((key, "blocked")));
        }
        if epoch_terminal(&work, head)?.is_some() {
            return Ok(Some((key, "stale_packet")));
        }
    }
    Ok(None)
}

pub(crate) async fn validate_configuration_pin(rpc: &Rpc, cfg: &Config) -> Result<B256> {
    let hash = rpc
        .call(cfg.coordinator, C::protocolConfigurationHashCall {})
        .await?;
    ensure!(
        cfg.protocol_hash.is_none_or(|pin| pin == hash),
        "Coordinator configuration hash mismatch"
    );
    Ok(hash)
}

pub struct TelegramObserver(tokio::task::JoinHandle<()>);
impl Drop for TelegramObserver {
    fn drop(&mut self) {
        self.0.abort();
    }
}
fn spawn_telegram_observer(
    notifier: crate::telegram::TelegramNotifier,
    rpc: Rpc,
    cfg: Config,
    wallet: alloy_primitives::Address,
    pins: crate::proxy::RuntimePins,
    catalog: B256,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let observation = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let pool = sqlx::sqlite::SqlitePoolOptions::new()
                    .max_connections(1)
                    .acquire_timeout(std::time::Duration::from_millis(250))
                    .connect_with(
                        sqlx::sqlite::SqliteConnectOptions::new()
                            .filename(&cfg.db)
                            .read_only(true)
                            .busy_timeout(std::time::Duration::from_millis(100)),
                    )
                    .await?;
                let result = telegram_snapshot(&rpc, &pool, wallet, &cfg, pins, catalog).await;
                pool.close().await;
                result
            })
            .await;
            match observation {
                Ok(Ok((snapshot, base_fee))) => {
                    notifier.update_status(snapshot);
                    // Advisory only: the send path enforces the caps; this warns before demand arrives.
                    if let Some(exceeded) =
                        fee_headroom(base_fee, cfg.min_priority_fee, cfg.max_fee)
                    {
                        tracing::warn!(base_fee=%base_fee,required=%exceeded.required,limit=%exceeded.limit,"Observed base fee exceeds the fulfillment fee cap; sends will be deferred until MAX_FEE_PER_GAS_WEI is raised");
                        notifier.notify(crate::telegram::Event::FeeBudget(exceeded));
                    }
                }
                // Not an RPC fault: the worker exits for a verified restart and the new process reports afresh.
                Ok(Err(error)) if crate::proxy::approved_upgrade(&error).is_some() => {
                    notifier.invalidate_status();
                }
                _ => {
                    notifier.invalidate_status();
                    notifier.notify(crate::telegram::Event::OperationalError {
                        class: crate::telegram::ErrorClass::RpcUnavailable,
                    });
                }
            }
        }
    })
}
async fn telegram_snapshot(
    rpc: &Rpc,
    pool: &sqlx::SqlitePool,
    wallet: alloy_primitives::Address,
    cfg: &Config,
    pins: crate::proxy::RuntimePins,
    catalog: B256,
) -> Result<(crate::telegram::StatusSnapshot, u128)> {
    use crate::telegram::{EpochState, Health, StatusSnapshot};
    let (chain_id, role) = (cfg.chain_id, cfg.role);
    pins.verify(rpc, cfg.approved_next()).await?;
    let registry = pins.registry.proxy;
    let head = rpc.head().await?;
    let pending:i64=sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE state IN ('pending','prepared','signed','submitted') AND deadline>=?")
        .bind(i64::try_from(head.timestamp)?).fetch_one(pool).await?;
    let served: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE state='served'")
        .fetch_one(pool)
        .await?;
    let last: Option<i64> =
        sqlx::query_scalar("SELECT MAX(observed_at) FROM audit_events WHERE kind='served'")
            .fetch_one(pool)
            .await?;
    let health: Option<String> =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='health:status'")
            .fetch_optional(pool)
            .await?;
    let health = health
        .and_then(|v| serde_json::from_str::<crate::health::Status>(&v).ok())
        .map(|mut status| {
            crate::health::check_freshness(&mut status, crate::health::now().unwrap_or(0), 30);
            if status.healthy {
                Health::Healthy
            } else {
                Health::Degraded
            }
        })
        .unwrap_or(Health::Unknown);
    let epoch = rpc
        .call(
            registry,
            ER::nextEpochToPrepareCall {
                number: U256::from(head.number),
            },
        )
        .await?;
    let epoch_state = if epoch == 0 {
        EpochState::Missing
    } else {
        let record = rpc
            .call(registry, ER::getEpochCall { epochId: epoch })
            .await?;
        if record.epochHash != B256::ZERO {
            EpochState::Published
        } else {
            let saved: Option<i64> = sqlx::query_scalar(
                "SELECT api IS NOT NULL FROM epoch_work WHERE registry=? AND catalog=? AND epoch=?",
            )
            .bind(registry.to_string())
            .bind(catalog.to_string())
            .bind(i64::try_from(epoch)?)
            .fetch_optional(pool)
            .await?;
            if saved == Some(1) {
                EpochState::Local
            } else {
                EpochState::Missing
            }
        }
    };
    let balance = rpc
        .request("eth_getBalance", json!([wallet, "latest"]))
        .await
        .ok()
        .and_then(|v| {
            v.as_str()
                .and_then(|s| U256::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        });
    let authorized = Some(authorize_wallet(rpc, registry, wallet, role).await.is_ok());
    let primary_alive: Option<String> =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='keeper:primary_alive'")
            .fetch_optional(pool)
            .await?;
    Ok((
        StatusSnapshot {
            observed_at_unix: Some(crate::health::now()?),
            health,
            pending: u64::try_from(pending)?,
            served: u64::try_from(served)?,
            last_serve_unix: last.map(u64::try_from).transpose()?,
            epoch_id: (epoch != 0).then_some(epoch),
            epoch_state,
            transaction_wallet: wallet,
            chain_id,
            balance_wei: balance,
            authorized,
            primary_alive: primary_alive.map(|value| value == "true"),
        },
        head.base_fee,
    ))
}

impl Worker {
    pub async fn new(cfg: Config) -> Result<Self> {
        let probe_rpc = Rpc::new(cfg.rpc_urls.clone())?;
        let vrf_key = prover::read_key(&cfg.vrf_key_file)?;
        let tx_secret = prover::read_key(&cfg.tx_key_file)?;
        let tx_key = PrivateKeySigner::from_bytes(&B256::from_slice(&tx_secret.to_bytes()))?;
        let mut scope_locks = crate::lease::acquire(
            &cfg.lock_dir,
            cfg.chain_id,
            cfg.coordinator,
            tx_key.address(),
        )?;
        ensure!(
            tx_key.address() != prover::address(&vrf_key),
            "Use different VRF and transaction keys"
        );
        if cfg.chain_id != 31337 {
            let fixture = SecretKey::from_slice(&U256::from(123456789u64).to_be_bytes::<32>())?;
            ensure!(
                prover::public_key(&vrf_key) != prover::public_key(&fixture),
                "Public VRF test key is prohibited off local chain"
            );
        }
        let scope = format!("{}:{}:{}", cfg.chain_id, cfg.coordinator, tx_key.address());
        let journal = Journal::open(&cfg.db, &scope).await?;
        // On rejected startup, finish SQLite shutdown while ownership locks remain
        // held. Dropping the pool alone can leave background WAL handles alive.
        type Started = (
            Rpc,
            crate::epoch::Publisher,
            crate::proxy::RuntimePins,
            Option<String>,
        );
        let startup: Result<Started> = async {
            let instance = journal
                .meta("instance_id")
                .await?
                .ok_or_else(|| anyhow::anyhow!("Journal identity missing"))?;
            crate::lease::bind(&mut scope_locks, &cfg.db, &instance)?;
            // Caps reload only through a restart, so a fee budget observation describes the
            // previous configuration. The next deferral under the reloaded caps re-raises it.
            crate::health::recovered(&journal, "fee_budget").await?;
            let mut healthy = Vec::new();
            let mut limited = 0;
            for url in &cfg.rpc_urls {
                let chain = match probe_rpc.at(url, "eth_chainId", json!([])).await {
                    Ok(v) => v,
                    Err(error) => {
                        if crate::rpc::is_rate_limited(&error) {
                            limited += 1;
                        }
                        tracing::warn!("Skipping unreachable RPC during startup");
                        continue;
                    }
                };
                ensure!(quantity(&chain)? == cfg.chain_id, "RPC chain mismatch");
                let code = match probe_rpc
                    .at(url, "eth_getCode", json!([cfg.coordinator, "latest"]))
                    .await
                {
                    Ok(code) => code,
                    Err(error) if crate::rpc::is_delivery_failure(&error) => {
                        if crate::rpc::is_rate_limited(&error) {
                            limited += 1;
                        }
                        tracing::warn!("Skipping unreachable RPC code probe during startup");
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let code: Bytes = serde_json::from_value(code)?;
                ensure!(!code.is_empty(), "Coordinator has no code");
                if let Some(expected) = cfg.code_hash {
                    ensure!(
                        keccak256(&code) == expected,
                        "Coordinator code hash mismatch"
                    );
                }
                healthy.push(url.clone());
            }
            if healthy.is_empty() && limited == cfg.rpc_urls.len() {
                // Only rate limits stood in the way; the caller waits and starts again.
                return Err(crate::rpc::rate_limited_error(
                    "No healthy verified RPC endpoint: every endpoint is rate limiting",
                ));
            }
            ensure!(!healthy.is_empty(), "No healthy verified RPC endpoint");
            let rpc = Rpc::new(healthy)?;
            let approved = cfg.approved_next();
            let pins=crate::proxy::RuntimePins::observe(&rpc,&cfg).await?;
            // Startup must not mix disagreeing endpoint implementation identities. While an approved upgrade
            // propagates, one endpoint can still serve the pinned implementation and another the approved next one;
            // when this endpoint's own view is also one the pins accept, startup waits for them to agree.
            for url in &rpc.urls {
                let endpoint = Rpc::new(vec![url.clone()])?;
                if let Err(error) = pins.verify(&endpoint, approved).await {
                    if crate::proxy::approved_upgrade(&error).is_none()
                        && !crate::rpc::is_delivery_failure(&error)
                        && crate::proxy::RuntimePins::observe(&endpoint, &cfg).await.is_ok()
                    {
                        return Err(crate::proxy::EndpointsDisagree(error.to_string()).into());
                    }
                    return Err(error);
                }
            }
            let pk = prover::public_key(&vrf_key);
            ensure!(
                rpc.call(cfg.coordinator, C::publicKeyXCall {}).await? == pk[0]
                    && rpc.call(cfg.coordinator, C::publicKeyYCall {}).await? == pk[1],
                "VRF key does not match coordinator"
            );
            validate_configuration_pin(&rpc, &cfg).await?;
            let registry = rpc.call(cfg.coordinator, C::epochRegistryCall {}).await?;
            match wallet_status(&rpc, registry, tx_key.address(), cfg.role).await? {
                WalletStatus::Misconfigured(reason) => bail!(reason),
                WalletStatus::Unauthorized(reason) => {
                    tracing::error!(reason=%reason,role=cfg.role.name(),"Transaction wallet is not authorized to publish; starting with sending disabled, reconciliation continues");
                    crate::health::unauthorized(&journal, &reason).await?;
                }
                WalletStatus::Authorized => crate::health::authorized(&journal).await?,
            }
            sqlx::query("INSERT INTO meta(key,value) VALUES('keeper:role',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
                .bind(cfg.role.name()).execute(&journal.pool).await?;
            // A follower starts by assuming the primary alive until it has observed it; a primary has no such view.
            if cfg.role.is_follower() {
                sqlx::query("INSERT INTO meta(key,value) VALUES('keeper:primary_alive','true') ON CONFLICT(key) DO UPDATE SET value=excluded.value")
                    .execute(&journal.pool).await?;
            } else {
                sqlx::query("DELETE FROM meta WHERE key='keeper:primary_alive'")
                    .execute(&journal.pool)
                    .await?;
            }
            let epoch = crate::epoch::Publisher::new(
                registry,
                rpc.call(registry, ER::catalogHashCall {}).await?,
                cfg.api_endpoints.clone(),
            );
            pins.verify(&rpc, approved).await?;
            let finalized = rpc.finalized_head().await?;
            if let Some(exceeded) = fee_headroom(finalized.base_fee, cfg.min_priority_fee, cfg.max_fee) {
                tracing::warn!(base_fee=%finalized.base_fee,required=%exceeded.required,limit=%exceeded.limit,"Observed base fee exceeds the fulfillment fee cap; sends will be deferred until MAX_FEE_PER_GAS_WEI is raised");
            }
            journal.enable_public_service(finalized.timestamp).await?;
            // A proxy accepted through its approved next hash is running the upgraded implementation. The journal's
            // previous identity tells a first start on it, which is announced once, from every later restart.
            let previous: Option<crate::proxy::RuntimePins> = journal
                .meta("runtime:pins")
                .await?
                .and_then(|saved| serde_json::from_str(&saved).ok());
            let mut notices = Vec::new();
            for service in pins.on_approved_next(&cfg) {
                let pin = pins.get(service);
                tracing::warn!(service=service.name(),implementation=%pin.implementation,code_hash=%pin.implementation_code_hash,
                    "Running on the approved next {} implementation; set {} to its runtime code hash and remove {}",
                    service.name(),service.pin_setting(),service.approval_setting());
                if previous.is_some_and(|previous| previous.get(service).implementation != pin.implementation) {
                    notices.push(format!(
                        "Keeper restarted on the approved {} implementation {}\nRuntime code hash: {}\nSet {} to it and remove {}.",
                        service.name(),pin.implementation,pin.implementation_code_hash,service.pin_setting(),service.approval_setting()
                    ));
                }
            }
            sqlx::query("INSERT INTO meta(key,value) VALUES('runtime:pins',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
                .bind(serde_json::to_string(&pins)?).execute(&journal.pool).await?;
            Ok((rpc, epoch, pins, (!notices.is_empty()).then(|| notices.join("\n\n"))))
        }
        .await;
        let (rpc, epoch, runtime_pins, upgrade_notice) = match startup {
            Ok(rpc) => rpc,
            Err(error) => {
                journal.pool.close().await;
                return Err(error);
            }
        };
        // The startup check recorded the verdict in the journal; sending starts from the same fact.
        let authorized = journal.meta("health:unauthorized").await?.is_none();
        Ok(Self {
            _scope_locks: scope_locks,
            epoch,
            cfg,
            rpc,
            journal,
            vrf_key,
            tx_key,
            telegram: None,
            discord: None,
            proof_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(4)),
            runtime_pins,
            authorized: std::sync::atomic::AtomicBool::new(authorized),
            primary: std::sync::Mutex::new(crate::liveness::PrimaryLiveness::default()),
            policy: std::sync::Mutex::new(SendPolicy {
                joined: false,
                tail_first: true,
                rank: 0,
                lanes: 1,
            }),
            epoch_demand_since: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            signals: crate::events::Signals::new(),
            // Startup verified both, just now.
            runtime_verified: std::sync::Mutex::new((tokio::time::Instant::now(), 0)),
            approved_upgrade: std::sync::OnceLock::new(),
            upgrade_notice,
            authorization_checked: std::sync::Mutex::new((tokio::time::Instant::now(), 0)),
            last_finalized: std::sync::atomic::AtomicU64::new(0),
        })
    }
    /// The approved implementation upgrade this process has seen, if any. Once set, the run loop exits with
    /// `proxy::APPROVED_UPGRADE_EXIT` and nothing more is signed or sent.
    pub fn approved_upgrade(&self) -> Option<crate::proxy::ApprovedUpgrade> {
        self.approved_upgrade.get().copied()
    }
    /// Refuse any signature or broadcast after an approved upgrade was seen. Every send path verifies the runtime pins
    /// immediately before it anyway; this holds even where an earlier failure was deferred rather than returned.
    fn ensure_not_upgraded(&self) -> Result<()> {
        match self.approved_upgrade() {
            Some(upgrade) => Err(upgrade.into()),
            None => Ok(()),
        }
    }
    /// The event hints this worker listens to; the caller connects a subscription to them.
    pub fn signals(&self) -> std::sync::Arc<crate::events::Signals> {
        self.signals.clone()
    }
    pub fn registry(&self) -> Address {
        self.epoch.registry
    }
    /// Whether anything is open: a signed or submitted transaction, a live job, an operator sweep, or a pushed
    /// work event on a block no tick has read yet. It decides the loop's cadence only, never what a tick does.
    pub async fn open_work(&self) -> Result<bool> {
        if self.signals.activity()
            > self
                .last_finalized
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(true);
        }
        if !self.journal.unresolved().await?.is_empty()
            || crate::sweep::request(&self.journal.pool).await?.is_some()
            || crate::sweep::attempt(&self.journal.pool).await?.is_some()
        {
            return Ok(true);
        }
        let open: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM jobs WHERE state IN ('pending','prepared','signed','submitted') AND deadline>=?)")
            .bind(i64::try_from(crate::health::now()?)?)
            .fetch_one(&self.journal.pool)
            .await?;
        Ok(open != 0)
    }
    /// Once per tick, a follower reads committer()'s confirmed nonce at the finalized block, records it against the
    /// work it can see waiting, and decides whether it is joining the queue this tick. The result drives every send
    /// decision below and is published for health and Telegram.
    async fn observe_primary(&self, head: &Head) -> Result<()> {
        let Role::Follower(plan) = self.cfg.role else {
            return Ok(());
        };
        let committer = self
            .rpc
            .call(self.epoch.registry, ER::committerCall {})
            .await?;
        // The peer nonce is read at the same finalized block whose timestamp times the comparison, so a lagging
        // backend cannot produce a false death.
        let nonce = self
            .rpc
            .nonce(committer, &format!("0x{:x}", head.number))
            .await?;
        // Judge the chain's queue, not this node's lag: the primary serves the oldest first, so a follower's
        // stale rows are exactly the oldest ones, and they would both overstate the queue age and count as work
        // waiting on a primary that already served them. A few of them are re-read at the finalized head.
        if !self.policy()?.joined {
            self.refresh_oldest_pending(head, plan.liveness / 2).await?;
        }
        let (pending, oldest_deadline) = self.journal.pending_queue(head.timestamp).await?;
        let oldest_age = oldest_deadline.map_or(0, |deadline| {
            head.timestamp
                .saturating_sub(deadline.saturating_sub(RESPONSE_TIMEOUT_SECONDS))
        });
        let work_waiting = self.sendable_work_waiting(head).await?;
        let dead = {
            let mut primary = self
                .primary
                .lock()
                .map_err(|_| anyhow::anyhow!("Primary liveness lock poisoned"))?;
            primary.observe(committer, nonce, head.timestamp, work_waiting);
            primary.dead(head.timestamp, plan.liveness)
        };
        let previous = self.policy()?;
        // The join rule, with hysteresis so a follower that has joined does not drop out mid-burst.
        let joined = if previous.joined {
            !(pending < FOLLOWER_LEAVE_PENDING && oldest_age < FOLLOWER_LEAVE_AGE_SECONDS && !dead)
        } else {
            oldest_age > plan.delay || pending > plan.queue_join || dead
        };
        *self
            .policy
            .lock()
            .map_err(|_| anyhow::anyhow!("Send policy lock poisoned"))? = SendPolicy {
            joined,
            tail_first: true,
            rank: plan.rank,
            lanes: plan.lanes,
        };
        tracing::debug!(
            pending,
            oldest_age,
            primary_dead = dead,
            joined,
            "Follower join rule"
        );
        if joined != previous.joined {
            tracing::warn!(
                pending,
                oldest_age,
                primary_dead = dead,
                lane = plan.rank,
                lanes = plan.lanes,
                "Follower {} the queue",
                if joined { "joined" } else { "left" }
            );
        }
        let value = if dead { "false" } else { "true" };
        if self.journal.meta("keeper:primary_alive").await?.as_deref() != Some(value) {
            sqlx::query("INSERT INTO meta(key,value) VALUES('keeper:primary_alive',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
                .bind(value).execute(&self.journal.pool).await?;
            // Operationally important and rate-limited to transitions: a follower deciding that the primary is
            // gone, or that it is back, belongs in the log an operator reads by default.
            tracing::warn!(committer=%committer,nonce,primary_dead=dead,pending,"Primary committer liveness changed");
        }
        Ok(())
    }
    /// Whether an epoch attempt with live paid demand is waiting to be published: work the primary should take.
    /// Work the fleet could act on right now, which is what a silent committer has to be judged against. An open
    /// request whose epoch has no packet and whose next fallback window has not opened yet is not such work: no
    /// keeper can publish for it, so the primary standing still says nothing about its health. A source ladder that
    /// has run out keeps counting, because a follower's own gateways may still reach the last source.
    async fn sendable_work_waiting(&self, head: &Head) -> Result<bool> {
        let waiting:i64=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM jobs LEFT JOIN epoch_demand ON epoch_demand.job=jobs.id LEFT JOIN epoch_work ON epoch_work.epoch=epoch_demand.epoch AND epoch_work.registry=? AND epoch_work.catalog=? WHERE jobs.state IN ('pending','prepared') AND jobs.deadline>? AND NOT(epoch_work.api IS NULL AND epoch_work.last_error IS NOT NULL AND epoch_work.state NOT IN ('committed','expired') AND epoch_work.fallback+1<epoch_work.sources AND ?<epoch_work.start+(epoch_work.fallback+1)*?))")
            .bind(self.epoch.registry.to_string()).bind(self.epoch.catalog.to_string())
            .bind(i64::try_from(head.timestamp.saturating_add(self.cfg.margin))?)
            .bind(i64::try_from(head.number)?)
            .bind(i64::try_from(crate::epoch::FALLBACK_DELAY_BLOCKS)?)
            .fetch_one(&self.journal.pool).await?;
        Ok(waiting != 0)
    }
    /// Re-read a bounded number of the oldest open jobs at the finalized head and retire the ones the chain has
    /// already settled. Only jobs older than `min_age` are read, so a young queue costs no calls at all.
    async fn refresh_oldest_pending(&self, head: &Head, min_age: u64) -> Result<()> {
        for job in self
            .journal
            .pending_oldest(head.timestamp, min_age, LIVENESS_REFRESH_JOBS)
            .await?
        {
            let request = self.request_at(job.id.parse()?, head.number).await?;
            let Some(state) = terminal(&request, head.timestamp) else {
                continue;
            };
            if request.fulfilled && !request.delivered {
                crate::audit::callback_failed(&self.journal.pool, &job.id).await?;
            }
            self.journal.state(&job.id, state).await?;
            tracing::debug!(request_id=%job.id,state,"Queue entry settled on chain before this node saw it");
        }
        Ok(())
    }
    /// This tick's send policy: what a primary may always do, or what the follower's join rule decided.
    fn policy(&self) -> Result<SendPolicy> {
        match self.cfg.role {
            Role::Primary => Ok(SendPolicy::PRIMARY),
            Role::Follower(_) => Ok(*self
                .policy
                .lock()
                .map_err(|_| anyhow::anyhow!("Send policy lock poisoned"))?),
        }
    }
    /// The observer has only public addresses, verified RPCs and a read-only journal connection.
    /// No Telegram configuration means no observer and no extra RPC traffic.
    pub fn attach_telegram(
        &mut self,
        notifier: crate::telegram::TelegramNotifier,
    ) -> TelegramObserver {
        let task = spawn_telegram_observer(
            notifier.clone(),
            self.rpc.clone(),
            self.cfg.clone(),
            self.tx_key.address(),
            self.runtime_pins,
            self.epoch.catalog,
        );
        if let Some(notice) = &self.upgrade_notice {
            notifier.notify(crate::telegram::Event::Upgrade(notice.clone()));
        }
        self.telegram = Some(notifier);
        TelegramObserver(task)
    }
    pub fn spawn_explorer(&self) -> Option<crate::explorer::Task> {
        match crate::explorer::Settings::from_env() {
            Ok(settings) => crate::explorer::spawn(
                settings,
                self.rpc.clone(),
                self.runtime_pins,
                self.cfg.approved_next(),
                self.cfg.chain_id,
                crate::explorer::StatusSource {
                    db: self.cfg.db.clone(),
                    keeper: self.tx_key.address(),
                    role: self.cfg.role,
                },
            ),
            Err(_) => {
                tracing::warn!("Optional public explorer disabled: invalid configuration");
                None
            }
        }
    }
    pub fn attach_discord(&mut self, notifier: crate::discord::Notifier) {
        self.discord = Some(notifier);
    }
    pub fn notify_error(&self, class: crate::telegram::ErrorClass) {
        // An approved upgrade is not an operational error: whatever failed with it is the process stopping for its
        // restart, which announces itself once it runs on the new implementation.
        if self.approved_upgrade().is_some() {
            return;
        }
        if let Some(notifier) = &self.telegram {
            notifier.notify(crate::telegram::Event::OperationalError { class });
        }
    }
    /// A configured cap, not chain state, prevents allowed work. Name the cap to raise,
    /// alert the operator and keep the durable health observation until an affordable send.
    async fn defer_for_budget(&self, work: &str, kind: &str, exceeded: &FeeBudget) -> Result<()> {
        tracing::error!(work_id=work,kind,cap=exceeded.cap.variable(),required=%exceeded.required,limit=%exceeded.limit,
            "Send deferred by fee budget; raise the named cap and restart. No cap is bypassed to meet a deadline");
        if let Some(notifier) = &self.telegram {
            notifier.notify(crate::telegram::Event::FeeBudget(*exceeded));
        }
        if self.cfg.send {
            crate::health::blocked(&self.journal, "fee_budget", crate::health::now()?).await?;
        }
        Ok(())
    }
    fn over_budget(&self, plan: &TxPlan) -> Option<FeeBudget> {
        over_budget(
            plan,
            self.cfg.max_fee,
            self.cfg.cancel_max_fee,
            self.cfg.max_cost,
        )
    }
    /// Bounded recent-median tip; see bounded_priority for the fallback.
    async fn priority_fee(&self) -> u128 {
        let observed = self.rpc.recent_priority_fee(PRIORITY_FEE_BLOCKS).await.ok();
        bounded_priority(
            observed,
            self.cfg.min_priority_fee,
            self.cfg.max_priority_fee,
        )
    }
    fn gas_over_budget(&self, gas: u64) -> Option<FeeBudget> {
        (gas > self.cfg.max_gas).then_some(FeeBudget {
            cap: FeeCap::MaxGas,
            required: u128::from(gas),
            limit: u128::from(self.cfg.max_gas),
        })
    }
    /// Both proxies' implementation slots and all four runtime codes, in one batched read, immediately before a
    /// signature or broadcast. Never cached: an upgrade between an earlier check and a send must not slip through.
    /// An RPC failure is reported as one, so a rate-limited endpoint is not mistaken for a changed implementation.
    /// A move to an approved next implementation is recorded and returned as `ApprovedUpgrade`: the tick stops, the
    /// process exits, and its restart verifies the new implementation. Any other change fails as it always has.
    async fn verify_runtime(&self) -> Result<()> {
        self.ensure_not_upgraded()?;
        let generation = self.signals.upgrades();
        let rpc = self.rpc.for_runtime_checks();
        // Keep repeated nested runtime/read state machines off the small Windows debug stack.
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            Box::pin(async {
                self.runtime_pins
                    .verify(&rpc, self.cfg.approved_next())
                    .await
                    .map_err(|error| {
                        if let Some(upgrade) = crate::proxy::approved_upgrade(&error) {
                            if self.approved_upgrade.set(upgrade).is_ok() {
                                tracing::warn!(service=upgrade.service.name(),proxy=%upgrade.proxy,from=%upgrade.from,to=%upgrade.to,code_hash=%upgrade.code_hash,
                                    "Proxy moved to its approved next implementation; nothing more is signed or sent by this process, which exits for a restart that verifies the new implementation");
                            }
                            error
                        } else if crate::rpc::is_delivery_failure(&error) {
                            error.context("Proxy runtime could not be verified")
                        } else {
                            anyhow::anyhow!("Proxy runtime changed: {error}")
                        }
                    })
            }),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Runtime implementation verification timed out"))??;
        *self
            .runtime_verified
            .lock()
            .map_err(|_| anyhow::anyhow!("Runtime verification lock poisoned"))? =
            (tokio::time::Instant::now(), generation);
        Ok(())
    }
    /// At the start of a tick the pins are verified only when an upgrade event (or a resubscription, which may
    /// have hidden one) arrived since the last verification, or when PIN_BACKSTOP has passed without one.
    async fn verify_runtime_if_due(&self) -> Result<()> {
        let (at, seen) = *self
            .runtime_verified
            .lock()
            .map_err(|_| anyhow::anyhow!("Runtime verification lock poisoned"))?;
        if seen != self.signals.upgrades() || at.elapsed() >= PIN_BACKSTOP {
            self.verify_runtime().await?;
        }
        Ok(())
    }
    /// The publishing right is re-read when a role event (or a resubscription) arrived since the last check, or
    /// when that check is older than AUTHORIZATION_BUSY with work open or AUTHORIZATION_IDLE without.
    async fn check_authorization_if_due(&self, busy: bool) -> Result<()> {
        let (at, seen) = *self
            .authorization_checked
            .lock()
            .map_err(|_| anyhow::anyhow!("Authorization lock poisoned"))?;
        let limit = if busy {
            AUTHORIZATION_BUSY
        } else {
            AUTHORIZATION_IDLE
        };
        if seen != self.signals.roles() || at.elapsed() >= limit {
            self.check_authorization().await?;
        }
        Ok(())
    }
    /// Whether this keeper may start new work: configured to send and still authorized to publish in its role.
    fn may_send(&self) -> bool {
        self.cfg.send && self.authorized.load(std::sync::atomic::Ordering::Relaxed)
    }
    /// Once per tick, check the wallet's right to publish. Losing it disables sending and raises a health fault;
    /// it never stops the process, because transactions this keeper already signed must still reconcile. A wallet
    /// holding the other role's key is a plain misconfiguration and does stop the tick.
    async fn check_authorization(&self) -> Result<()> {
        let generation = self.signals.roles();
        let status = wallet_status(
            &self.rpc.for_runtime_checks(),
            self.epoch.registry,
            self.tx_key.address(),
            self.cfg.role,
        )
        .await?;
        *self
            .authorization_checked
            .lock()
            .map_err(|_| anyhow::anyhow!("Authorization lock poisoned"))? =
            (tokio::time::Instant::now(), generation);
        let authorized = matches!(status, WalletStatus::Authorized);
        let changed = self
            .authorized
            .swap(authorized, std::sync::atomic::Ordering::Relaxed)
            != authorized;
        match status {
            WalletStatus::Misconfigured(reason) => bail!(reason),
            WalletStatus::Unauthorized(reason) => {
                crate::health::unauthorized(&self.journal, &reason).await?;
                if changed {
                    tracing::error!(reason=%reason,role=self.cfg.role.name(),"Transaction wallet lost its right to publish; sending disabled, reconciliation continues");
                    self.follower_notice(crate::telegram::Event::Authorization(format!(
                        "Keeper sending disabled: {reason}"
                    )));
                }
            }
            WalletStatus::Authorized => {
                crate::health::authorized(&self.journal).await?;
                if changed {
                    tracing::warn!(
                        role = self.cfg.role.name(),
                        "Transaction wallet is authorized to publish again; sending resumes"
                    );
                    self.follower_notice(crate::telegram::Event::Authorization(
                        "Keeper sending enabled: the transaction wallet may publish again".into(),
                    ));
                }
            }
        }
        Ok(())
    }
    /// Whether a request is already fulfilled or refunded at the latest block, ahead of finalized state.
    async fn settled_at_latest(&self, id: &str) -> Result<bool> {
        let head = self.rpc.head().await?;
        let request = self.request_at(id.parse()?, head.number).await?;
        Ok(request.fulfilled || request.refunded)
    }
    /// Whether an epoch is already published at the latest block, ahead of finalized state.
    async fn epoch_published_at_latest(&self, epoch: u64) -> Result<bool> {
        let head = self.rpc.head().await?;
        let record = self
            .rpc
            .call_at(
                self.epoch.registry,
                ER::getEpochCall { epochId: epoch },
                head.number,
            )
            .await?;
        Ok(record.epochHash != B256::ZERO)
    }
    fn follower_notice(&self, event: crate::telegram::Event) {
        if let Some(notifier) = &self.telegram {
            notifier.notify(event);
        }
    }
    pub async fn request(&self, id: U256) -> Result<Request> {
        self.rpc
            .call(self.cfg.coordinator, C::getRequestCall { id })
            .await
    }
    async fn request_at(&self, id: U256, number: u64) -> Result<Request> {
        self.rpc
            .call_at(self.cfg.coordinator, C::getRequestCall { id }, number)
            .await
    }
    /// Many requests' state at one block tag, in JSON-RPC batches of REQUEST_BATCH: each batch comes from one
    /// endpoint and costs one HTTP request. Results are in `ids` order.
    async fn requests_in(&self, ids: &[U256], tag: &str) -> Result<Vec<Request>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(REQUEST_BATCH) {
            let calls: Vec<(&str, serde_json::Value)> = chunk
                .iter()
                .map(|id| {
                    (
                        "eth_call",
                        json!([{"to":self.cfg.coordinator,"data":Bytes::from(C::getRequestCall { id: *id }.abi_encode())},tag]),
                    )
                })
                .collect();
            for value in self.rpc.batch(&calls).await? {
                let bytes: Bytes = serde_json::from_value(value)?;
                out.push(C::getRequestCall::abi_decode_returns(&bytes)?);
            }
        }
        Ok(out)
    }
    /// The finalized head, and in the same batched read from the same endpoint the block this journal saved as
    /// its finalized checkpoint, which must still be on the chain the endpoint serves.
    async fn finalized_head_checked(&self) -> Result<Head> {
        let saved: Option<(u64, String)> = self
            .journal
            .meta("finalized_checkpoint")
            .await?
            .map(|saved| serde_json::from_str(&saved))
            .transpose()?;
        let mut calls = vec![("eth_getBlockByNumber", json!(["finalized", false]))];
        if let Some((number, _)) = &saved {
            calls.push((
                "eth_getBlockByNumber",
                json!([format!("0x{number:x}"), false]),
            ));
        }
        let blocks = self.rpc.batch(&calls).await?;
        let head = Head::from_block(&blocks[0])?;
        if let Some((number, hash)) = saved {
            let block = Head::from_block(&blocks[1])?;
            ensure!(block.number == number, "Unexpected block number");
            ensure!(
                block.hash.to_string() == hash,
                "Finalized chain checkpoint changed; preserve journal and investigate RPC/finality before resuming"
            );
        }
        Ok(head)
    }
    pub async fn tick(&self) -> Result<()> {
        // Preparation never extends past this point; the rest of the tick stays for settlement.
        let tick_deadline = tokio::time::Instant::now()
            + std::time::Duration::from_millis(self.cfg.tick_timeout_seconds * 800);
        self.verify_runtime_if_due().await?;
        self.check_authorization_if_due(self.open_work().await?)
            .await?;
        let head = self.finalized_head_checked().await?;
        self.last_finalized
            .fetch_max(head.number, std::sync::atomic::Ordering::SeqCst);
        self.observe_primary(&head).await?;
        self.journal
            .finalized_checkpoint(head.number, &head.hash.to_string())
            .await?;
        // Settlement must run before any potentially large discovery backlog.
        let lane_busy = self.reconcile(&head).await.inspect_err(|_| {
            self.notify_error(crate::telegram::ErrorClass::ReceiptRecovery);
        })?;
        // An operator sweep shares the nonce lane, only while no game or epoch attempt is live.
        let lane_busy = lane_busy
            || self.sweep(&head).await.inspect_err(|_| {
                self.notify_error(crate::telegram::ErrorClass::TransactionSubmission);
            })?;
        self.journal.compact_history(crate::health::now()?).await?;
        self.journal
            .expire_unstarted(head.timestamp.try_into()?)
            .await?;
        // Discover funded demand before deciding whether any epoch should be published.
        let budget = std::time::Duration::from_millis(self.cfg.tick_timeout_seconds * 250);
        match tokio::time::timeout(budget, self.discover(&head)).await {
            Ok(result) => result?,
            Err(_) => tracing::debug!("Discovery yielded at its tick budget"),
        }
        let demand: Option<i64> = sqlx::query_scalar("SELECT epoch_demand.epoch FROM epoch_demand JOIN jobs ON jobs.id=epoch_demand.job LEFT JOIN epoch_work ON epoch_work.epoch=epoch_demand.epoch AND epoch_work.registry=? AND epoch_work.catalog=? WHERE jobs.state IN ('pending','prepared','signed','submitted') AND jobs.deadline>? AND (epoch_work.state IS NULL OR epoch_work.state IN ('pending','prepared','signed','submitted') OR (epoch_work.state='blocked' AND epoch_work.api IS NULL AND epoch_work.fallback<epoch_work.sources-1)) ORDER BY jobs.deadline LIMIT 1")
            .bind(self.epoch.registry.to_string()).bind(self.epoch.catalog.to_string()).bind(i64::try_from(head.timestamp.saturating_add(self.cfg.margin))?).fetch_optional(&self.journal.pool).await?;
        let ready = {
            let epoch = &self.epoch;
            match tokio::time::timeout(
                std::time::Duration::from_secs(2),
                epoch.poll(
                    &self.rpc,
                    &self.journal.pool,
                    &head,
                    self.cfg.api_override.clone(),
                    demand.map(u64::try_from).transpose()?,
                ),
            )
            .await
            {
                Ok(Ok(ready)) => ready,
                Ok(Err(error)) => {
                    self.notify_error(crate::telegram::ErrorClass::EpochPreparation);
                    tracing::warn!(error=%error,"Epoch preparation deferred");
                    None
                }
                Err(_) => {
                    tracing::debug!("Epoch preparation yielded");
                    None
                }
            }
        };
        let mut sent = false;
        if self.may_send()
            && !lane_busy
            && let Some(work) = ready
        {
            let budget = std::time::Duration::from_millis(self.cfg.tick_timeout_seconds * 250)
                .min(std::time::Duration::from_secs(5));
            match tokio::time::timeout(budget, self.send_epoch(&work)).await {
                Ok(Ok(())) => {}
                // Not a failed publication: the process stops for its restart and the work keeps its schedule.
                Ok(Err(error)) if crate::proxy::approved_upgrade(&error).is_some() => {
                    return Err(error);
                }
                outcome => {
                    // Back off from completion, not entry: a hung estimate must not
                    // consume the next tick's maintenance slice again immediately.
                    sqlx::query("UPDATE epoch_work SET retry_at=? WHERE key=?")
                        .bind(i64::try_from(crate::health::now()?.saturating_add(3))?)
                        .bind(&work.key)
                        .execute(&self.journal.pool)
                        .await?;
                    match outcome {
                        Ok(Err(error)) => {
                            match error
                                .downcast_ref::<SendDeferred>()
                                .and_then(|deferred| deferred.budget)
                            {
                                Some(exceeded) => {
                                    self.defer_for_budget(&work.key, "epoch", &exceeded).await?
                                }
                                None => {
                                    self.notify_error(crate::telegram::ErrorClass::EpochPreparation)
                                }
                            }
                            tracing::warn!(epoch_key=%work.key,error=%error,"Epoch publication deferred")
                        }
                        Err(_) => {
                            self.notify_error(crate::telegram::ErrorClass::EpochPreparation);
                            tracing::warn!(epoch_key=%work.key,"Epoch publication yielded at its maintenance budget")
                        }
                        _ => unreachable!(),
                    }
                }
            }
            // The single connection orders this barrier after any commit queued
            // before cancellation. A signed maintenance nonce blocks game signing.
            sent = !self.journal.unresolved().await?.is_empty();
        }
        if self.cfg.send {
            self.observe_epoch_demand(&head).await?;
        }
        let mut send_budget = std::time::Duration::from_millis(self.cfg.tick_timeout_seconds * 250);
        if self.may_send() && !lane_busy && !sent {
            sent = self.service_prepared(&mut send_budget).await?;
        }
        let can_send = self.may_send() && !lane_busy;
        let (attempted, stalled) = self
            .prepare_pending(&[], can_send && !sent, tick_deadline)
            .await?;
        if can_send && !sent {
            sent = self.service_prepared(&mut send_budget).await?;
        }
        // A pass that reached its cap without a proof would stall the next batch the same way.
        if !stalled {
            self.prepare_pending(&attempted, can_send && !sent, tick_deadline)
                .await?;
            if can_send && !sent {
                self.service_prepared(&mut send_budget).await?;
            }
        }
        self.check_lane_health().await?;
        Ok(())
    }
    async fn service_prepared(&self, budget: &mut std::time::Duration) -> Result<bool> {
        let began = tokio::time::Instant::now();
        let result = self.service_prepared_until(began + *budget).await;
        *budget = budget.saturating_sub(began.elapsed());
        result
    }
    async fn service_prepared_until(&self, until: tokio::time::Instant) -> Result<bool> {
        if !self.journal.unresolved().await?.is_empty() {
            return Ok(true);
        }
        let policy = self.policy()?;
        // A follower walks the newest end of the queue backwards; the primary works the oldest end forwards.
        let due = if policy.tail_first {
            self.journal
                .prepared_due_tail(crate::health::now()?)
                .await?
        } else {
            self.journal.prepared_due(crate::health::now()?).await?
        };
        let mut jobs = prepared_in_order(due, policy.tail_first)?;
        if let Some(next) = self.journal.meta("preflight_next").await?
            && let Some(index) = jobs.iter().position(|job| job.id == next)
        {
            jobs.rotate_left(index);
        }
        let excluded = self.journal.batch_excluded_prepared().await?;
        let (jobs, resend) = resend_first(jobs, &excluded, &policy, crate::health::now()?);
        // Requests that must go one at a time are sent before the next batch. Each attempt backs
        // its request off first, so one the node keeps rejecting cannot hold batching back.
        if resend > 0 {
            if self.send_singles(&jobs[..resend], &policy, until).await? {
                return Ok(true);
            }
            if tokio::time::Instant::now() >= until {
                return Ok(false);
            }
        }
        if self.cfg.fulfill_batch_max > 1 && jobs.len() > 1 {
            let candidates: Vec<Job> = jobs
                .iter()
                .filter(|job| !excluded.contains(&job.id))
                .take(self.cfg.fulfill_batch_max)
                .cloned()
                .collect();
            if candidates.len() > 1 {
                // The single path's durable bookkeeping, for every candidate at once: backoff
                // before preflight and the neighbor to start from if this preflight hangs.
                let ids: Vec<String> = candidates.iter().map(|job| job.id.clone()).collect();
                let work = format!("batch:{}", ids.join(","));
                self.journal
                    .preflight_backoff_many(&ids, crate::health::now()?.saturating_add(2))
                    .await?;
                self.journal
                    .preflight_next(candidates.get(1).map(|job| job.id.as_str()))
                    .await?;
                match tokio::time::timeout_at(until, self.send_prepared_batch(&candidates)).await {
                    Ok(Ok(BatchOutcome::Sent)) => {
                        self.journal.preflight_next(None).await?;
                    }
                    Ok(Ok(BatchOutcome::Single)) => {}
                    Ok(Err(error)) => {
                        let Some(deferred) = error.downcast_ref::<SendDeferred>() else {
                            return Err(error);
                        };
                        if let Some(exceeded) = &deferred.budget {
                            self.defer_for_budget(&work, "fulfill_batch", exceeded)
                                .await?;
                        } else if self.cfg.send {
                            crate::health::blocked(
                                &self.journal,
                                "settlement",
                                crate::health::now()?,
                            )
                            .await?;
                        }
                        tracing::warn!(work_id=%work,error=%error,"Batch settlement deferred");
                        // A stale nonce, an unreachable node or an unaffordable price defers
                        // every single send of this tick just the same.
                        return Ok(!self.journal.unresolved().await?.is_empty());
                    }
                    Err(_) => {
                        // Only a timeout over work this keeper is responsible for is evidence: a follower reading
                        // the primary's requests before leaving them alone has nothing to settle.
                        let now = crate::health::now()?;
                        if self.cfg.send
                            && owns_any(&policy, &candidates, now)
                            && self.journal.unresolved().await?.is_empty()
                        {
                            crate::health::blocked(&self.journal, "settlement", now).await?;
                        }
                        tracing::debug!(work_id=%work,"Batch settlement yielded at its tick budget");
                        return Ok(!self.journal.unresolved().await?.is_empty());
                    }
                }
                if !self.journal.unresolved().await?.is_empty() {
                    return Ok(true);
                }
            }
        }
        // Whatever led the order this pass has had its single attempt already.
        self.send_singles(&jobs[resend..], &policy, until).await
    }
    /// The single path: one request per transaction in `jobs` order, at most four preflights,
    /// stopping at the first send that leaves a signed nonce. Returns whether the lane is busy.
    async fn send_singles(
        &self,
        jobs: &[Job],
        policy: &SendPolicy,
        until: tokio::time::Instant,
    ) -> Result<bool> {
        for (index, job) in jobs.iter().take(4).enumerate() {
            if tokio::time::Instant::now() >= until {
                break;
            }
            // Durable backoff is recorded BEFORE preflight: timeout/restart cannot pin the queue.
            self.journal
                .preflight_backoff(&job.id, crate::health::now()?.saturating_add(2))
                .await?;
            // Persist the neighbor before preflight so even a restart after the
            // backoff expires cannot give a slow head every tick's whole budget.
            // Each candidate keeps the full remaining budget for multi-RPC work.
            let next = jobs.get(index + 1).or_else(|| jobs.first());
            self.journal
                .preflight_next(next.map(|job| job.id.as_str()))
                .await?;
            match tokio::time::timeout_at(until, self.send_prepared(&job.id)).await {
                Ok(Ok(())) => {
                    self.journal.preflight_next(None).await?;
                }
                Ok(Err(error)) => {
                    self.journal
                        .preflight_backoff(&job.id, crate::health::now()?.saturating_add(2))
                        .await?;
                    let Some(deferred) = error.downcast_ref::<SendDeferred>() else {
                        return Err(error);
                    };
                    // A budget deferral is reported by its own observation only: the send is
                    // forbidden by configuration, not allowed work without progress.
                    if let Some(exceeded) = &deferred.budget {
                        self.defer_for_budget(&job.id, "fulfill", exceeded).await?;
                    } else if self.cfg.send {
                        crate::health::blocked(&self.journal, "settlement", crate::health::now()?)
                            .await?;
                    }
                    tracing::warn!(request_id=%job.id,error=%error,"Settlement deferred");
                }
                Err(_) => {
                    self.journal
                        .preflight_backoff(&job.id, crate::health::now()?.saturating_add(2))
                        .await?;
                    // Serialize after any journal write queued before cancellation.
                    // A signed nonce belongs to reconciliation; otherwise repeated
                    // preflight timeouts are a durable lack of settlement progress.
                    let now = crate::health::now()?;
                    if self.cfg.send
                        && owns_any(policy, std::slice::from_ref(job), now)
                        && self.journal.unresolved().await?.is_empty()
                    {
                        crate::health::blocked(&self.journal, "settlement", now).await?;
                    }
                    tracing::debug!(request_id=%job.id,"Settlement yielded at its tick budget")
                }
            }
            // Cancellation or a failed broadcast can still leave a durable nonce.
            // This read uses the journal's same single-connection pool, ordering it
            // after any already-enqueued commit from the interrupted operation.
            if !self.journal.unresolved().await?.is_empty() {
                return Ok(true);
            }
        }
        Ok(false)
    }
    /// Returns the claimed IDs and whether the pass reached its cap without journaling a proof.
    ///
    /// A pass yields at its time slice only when that helps the sender: this pass journaled
    /// a proof and the lane may send. Cancelling otherwise gains nothing and, under uniformly
    /// slow RPC, would discard every pass before any proof is saved; such a pass continues
    /// to a hard cap that also stays inside the tick deadline.
    async fn prepare_pending(
        &self,
        excluded: &[String],
        sender_waiting: bool,
        tick_deadline: tokio::time::Instant,
    ) -> Result<(Vec<String>, bool)> {
        let started = tokio::time::Instant::now();
        if started >= tick_deadline {
            return Ok((Vec::new(), true));
        }
        let slice = std::time::Duration::from_millis(self.cfg.tick_timeout_seconds * 150);
        let cap = tick_deadline
            .min(started + std::time::Duration::from_millis(self.cfg.tick_timeout_seconds * 350));
        let journaled = std::sync::atomic::AtomicUsize::new(0);
        let mut attempted = Vec::new();
        let mut batch = Box::pin(self.prepare_pending_batch(excluded, &mut attempted, &journaled));
        let mut until = cap.min(started + slice);
        let stalled = loop {
            // While the batch is paused it may hold the single journal connection, so only
            // in-memory checks run here before it is polled again or dropped.
            match tokio::time::timeout_at(until, &mut batch).await {
                Ok(result) => {
                    result?;
                    break false;
                }
                Err(_) => {
                    let progress = journaled.load(std::sync::atomic::Ordering::Relaxed) > 0;
                    let now = tokio::time::Instant::now();
                    if now >= cap || (sender_waiting && progress) {
                        tracing::debug!(
                            progress,
                            "Preparation yielded; journaled proofs and claim order retained"
                        );
                        break !progress;
                    }
                    until = cap.min(now + std::time::Duration::from_millis(100));
                }
            }
        };
        drop(batch);
        Ok((attempted, stalled))
    }
    async fn prepare_pending_batch(
        &self,
        excluded: &[String],
        attempted: &mut Vec<String>,
        journaled: &std::sync::atomic::AtomicUsize,
    ) -> Result<()> {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM jobs WHERE state='pending' AND call IS NULL)",
        )
        .fetch_one(&self.journal.pool)
        .await?;
        if waiting == 0 {
            return Ok(());
        }
        let head = self.rpc.finalized_head().await?;
        let now = head.timestamp;
        let wall_millis: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis()
            .try_into()?;
        let jobs = self
            .journal
            .claim_preparation(
                wall_millis,
                now.saturating_add(self.cfg.margin),
                excluded,
                self.policy()?.tail_first,
            )
            .await?;
        *attempted = jobs.iter().map(|job| job.id.clone()).collect();
        let policy = self.policy()?;
        let outcomes = stream::iter(
            jobs.into_iter()
                .filter(|job| {
                    !excluded.contains(&job.id) && preparation_candidate(job, now, self.cfg.margin)
                })
                .take(8),
        )
        .map(|job| async move {
            let id: U256 = job.id.parse()?;
            let request = self.request_at(id,head.number).await?;

            if let Some(state) = terminal(&request, now) {
                if request.fulfilled && !request.delivered {
                    crate::audit::callback_failed(&self.journal.pool, &job.id).await?;
                }
                self.journal.state(&job.id, state).await?;
                return Ok::<_, anyhow::Error>(());
            }
            if now + self.cfg.margin >= request.deadline {
                return Ok(());
            }
            sqlx::query("INSERT INTO epoch_demand(job,epoch) VALUES(?,?) ON CONFLICT(job) DO UPDATE SET epoch=excluded.epoch")
                .bind(&job.id)
                .bind(i64::try_from(request.epochId)?)
                .execute(&self.journal.pool)
                .await?;
            // Observe pending work even when a stage waits without returning an error.
            note_preparation_attempt(
                &self.journal,
                self.cfg.send,
                &policy,
                &job.id,
                request.deadline,
                now,
                crate::health::now()?,
            )
            .await?;
            match self.prepare(&job, request).await {
                Ok(true) => {
                    journaled.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(false) => {}
                Err(error) => {
                    if error.downcast_ref::<sqlx::Error>().is_some() {
                        return Err(error);
                    }
                    tracing::warn!(request_id=%job.id,error=%error,"Job preparation deferred");
                }
            }
            Ok(())
        })
        .buffer_unordered(4);
        tokio::pin!(outcomes);
        while let Some(outcome) = outcomes.next().await {
            outcome?;
        }
        Ok(())
    }
    async fn discover(&self, head: &Head) -> Result<()> {
        self.journal
            .finalized_checkpoint(head.number, &head.hash.to_string())
            .await?;
        let end: u64 = self
            .rpc
            .call_at(self.cfg.coordinator, C::nextRequestIdCall {}, head.number)
            .await?
            .try_into()?;
        let mut cursor: u64 = self
            .journal
            .meta("cursor")
            .await?
            .unwrap_or_else(|| "1".into())
            .parse()?;
        if end.saturating_sub(cursor) > 256
            && self
                .request_at(U256::from(cursor), head.number)
                .await?
                .deadline
                < head.timestamp
        {
            cursor += 1;
            self.journal.cursor(&cursor.to_string()).await?;
            let lower = live_lower_bound_from(cursor, end, head.timestamp, |id| async move {
                let deadline = self.request_at(U256::from(id), head.number).await?.deadline;
                // Every expired midpoint proves the whole earlier prefix expired.
                // Save that safe progress even if the search budget ends next RPC.
                if deadline < head.timestamp {
                    self.journal.cursor(&(id + 1).to_string()).await?;
                }
                Ok(deadline)
            })
            .await?;
            cursor = cursor.max(lower);
        }
        if cursor >= end {
            return Ok(());
        }
        let call = C::getPendingRequestIdsCall {
            fromId: U256::from(cursor),
            limit: U256::from(256),
        };
        let mut page = self
            .rpc
            .call_at(self.cfg.coordinator, call.clone(), head.number)
            .await?;
        let mut next: u64 = page.nextCursor.try_into()?;
        if !discovery_page_advances(cursor, next, &page.ids)? {
            // All URLs here passed startup chain/code verification. A stale success
            // does not trigger transport failover, so explicitly seek a fresh page.
            for url in &self.rpc.urls {
                let value = match self
                    .rpc
                    .at(
                        url,
                        "eth_call",
                        json!([{"to":self.cfg.coordinator,"data":Bytes::from(call.abi_encode())},format!("0x{:x}",head.number)]),
                    )
                    .await
                {
                    Ok(value) => value,
                    Err(error) if crate::rpc::is_delivery_failure(&error) => continue,
                    Err(error) => return Err(error),
                };
                let bytes: Bytes = serde_json::from_value(value)?;
                let candidate = C::getPendingRequestIdsCall::abi_decode_returns(&bytes)?;
                let candidate_next: u64 = candidate.nextCursor.try_into()?;
                if discovery_page_advances(cursor, candidate_next, &candidate.ids)? {
                    page = candidate;
                    next = candidate_next;
                    break;
                }
            }
            if next <= cursor {
                tracing::warn!(cursor, "Discovery deferred: verified RPC pages are stale");
                return Ok(());
            }
        }
        // Slices are read in order, one batched request each, and committed id by id: the cursor only ever
        // covers a contiguous prefix of read requests, so a slow or failed slice cannot be skipped.
        let tag = format!("0x{:x}", head.number);
        for slice in page.ids.chunks(REQUEST_BATCH) {
            for (id, r) in slice.iter().zip(self.requests_in(slice, &tag).await?) {
                let id: u64 = (*id).try_into()?;
                let next_id = (id + 1).to_string();
                if terminal(&r, head.timestamp).is_none() {
                    self.journal
                        .discovered_epoch(
                            &id.to_string(),
                            r.deadline.try_into()?,
                            &next_id,
                            Some(r.epochId),
                        )
                        .await?;
                } else {
                    self.journal.cursor(&next_id).await?;
                }
            }
        }
        self.journal.cursor(&next.to_string()).await?;
        Ok(())
    }
    /// Returns whether this call journaled a new proof.
    async fn prepare(&self, job: &Job, request: Request) -> Result<bool> {
        if request.epochHash == B256::ZERO {
            return Ok(false);
        }
        if job.call.is_some() {
            return Ok(false);
        }
        let id: U256 = job.id.parse()?;
        let context = self
            .rpc
            .call(self.cfg.coordinator, C::getProofContextCall { id })
            .await?;
        let now = self.rpc.head().await?.timestamp;
        ensure!(
            !context.fulfilled
                && !context.refunded
                && context.deadline == request.deadline
                && now + self.cfg.margin < context.deadline,
            "No proof-generation time budget remains"
        );
        let key = self.vrf_key.clone();
        // Dropping a preparation future cannot abort an already-running blocking proof.
        // Keep its permit inside the closure so cancellation cannot accumulate CPU tasks.
        let permit = self.proof_slots.clone().acquire_owned().await?;
        let proof = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            prover::prove(context.seed, &key)
        })
        .await??;
        // send_prepared executes the full fulfillment through eth_estimateGas before
        // signing. That preflight verifies this proof and all contract conditions.
        let call = C::fulfillRandomnessCall {
            id,
            proof: proof.clone(),
        }
        .abi_encode();
        self.journal
            .prepared(
                &job.id,
                &serde_json::to_string(&proof)?,
                &format!("0x{}", hex::encode(call)),
            )
            .await?;
        crate::health::preparation_progress(&self.journal, &job.id, crate::health::now()?).await?;
        tracing::info!(request_id=%job.id,"VRF proof prepared and journaled");
        Ok(true)
    }
    async fn send_prepared(&self, id: &str) -> Result<()> {
        let Some(job) = self.journal.job(id).await? else {
            return Ok(());
        };
        let Some(call) = job.call else {
            return Ok(());
        };
        // Estimation is not a send: the pins are verified after it, immediately before signing.
        let (request, head, latest, pending) = tokio::try_join!(
            self.request(id.parse()?),
            self.rpc.finalized_head(),
            self.rpc.nonce(self.tx_key.address(), "latest"),
            self.rpc.nonce(self.tx_key.address(), "pending"),
        )?;
        if let Some(state) = terminal(&request, head.timestamp) {
            if request.fulfilled && !request.delivered {
                crate::audit::callback_failed(&self.journal.pool, id).await?;
            }
            self.journal.state(id, state).await?;
            return Ok(());
        }
        if head.timestamp + self.cfg.margin >= request.deadline
            || !self.policy()?.allows(id, request.deadline, head.timestamp)
        {
            return Ok(());
        }
        let floor = self.journal.nonce_floor().await?;
        if latest < floor || pending < floor {
            return Err(SendDeferred::new(anyhow::anyhow!(
                "Stale RPC nonce below durable nonce floor {floor}; deferring signature"
            ))
            .into());
        }
        if latest != pending {
            return Err(SendDeferred::new(anyhow::anyhow!(
                "Dedicated transaction wallet has unjournaled pending transactions"
            ))
            .into());
        }
        let estimate = self
            .rpc
            .request(
                "eth_estimateGas",
                json!([{"from":self.tx_key.address(),"to":self.cfg.coordinator,"data":call}]),
            )
            .await;
        let gas = match estimate {
            Ok(value) => quantity(&value)?,
            Err(error) if crate::rpc::is_delivery_failure(&error) => {
                if crate::rpc::is_node_error_response(&error) {
                    // Another keeper or submitter may have settled the request since the finalized read:
                    // that is not a rejection of this proof. Finalized state classifies it next tick.
                    if self.settled_at_latest(id).await? {
                        tracing::info!(request_id=%id,role=self.cfg.role.name(),"Request already settled by another submitter; nothing to send");
                        return Ok(());
                    }
                    // This request's own proof or readiness was rejected: it keeps its
                    // single retries but must not drag every batch back to single sends.
                    self.journal.exclude_from_batches(id).await?;
                }
                return Err(SendDeferred::new(error).into());
            }
            Err(error) => return Err(error),
        };
        let used = gas;
        // The callback's full budget is reserved: never under-provisioned to fit a cap.
        let gas = fulfillment_gas(used, &[request.callbackGasLimit])?;
        if let Some(exceeded) = self.gas_over_budget(gas) {
            return Err(SendDeferred::budget(exceeded).into());
        }
        let priority = self.priority_fee().await;
        let fee = required_fee(head.base_fee, priority)?;
        if self.cfg.fee_coverage_bps != 0 {
            let paid: u128 = self
                .rpc
                .call(
                    self.cfg.coordinator,
                    C::requestFeePaidCall {
                        requestId: id.parse()?,
                    },
                )
                .await?
                .try_into()?;
            if let Some(exceeded) = uncovered(
                expected_cost(head.base_fee, priority, used),
                paid,
                self.cfg.fee_coverage_bps,
            ) {
                return Err(SendDeferred::budget(exceeded).into());
            }
        }
        // Recheck implementations and time after estimation; neither cached ABI nor old head authorizes a send.
        self.verify_runtime().await?;
        let now = self.rpc.head().await?.timestamp;
        if now + self.cfg.margin >= request.deadline {
            return Ok(());
        }
        let plan = TxPlan {
            nonce: latest,
            gas,
            fee,
            priority,
            payload: call,
            kind: "fulfill".into(),
        };
        if let Some(exceeded) = self.over_budget(&plan) {
            return Err(SendDeferred::budget(exceeded).into());
        }
        self.sign_and_journal(id, plan, now).await?;
        self.broadcast_latest(now).await
    }
    /// One fulfillRandomnessBatch for the earliest-deadline prepared requests. Each member
    /// passes exactly the checks of send_prepared; the node's preflight then verifies every
    /// proof at once. The gas limit reserves every member's full callback budget, and a batch
    /// that then exceeds a gas or cost cap shrinks to the members that fit.
    async fn send_prepared_batch(&self, candidates: &[Job]) -> Result<BatchOutcome> {
        // Estimation is not a send: the pins are verified after it, immediately before signing.
        let (head, latest, pending) = tokio::try_join!(
            self.rpc.finalized_head(),
            self.rpc.nonce(self.tx_key.address(), "latest"),
            self.rpc.nonce(self.tx_key.address(), "pending"),
        )?;
        let policy = self.policy()?;
        let ids = candidates
            .iter()
            .map(|job| job.id.parse())
            .collect::<std::result::Result<Vec<U256>, _>>()?;
        let mut members = Vec::new();
        for (job, request) in candidates
            .iter()
            .zip(self.requests_in(&ids, "finalized").await?)
        {
            if let Some(state) = terminal(&request, head.timestamp) {
                if request.fulfilled && !request.delivered {
                    crate::audit::callback_failed(&self.journal.pool, &job.id).await?;
                }
                self.journal.state(&job.id, state).await?;
                continue;
            }
            if head.timestamp + self.cfg.margin >= request.deadline
                || !policy.allows(&job.id, request.deadline, head.timestamp)
            {
                continue;
            }
            let Some(call) = &job.call else {
                continue;
            };
            // The batch carries exactly the proof the journaled single calldata carries.
            let decoded = C::fulfillRandomnessCall::abi_decode(&call.parse::<Bytes>()?)?;
            ensure!(
                decoded.id.to_string() == job.id,
                "Journaled calldata belongs to another request"
            );
            members.push(Member {
                id: job.id.clone(),
                request_id: decoded.id,
                proof: decoded.proof,
                deadline: request.deadline,
                callback_gas: request.callbackGasLimit,
                fee_paid: 0,
            });
        }
        if members.len() < 2 {
            return Ok(BatchOutcome::Single);
        }
        if self.cfg.fee_coverage_bps != 0 {
            let fees = stream::iter(members.iter())
                .map(|member| async move {
                    let fee: u128 = self
                        .rpc
                        .call(
                            self.cfg.coordinator,
                            C::requestFeePaidCall {
                                requestId: member.request_id,
                            },
                        )
                        .await?
                        .try_into()?;
                    Ok::<_, anyhow::Error>(fee)
                })
                .buffered(8)
                .collect::<Vec<_>>()
                .await;
            for (member, fee) in members.iter_mut().zip(fees) {
                member.fee_paid = fee?;
            }
        }
        let floor = self.journal.nonce_floor().await?;
        if latest < floor || pending < floor {
            return Err(SendDeferred::new(anyhow::anyhow!(
                "Stale RPC nonce below durable nonce floor {floor}; deferring signature"
            ))
            .into());
        }
        if latest != pending {
            return Err(SendDeferred::new(anyhow::anyhow!(
                "Dedicated transaction wallet has unjournaled pending transactions"
            ))
            .into());
        }
        let priority = self.priority_fee().await;
        let fee = required_fee(head.base_fee, priority)?;
        // The price per gas does not depend on the member count: an unaffordable price is a
        // budget deferral before any estimate, and no smaller batch could change it.
        if fee > self.cfg.max_fee {
            return Err(SendDeferred::budget(FeeBudget {
                cap: FeeCap::MaxFeePerGas,
                required: fee,
                limit: self.cfg.max_fee,
            })
            .into());
        }
        let mut shrinks = 0;
        let (plan, now) = loop {
            let payload = batch_payload(&members);
            let estimate = self
                .rpc
                .request(
                    "eth_estimateGas",
                    json!([{"from":self.tx_key.address(),"to":self.cfg.coordinator,"data":payload}]),
                )
                .await;
            let gas = match estimate {
                Ok(value) => quantity(&value)?,
                Err(error) if crate::rpc::is_node_error_response(&error) => {
                    // One member's proof or readiness fails the whole call. The single path
                    // preflights each request on its own and backs off only the bad one.
                    tracing::warn!(members=members.len(),error=%error,"Batch preflight rejected by the node; falling back to single sends this tick");
                    return Ok(BatchOutcome::Single);
                }
                Err(error) if crate::rpc::is_delivery_failure(&error) => {
                    return Err(SendDeferred::new(error).into());
                }
                Err(error) => return Err(error),
            };
            let used = gas;
            // Every member's full callback budget is reserved, so no callback can leave a later
            // member short of the coordinator's gas check (see fulfillment_gas).
            let limits: Vec<u32> = members.iter().map(|member| member.callback_gas).collect();
            let gas = fulfillment_gas(used, &limits)?;
            let plan = TxPlan {
                nonce: latest,
                gas,
                fee,
                priority,
                payload,
                kind: "fulfill_batch".into(),
            };
            if let Some(exceeded) = self
                .gas_over_budget(gas)
                .or_else(|| self.over_budget(&plan))
            {
                // A batch that does not fit is shrunk, never under-provisioned: it keeps the
                // earliest members that fit the caps with their full budgets.
                let fit = members_within(
                    used,
                    &limits,
                    gas_cap(self.cfg.max_gas, self.cfg.max_cost, fee),
                )
                .min(members.len() - 1);
                if fit >= 2 && shrinks < MAX_BATCH_SHRINKS {
                    shrinks += 1;
                    tracing::info!(members=members.len(),next=fit,exceeded=%exceeded,"Batch with full callback budgets exceeds a configured cap; shrinking it to the members that fit");
                    members.truncate(fit);
                    continue;
                }
                // The single path prices one request on its own and classifies its own budget
                // deferral; no cap is bypassed either way.
                tracing::warn!(members=members.len(),exceeded=%exceeded,"Batch with full callback budgets still exceeds a configured cap; using the single path");
                return Ok(BatchOutcome::Single);
            }
            // Recheck implementations and time after estimation; neither cached ABI nor old head authorizes a send.
            let fees = members
                .iter()
                .fold(0u128, |sum, member| sum.saturating_add(member.fee_paid));
            if let Some(exceeded) = uncovered(
                expected_cost(head.base_fee, priority, used),
                fees,
                self.cfg.fee_coverage_bps,
            ) {
                if members.len() > 2 {
                    // Cross-subsidy stops here: the lowest-fee member waits for a cheaper send.
                    let lowest = (0..members.len())
                        .min_by_key(|&i| members[i].fee_paid)
                        .unwrap_or(0);
                    tracing::info!(members=members.len(),dropped=%members[lowest].id,exceeded=%exceeded,"Batch fees do not cover the expected cost; dropping the lowest-fee member");
                    members.remove(lowest);
                    continue;
                }
                tracing::info!(members=members.len(),exceeded=%exceeded,"Batch fees do not cover the expected cost; using the single path");
                return Ok(BatchOutcome::Single);
            }
            self.verify_runtime().await?;
            let now = self.rpc.head().await?.timestamp;
            let before = members.len();
            members.retain(|member| now + self.cfg.margin < member.deadline);
            if members.len() != before {
                if members.len() < 2 {
                    return Ok(BatchOutcome::Single);
                }
                // The payload changed with the member list; the estimate must match it.
                continue;
            }
            break (plan, now);
        };
        let ids: Vec<String> = members.iter().map(|member| member.id.clone()).collect();
        let key = crate::journal::batch_key(&ids)?;
        self.sign_and_journal_members(&key, plan, now, &ids).await?;
        self.broadcast_latest(now).await?;
        Ok(BatchOutcome::Sent)
    }
    /// Current chain state of every member, in journal order.
    async fn member_requests(&self, members: &[String]) -> Result<Vec<Request>> {
        let ids = members
            .iter()
            .map(|id| id.parse())
            .collect::<std::result::Result<Vec<U256>, _>>()?;
        self.requests_in(&ids, "finalized").await
    }
    pub async fn stop_epoch_fetch(&self) -> Result<()> {
        self.epoch.finish_fetch().await?;
        Ok(())
    }
    /// Health stage `epoch`: live paid demand stuck on blocked or stale epoch work. It clears
    /// as soon as no such demand remains, after publication or once the demand expires.
    async fn observe_epoch_demand(&self, head: &Head) -> Result<()> {
        let stalled = stalled_epoch_demand(
            &self.journal.pool,
            self.epoch.registry,
            self.epoch.catalog,
            head,
            self.cfg.margin,
        )
        .await?;
        let Some((key, reason)) = stalled else {
            return crate::health::recovered(&self.journal, "epoch").await;
        };
        let first = self.journal.meta("health:blocked:epoch").await?.is_none();
        crate::health::blocked(&self.journal, "epoch", crate::health::now()?).await?;
        self.notify_error(crate::telegram::ErrorClass::EpochPreparation);
        if first {
            tracing::error!(epoch_key=%key,reason,"Live paid demand cannot be published: epoch work is blocked or its saved packet is too old; inspect the selected source gateway");
        }
        Ok(())
    }
    /// Revalidate a funded request immediately before signing/broadcasting.
    /// Checkpoints and idle snapshots alone never authorize an epoch transaction.
    async fn epoch_demand_head(&self, epoch: u64, now: u64) -> Result<Option<Head>> {
        let ids: Vec<String> = sqlx::query_scalar("SELECT jobs.id FROM epoch_demand JOIN jobs ON jobs.id=epoch_demand.job WHERE epoch_demand.epoch=? AND jobs.state IN ('pending','prepared','signed','submitted') AND jobs.deadline>? ORDER BY jobs.deadline DESC LIMIT 256")
            .bind(i64::try_from(epoch)?).bind(i64::try_from(now.saturating_add(self.cfg.margin))?).fetch_all(&self.journal.pool).await?;
        let mut requests = stream::iter(ids)
            .map(|id| async move {
                let r = self.request(id.parse()?).await?;
                Ok::<_, anyhow::Error>(r)
            })
            .buffered(8);
        while let Some(request) = requests.next().await {
            let r = request?;
            if r.epochId == epoch
                && terminal(&r, now).is_none()
                && now.saturating_add(self.cfg.margin) < r.deadline
            {
                // Request RPCs may have been slow. Expiry must use a head read AFTER them.
                self.verify_runtime().await?;
                let fresh = self.rpc.head().await?;
                if terminal(&r, fresh.timestamp).is_none()
                    && fresh.timestamp.saturating_add(self.cfg.margin) < r.deadline
                {
                    return Ok(Some(fresh));
                }
            }
        }
        Ok(None)
    }
    async fn send_epoch(&self, work: &crate::epoch::Work) -> Result<()> {
        let epoch = &self.epoch;
        ensure!(
            epoch.key(work.epoch) == work.key,
            "Epoch registry identity mismatch"
        );
        if !self.journal.unresolved().await?.is_empty()
            || work.retry_at > crate::health::now()? as i64
        {
            return Ok(());
        }
        // A prepared snapshot without any open request for its epoch is the idle state: nothing to publish,
        // and nothing to read from chain to find that out.
        let open:i64=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM epoch_demand JOIN jobs ON jobs.id=epoch_demand.job WHERE epoch_demand.epoch=? AND jobs.state IN ('pending','prepared','signed','submitted'))")
            .bind(i64::try_from(work.epoch)?).fetch_one(&self.journal.pool).await?;
        if open == 0 {
            return Ok(());
        }
        // Independent preflight observations share one RPC round. They authorize only
        // estimation; exact implementations and actual demand are checked again before signing.
        let (head, record, latest, pending) = tokio::try_join!(
            self.rpc.finalized_head(),
            self.rpc.call(
                epoch.registry,
                ER::getEpochCall {
                    epochId: work.epoch
                }
            ),
            self.rpc.nonce(self.tx_key.address(), "latest"),
            self.rpc.nonce(self.tx_key.address(), "pending"),
        )?;
        if let Some(state) = epoch_terminal(work, &head)? {
            crate::epoch::state(&self.journal.pool, &work.key, state).await?;
            return Ok(());
        }
        let demand:i64=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM epoch_demand JOIN jobs ON jobs.id=epoch_demand.job WHERE epoch_demand.epoch=? AND jobs.state IN ('pending','prepared','signed','submitted') AND jobs.deadline>?)")
            .bind(i64::try_from(work.epoch)?).bind(i64::try_from(head.timestamp.saturating_add(self.cfg.margin))?).fetch_one(&self.journal.pool).await?;
        if demand == 0 {
            return Ok(());
        }
        if record.epochHash != B256::ZERO {
            crate::epoch::state(&self.journal.pool, &work.key, "committed").await?;
            return Ok(());
        }
        if let Role::Follower(plan) = self.cfg.role {
            // A follower publishes an attempt only once it has been publishable for its delay: the window must be
            // open and this node must have seen live paid demand for it that long ago. Anchoring on demand as well
            // as on the window matters when the first paid request of an idle epoch arrives late in a window, where
            // the window alone would already be satisfied and the follower would race the primary's publication.
            let window = work.start.saturating_add(
                u64::from(work.fallback).saturating_mul(crate::epoch::FALLBACK_DELAY_BLOCKS),
            );
            if head.number < window {
                return Ok(());
            }
            let demand_since = {
                let mut seen = self
                    .epoch_demand_since
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Epoch demand lock poisoned"))?;
                *seen.entry(work.key.clone()).or_insert(head.timestamp)
            };
            let anchor = self.rpc.block(window).await?.timestamp.max(demand_since);
            if head.timestamp < anchor.saturating_add(plan.delay) {
                return Ok(());
            }
        }
        let api: ApiProof = serde_json::from_str(
            work.api
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Epoch packet missing"))?,
        )?;
        if api.timestamp > U256::from(head.timestamp) {
            return Ok(());
        }
        ensure!(
            latest == pending && latest >= self.journal.nonce_floor().await?,
            "Epoch nonce is ambiguous or behind durable floor"
        );
        let call = if work.fallback == 0 {
            ER::commitEpochCall {
                epochId: work.epoch,
                attestation: api,
            }
            .abi_encode()
        } else {
            ER::commitEpochFallbackCall {
                epochId: work.epoch,
                attempt: work.fallback,
                attestation: api,
            }
            .abi_encode()
        };
        let payload = format!("0x{}", hex::encode(call));
        sqlx::query("UPDATE epoch_work SET retry_at=? WHERE key=?")
            .bind(i64::try_from(crate::health::now()?.saturating_add(2))?)
            .bind(&work.key)
            .execute(&self.journal.pool)
            .await?;
        let estimate = self
            .rpc
            .request(
                "eth_estimateGas",
                json!([{"from":self.tx_key.address(),"to":epoch.registry,"data":payload}]),
            )
            .await;
        let gas = match estimate {
            Ok(value) => quantity(&value)?,
            Err(error) => {
                if crate::rpc::is_node_error_response(&error)
                    && self.epoch_published_at_latest(work.epoch).await?
                {
                    // Another committer published first; finalized state marks the work committed next tick.
                    tracing::info!(epoch_key=%work.key,role=self.cfg.role.name(),"Epoch already published by another committer; nothing to send");
                    return Ok(());
                }
                if !crate::rpc::is_delivery_failure(&error) {
                    crate::epoch::state(&self.journal.pool, &work.key, "blocked").await?;
                }
                return Err(error);
            }
        }
        .checked_mul(12)
        .ok_or_else(|| anyhow::anyhow!("Epoch gas overflow"))?
            / 10
            + 50000;
        if let Some(exceeded) = self.gas_over_budget(gas) {
            return Err(SendDeferred::budget(exceeded).into());
        }
        let priority = self.priority_fee().await;
        let plan = TxPlan {
            nonce: latest,
            gas,
            fee: required_fee(head.base_fee, priority)?,
            priority,
            payload,
            kind: "epoch".into(),
        };
        // Budget precedes the demand recheck: an unaffordable send needs no further RPC round.
        if let Some(exceeded) = self.over_budget(&plan) {
            return Err(SendDeferred::budget(exceeded).into());
        }
        let Some(fresh) = self.epoch_demand_head(work.epoch, head.timestamp).await? else {
            return Ok(());
        };
        if epoch_terminal(work, &fresh)?.is_some() {
            return Ok(());
        }
        self.sign_and_journal(&work.key, plan, fresh.timestamp)
            .await?;
        self.broadcast_latest(fresh.timestamp).await
    }
    async fn reconcile_epoch(&self, attempts: &[Attempt], head: &Head) -> Result<bool> {
        let latest = attempts
            .last()
            .ok_or_else(|| anyhow::anyhow!("Missing epoch attempt"))?;
        let epoch = &self.epoch;
        let work = crate::epoch::work(&self.journal.pool, &latest.job).await?;
        ensure!(
            epoch.key(work.epoch) == work.key,
            "Epoch registry identity mismatch"
        );
        let record = self
            .rpc
            .call(
                epoch.registry,
                ER::getEpochCall {
                    epochId: work.epoch,
                },
            )
            .await?;
        let terminal = if record.epochHash != B256::ZERO {
            Some("committed")
        } else if let Some(state) = epoch_terminal(&work, head)? {
            Some(state)
        } else if self
            .epoch_demand_head(work.epoch, head.timestamp)
            .await?
            .is_none()
        {
            // Resolve/cancel the nonce without discarding this immutable snapshot.
            Some("prepared")
        } else {
            None
        };
        for a in attempts {
            if let Some(receipt) = self.rpc.receipt(&a.hash).await? {
                if !self.rpc.receipt_is_finalized(&a.hash, &receipt).await? {
                    return Ok(true);
                }
                self.journal
                    .finalized_receipt(
                        &a.hash,
                        quantity(&receipt["blockNumber"])?,
                        receipt["blockHash"]
                            .as_str()
                            .ok_or_else(|| anyhow::anyhow!("Missing receipt block hash"))?,
                        quantity(&receipt["status"])?,
                    )
                    .await?;
                let status = quantity(&receipt["status"])?;
                self.journal
                    .resolve_nonce_epoch(
                        a.nonce,
                        &a.job,
                        epoch_resolved_state(terminal, &a.kind, status),
                    )
                    .await?;
                tracing::info!(epoch_key=%a.job,tx_hash=%a.hash,status,"Epoch receipt reconciled");
                if status == 1
                    && a.kind == "epoch"
                    && self.cfg.role.is_follower()
                    && let Ok(tx_hash) = a.hash.parse()
                {
                    tracing::warn!(epoch=work.epoch,tx_hash=%a.hash,"Follower published epoch {}",work.epoch);
                    self.follower_notice(crate::telegram::Event::EpochPublished {
                        epoch: work.epoch,
                        tx_hash,
                    });
                }
                return Ok(false);
            }
        }
        if self.rpc.nonce(self.tx_key.address(), "finalized").await? > latest.nonce as u64 {
            if record.epochHash != B256::ZERO {
                self.journal
                    .resolve_nonce_epoch(latest.nonce, &latest.job, "committed")
                    .await?;
                return Ok(false);
            }
            if awaiting_receipt_visibility(head.timestamp, latest) {
                return Ok(true);
            }
            bail!("Epoch nonce consumed without receipt or terminal registry state");
        }
        if !self.cfg.send {
            return Ok(true);
        }
        if terminal.is_some() && latest.kind != "epoch_cancel" {
            let tip = self.priority_fee().await;
            let (fee, priority) = replacement_fees(
                latest.priority.parse()?,
                latest.fee.parse()?,
                head.base_fee,
                tip,
            )?;
            if self
                .replace_within_budget(
                    &latest.job,
                    TxPlan {
                        nonce: latest.nonce.try_into()?,
                        gas: 21000,
                        fee,
                        priority,
                        payload: "0x".into(),
                        kind: "epoch_cancel".into(),
                    },
                    head.timestamp,
                )
                .await?
            {
                self.broadcast_latest(head.timestamp).await?;
            }
            return Ok(true);
        }
        if head.timestamp.saturating_sub(latest.created as u64) >= 10
            && attempts.iter().filter(|a| a.kind == latest.kind).count() < 4
            && (latest.kind == "epoch_cancel" || terminal.is_none())
        {
            let tip = self.priority_fee().await;
            let (fee, priority) = replacement_fees(
                latest.priority.parse()?,
                latest.fee.parse()?,
                head.base_fee,
                tip,
            )?;
            if self
                .replace_within_budget(
                    &latest.job,
                    TxPlan {
                        nonce: latest.nonce.try_into()?,
                        gas: latest.gas.try_into()?,
                        fee,
                        priority,
                        payload: latest.payload.clone(),
                        kind: latest.kind.clone(),
                    },
                    head.timestamp,
                )
                .await?
            {
                self.broadcast_latest(head.timestamp).await?;
            }
        } else if head.timestamp.saturating_sub(latest.broadcast as u64) >= 2 {
            self.broadcast_latest(head.timestamp).await?;
        }
        Ok(true)
    }
    async fn sign_and_journal(&self, job: &str, plan: TxPlan, now: u64) -> Result<()> {
        // A batch replacement re-signs the journaled member list; only the first attempt of
        // a batch passes its members explicitly (send_prepared_batch).
        let members = if plan.kind == "fulfill_batch" {
            self.journal.batch_members(job).await?
        } else {
            Vec::new()
        };
        self.sign_and_journal_members(job, plan, now, &members)
            .await
    }
    async fn sign_and_journal_members(
        &self,
        job: &str,
        plan: TxPlan,
        now: u64,
        members: &[String],
    ) -> Result<()> {
        self.ensure_not_upgraded()?;
        ensure!(
            self.over_budget(&plan).is_none(),
            "Transaction exceeds fee/cost budget"
        );
        let TxPlan {
            nonce,
            gas,
            fee,
            priority,
            payload,
            kind,
        } = plan;
        let to = if kind == "cancel" || kind == "epoch_cancel" {
            self.tx_key.address()
        } else if kind == "epoch" {
            self.epoch.registry
        } else {
            self.cfg.coordinator
        };
        let tx = TxEip1559 {
            chain_id: self.cfg.chain_id,
            nonce,
            gas_limit: gas,
            max_fee_per_gas: fee,
            max_priority_fee_per_gas: priority,
            to: TxKind::Call(to),
            value: U256::ZERO,
            access_list: Default::default(),
            input: payload.parse()?,
        };
        let sig = self.tx_key.sign_hash_sync(&tx.signature_hash())?;
        let signed = tx.into_signed(sig);
        let raw = signed.encoded_2718();
        let hash = keccak256(&raw);
        let a = Attempt {
            id: 0,
            job: job.into(),
            nonce: nonce.try_into()?,
            hash: hash.to_string(),
            raw: format!("0x{}", hex::encode(raw)),
            kind,
            fee: fee.to_string(),
            state: "signed".into(),
            gas: gas.try_into()?,
            priority: priority.to_string(),
            payload,
            created: now.try_into()?,
            broadcast: 0,
        };
        // No await to a network broadcaster may occur before this commits.
        if a.kind == "fulfill_batch" {
            self.journal.signed_batch(&a, members).await?;
        } else {
            self.journal.signed(&a).await?;
        }
        self.lane_started(a.nonce).await?;
        // An affordable signed send is the only evidence that clears a budget observation.
        crate::health::recovered(&self.journal, "fee_budget").await?;
        Ok(())
    }
    async fn replace_within_budget(&self, job: &str, plan: TxPlan, now: u64) -> Result<bool> {
        self.verify_runtime().await?;
        if let Some(exceeded) = self.over_budget(&plan) {
            // The nonce is retained for reconciliation; caps are never bypassed for recovery.
            self.defer_for_budget(job, &plan.kind, &exceeded).await?;
            return Ok(false);
        }
        let now = if plan.kind == "epoch" {
            let work = crate::epoch::work(&self.journal.pool, job).await?;
            let Some(fresh) = self.epoch_demand_head(work.epoch, now).await? else {
                return Ok(false);
            };
            if epoch_terminal(&work, &fresh)?.is_some() {
                return Ok(false);
            }
            fresh.timestamp
        } else if plan.kind == "fulfill" {
            let request = self.request(job.parse()?).await?;
            self.verify_runtime().await?;
            let fresh = self.rpc.head().await?;
            if !timely(&request, fresh.timestamp, self.cfg.margin) {
                return Ok(false);
            }
            fresh.timestamp
        } else if plan.kind == "fulfill_batch" {
            // The identical payload is worth a higher fee while any member can still be served.
            let members = self.journal.batch_members(job).await?;
            let requests = self.member_requests(&members).await?;
            self.verify_runtime().await?;
            let fresh = self.rpc.head().await?;
            if !requests
                .iter()
                .any(|request| timely(request, fresh.timestamp, self.cfg.margin))
            {
                return Ok(false);
            }
            fresh.timestamp
        } else {
            now
        };
        self.sign_and_journal(job, plan, now).await?;
        Ok(true)
    }
    /// Operator sweep (see crate::sweep). Returns whether the nonce lane is busy.
    async fn sweep(&self, head: &Head) -> Result<bool> {
        if let Some(attempt) = crate::sweep::attempt(&self.journal.pool).await? {
            return self.reconcile_sweep(attempt, head).await;
        }
        if !self.cfg.send {
            return Ok(false);
        }
        match crate::sweep::request(&self.journal.pool).await? {
            Some(request) => self.start_sweep(request, head).await,
            None => Ok(false),
        }
    }
    async fn refuse_sweep(&self, request: &crate::sweep::Request, detail: String) -> Result<bool> {
        tracing::warn!(detail = %detail, "Operator sweep refused; nothing was signed");
        crate::sweep::refuse(
            &self.journal.pool,
            request,
            detail.clone(),
            crate::health::now()?,
        )
        .await?;
        if let Some(notifier) = &self.telegram {
            notifier.notify(crate::telegram::Event::Sweep(format!(
                "Keeper sweep refused; nothing was sent\n{detail}"
            )));
        }
        Ok(false)
    }
    async fn start_sweep(&self, request: crate::sweep::Request, head: &Head) -> Result<bool> {
        let wallet = self.tx_key.address();
        let floor = self.journal.nonce_floor().await?;
        let (latest, pending) = tokio::try_join!(
            self.rpc.nonce(wallet, "latest"),
            self.rpc.nonce(wallet, "pending")
        )?;
        if latest < floor || latest != pending {
            // The game path reports stale or unjournaled nonces; the request simply waits.
            tracing::debug!(
                latest,
                pending,
                floor,
                "Operator sweep waits for a settled nonce lane"
            );
            return Ok(false);
        }
        let recipient = self
            .rpc
            .call(self.cfg.coordinator, C::feeRecipientCall {})
            .await?;
        if recipient == Address::ZERO || recipient == wallet {
            return self
                .refuse_sweep(
                    &request,
                    format!("Coordinator fee recipient {recipient} cannot receive a sweep"),
                )
                .await;
        }
        let balance = self
            .rpc
            .request("eth_getBalance", json!([wallet, "latest"]))
            .await?;
        let balance = U256::from_str_radix(
            balance
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Invalid balance response"))?
                .trim_start_matches("0x"),
            16,
        )?;
        let gas = match self
            .rpc
            .request(
                "eth_estimateGas",
                json!([{"from": wallet, "to": recipient, "value": "0x1"}]),
            )
            .await
        {
            Ok(value) => quantity(&value)?,
            Err(error) if crate::rpc::is_node_error_response(&error) => {
                return self
                    .refuse_sweep(
                        &request,
                        format!("The fee recipient rejects a plain transfer: {error}"),
                    )
                    .await;
            }
            Err(error) => return Err(error),
        };
        let gas = (gas.saturating_mul(12) / 10).max(21_000);
        if gas > crate::sweep::MAX_TRANSFER_GAS {
            return self
                .refuse_sweep(
                    &request,
                    format!("A transfer to the fee recipient needs {gas} gas"),
                )
                .await;
        }
        let priority = self.priority_fee().await;
        let plan = TxPlan {
            nonce: latest,
            gas,
            fee: required_fee(head.base_fee, priority)?,
            priority,
            payload: "0x".into(),
            kind: "sweep".into(),
        };
        if let Some(exceeded) = self.over_budget(&plan) {
            return self
                .refuse_sweep(&request, format!("Transfer exceeds a fee cap: {exceeded}"))
                .await;
        }
        let reserve = U256::from(crate::sweep::MIN_RESERVE_WEI.max(self.cfg.max_cost));
        let gas_cost = U256::from(plan.fee) * U256::from(plan.gas);
        let value = match crate::sweep::plan(&request, balance, gas_cost, reserve) {
            Ok(value) => value,
            Err(detail) => return self.refuse_sweep(&request, detail).await,
        };
        // Fresh pins and committer authorization gate every signature, as for game work.
        self.verify_runtime().await?;
        let signed = self.sign_transfer(&plan, recipient, value, crate::health::now()?)?;
        let mut attempt = crate::sweep::Attempt {
            request,
            nonce: latest,
            to: recipient.to_string(),
            value: value.to_string(),
            txs: vec![signed],
        };
        // No broadcast may happen before the signed transfer is committed.
        crate::sweep::start(&self.journal.pool, &attempt).await?;
        tracing::info!(nonce = latest, to = %recipient, value = %value, tx_hash = %attempt.txs[0].hash, "Operator sweep signed and journaled");
        self.broadcast_sweep(&mut attempt).await?;
        Ok(true)
    }
    fn sign_transfer(
        &self,
        plan: &TxPlan,
        to: Address,
        value: U256,
        now: u64,
    ) -> Result<crate::sweep::SignedTx> {
        self.ensure_not_upgraded()?;
        let tx = TxEip1559 {
            chain_id: self.cfg.chain_id,
            nonce: plan.nonce,
            gas_limit: plan.gas,
            max_fee_per_gas: plan.fee,
            max_priority_fee_per_gas: plan.priority,
            to: TxKind::Call(to),
            value,
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let sig = self.tx_key.sign_hash_sync(&tx.signature_hash())?;
        let raw = tx.into_signed(sig).encoded_2718();
        Ok(crate::sweep::SignedTx {
            kind: plan.kind.clone(),
            hash: keccak256(&raw).to_string(),
            raw: format!("0x{}", hex::encode(raw)),
            gas: plan.gas,
            fee: plan.fee.to_string(),
            priority: plan.priority.to_string(),
            created: now,
            broadcast: 0,
        })
    }
    async fn broadcast_sweep(&self, attempt: &mut crate::sweep::Attempt) -> Result<()> {
        // Every broadcast, a retry included, passes the runtime pins immediately before it.
        self.verify_runtime().await?;
        let last = attempt
            .txs
            .last_mut()
            .ok_or_else(|| anyhow::anyhow!("Sweep attempt has no transaction"))?;
        last.broadcast = crate::health::now()?;
        let (raw, hash) = (last.raw.clone(), last.hash.clone());
        crate::sweep::save(&self.journal.pool, attempt).await?;
        match self.rpc.broadcast(&raw, hash.parse()?).await? {
            crate::rpc::BroadcastOutcome::Rejected(reason) => {
                self.notify_error(crate::telegram::ErrorClass::TransactionSubmission);
                tracing::error!(tx_hash = %hash, reason, "Node rejected sweep transaction; signed nonce retained for reconciliation");
            }
            crate::rpc::BroadcastOutcome::Ambiguous => {
                tracing::warn!(tx_hash = %hash, "Sweep broadcast ambiguous; retaining nonce for reconciliation")
            }
            _ => tracing::info!(tx_hash = %hash, "Sweep transaction broadcast"),
        }
        Ok(())
    }
    async fn reconcile_sweep(
        &self,
        mut attempt: crate::sweep::Attempt,
        head: &Head,
    ) -> Result<bool> {
        for tx in &attempt.txs {
            let Some(receipt) = self.rpc.receipt(&tx.hash).await? else {
                continue;
            };
            if !self.rpc.receipt_is_finalized(&tx.hash, &receipt).await? {
                return Ok(true);
            }
            let state = match (tx.kind.as_str(), quantity(&receipt["status"])?) {
                ("sweep", 1) => "sent",
                ("sweep", _) => "reverted",
                _ => "cancelled",
            };
            let value: U256 = attempt.value.parse()?;
            let outcome = crate::sweep::Outcome {
                state: state.into(),
                detail: String::new(),
                to: Some(attempt.to.clone()),
                value: (state == "sent").then(|| attempt.value.clone()),
                tx_hash: Some(tx.hash.clone()),
                requested_at: attempt.request.requested_at,
                finished_at: crate::health::now()?,
            };
            crate::sweep::finish(&self.journal.pool, attempt.nonce, &outcome).await?;
            tracing::info!(state, nonce = attempt.nonce, to = %attempt.to, value = %value, tx_hash = %tx.hash, "Operator sweep resolved");
            if let Some(notifier) = &self.telegram {
                notifier.notify(crate::telegram::Event::Sweep(match state {
                    "sent" => format!(
                        "Keeper sweep sent: {} USDC to {}\nTransaction: {}",
                        crate::sweep::format_usdc(value),
                        attempt.to,
                        tx.hash
                    ),
                    _ => format!(
                        "Keeper sweep {state}; no funds moved\nTransaction: {}",
                        tx.hash
                    ),
                }));
            }
            return Ok(false);
        }
        let last = attempt
            .txs
            .last()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Sweep attempt has no transaction"))?;
        let now = crate::health::now()?;
        let wallet = self.tx_key.address();
        if self.rpc.nonce(wallet, "finalized").await? > attempt.nonce {
            if now.saturating_sub(last.broadcast.max(last.created))
                < RECEIPT_VISIBILITY_GRACE_SECONDS
            {
                return Ok(true);
            }
            bail!(
                "Sweep nonce consumed but no sweep receipt found; dedicated-wallet conflict needs inspection"
            );
        }
        if !self.cfg.send {
            return Ok(true);
        }
        if now.saturating_sub(last.created) >= crate::sweep::REPLACE_AFTER_SECONDS {
            if attempt.txs.len() < crate::sweep::MAX_ATTEMPTS {
                // A stuck sweep must not hold the service lane: cancel the nonce, never re-price the transfer.
                let tip = self.priority_fee().await;
                let (fee, priority) = replacement_fees(
                    last.priority.parse()?,
                    last.fee.parse()?,
                    head.base_fee,
                    tip,
                )?;
                let plan = TxPlan {
                    nonce: attempt.nonce,
                    gas: 21000,
                    fee,
                    priority,
                    payload: "0x".into(),
                    kind: "cancel".into(),
                };
                if let Some(exceeded) = self.over_budget(&plan) {
                    self.defer_for_budget("sweep", "cancel", &exceeded).await?;
                    return Ok(true);
                }
                self.verify_runtime().await?;
                attempt
                    .txs
                    .push(self.sign_transfer(&plan, wallet, U256::ZERO, now)?);
                // The replacement is committed before it is broadcast.
                crate::sweep::save(&self.journal.pool, &attempt).await?;
                tracing::warn!(
                    nonce = attempt.nonce,
                    "Sweep not included; cancelling its nonce"
                );
                self.broadcast_sweep(&mut attempt).await?;
                return Ok(true);
            }
            self.notify_error(crate::telegram::ErrorClass::TransactionSubmission);
        }
        if now.saturating_sub(last.broadcast) >= 2 {
            self.broadcast_sweep(&mut attempt).await?;
        }
        Ok(true)
    }
    async fn broadcast_latest(&self, observed_at: u64) -> Result<()> {
        let Some(a) = self.journal.unresolved().await?.pop() else {
            return Ok(());
        };
        // Bookkeeping is done first; the fresh final pin/deadline gate below authorizes the send.
        let now = observed_at;
        self.journal
            .broadcast_attempt(a.id, now.try_into()?)
            .await?;
        if a.kind == "epoch" {
            let epoch = &self.epoch;
            let work = crate::epoch::work(&self.journal.pool, &a.job).await?;
            ensure!(
                epoch.key(work.epoch) == work.key,
                "Epoch registry identity mismatch"
            );
            let Some(fresh) = self.epoch_demand_head(work.epoch, observed_at).await? else {
                return Ok(());
            };
            if epoch_terminal(&work, &fresh)?.is_some() {
                return Ok(());
            }
        }
        if a.kind == "fulfill" {
            let r = self.request(a.job.parse()?).await?;
            self.verify_runtime().await?;
            let now = self.rpc.head().await?.timestamp;
            if !timely(&r, now, self.cfg.margin) {
                return Ok(());
            }
        }
        if a.kind == "fulfill_batch" {
            // Terminal members are skipped on chain, so the batch is sent while any member is
            // still live and timely; once none is, only the nonce cancellation may go out.
            let members = self.journal.batch_members(&a.job).await?;
            ensure!(!members.is_empty(), "Batch members missing from journal");
            let requests = self.member_requests(&members).await?;
            self.verify_runtime().await?;
            let now = self.rpc.head().await?.timestamp;
            if !requests.iter().any(|r| timely(r, now, self.cfg.margin)) {
                return Ok(());
            }
        }
        if a.kind == "cancel" || a.kind == "epoch_cancel" {
            self.verify_runtime().await?;
        }
        self.ensure_not_upgraded()?;
        let outcome = self.rpc.broadcast(&a.raw, a.hash.parse()?).await?;
        if let crate::rpc::BroadcastOutcome::Rejected(reason) = outcome {
            self.notify_error(crate::telegram::ErrorClass::TransactionSubmission);
            crate::health::rejection(&self.journal, reason).await?;
            tracing::error!(work_id=%a.job,tx_hash=%a.hash,reason,"Node rejected transaction; signed nonce retained for reconciliation");
            return Ok(());
        }
        if outcome == crate::rpc::BroadcastOutcome::Ambiguous {
            tracing::warn!(work_id=%a.job,tx_hash=%a.hash,"Broadcast ambiguous; retaining nonce for reconciliation");
            return Ok(());
        }
        crate::health::clear_rejection(&self.journal).await?;
        self.journal.tx_state(a.id, "submitted").await?;
        if a.kind.starts_with("epoch") {
            crate::epoch::state(&self.journal.pool, &a.job, "submitted").await?;
            tracing::info!(epoch_key=%a.job,tx_hash=%a.hash,nonce=a.nonce,kind=%a.kind,"Epoch transaction broadcast");
            return Ok(());
        }
        crate::health::recovered(&self.journal, "settlement").await?;
        if is_batch_job(&a.job) {
            self.journal.members_state(&a.job, "submitted").await?;
            tracing::info!(batch_key=%a.job,tx_hash=%a.hash,nonce=a.nonce,kind=%a.kind,"Batch transaction broadcast");
            return Ok(());
        }
        self.journal.state(&a.job, "submitted").await?;
        tracing::info!(request_id=%a.job,tx_hash=%a.hash,nonce=a.nonce,kind=%a.kind,"Transaction broadcast");
        Ok(())
    }
    async fn lane_started(&self, nonce: i64) -> Result<u64> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let key = format!("nonce_started:{nonce}");
        // Wall-clock age persists across restarts and replacements, even if chain time stalls.
        // Existing journals begin observation on their first upgraded reconciliation.
        sqlx::query("INSERT OR IGNORE INTO meta(key,value) VALUES(?,?)")
            .bind(&key)
            .bind(now.to_string())
            .execute(&self.journal.pool)
            .await?;
        self.journal
            .meta(&key)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Missing nonce health timestamp"))?
            .parse()
            .map_err(Into::into)
    }
    /// Called after independent preparations, so lane degradation does not starve API work.
    pub async fn check_lane_health(&self) -> Result<()> {
        retire_follower_settlement(
            &self.journal,
            self.cfg.role,
            &self.policy()?,
            crate::health::now()?,
        )
        .await?;
        let attempts = self.journal.unresolved().await?;
        let lane = if let Some(first) = attempts.first() {
            Some((first.nonce, self.lane_started(first.nonce).await?))
        } else {
            None
        };
        crate::health::assess(
            &self.journal,
            self.may_send(),
            crate::health::now()?,
            self.cfg.progress_stuck_seconds,
            lane,
            self.cfg.nonce_stuck_seconds,
        )
        .await?;
        Ok(())
    }
    async fn reconcile(&self, head: &Head) -> Result<bool> {
        let attempts = self.journal.unresolved().await?;
        if attempts.is_empty() {
            return Ok(false);
        }
        let latest = attempts.last().unwrap();
        ensure!(
            attempts.iter().all(|a| a.nonce == latest.nonce),
            "Multiple unresolved nonce lanes; refusing to guess"
        );
        if latest.kind.starts_with("epoch") {
            ensure!(
                attempts
                    .iter()
                    .all(|a| a.kind.starts_with("epoch") && a.job == latest.job),
                "Mixed maintenance/game nonce lane"
            );
            return self.reconcile_epoch(&attempts, head).await;
        }
        ensure!(
            attempts.iter().all(|a| !a.kind.starts_with("epoch")),
            "Mixed maintenance/game nonce lane"
        );
        if is_batch_job(&latest.job) {
            ensure!(
                attempts.iter().all(|a| a.job == latest.job),
                "Mixed batch nonce lane"
            );
            return self.reconcile_batch(&attempts, head).await;
        }
        for a in &attempts {
            if let Some(receipt) = self.rpc.receipt(&a.hash).await? {
                if !self.rpc.receipt_is_finalized(&a.hash, &receipt).await? {
                    return Ok(true);
                }
                self.journal
                    .finalized_receipt(
                        &a.hash,
                        quantity(&receipt["blockNumber"])?,
                        receipt["blockHash"]
                            .as_str()
                            .ok_or_else(|| anyhow::anyhow!("Missing receipt block hash"))?,
                        quantity(&receipt["status"])?,
                    )
                    .await?;
                let status = quantity(&receipt["status"])?;
                let r = self.request(a.job.parse()?).await?;
                if r.fulfilled && !r.delivered {
                    crate::audit::callback_failed(&self.journal.pool, &a.job).await?;
                }
                self.journal
                    .resolve_nonce_job(
                        a.nonce,
                        &a.job,
                        resolved_state(&r, head.timestamp, &a.kind, status),
                    )
                    .await?;
                if status == 1
                    && a.kind == "fulfill"
                    && r.fulfilled
                    && let Some(notifier) = &self.telegram
                    && let (Ok(request_id), Ok(tx_hash)) = (a.job.parse(), a.hash.parse())
                {
                    notifier.notify(crate::telegram::Event::Fulfilled {
                        request_id,
                        tx_hash,
                    });
                }
                tracing::info!(request_id=%a.job,tx_hash=%a.hash,status,"Receipt reconciled");
                if status == 1 && a.kind == "fulfill" && r.fulfilled && self.cfg.role.is_follower()
                {
                    tracing::warn!(request_id=%a.job,tx_hash=%a.hash,"Follower served request {}",a.job);
                }
                if let Some(notifier) = &self.discord
                    && let (Ok(request_id), Ok(tx_hash)) = (a.job.parse(), a.hash.parse())
                    && let Some(proof) = crate::discord::ProofAccepted::from_receipt(
                        status, &a.kind, request_id, tx_hash, &r,
                    )
                {
                    notifier.notify(proof);
                }
                return Ok(false);
            }
        }
        let r = self.request(latest.job.parse()?).await?;
        let terminal_state = terminal(&r, head.timestamp);
        let chain_nonce = self.rpc.nonce(self.tx_key.address(), "finalized").await?;
        if chain_nonce > latest.nonce as u64 {
            // A missing receipt is not failure. Only a known terminal contract state can resolve it safely.
            if let Some(state) = terminal_state {
                if r.fulfilled && !r.delivered {
                    crate::audit::callback_failed(&self.journal.pool, &latest.job).await?;
                }
                self.journal
                    .resolve_nonce_job(latest.nonce, &latest.job, state)
                    .await?;
                return Ok(false);
            }
            if awaiting_receipt_visibility(head.timestamp, latest) {
                tracing::debug!(
                    nonce = latest.nonce,
                    "Consumed nonce awaits a visible receipt"
                );
                return Ok(true);
            }
            bail!(
                "Nonce consumed but no receipt or terminal request found; dedicated-wallet conflict needs inspection"
            );
        }
        if !self.cfg.send {
            return Ok(true);
        }
        if terminal_state.is_some() && latest.kind != "cancel" {
            // Cancel the NONCE with a zero-value self transaction, never retry expired randomness.
            let tip = self.priority_fee().await;
            let (fee, priority) = replacement_fees(
                latest.priority.parse()?,
                latest.fee.parse()?,
                head.base_fee,
                tip,
            )?;
            if self
                .replace_within_budget(
                    &latest.job,
                    TxPlan {
                        nonce: latest.nonce.try_into()?,
                        gas: 21000,
                        fee,
                        priority,
                        payload: "0x".into(),
                        kind: "cancel".into(),
                    },
                    head.timestamp,
                )
                .await?
            {
                self.broadcast_latest(head.timestamp).await?;
            }
            return Ok(true);
        }
        if head.timestamp.saturating_sub(latest.created as u64) >= 10
            && attempts.iter().filter(|a| a.kind == latest.kind).count() < 4
            && (latest.kind == "cancel" || head.timestamp + self.cfg.margin < r.deadline)
        {
            let tip = self.priority_fee().await;
            let (fee, priority) = replacement_fees(
                latest.priority.parse()?,
                latest.fee.parse()?,
                head.base_fee,
                tip,
            )?;
            let replaced = self
                .replace_within_budget(
                    &latest.job,
                    TxPlan {
                        nonce: latest.nonce.try_into()?,
                        gas: latest.gas.try_into()?,
                        fee,
                        priority,
                        payload: latest.payload.clone(),
                        kind: latest.kind.clone(),
                    },
                    head.timestamp,
                )
                .await?;
            if replaced || head.timestamp.saturating_sub(latest.broadcast as u64) >= 2 {
                // broadcast_latest independently forbids late or terminal fulfillments.
                self.broadcast_latest(head.timestamp).await?;
            }
        } else if head.timestamp.saturating_sub(latest.broadcast as u64) >= 2 {
            self.broadcast_latest(head.timestamp).await?;
        }
        Ok(true)
    }
    /// The single-request reconciliation applied to every member of one batch nonce. Chain
    /// state is read per member; the journal resolves all of them, or none, with the nonce.
    async fn reconcile_batch(&self, attempts: &[Attempt], head: &Head) -> Result<bool> {
        let latest = attempts
            .last()
            .ok_or_else(|| anyhow::anyhow!("Missing batch attempt"))?;
        let key = latest.job.as_str();
        let members = self.journal.batch_members(key).await?;
        ensure!(!members.is_empty(), "Batch members missing from journal");
        for a in attempts {
            if let Some(receipt) = self.rpc.receipt(&a.hash).await? {
                if !self.rpc.receipt_is_finalized(&a.hash, &receipt).await? {
                    return Ok(true);
                }
                self.journal
                    .finalized_receipt(
                        &a.hash,
                        quantity(&receipt["blockNumber"])?,
                        receipt["blockHash"]
                            .as_str()
                            .ok_or_else(|| anyhow::anyhow!("Missing receipt block hash"))?,
                        quantity(&receipt["status"])?,
                    )
                    .await?;
                let status = quantity(&receipt["status"])?;
                let requests = self.member_requests(&members).await?;
                let mut states = Vec::with_capacity(members.len());
                for (id, r) in members.iter().zip(&requests) {
                    if r.fulfilled && !r.delivered {
                        crate::audit::callback_failed(&self.journal.pool, id).await?;
                    }
                    states.push((
                        id.clone(),
                        batch_member_state(r, head.timestamp, &a.kind, status),
                    ));
                }
                // The journal returns a reverted batch's live members to `prepared`, out of later
                // batches and due at once, in the same commit that resolves the nonce.
                self.journal
                    .resolve_nonce_batch(a.nonce, key, &states)
                    .await?;
                let resend = states
                    .iter()
                    .filter(|(_, state)| *state == "prepared")
                    .count();
                if resend > 0 {
                    self.notify_error(crate::telegram::ErrorClass::TransactionSubmission);
                    tracing::warn!(batch_key=%key,tx_hash=%a.hash,members=members.len(),resend,"Batch reverted on chain; resending its live members one at a time");
                }
                // Notifications name only the members this receipt served. A member already
                // fulfilled elsewhere is skipped on chain and has no RandomnessFulfilled log here.
                let served = fulfilled_in_receipt(&receipt, self.cfg.coordinator)?;
                let mut notified = 0usize;
                if status == 1 && a.kind == "fulfill_batch" {
                    for (id, r) in members.iter().zip(&requests) {
                        let (Ok(request_id), Ok(tx_hash)) =
                            (id.parse::<U256>(), a.hash.parse::<B256>())
                        else {
                            continue;
                        };
                        if !r.fulfilled || !served.contains(&request_id) {
                            continue;
                        }
                        notified += 1;
                        if let Some(notifier) = &self.telegram {
                            notifier.notify(crate::telegram::Event::Fulfilled {
                                request_id,
                                tx_hash,
                            });
                        }
                        if let Some(notifier) = &self.discord
                            && let Some(proof) = crate::discord::ProofAccepted::from_receipt(
                                status, &a.kind, request_id, tx_hash, r,
                            )
                        {
                            notifier.notify(proof);
                        }
                    }
                }
                tracing::info!(batch_key=%key,tx_hash=%a.hash,status,members=members.len(),served=served.len(),notified,"Batch receipt reconciled");
                if notified > 0 && self.cfg.role.is_follower() {
                    tracing::warn!(batch_key=%key,tx_hash=%a.hash,served=notified,"Follower served {notified} requests in one batch");
                }
                return Ok(false);
            }
        }
        let requests = self.member_requests(&members).await?;
        let all_terminal = requests
            .iter()
            .all(|r| terminal(r, head.timestamp).is_some());
        let any_timely = requests
            .iter()
            .any(|r| timely(r, head.timestamp, self.cfg.margin));
        let chain_nonce = self.rpc.nonce(self.tx_key.address(), "finalized").await?;
        if chain_nonce > latest.nonce as u64 {
            // A missing receipt is not failure. Only known terminal contract state for every
            // member can resolve the nonce safely; one live member leaves it for inspection.
            if all_terminal {
                let mut states = Vec::with_capacity(members.len());
                for (id, r) in members.iter().zip(&requests) {
                    if r.fulfilled && !r.delivered {
                        crate::audit::callback_failed(&self.journal.pool, id).await?;
                    }
                    let state = terminal(r, head.timestamp).ok_or_else(|| {
                        anyhow::anyhow!("Batch member state changed under reconciliation")
                    })?;
                    states.push((id.clone(), state));
                }
                self.journal
                    .resolve_nonce_batch(latest.nonce, key, &states)
                    .await?;
                return Ok(false);
            }
            if awaiting_receipt_visibility(head.timestamp, latest) {
                tracing::debug!(
                    nonce = latest.nonce,
                    "Consumed batch nonce awaits a visible receipt"
                );
                return Ok(true);
            }
            bail!(
                "Batch nonce consumed but no receipt or terminal state for every member; dedicated-wallet conflict needs inspection"
            );
        }
        if !self.cfg.send {
            return Ok(true);
        }
        if all_terminal && latest.kind != "cancel" {
            // Cancel the NONCE with a zero-value self transaction; nothing in this batch can be served.
            let tip = self.priority_fee().await;
            let (fee, priority) = replacement_fees(
                latest.priority.parse()?,
                latest.fee.parse()?,
                head.base_fee,
                tip,
            )?;
            if self
                .replace_within_budget(
                    key,
                    TxPlan {
                        nonce: latest.nonce.try_into()?,
                        gas: 21000,
                        fee,
                        priority,
                        payload: "0x".into(),
                        kind: "cancel".into(),
                    },
                    head.timestamp,
                )
                .await?
            {
                self.broadcast_latest(head.timestamp).await?;
            }
            return Ok(true);
        }
        if head.timestamp.saturating_sub(latest.created as u64) >= 10
            && attempts.iter().filter(|a| a.kind == latest.kind).count() < 4
            && (latest.kind == "cancel" || any_timely)
        {
            let tip = self.priority_fee().await;
            let (fee, priority) = replacement_fees(
                latest.priority.parse()?,
                latest.fee.parse()?,
                head.base_fee,
                tip,
            )?;
            let replaced = self
                .replace_within_budget(
                    key,
                    TxPlan {
                        nonce: latest.nonce.try_into()?,
                        gas: latest.gas.try_into()?,
                        fee,
                        priority,
                        payload: latest.payload.clone(),
                        kind: latest.kind.clone(),
                    },
                    head.timestamp,
                )
                .await?;
            if replaced || head.timestamp.saturating_sub(latest.broadcast as u64) >= 2 {
                // broadcast_latest independently forbids a batch with no live, timely member.
                self.broadcast_latest(head.timestamp).await?;
            }
        } else if head.timestamp.saturating_sub(latest.broadcast as u64) >= 2 {
            self.broadcast_latest(head.timestamp).await?;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn follower(joined: bool) -> SendPolicy {
        SendPolicy {
            joined,
            tail_first: true,
            rank: 0,
            lanes: 1,
        }
    }
    async fn assessed(j: &Journal, at: u64) -> crate::health::Status {
        crate::health::assess(j, true, at, 20, None, 120)
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn a_follower_that_sees_the_primary_serve_its_jobs_stays_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(&dir.path().join("follower.sqlite"), "scope")
            .await
            .unwrap();
        let waiting = follower(false);
        // Requests open at 1000..1004 with 60 s deadlines; the follower keeps looking at them for 30 s while the
        // primary publishes and serves, as it does before every takeover, and its epoch view often lags.
        for id in 1..=5u64 {
            j.discovered(&id.to_string(), i64::try_from(1060 + id).unwrap(), "6")
                .await
                .unwrap();
        }
        for second in 1000..1030u64 {
            for id in 1..=5u64 {
                note_preparation_attempt(
                    &j,
                    true,
                    &waiting,
                    &id.to_string(),
                    1060 + id,
                    second,
                    second,
                )
                .await
                .unwrap();
            }
            assert!(assessed(&j, second).await.healthy, "at {second}");
        }
        // A request the primary leaves until the safety age is this follower's too, until the primary serves it.
        note_preparation_attempt(&j, true, &waiting, "5", 1065, 1046, 1046)
            .await
            .unwrap();
        for id in 1..=5 {
            j.state(&id.to_string(), "served").await.unwrap();
        }
        // Long after, with nothing open and no proof ever journaled by this follower, it is still healthy.
        for at in [1070, 1100, 5000] {
            assert!(assessed(&j, at).await.healthy);
        }
        j.pool.close().await;
    }
    #[tokio::test]
    async fn a_joined_follower_that_cannot_prepare_its_lane_is_stalled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("joined.sqlite");
        let j = Journal::open(&path, "scope").await.unwrap();
        j.discovered("7", 1060, "8").await.unwrap();
        // Seen before joining: readiness only, no wait yet.
        note_preparation_attempt(&j, true, &follower(false), "7", 1060, 1000, 1000)
            .await
            .unwrap();
        // Joined at 1010; every later pass fails to journal a proof.
        for second in 1010..=1030u64 {
            note_preparation_attempt(&j, true, &follower(true), "7", 1060, second, second)
                .await
                .unwrap();
        }
        assert!(assessed(&j, 1029).await.healthy);
        let stalled = assessed(&j, 1030).await;
        assert_eq!(stalled.faults, vec!["preparation_stalled"]);
        // A restart keeps the evidence, and without sending there is no fault to report.
        j.pool.close().await;
        let j = Journal::open(&path, "scope").await.unwrap();
        assert!(!assessed(&j, 1031).await.healthy);
        assert!(
            crate::health::assess(&j, false, 1031, 20, None, 120)
                .await
                .unwrap()
                .healthy
        );
        // Leaving the queue does not reset or drop the wait; the primary serving the request does.
        note_preparation_attempt(&j, true, &follower(false), "7", 1060, 1035, 1035)
            .await
            .unwrap();
        assert!(!assessed(&j, 1035).await.healthy);
        j.state("7", "served").await.unwrap();
        assert!(assessed(&j, 1036).await.healthy);
        j.pool.close().await;
    }
    #[tokio::test]
    async fn a_primary_with_one_stuck_job_is_stalled_while_others_are_prepared_and_served() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(&dir.path().join("primary.sqlite"), "scope")
            .await
            .unwrap();
        let primary = SendPolicy::PRIMARY;
        j.discovered("1", 1060, "2").await.unwrap();
        // Request 1 never gets a proof. A new request arrives every two seconds and is prepared and served at once,
        // each proof arriving before request 1 has waited the full limit: no clear may restart request 1's wait.
        for second in (1000..1040u64).step_by(2) {
            note_preparation_attempt(&j, true, &primary, "1", 1060, second, second)
                .await
                .unwrap();
            let other = (second - 998).to_string();
            j.discovered(&other, i64::try_from(second + 60).unwrap(), "0")
                .await
                .unwrap();
            note_preparation_attempt(&j, true, &primary, &other, second + 60, second, second)
                .await
                .unwrap();
            j.prepared(&other, "proof", "call").await.unwrap();
            crate::health::preparation_progress(&j, &other, second + 1)
                .await
                .unwrap();
            j.state(&other, "served").await.unwrap();
            let status = assessed(&j, second + 1).await;
            assert_eq!(status.healthy, second + 1 < 1020, "at {}", second + 1);
        }
        // Expiry keeps the evidence: request 1 waited 60 s, owned, and expired without a proof.
        j.expire_unstarted(1061).await.unwrap();
        assert_eq!(assessed(&j, 1070).await.faults, vec!["preparation_stalled"]);
        // A proof journaled after request 1's deadline is the progress that ends it.
        j.discovered("99", 1130, "100").await.unwrap();
        crate::health::preparation_progress(&j, "99", 1075)
            .await
            .unwrap();
        assert!(assessed(&j, 1076).await.healthy);
        // Without later progress, an expired stuck request is reported for a bounded time only.
        j.discovered("50", 2060, "51").await.unwrap();
        note_preparation_attempt(&j, true, &primary, "50", 2060, 2000, 2000)
            .await
            .unwrap();
        j.expire_unstarted(2061).await.unwrap();
        assert!(!assessed(&j, 2061).await.healthy);
        let retention = crate::health::EXPIRED_PREPARATION_EVIDENCE_SECONDS;
        assert!(!assessed(&j, 2060 + retention).await.healthy);
        assert!(assessed(&j, 2061 + retention).await.healthy);
        j.pool.close().await;
    }
    #[tokio::test]
    async fn a_follower_settlement_observation_lasts_only_while_it_owns_something_to_send() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(&dir.path().join("settle.sqlite"), "scope")
            .await
            .unwrap();
        let role = Role::Follower(crate::config::FollowerPlan {
            delay: 20,
            queue_join: 150,
            liveness: 10,
            rank: 0,
            lanes: 1,
        });
        j.discovered("1", 1060, "2").await.unwrap();
        j.prepared("1", "proof", "call").await.unwrap();
        // Joined, a deferral on its own prepared request is kept and becomes the fault.
        crate::health::blocked(&j, "settlement", 1000)
            .await
            .unwrap();
        retire_follower_settlement(&j, role, &follower(true), 1010)
            .await
            .unwrap();
        retire_follower_settlement(&j, role, &follower(true), 1020)
            .await
            .unwrap();
        assert_eq!(assessed(&j, 1020).await.faults, vec!["settlement_stalled"]);
        // Out of the queue with the request not yet at its safety age, nothing is this follower's to send.
        retire_follower_settlement(&j, role, &follower(false), 1030)
            .await
            .unwrap();
        assert!(assessed(&j, 1030).await.healthy);
        // At the safety age it is every node's, so the observation stays.
        crate::health::blocked(&j, "settlement", 1041)
            .await
            .unwrap();
        retire_follower_settlement(&j, role, &follower(false), 1041)
            .await
            .unwrap();
        assert!(!assessed(&j, 1061).await.healthy);
        // A primary keeps its own observation until its next broadcast, work or no work.
        j.state("1", "served").await.unwrap();
        retire_follower_settlement(&j, Role::Primary, &SendPolicy::PRIMARY, 1062)
            .await
            .unwrap();
        assert!(!assessed(&j, 1062).await.healthy);
        retire_follower_settlement(&j, role, &follower(false), 1063)
            .await
            .unwrap();
        assert!(assessed(&j, 1063).await.healthy);
        // Timeouts count only over owned work.
        let jobs = [j.job("1").await.unwrap().unwrap()];
        assert!(!owns_any(&follower(false), &jobs, 1000));
        assert!(owns_any(&follower(true), &jobs, 1000));
        assert!(owns_any(&follower(false), &jobs, 1040));
        assert!(owns_any(&SendPolicy::PRIMARY, &jobs, 1000));
        j.pool.close().await;
    }
    #[tokio::test]
    async fn a_stale_marker_from_an_earlier_release_clears_itself() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.sqlite");
        let j = Journal::open(&path, "scope").await.unwrap();
        // The earlier release's single marker, left behind days ago by requests the primary served.
        crate::health::blocked(&j, "preparation", 100)
            .await
            .unwrap();
        j.discovered("1", 1060, "2").await.unwrap();
        j.state("1", "served").await.unwrap();
        // An open request this follower is not responsible for does not keep it either.
        j.discovered("2", 500_060, "3").await.unwrap();
        note_preparation_attempt(&j, true, &follower(false), "2", 500_060, 500_000, 500_000)
            .await
            .unwrap();
        j.pool.close().await;
        let j = Journal::open(&path, "scope").await.unwrap();
        assert!(assessed(&j, 500_001).await.healthy);
        assert!(
            j.meta("health:blocked:preparation")
                .await
                .unwrap()
                .is_none()
        );
        // On a primary with open work the marker still counts until that work stops waiting, then it goes too.
        crate::health::blocked(&j, "preparation", 100)
            .await
            .unwrap();
        note_preparation_attempt(
            &j,
            true,
            &SendPolicy::PRIMARY,
            "2",
            500_060,
            500_002,
            500_002,
        )
        .await
        .unwrap();
        assert_eq!(
            assessed(&j, 500_003).await.faults,
            vec!["preparation_stalled"]
        );
        j.state("2", "served").await.unwrap();
        assert!(assessed(&j, 500_004).await.healthy);
        assert!(
            j.meta("health:blocked:preparation")
                .await
                .unwrap()
                .is_none()
        );
        j.pool.close().await;
    }
    #[test]
    fn epoch_boundary_does_not_expire_immutable_packet_but_signed_freshness_does() {
        let api = ApiProof {
            timestamp: U256::from(100),
            data: Bytes::new(),
            signature: Bytes::new(),
        };
        let work = crate::epoch::Work {
            key: "epoch".into(),
            epoch: 1,
            start: 200,
            state: "prepared".into(),
            api: Some(serde_json::to_string(&api).unwrap()),
            fallback: 0,
            sources: 4,
            attempts: 1,
            retry_at: 0,
            last_error: None,
        };
        assert_eq!(
            epoch_terminal(
                &work,
                &Head {
                    hash: B256::ZERO,
                    number: 450,
                    timestamp: 340,
                    base_fee: 1
                }
            )
            .unwrap(),
            None
        );
        assert_eq!(
            epoch_terminal(
                &work,
                &Head {
                    hash: B256::ZERO,
                    number: 450,
                    timestamp: 341,
                    base_fee: 1
                }
            )
            .unwrap(),
            Some("blocked")
        );
    }

    #[test]
    fn budget_deferrals_name_the_exceeded_cap_and_are_distinguishable() {
        let plan = |kind: &str, fee: u128, gas: u64| TxPlan {
            nonce: 0,
            gas,
            fee,
            priority: 1_000_000_000,
            payload: "0x".into(),
            kind: kind.into(),
        };
        let (fulfill, cancel, cost) = (100, 113, 113 * 21000);
        assert_eq!(
            over_budget(&plan("fulfill", 100, 2000), fulfill, cancel, cost),
            None
        );
        assert_eq!(
            over_budget(&plan("fulfill", 101, 21000), fulfill, cancel, cost),
            Some(FeeBudget {
                cap: FeeCap::MaxFeePerGas,
                required: 101,
                limit: 100
            })
        );
        // Cancellations and epoch cancellations use the recovery cap; epochs use the fulfillment cap.
        assert_eq!(
            over_budget(&plan("cancel", 113, 21000), fulfill, cancel, cost),
            None
        );
        assert_eq!(
            over_budget(&plan("epoch_cancel", 114, 21000), fulfill, cancel, cost).map(|e| e.cap),
            Some(FeeCap::CancelMaxFeePerGas)
        );
        assert_eq!(
            over_budget(&plan("epoch", 101, 21000), fulfill, cancel, cost).map(|e| e.cap),
            Some(FeeCap::MaxFeePerGas)
        );
        assert_eq!(
            over_budget(&plan("fulfill", 100, 23731), fulfill, cancel, cost),
            Some(FeeBudget {
                cap: FeeCap::MaxTxCost,
                required: 2_373_100,
                limit: cost
            })
        );
        assert_eq!(
            over_budget(
                &plan("fulfill", u128::MAX, u64::MAX),
                u128::MAX,
                u128::MAX,
                u128::MAX
            )
            .map(|e| (e.cap, e.required)),
            Some((FeeCap::MaxTxCost, u128::MAX))
        );
        let deferred: anyhow::Error = SendDeferred::new(anyhow::anyhow!("stale nonce")).into();
        assert!(
            deferred
                .downcast_ref::<SendDeferred>()
                .is_some_and(|d| d.budget.is_none())
        );
        let exceeded = FeeBudget {
            cap: FeeCap::MaxGas,
            required: 6_000_001,
            limit: 6_000_000,
        };
        let deferred: anyhow::Error = SendDeferred::budget(exceeded).into();
        assert_eq!(
            deferred
                .downcast_ref::<SendDeferred>()
                .and_then(|d| d.budget),
            Some(exceeded)
        );
        assert!(deferred.to_string().contains("MAX_GAS=6000000"));
    }
    #[test]
    fn fee_rule_and_headroom_follow_twice_base_fee_plus_priority() {
        const GWEI: u128 = 1_000_000_000;
        assert_eq!(required_fee(251 * GWEI, GWEI).unwrap(), 503_000_000_000);
        assert_eq!(required_fee(251 * GWEI, 22 * GWEI).unwrap(), 524 * GWEI);
        assert!(required_fee(u128::MAX, GWEI).is_err());
        assert_eq!(fee_headroom(251 * GWEI, GWEI, 2_000 * GWEI), None);
        // Fee coverage: 405k gas at 176 gwei + 5 gwei tip costs 0.0733 USDC.
        let cost = expected_cost(176 * GWEI, 5 * GWEI, 405_000);
        assert_eq!(cost, 73_305_000_000_000_000);
        assert_eq!(uncovered(cost, 352_000_000_000_000_000, 10_000), None);
        assert_eq!(
            uncovered(cost, 50_000_000_000_000_000, 10_000),
            Some(FeeBudget {
                cap: FeeCap::FeeCoverage,
                required: cost,
                limit: 50_000_000_000_000_000
            })
        );
        // Half coverage tolerates a bounded loss; 0 disables the rule entirely.
        assert_eq!(uncovered(cost, 40_000_000_000_000_000, 5_000), None);
        assert_eq!(uncovered(cost, 0, 0), None);
        assert_eq!(uncovered(u128::MAX, 0, 100_000).map(|e| e.limit), Some(0));
        assert!(
            uncovered(cost, 1, 10_000)
                .unwrap()
                .to_string()
                .contains("under FEE_COVERAGE_BPS")
        );
        // A missing or implausible fee history is clamped; it can never exceed the maximum tip.
        assert_eq!(bounded_priority(None, GWEI, 50 * GWEI), GWEI);
        assert_eq!(bounded_priority(Some(0), GWEI, 50 * GWEI), GWEI);
        assert_eq!(bounded_priority(Some(5 * GWEI), GWEI, 50 * GWEI), 5 * GWEI);
        assert_eq!(
            bounded_priority(Some(u128::MAX), GWEI, 50 * GWEI),
            50 * GWEI
        );
        // Replacements outbid the previous attempt by 12.5% and follow a higher market tip.
        assert_eq!(
            replacement_fees(GWEI, 41 * GWEI, 20 * GWEI, 0).unwrap(),
            (bump(41 * GWEI).unwrap(), bump(GWEI).unwrap())
        );
        let (fee, priority) = replacement_fees(GWEI, 41 * GWEI, 20 * GWEI, 22 * GWEI).unwrap();
        assert_eq!((fee, priority), (62 * GWEI, 22 * GWEI));
        assert_eq!(
            fee_headroom(176_000_000_000, GWEI, 100_000_000_000),
            Some(FeeBudget {
                cap: FeeCap::MaxFeePerGas,
                required: 353_000_000_000,
                limit: 100_000_000_000
            })
        );
        assert_eq!(fee_headroom(u128::MAX, 0, u128::MAX), None);
        assert_eq!(
            fee_headroom(u128::MAX, 0, u128::MAX - 1).map(|e| e.required),
            Some(u128::MAX)
        );
    }

    #[test]
    fn discovery_defers_only_empty_nonadvancing_pages() {
        assert!(!discovery_page_advances(20, 19, &[]).unwrap());
        assert!(!discovery_page_advances(20, 20, &[]).unwrap());
        assert!(discovery_page_advances(20, 21, &[U256::from(20)]).unwrap());
        assert!(discovery_page_advances(20, 276, &[]).unwrap());
        for (next, ids) in [
            (19, vec![20]),
            (20, vec![20]),
            (277, vec![]),
            (22, vec![19]),
            (22, vec![22]),
            (23, vec![21, 20]),
            (22, vec![20, 20]),
        ] {
            let ids = ids.into_iter().map(U256::from).collect::<Vec<_>>();
            assert!(discovery_page_advances(20, next, &ids).is_err());
        }
        assert!(discovery_page_advances(20, 21, &[U256::MAX]).is_err());
    }
    #[test]
    fn prepared_queue_prefers_earliest_deadline_and_numeric_ties() {
        let job = |id: &str, deadline: i64, state: &str| Job {
            id: id.into(),
            deadline,
            state: state.into(),
            proof: None,
            call: Some("0x01".into()),
        };
        let queue = || {
            vec![
                job("10", 120, "prepared"),
                job("2", 120, "prepared"),
                job("9", 110, "prepared"),
                job("1", 100, "signed"),
            ]
        };
        let ordered = prepared_in_order(queue(), false).unwrap();
        assert_eq!(
            ordered.into_iter().map(|j| j.id).collect::<Vec<_>>(),
            ["9", "2", "10"]
        );
        // A follower takes the same page from its newest end, so the two fronts meet in the middle.
        let tail = prepared_in_order(queue(), true).unwrap();
        assert_eq!(
            tail.into_iter().map(|j| j.id).collect::<Vec<_>>(),
            ["10", "2", "9"]
        );
        assert!(prepared_in_order(vec![job("bad", 100, "prepared")], false).is_err());
    }
    #[test]
    fn preparation_slots_exclude_prepared_signed_and_expiring_jobs() {
        let eligible = Job {
            id: "9".into(),
            deadline: 160,
            state: "pending".into(),
            proof: None,
            call: None,
        };
        let mut jobs = Vec::new();
        for state in ["prepared", "signed", "submitted", "blocked"] {
            let mut job = eligible.clone();
            job.state = state.into();
            jobs.push(job);
        }
        let mut with_call = eligible.clone();
        with_call.call = Some("0x01".into());
        jobs.push(with_call);
        let mut expiring = eligible.clone();
        expiring.deadline = 105;
        jobs.push(expiring);
        jobs.push(eligible);
        let ids: Vec<_> = jobs
            .iter()
            .filter(|job| preparation_candidate(job, 100, 5))
            .take(1)
            .map(|job| job.id.as_str())
            .collect();
        assert_eq!(ids, ["9"]);
    }

    #[tokio::test]
    async fn live_demand_on_blocked_or_stale_epoch_work_is_a_stall_until_it_expires() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("epochs.sqlite"), "scope")
            .await
            .unwrap();
        let (registry, catalog) = (
            alloy_primitives::Address::repeat_byte(1),
            B256::repeat_byte(2),
        );
        let packet = serde_json::to_string(&ApiProof {
            timestamp: U256::from(100),
            data: Bytes::new(),
            signature: Bytes::new(),
        })
        .unwrap();
        // Epoch 1's last source is blocked; epoch 4's selected source is blocked with fallbacks ahead.
        for (epoch, state, api, fallback) in [
            (1, "blocked", None, 3),
            (2, "prepared", Some(packet.as_str()), 0),
            (3, "pending", None, 0),
            (4, "blocked", None, 0),
        ] {
            sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start,state,api,fallback) VALUES(?,?,?,?,200,?,?,?)")
                .bind(format!("epoch:{epoch}")).bind(registry.to_string()).bind(catalog.to_string()).bind(epoch).bind(state).bind(api).bind(fallback)
                .execute(&journal.pool).await.unwrap();
        }
        let head = |timestamp| Head {
            hash: B256::ZERO,
            number: 450,
            timestamp,
            base_fee: 1,
        };
        let pool = &journal.pool;
        let stalled =
            |at| async move { stalled_epoch_demand(pool, registry, catalog, &head(at), 5).await };
        assert_eq!(stalled(100).await.unwrap(), None);
        // Retrying (pending) work, a source awaiting its fallback and a fresh packet with live demand are not stalls.
        journal
            .discovered_epoch("40", 300, "41", Some(4))
            .await
            .unwrap();
        journal
            .discovered_epoch("30", 300, "31", Some(3))
            .await
            .unwrap();
        journal
            .discovered_epoch("20", 300, "21", Some(2))
            .await
            .unwrap();
        assert_eq!(stalled(100).await.unwrap(), None);
        journal
            .discovered_epoch("10", 300, "11", Some(1))
            .await
            .unwrap();
        assert_eq!(
            stalled(100).await.unwrap(),
            Some(("epoch:1".into(), "blocked"))
        );
        // Demand within the send margin of its deadline, or expired, no longer counts.
        assert_eq!(stalled(295).await.unwrap(), None);
        journal.state("10", "expired").await.unwrap();
        assert_eq!(stalled(100).await.unwrap(), None);
        // A saved packet far older than the freshness bound at the head is a stall for its live
        // demand; the exact bound is covered by the epoch_terminal test above.
        journal
            .discovered_epoch("21", 2000, "22", Some(2))
            .await
            .unwrap();
        assert_eq!(stalled(150).await.unwrap(), None);
        assert_eq!(
            stalled(1000).await.unwrap(),
            Some(("epoch:2".into(), "stale_packet"))
        );
        // Another registry's identical epoch numbers are never consulted.
        assert_eq!(
            stalled_epoch_demand(
                &journal.pool,
                alloy_primitives::Address::repeat_byte(9),
                catalog,
                &head(1000),
                5
            )
            .await
            .unwrap(),
            None
        );
        journal.pool.close().await;
    }

    #[tokio::test]
    async fn binary_recovery_skips_expired_history() {
        let calls = std::cell::Cell::new(0);
        let first = live_lower_bound(1001, 100, |id| {
            calls.set(calls.get() + 1);
            std::future::ready(Ok(if id <= 990 { 90 } else { 110 }))
        })
        .await
        .unwrap();
        assert_eq!(first, 991);
        assert!(calls.get() <= 10);
    }
    #[test]
    fn lanes_split_the_tail_by_request_id_and_every_node_honours_the_safety_age() {
        let lanes = |id: &str| lane_of(id, 3);
        assert_eq!(
            [lanes("0"), lanes("1"), lanes("2"), lanes("3")],
            [Some(0), Some(1), Some(2), Some(0)]
        );
        assert_eq!(lane_of("not-a-number", 3), None);
        let follower = SendPolicy {
            joined: true,
            tail_first: true,
            rank: 1,
            lanes: 3,
        };
        // A request created at 1000 has deadline 1060; the safety age opens at 1040.
        assert!(follower.allows("1", 1060, 1000));
        assert!(
            !follower.allows("2", 1060, 1000),
            "another lane's request is left alone"
        );
        assert!(
            follower.allows("2", 1060, 1040),
            "the safety age overrides every lane"
        );
        let waiting = SendPolicy {
            joined: false,
            ..follower
        };
        assert!(!waiting.allows("1", 1060, 1039));
        assert!(waiting.allows("1", 1060, 1040));
        // A primary sends everything, whatever the id.
        for id in ["1", "2", "3"] {
            assert!(SendPolicy::PRIMARY.allows(id, 1060, 1000));
        }
    }
    #[test]
    fn a_follower_joins_only_when_the_queue_is_old_deep_or_orphaned() {
        // The join rule as the tick evaluates it, from the queue this node can see and the primary's liveness.
        let plan = crate::config::FollowerPlan {
            delay: 20,
            queue_join: 150,
            liveness: 10,
            rank: 0,
            lanes: 1,
        };
        let join = |joined: bool, pending: u64, oldest_age: u64, dead: bool| {
            if joined {
                !(pending < FOLLOWER_LEAVE_PENDING
                    && oldest_age < FOLLOWER_LEAVE_AGE_SECONDS
                    && !dead)
            } else {
                oldest_age > plan.delay || pending > plan.queue_join || dead
            }
        };
        // A healthy primary working a young queue is left alone, however deep it is under the join threshold.
        assert!(!join(false, 120, 8, false));
        // The oldest request ages past the join delay, or the queue outgrows the join size, or the primary is dead.
        assert!(join(false, 1, 21, false));
        assert!(join(false, 151, 3, false));
        assert!(join(false, 0, 0, true));
        // Hysteresis: a joined follower stays until the queue is both short and young again.
        assert!(join(true, 40, 3, false));
        assert!(join(true, 10, 12, false));
        assert!(join(true, 10, 3, true));
        assert!(!join(true, 10, 3, false));
    }
    #[test]
    fn request_states_and_margin() {
        let mut r = Request {
            deadline: 100,
            ..Default::default()
        };
        assert_eq!(terminal(&r, 100), None);
        assert_eq!(terminal(&r, 101), Some("expired"));
        assert!(timely(&r, 94, 5));
        assert!(!timely(&r, 95, 5));
        r.fulfilled = true;
        assert_eq!(terminal(&r, 101), Some("served"));
        assert!(!timely(&r, 50, 5));
    }
    #[test]
    fn resolved_state_classifies_single_attempts_and_cancellations() {
        let live = Request {
            deadline: 100,
            ..Default::default()
        };
        let served = Request {
            fulfilled: true,
            ..live.clone()
        };
        let refunded = Request {
            refunded: true,
            ..live.clone()
        };
        for kind in ["fulfill", "cancel"] {
            for status in [0, 1] {
                assert_eq!(resolved_state(&served, 50, kind, status), "served");
                assert_eq!(resolved_state(&refunded, 50, kind, status), "refunded");
                assert_eq!(resolved_state(&live, 101, kind, status), "expired");
            }
            assert_eq!(resolved_state(&live, 50, kind, 0), "blocked");
        }
        assert_eq!(resolved_state(&live, 50, "cancel", 1), "blocked");
        assert_eq!(resolved_state(&live, 50, "fulfill", 1), "inconsistent");
    }
    #[test]
    fn epoch_resolution_keeps_a_cancelled_packet_publishable() {
        for kind in ["epoch", "epoch_cancel"] {
            for status in [0, 1] {
                for state in ["committed", "blocked", "prepared"] {
                    assert_eq!(epoch_resolved_state(Some(state), kind, status), state);
                }
            }
            assert_eq!(epoch_resolved_state(None, kind, 0), "blocked");
        }
        // Demand that arrived after the cancellation was signed is served from the same saved packet.
        assert_eq!(epoch_resolved_state(None, "epoch_cancel", 1), "prepared");
        assert_eq!(epoch_resolved_state(None, "epoch", 1), "inconsistent");
    }
    #[test]
    fn consumed_nonce_waits_briefly_for_a_visible_receipt_then_fails_closed() {
        let attempt = |created: i64, broadcast: i64| Attempt {
            id: 1,
            job: "7".into(),
            nonce: 3,
            hash: "0x".into(),
            raw: "0x".into(),
            kind: "fulfill".into(),
            fee: "1".into(),
            state: "submitted".into(),
            gas: 21000,
            priority: "1".into(),
            payload: "0x".into(),
            created,
            broadcast,
        };
        assert!(awaiting_receipt_visibility(1_000, &attempt(990, 995)));
        assert!(awaiting_receipt_visibility(1_024, &attempt(990, 995)));
        assert!(!awaiting_receipt_visibility(1_025, &attempt(990, 995)));
        // An attempt never broadcast still ages from its signing time.
        assert!(!awaiting_receipt_visibility(1_100, &attempt(990, 0)));
    }
    #[test]
    fn a_reverted_batch_resends_its_live_members_and_only_their_own_revert_blocks_them() {
        let live = Request {
            deadline: 100,
            ..Default::default()
        };
        let served = Request {
            fulfilled: true,
            ..live.clone()
        };
        let refunded = Request {
            refunded: true,
            ..live.clone()
        };
        // Chain state wins whatever the batch did.
        for kind in ["fulfill_batch", "cancel"] {
            for status in [0, 1] {
                assert_eq!(batch_member_state(&served, 50, kind, status), "served");
                assert_eq!(batch_member_state(&refunded, 50, kind, status), "refunded");
                assert_eq!(batch_member_state(&live, 101, kind, status), "expired");
            }
        }
        // A live member of a reverted batch is resent, not blocked.
        assert_eq!(
            batch_member_state(&live, 50, "fulfill_batch", 0),
            "prepared"
        );
        // A landed batch that left a member live is still inconsistent, a cancelled lane still blocks.
        assert_eq!(
            batch_member_state(&live, 50, "fulfill_batch", 1),
            "inconsistent"
        );
        assert_eq!(batch_member_state(&live, 50, "cancel", 1), "blocked");
        assert_eq!(batch_member_state(&live, 50, "cancel", 0), "blocked");
        // The request's own single attempt reverting is what fails it permanently.
        assert_eq!(resolved_state(&live, 50, "fulfill", 0), "blocked");
    }
    /// A gas model of fulfillRandomnessBatch: each member spends `before` gas up to its callback,
    /// must pass the coordinator's check `gasleft() >= limit + limit / 63 + CALLBACK_RESERVE`
    /// (InsufficientCallbackGas reverts the whole batch), burns at most its limit in the callback
    /// and spends `after` once it returns. Whether the batch completes with `gas`.
    fn batch_lands(gas: u64, members: &[(u64, u32, u64, u64)]) -> bool {
        let mut left = gas;
        for &(before, limit, burn, after) in members {
            let Some(at_check) = left.checked_sub(before) else {
                return false;
            };
            if at_check < callback_budget(limit) {
                return false;
            }
            let Some(rest) = (at_check - burn.min(u64::from(limit))).checked_sub(after) else {
                return false;
            };
            left = rest;
        }
        true
    }
    /// What eth_estimateGas returns for the model: the least gas with which it completes.
    fn estimated(members: &[(u64, u32, u64, u64)]) -> u64 {
        let (mut low, mut high) = (0u64, 100_000_000u64);
        while low < high {
            let mid = low + (high - low) / 2;
            if batch_lands(mid, members) {
                high = mid
            } else {
                low = mid + 1
            }
        }
        low
    }
    #[test]
    fn a_callback_that_burns_its_budget_only_on_chain_cannot_abort_a_full_budget_batch() {
        // The batch-abort issue: the draw, then two helpers with 1,000,000-gas callbacks that are cheap in
        // the unpriced simulation and, once the draw has lost, burn their whole limit on chain.
        let (before, after) = (180_000, 40_000);
        let simulated = [
            (before, 100_000, 25_000, after),
            (before, 1_000_000, 2_000, after),
            (before, 1_000_000, 2_000, after),
        ];
        let on_chain = [
            (before, 100_000, 25_000, after),
            (before, 1_000_000, 1_000_000, after),
            (before, 1_000_000, 1_000_000, after),
        ];
        let estimate = estimated(&simulated);
        let limits = [100_000, 1_000_000, 1_000_000];
        // The previous sizing, estimate * 1.2 + 50,000: the second helper's check fails on chain.
        assert!(!batch_lands(estimate * 12 / 10 + 50_000, &on_chain));
        // Every member's full budget reserved: the batch lands with the draw's result in it.
        assert!(batch_lands(
            fulfillment_gas(estimate, &limits).unwrap(),
            &on_chain
        ));
        // The same holds for any mix of limits and of simulated and on-chain burns: no member can
        // starve a later one.
        for count in 1..=16usize {
            for limit in [30_000u32, 100_000, 250_000, 1_000_000] {
                for simulated_burn in [0, 1_000, u64::from(limit) / 2, u64::from(limit)] {
                    let limits = vec![limit; count];
                    let simulated: Vec<_> = limits
                        .iter()
                        .map(|&limit| (before, limit, simulated_burn, after))
                        .collect();
                    let on_chain: Vec<_> = limits
                        .iter()
                        .map(|&limit| (before, limit, u64::from(limit), after))
                        .collect();
                    let gas = fulfillment_gas(estimated(&simulated), &limits).unwrap();
                    assert!(
                        batch_lands(gas, &on_chain),
                        "{count} x {limit} burning {simulated_burn} in simulation"
                    );
                }
            }
        }
    }
    #[test]
    fn fulfillment_gas_reserves_every_callback_budget_above_the_padded_estimate() {
        assert_eq!(callback_budget(100_000), 100_000 + 1_587 + 140_000);
        assert_eq!(callback_budget(1_000_000), 1_000_000 + 15_873 + 140_000);
        // A single: the budget on top of the estimate, never below the old padding.
        assert_eq!(
            fulfillment_gas(300_000, &[100_000]).unwrap(),
            300_000 + 241_587
        );
        assert_eq!(fulfillment_gas(2_000_000, &[30_000]).unwrap(), 2_450_000);
        assert_eq!(fulfillment_gas(1_000_000, &[]).unwrap(), 1_250_000);
        // Sixteen 100,000-gas members at the Arc testnet batch estimate: about 8.2M.
        assert_eq!(
            fulfillment_gas(4_300_000, &[100_000; 16]).unwrap(),
            4_300_000 + 16 * 241_587
        );
        // The worst single request the coordinator accepts (a 1,000,000-gas callback) fits 3M.
        let worst_single_estimate = 21_000 + 7_300 + 180_000 + callback_budget(1_000_000);
        assert!(fulfillment_gas(worst_single_estimate, &[1_000_000]).unwrap() <= 3_000_000);
        assert!(fulfillment_gas(u64::MAX, &[1]).is_err());
        assert!(fulfillment_gas(u64::MAX / 10, &[]).is_err());
    }
    #[test]
    fn an_over_cap_batch_shrinks_to_the_members_that_fit_with_full_budgets() {
        // MAX_TX_COST_WEI binds only when it allows less gas than MAX_GAS at this price.
        assert_eq!(
            gas_cap(6_000_000, 4 * 10u128.pow(18), 41_000_000_000),
            6_000_000
        );
        assert_eq!(
            gas_cap(10_000_000, 4 * 10u128.pow(18), 503_000_000_000),
            7_952_286
        );
        assert_eq!(gas_cap(6_000_000, 4 * 10u128.pow(18), 0), 6_000_000);
        assert_eq!(gas_cap(6_000_000, 0, 1), 0);
        // Sixteen 100,000-gas members estimated at 4.3M: eleven fit 6M, all sixteen fit 10M.
        let limits = [100_000u32; 16];
        assert_eq!(members_within(4_300_000, &limits, 6_000_000), 11);
        assert_eq!(members_within(4_300_000, &limits, 10_000_000), 16);
        // 1,000,000-gas callbacks: four fit 6M.
        assert_eq!(members_within(4_300_000, &[1_000_000; 16], 6_000_000), 4);
        // The prediction keeps the order: a heavy member in front limits the prefix.
        let mut mixed = [100_000u32; 8];
        mixed[1] = 1_000_000;
        assert_eq!(members_within(2_200_000, &mixed, 3_000_000), 4);
        assert_eq!(members_within(2_200_000, &mixed, 1_000_000), 1);
        // Nothing fits, or nothing to fit.
        assert_eq!(members_within(4_300_000, &limits, 400_000), 0);
        assert_eq!(members_within(0, &[], 6_000_000), 0);
        // Whatever fits by the prediction is really within the cap at the predicted estimate.
        for cap in (1_000_000..12_000_000).step_by(250_000) {
            let fit = members_within(4_300_000, &limits, cap);
            if fit > 0 {
                let share = 4_300_000u64.div_ceil(16);
                assert!(fulfillment_gas(share * fit as u64, &limits[..fit]).unwrap() <= cap);
            }
        }
    }
    #[test]
    fn requests_left_out_of_batches_that_this_node_owns_lead_the_send_order() {
        let job = |id: &str, deadline: i64| Job {
            id: id.into(),
            deadline,
            state: "prepared".into(),
            proof: Some("proof".into()),
            call: Some("call".into()),
        };
        let ids = |jobs: &[Job]| jobs.iter().map(|job| job.id.clone()).collect::<Vec<_>>();
        let queue = vec![
            job("1", 1060),
            job("2", 1061),
            job("3", 1062),
            job("4", 1063),
        ];
        let excluded: std::collections::HashSet<String> = ["3".to_string(), "9".to_string()].into();
        let (order, leading) = resend_first(queue.clone(), &excluded, &SendPolicy::PRIMARY, 1000);
        assert_eq!(ids(&order), ["3", "1", "2", "4"]);
        assert_eq!(leading, 1);
        // Nothing excluded: the queue order is untouched.
        let (order, leading) = resend_first(
            queue.clone(),
            &Default::default(),
            &SendPolicy::PRIMARY,
            1000,
        );
        assert_eq!((ids(&order), leading), (ids(&queue), 0));
        // A follower that has not joined does not own the request, until it reaches the safety age.
        let (order, leading) = resend_first(queue.clone(), &excluded, &follower(false), 1000);
        assert_eq!((ids(&order), leading), (ids(&queue), 0));
        let (order, leading) = resend_first(queue, &excluded, &follower(false), 1042);
        assert_eq!(leading, 1);
        assert_eq!(order[0].id, "3");
    }
    #[test]
    fn batch_payload_carries_each_member_proof_in_order() {
        let proof = |seed: u64| VrfProof {
            pk: [U256::from(1), U256::from(2)],
            gamma: [U256::from(3), U256::from(4)],
            c: U256::from(5),
            s: U256::from(6),
            seed: U256::from(seed),
            uWitness: alloy_primitives::Address::repeat_byte(7),
            cGammaWitness: [U256::from(8), U256::from(9)],
            sHashWitness: [U256::from(10), U256::from(11)],
            zInv: U256::from(12),
        };
        let members: Vec<Member> = [9u64, 7]
            .into_iter()
            .map(|id| Member {
                id: id.to_string(),
                request_id: U256::from(id),
                proof: proof(id * 100),
                deadline: 0,
                callback_gas: 100_000,
                fee_paid: 0,
            })
            .collect();
        let payload = batch_payload(&members);
        let bytes: Bytes = payload.parse().unwrap();
        assert_eq!(&bytes[..4], C::fulfillRandomnessBatchCall::SELECTOR);
        let decoded = C::fulfillRandomnessBatchCall::abi_decode(&bytes).unwrap();
        assert_eq!(decoded.ids, vec![U256::from(9), U256::from(7)]);
        assert_eq!(
            decoded.proofs.iter().map(|p| p.seed).collect::<Vec<_>>(),
            vec![U256::from(900), U256::from(700)]
        );
        assert_eq!(payload, batch_payload(&members), "payload is deterministic");
    }
    #[test]
    fn receipt_logs_name_only_members_the_coordinator_fulfilled() {
        let coordinator = alloy_primitives::Address::repeat_byte(1);
        let served = C::RandomnessFulfilled::SIGNATURE_HASH;
        let skipped = C::FulfillmentSkipped::SIGNATURE_HASH;
        let id = |n: u64| B256::from(U256::from(n).to_be_bytes::<32>());
        let receipt = json!({"logs":[
            {"address":coordinator,"topics":[served,id(5),B256::ZERO]},
            {"address":coordinator,"topics":[skipped,id(6)]},
            {"address":alloy_primitives::Address::repeat_byte(2),"topics":[served,id(7),B256::ZERO]},
            {"address":coordinator,"topics":[served,id(8),B256::ZERO]},
        ]});
        let ids = fulfilled_in_receipt(&receipt, coordinator).unwrap();
        assert_eq!(
            ids.into_iter().collect::<Vec<_>>(),
            vec![U256::from(5), U256::from(8)]
        );
        assert!(
            fulfilled_in_receipt(&json!({"status":"0x1"}), coordinator)
                .unwrap()
                .is_empty()
        );
    }
}
