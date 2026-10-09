use crate::abi::NodeInterface;
use crate::config::FinalityMode;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_sol_types::SolCall;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

/// The most blocks one `block_hashes` batch names: the size of an audit page of the finality audit.
pub const MAX_BLOCK_HASHES: usize = 64;
/// How long an endpoint that showed another block than the other endpoints agree on stays last in the read order
/// (soft finality; see `finality::verdict`).
pub const DISAGREEMENT_COOLDOWN: Duration = Duration::from_secs(300);
/// One endpoint's answer about a block whose hash is disputed: the hash it has for the block, and the number of its
/// finalized head when it served one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockView {
    /// The endpoint's position in `RPC_URLS`, from 0. Never its address, which may hold a key.
    pub endpoint: usize,
    pub hash: B256,
    pub finalized: Option<u64>,
}
/// Arbitrum's NodeInterface, a virtual contract that only `eth_call` reaches.
pub const NODE_INTERFACE: Address = Address::new([
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xc8,
]);
#[derive(Debug, PartialEq, Eq)]
pub enum BroadcastOutcome {
    Acknowledged,
    AlreadyKnown,
    Ambiguous,
    Rejected(&'static str),
}
#[derive(Debug)]
struct DeliveryError {
    rejection: Option<&'static str>,
    known: bool,
    known_hash: Option<B256>,
    /// The node answered with a JSON-RPC error object (for eth_estimateGas, a revert),
    /// as opposed to transport, HTTP or timeout failures where no node answered at all.
    responded: bool,
    /// That error said the call reverted: JSON-RPC code 3, or a message that says so.
    reverted: bool,
    /// The endpoint refused the call for its request rate (HTTP 429 or an equivalent provider error).
    rate_limited: bool,
    /// The endpoint answered a well-formed read with something the read could not use: no JSON-RPC answer, or a
    /// result of the wrong shape (null, not hex, a missing field). That endpoint failed, not the request or the chain.
    malformed: bool,
    /// The JSON-RPC error code the node answered with.
    code: Option<i64>,
    /// The endpoint refused a JSON-RPC batch for its size: an HTTP error or a JSON-RPC error that names batches.
    batch_refused: bool,
    detail: String,
}
impl std::fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.malformed {
            write!(f, "RPC answer unusable: {}", self.detail)
        } else {
            write!(f, "RPC delivery uncertain: {}", self.detail)
        }
    }
}
impl std::error::Error for DeliveryError {}
impl DeliveryError {
    /// The endpoint failed as a provider does when it is overloaded or down: no answer (transport, timeout), an HTTP
    /// error, a rate limit, or a JSON-RPC server error (-32000) that is no rejection, revert or known transaction. An
    /// answer the read could not use is not one: it may be another chain's or another contract's.
    fn provider(&self) -> bool {
        if self.malformed {
            return false;
        }
        if self.rate_limited || !self.responded {
            return true;
        }
        self.code == Some(-32000) && self.rejection.is_none() && !self.known && !self.reverted
    }
}
/// No usable answer arrived: the endpoints did not answer, answered with a JSON-RPC error, rate limited, or (for a
/// read) answered with something the read could not use. Not a verdict on the chain.
pub fn is_delivery_failure(error: &anyhow::Error) -> bool {
    error.is::<DeliveryError>()
}
/// The call failed only because every endpoint it could use was rate limiting. Not a fault of the chain, the keys
/// or the configuration: the caller should back off and try again rather than treat the work as failed.
pub fn is_rate_limited(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<DeliveryError>()
            .is_some_and(|d| d.rate_limited)
    })
}
/// The call failed only because the endpoints failed as providers do when they are overloaded or down (HTTP 429 or
/// 5xx, a rate limit, -32000, no answer in time): not a verdict on the chain, the keys or the configuration.
pub fn is_provider_failure(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<DeliveryError>()
            .is_some_and(DeliveryError::provider)
    })
}
/// Provider wording for "slow down". Codes alone are ambiguous (-32005 also reports oversized log queries), so an
/// error counts only with a rate code or rate wording.
fn rate_limit_response(code: i64, message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    code == 429
        || code == -32029
        || (code == -32005 && !message.contains("result"))
        || [
            "rate limit",
            "rate-limit",
            "ratelimit",
            "too many requests",
            "request limit",
            "requests limit",
            "compute units per second",
            "throughput",
        ]
        .iter()
        .any(|needle| message.contains(needle))
}
/// Provider wording for a JSON-RPC batch it does not take, such as "Batch of more than 3 requests are not allowed on free
/// plan" or "batch size too large".
fn names_batches(message: &str) -> bool {
    message.to_ascii_lowercase().contains("batch")
}
/// Whether an array of JSON-RPC calls was refused for its size: the provider answered with an HTTP error or a JSON-RPC
/// error that names batches (or HTTP 400 or 413). A rate limit is not a refusal.
fn batch_refused(error: &anyhow::Error) -> bool {
    !is_rate_limited(error)
        && error
            .downcast_ref::<DeliveryError>()
            .is_some_and(|delivery| delivery.batch_refused)
}
/// A failure that happened only because every endpoint was rate limiting, for callers that observed that themselves.
pub fn rate_limited_error(context: &'static str) -> anyhow::Error {
    anyhow::Error::new(rate_limited("every endpoint is rate limiting")).context(context)
}
fn rate_limited(detail: impl Into<String>) -> DeliveryError {
    DeliveryError {
        rate_limited: true,
        ..uncertain(detail)
    }
}
/// A point in [base/2, base], so keepers sharing endpoints do not retry in lockstep.
fn jittered(base: Duration) -> Duration {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    base / 2 + base.mul_f64((hasher.finish() % 1000) as f64 / 2000.0)
}
/// Back-off after the n-th consecutive rate-limited answer from one endpoint: 1 s doubling to 30 s, jittered.
fn rate_limit_backoff(strikes: u32) -> Duration {
    jittered(
        Duration::from_secs(1u64 << strikes.saturating_sub(1).min(5)).min(Duration::from_secs(30)),
    )
}
/// A delivery failure in which a node actually answered with an error, e.g. an estimate
/// that reverted. Still not proof of a deterministic revert on every endpoint, so callers
/// may only use it to choose a safer path, never to resolve or discard a nonce.
pub fn is_node_error_response(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<DeliveryError>()
        .is_some_and(|delivery| delivery.responded)
}
/// A node answered that the call reverted, as opposed to refusing it for another reason (a gas
/// limit it does not estimate, a request it does not take). As with `is_node_error_response`,
/// never proof that the call reverts on every endpoint.
pub fn is_revert(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<DeliveryError>()
        .is_some_and(|delivery| delivery.responded && delivery.reverted)
}
fn known_transaction(message: &str) -> (bool, Option<B256>) {
    let message = message.trim().to_ascii_lowercase();
    if matches!(message.as_str(), "already known" | "known transaction") {
        return (true, None);
    }
    if let Some(hash) = message.strip_prefix("known transaction: ")
        && hash.len() == 66
        && hash.starts_with("0x")
        && let Ok(hash) = hash.parse::<B256>()
    {
        return (true, Some(hash));
    }
    (false, None)
}
// A rejection describes this node only; it NEVER releases a signed nonce.
fn rejection(message: &str) -> Option<&'static str> {
    let message = message.trim().to_ascii_lowercase();
    for (prefix, reason) in [
        ("insufficient funds", "insufficient_funds"),
        (
            "sender doesn't have enough funds to send tx.",
            "insufficient_funds",
        ),
        ("transaction underpriced", "transaction_underpriced"),
        ("intrinsic gas too low", "intrinsic_gas"),
        ("intrinsic gas exceeds gas limit", "intrinsic_gas"),
        ("invalid chain id", "invalid_chain_id"),
        ("invalid sender", "invalid_signature"),
        ("invalid signature", "invalid_signature"),
        (
            "replacement transaction underpriced",
            "replacement_underpriced",
        ),
        ("replacement underpriced", "replacement_underpriced"),
    ] {
        if message == prefix
            || message.starts_with(&format!("{prefix}:"))
            || message.starts_with(&format!("{prefix} "))
        {
            return Some(reason);
        }
    }
    None
}
fn uncertain(detail: impl Into<String>) -> DeliveryError {
    DeliveryError {
        rejection: None,
        known: false,
        known_hash: None,
        responded: false,
        reverted: false,
        rate_limited: false,
        malformed: false,
        code: None,
        batch_refused: false,
        detail: detail.into(),
    }
}
/// An answer a read could not use, as the failure of the endpoint that gave it.
fn malformed(detail: impl Into<String>) -> DeliveryError {
    DeliveryError {
        malformed: true,
        ..uncertain(detail)
    }
}
/// An answer that two reads of one block contradict (a revert for state another read there says exists): the failure of
/// the endpoint that gave it, a delivery failure and not a revert.
pub fn unusable(detail: impl Into<String>) -> anyhow::Error {
    malformed(detail).into()
}
fn broadcast_result(result: Result<Value>, expected: B256) -> Result<BroadcastOutcome> {
    match result {
        Err(e) if e.is::<DeliveryError>() => {
            let delivery = e.downcast_ref::<DeliveryError>().unwrap();
            if let Some(hash) = delivery.known_hash {
                ensure!(
                    hash == expected,
                    "RPC acknowledged a different known transaction hash"
                );
            }
            tracing::warn!(error=%delivery, "Transaction delivery response");
            Ok(if delivery.known {
                BroadcastOutcome::AlreadyKnown
            } else if let Some(reason) = delivery.rejection {
                BroadcastOutcome::Rejected(reason)
            } else {
                BroadcastOutcome::Ambiguous
            })
        }
        Err(e) => Err(e),
        Ok(v) => {
            let actual: B256 = serde_json::from_value(v)?;
            ensure!(
                actual == expected,
                "RPC returned a different transaction hash"
            );
            Ok(BroadcastOutcome::Acknowledged)
        }
    }
}

/// Consecutive rate-limited answers from one endpoint and the end of the back-off they earned.
type RateLimit = (u32, Option<tokio::time::Instant>);
/// How long reconciliation waits for some endpoint to serve a used nonce's receipt before it resolves the nonce
/// from contract state instead, as it did before it asked: a hung endpoint must not stretch a tick.
const RECEIPT_SEARCH: Duration = Duration::from_secs(2);
#[derive(Clone)]
pub struct Rpc {
    pub client: reqwest::Client,
    pub urls: Vec<String>,
    active: Arc<AtomicUsize>,
    cooldowns: Arc<Mutex<Vec<Option<tokio::time::Instant>>>>,
    /// Per endpoint: consecutive rate-limited answers and the end of the back-off they earned. An endpoint inside
    /// its back-off is not asked at all, so a limit is never answered with more traffic.
    limits: Arc<Mutex<Vec<RateLimit>>>,
    /// Per endpoint: the most calls one JSON-RPC batch to it may carry. Unbounded until it refuses a batch for its size,
    /// then the largest chunk that it answered (`batch_at`); 1 for an endpoint that answers a batch with something other
    /// than a matching array, which is asked one call at a time. Kept for the life of the process.
    batch_limits: Arc<Mutex<Vec<usize>>>,
    /// Per endpoint: whether it is held out of every read. A round keeper holds the endpoints that did not answer at
    /// startup, and admits each once it answers its probe (`Worker::readmit_endpoints`). Nothing is held otherwise.
    held: Arc<Mutex<Vec<bool>>>,
    read_budget: Duration,
    attempt_budget: Duration,
    /// The block the keeper decides on: see `decision_head`. Finalized until `with_finality` says otherwise.
    finality: FinalityMode,
    /// In soft mode the decision head is this many blocks below the latest one; 0 in finalized mode.
    soft_depth: u64,
}
#[derive(Clone, Debug)]
pub struct Head {
    pub number: u64,
    pub hash: B256,
    pub timestamp: u64,
    pub base_fee: u128,
}
impl Head {
    /// A block object as eth_getBlockByNumber returns it. Every field is required: a null answer (an endpoint that
    /// does not serve the tag) is no block, and a block without a base fee would price a send at its tip alone.
    pub fn from_block(v: &Value) -> Result<Self> {
        ensure!(v.is_object(), "Expected a block object");
        Ok(Self {
            number: quantity(&v["number"]).context("Block number")?,
            hash: serde_json::from_value(v["hash"].clone()).context("Block hash")?,
            timestamp: quantity(&v["timestamp"]).context("Block timestamp")?,
            base_fee: wide_quantity(&v["baseFeePerGas"])
                .context("Block base fee")?
                .try_into()
                .context("Block base fee")?,
        })
    }
}
/// A JSON-RPC quantity: "0x" and 1 to 64 hex digits. Anything else (null, a number, "0x", digits without the
/// prefix, separators) is an unusable answer, never a value such as zero.
fn wide_quantity(v: &Value) -> Result<U256> {
    let digits = v
        .as_str()
        .and_then(|s| s.strip_prefix("0x"))
        .filter(|d| (1..=64).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| anyhow::anyhow!("Expected RPC hex quantity"))?;
    Ok(U256::from_str_radix(digits, 16)?)
}
pub fn quantity(v: &Value) -> Result<u64> {
    Ok(wide_quantity(v)?.try_into()?)
}
/// An answer to eth_getTransactionReceipt for `expected`: `None` for null, a receipt of that transaction with the
/// fields reconciliation reads, or unusable.
fn usable_receipt(v: Value, expected: B256) -> Result<Option<Value>> {
    if v.is_null() {
        return Ok(None);
    }
    let actual: B256 =
        serde_json::from_value(v["transactionHash"].clone()).context("Receipt transaction hash")?;
    ensure!(actual == expected, "Receipt of another transaction");
    serde_json::from_value::<B256>(v["blockHash"].clone()).context("Receipt block hash")?;
    quantity(&v["blockNumber"]).context("Receipt block number")?;
    ensure!(
        quantity(&v["status"]).context("Receipt status")? <= 1,
        "Invalid receipt status"
    );
    Ok(Some(v))
}
/// The number of the block a receipt of transaction `hash` names, once the receipt is shown to be that transaction's
/// and to carry a valid status.
fn receipt_number(hash: &str, receipt: &Value) -> Result<u64> {
    let actual: B256 = serde_json::from_value(receipt["transactionHash"].clone())?;
    ensure!(
        actual == hash.parse::<B256>()?,
        "Unexpected receipt transaction"
    );
    ensure!(quantity(&receipt["status"])? <= 1, "Invalid receipt status");
    quantity(&receipt["blockNumber"])
}
/// The answer to a request for block `number`: that block. Null (a block the endpoint has not served yet) or any
/// other block is unusable.
fn numbered_block(v: &Value, number: u64) -> Result<Head> {
    let block = Head::from_block(v)?;
    ensure!(block.number == number, "Unexpected block number");
    Ok(block)
}
impl Rpc {
    pub fn new(urls: Vec<String>) -> Result<Self> {
        ensure!(!urls.is_empty(), "At least one RPC required");
        let count = urls.len();
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(8))
                .connect_timeout(Duration::from_secs(3))
                .build()?,
            urls,
            active: Arc::new(AtomicUsize::new(0)),
            cooldowns: Arc::new(Mutex::new(vec![None; count])),
            limits: Arc::new(Mutex::new(vec![(0, None); count])),
            batch_limits: Arc::new(Mutex::new(vec![usize::MAX; count])),
            held: Arc::new(Mutex::new(vec![false; count])),
            read_budget: Duration::from_secs(8u64.saturating_mul(count as u64)),
            attempt_budget: Duration::from_secs(8),
            finality: FinalityMode::Finalized,
            soft_depth: 0,
        })
    }
    /// These endpoints, deciding on `mode`. `Finalized` is what `new` gives and what Arc runs: the keeper decides on the
    /// finalized head and reads contract views at the `finalized` tag. `Soft` decides on the sequencer's latest block
    /// less `soft_depth` blocks and reads views at `latest`; see `decision_head`. Every primitive below that depends on
    /// the mode is exactly the finalized code path when the mode is `Finalized`.
    pub fn with_finality(self, mode: FinalityMode, soft_depth: u64) -> Self {
        Self {
            finality: mode,
            soft_depth: match mode {
                FinalityMode::Finalized => 0,
                FinalityMode::Soft => soft_depth,
            },
            ..self
        }
    }
    pub fn finality(&self) -> FinalityMode {
        self.finality
    }
    /// These endpoints, with the ones whose position `admitted` does not list held out of every read until `admit`.
    pub fn holding(self, admitted: &[bool]) -> Self {
        {
            let mut held = self.held.lock().expect("RPC held mutex");
            for (i, held) in held.iter_mut().enumerate() {
                *held = !admitted.get(i).copied().unwrap_or(true);
            }
        }
        if let Some(first) = admitted.iter().position(|admitted| *admitted) {
            self.active.store(first, Ordering::Relaxed);
        }
        self
    }
    /// The positions of the endpoints held out of reads.
    pub fn held(&self) -> Vec<usize> {
        let held = self.held.lock().expect("RPC held mutex");
        (0..held.len()).filter(|i| held[*i]).collect()
    }
    /// Endpoint `i` answers reads from now on.
    pub fn admit(&self, i: usize) {
        self.held.lock().expect("RPC held mutex")[i] = false;
    }
    /// The endpoints that answer reads, in configured order.
    pub fn admitted_urls(&self) -> Vec<String> {
        let held = self.held.lock().expect("RPC held mutex");
        self.urls
            .iter()
            .zip(held.iter())
            .filter(|(_, held)| !**held)
            .map(|(url, _)| url.clone())
            .collect()
    }
    /// How many endpoints answer reads: the configured ones less the held ones.
    pub fn admitted(&self) -> usize {
        self.urls.len() - self.held().len()
    }
    /// Endpoints in the order a read tries them: the last one that answered first, slow ones last, and the ones
    /// inside a rate-limit back-off, or held, not at all. Empty when every endpoint is backing off.
    fn read_order(&self) -> Vec<usize> {
        let count = self.urls.len();
        let start = self.active.load(Ordering::Relaxed) % count;
        let now = tokio::time::Instant::now();
        let limits = self.limits.lock().expect("RPC rate-limit mutex");
        let held = self.held.lock().expect("RPC held mutex");
        let mut order: Vec<usize> = (0..count)
            .map(|offset| (start + offset) % count)
            .filter(|i| !held[*i] && limits[*i].1.is_none_or(|until| until <= now))
            .collect();
        drop(held);
        drop(limits);
        let cooldowns = self.cooldowns.lock().expect("RPC cooldown mutex");
        order.sort_by_key(|i| cooldowns[*i].is_some_and(|until| until > now));
        order
    }
    /// Record one endpoint's answer for rate limiting: a limit extends its back-off and moves reads elsewhere,
    /// anything else ends the episode.
    fn note_rate_limit(&self, i: usize, limited: bool) {
        let mut limits = self.limits.lock().expect("RPC rate-limit mutex");
        let (strikes, until) = &mut limits[i];
        if !limited {
            if *strikes > 0 {
                tracing::info!(endpoint = i, "RPC endpoint no longer rate limiting");
            }
            *strikes = 0;
            *until = None;
            return;
        }
        *strikes = strikes.saturating_add(1);
        let backoff = rate_limit_backoff(*strikes);
        *until = Some(tokio::time::Instant::now() + backoff);
        if *strikes == 1 {
            tracing::warn!(
                endpoint = i,
                "RPC endpoint is rate limiting; backing off and using the other endpoints"
            );
        } else {
            tracing::debug!(
                endpoint = i,
                strikes = *strikes,
                backoff_ms = backoff.as_millis() as u64,
                "RPC endpoint still rate limiting"
            );
        }
        drop(limits);
        self.active
            .compare_exchange(
                i,
                (i + 1) % self.urls.len(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .ok();
    }
    fn all_limited_error(method: &str) -> anyhow::Error {
        anyhow::Error::new(rate_limited("every endpoint is backing off")).context(format!(
            "All configured RPC endpoints are rate limiting {method}"
        ))
    }
    /// Runtime identity reads must reach a fallback inside the caller's 3-second budget.
    pub fn for_runtime_checks(&self) -> Self {
        Self {
            read_budget: Duration::from_millis(2400),
            attempt_budget: Duration::from_millis(800),
            ..self.clone()
        }
    }
    /// One HTTP POST of a JSON-RPC body; the parsed JSON answer, whatever its shape.
    async fn post(&self, url: &str, body: &Value) -> Result<Value> {
        let response = self
            .client
            .post(url)
            .json(body)
            .send()
            .await
            .map_err(|_| uncertain("transport"))?;
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(rate_limited("HTTP status 429").into());
        }
        let mut response = response;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            // A provider that refuses a batch for its size says so in the body of its HTTP error, or answers 400 or 413.
            let mut said = Vec::new();
            while said.len() < 4096
                && let Ok(Some(chunk)) = response.chunk().await
            {
                said.extend_from_slice(&chunk);
            }
            return Err(DeliveryError {
                batch_refused: matches!(status, 400 | 413)
                    || names_batches(&String::from_utf8_lossy(&said)),
                ..uncertain(format!("HTTP status {status}"))
            }
            .into());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| uncertain("transport"))? {
            ensure!(
                bytes.len() + chunk.len() <= 2 * 1024 * 1024,
                "RPC response exceeds 2 MiB"
            );
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("Invalid RPC JSON"))
    }
    /// The result of one JSON-RPC response object, or its error as a delivery failure a node answered.
    fn outcome(body: &Value) -> Result<Value> {
        if let Some(error) = body.get("error") {
            let code = error["code"]
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("Invalid RPC error code"))?;
            let message = error["message"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Invalid RPC error message"))?;
            return Err(DeliveryError {
                rejection: rejection(message),
                known: known_transaction(message).0,
                known_hash: known_transaction(message).1,
                responded: true,
                reverted: code == 3 || message.to_ascii_lowercase().contains("revert"),
                rate_limited: rate_limit_response(code, message),
                malformed: false,
                code: Some(code),
                // A revert that names a batch (a batch fulfillment's) is no refusal.
                batch_refused: names_batches(message)
                    && code != 3
                    && !message.to_ascii_lowercase().contains("revert"),
                detail: format!("JSON-RPC code {code}"),
            }
            .into());
        }
        body.get("result")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Missing RPC result"))
    }
    pub async fn at(&self, url: &str, method: &str, params: Value) -> Result<Value> {
        let body = self
            .post(
                url,
                &json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}),
            )
            .await?;
        Self::outcome(&body)
    }
    /// One read, answered as-is by the first endpoint that answers: null and any other result are that answer.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_as(method, params, Ok).await
    }
    /// `request`, with the answer checked inside the read: an endpoint whose answer `parse` rejects (null where a
    /// value belongs, not hex, a missing field) has failed like one that did not answer, so the read moves on to
    /// the next endpoint and fails only when every endpoint has failed. `parse` checks shape only: a verdict on a
    /// well-formed answer belongs to the caller, or a read would pass over an endpoint that disagrees for one
    /// that agrees.
    pub async fn request_as<T, P>(&self, method: &str, params: Value, parse: P) -> Result<T>
    where
        P: Fn(Value) -> Result<T>,
    {
        let (params, parse) = (&params, &parse);
        self.hedged(method, move |i| async move {
            parse(self.at(&self.urls[i], method, params.clone()).await?)
        })
        .await
    }
    /// Independent reads sent as one JSON-RPC batch per endpoint attempt, with the same endpoint order, hedging
    /// and rate-limit back-off as `request`. Results come back in call order and always from one endpoint. An
    /// endpoint that answers a batch with anything but a matching array is asked the same calls one by one from
    /// then on.
    pub async fn batch(&self, calls: &[(&str, Value)]) -> Result<Vec<Value>> {
        self.batch_as(calls, Ok).await
    }
    /// `batch`, with the answers checked inside the read as `request_as` checks one. Every value `parse` sees, and
    /// so everything it returns, comes from one endpoint's answer.
    pub async fn batch_as<T, P>(&self, calls: &[(&str, Value)], parse: P) -> Result<T>
    where
        P: Fn(Vec<Value>) -> Result<T>,
    {
        ensure!(!calls.is_empty() && calls.len() <= 32, "RPC batch size");
        let parse = &parse;
        self.hedged(&format!("{} batch", calls[0].0), move |i| async move {
            parse(self.batch_at(i, calls).await?)
        })
        .await
    }
    /// Run one idempotent read against the endpoints until an attempt succeeds. `attempt(i)` makes every call of
    /// the read to endpoint `i` alone (`at`, `batch_at`), so whatever one attempt returns is one endpoint's view.
    ///
    /// An attempt fails when its endpoint does not answer, answers with a JSON-RPC error or rate limits, or
    /// answers something the attempt cannot use: any error of the attempt's own that is not a delivery failure
    /// counts as such an answer. That endpoint then moves last for a while, reads stop starting at it, and the
    /// next endpoint answers the whole read afresh; the read fails only when every endpoint has failed, and then
    /// as a delivery failure. Attempts check shape only: a verdict on a well-formed answer belongs after the read.
    pub(crate) async fn hedged<T, F, Fut>(&self, label: &str, attempt: F) -> Result<T>
    where
        F: Fn(usize) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let deadline = tokio::time::Instant::now() + self.read_budget;
        // Cooling endpoints move last instead of disappearing: a single configured
        // RPC, or every RPC cooling at once, must still be attempted. Endpoints backing
        // off a rate limit are skipped; when all are, the read fails as rate limited.
        let order = self.read_order();
        if order.is_empty() {
            return Err(Self::all_limited_error(label));
        }
        let mut order = order.into_iter().peekable();
        let mut last_error = None;
        let mut last_limited = None;
        let mut limited_only = true;
        // Hedge instead of cancelling: a slow attempt keeps running while the next candidate
        // starts, so an endpoint slower than one attempt budget can still win before the
        // deadline no matter where it sits in the order. All attempts are idempotent reads.
        let mut attempts = futures_util::stream::FuturesUnordered::new();
        let mut launched: Vec<(usize, tokio::time::Instant)> = Vec::new();
        let mut next_hedge = deadline;
        loop {
            // Every loop turn follows a failed attempt or an elapsed hedge interval.
            if let Some(i) = order.next() {
                let began = tokio::time::Instant::now();
                launched.push((i, began));
                next_hedge = began + self.attempt_budget;
                let future = attempt(i);
                attempts.push(async move { (i, future.await) });
            }
            if attempts.is_empty() {
                break;
            }
            let wake = if order.peek().is_some() {
                next_hedge.min(deadline)
            } else {
                deadline
            };
            tokio::select! {
                Some((i, result)) = futures_util::StreamExt::next(&mut attempts) => {
                    let position = launched.iter().position(|(j, _)| *j == i).expect("launched RPC attempt");
                    let (_, began) = launched.swap_remove(position);
                    match result {
                        Ok(v) => {
                            self.cool_slow_attempts(&launched);
                            self.note_rate_limit(i, false);
                            self.active.store(i, Ordering::Relaxed);
                            return Ok(v);
                        }
                        Err(e) => {
                            self.cool_slow_attempts(&[(i, began)]);
                            let limited = is_rate_limited(&e);
                            self.note_rate_limit(i, limited);
                            if limited {
                                tracing::debug!(method=label,endpoint=i,"RPC attempt rate limited");
                                last_limited = Some(e);
                                continue;
                            }
                            limited_only = false;
                            let e = if is_delivery_failure(&e) {
                                e
                            } else {
                                // The endpoint answered, but with nothing this read can use (not JSON-RPC, null,
                                // not hex, a missing field). It failed like one that did not answer.
                                self.cool(i);
                                malformed(format!("{label} answer: {e:#}")).into()
                            };
                            tracing::warn!(method=label,endpoint=i,error=%e,"RPC attempt failed");
                            last_error = Some(e);
                        }
                    }
                }
                () = tokio::time::sleep_until(wake) => {
                    if tokio::time::Instant::now() >= deadline {
                        self.cool_slow_attempts(&launched);
                        last_error = Some(uncertain("read attempt timed out").into());
                        limited_only = false;
                        break;
                    }
                }
            }
        }
        if limited_only && last_limited.is_some() {
            return Err(Self::all_limited_error(label));
        }
        // A mix of failures reports a non-rate-limit one: the read did not fail only because of rate limits.
        Err(last_error
            .or(last_limited)
            .unwrap_or_else(|| anyhow::anyhow!("No configured RPC endpoints"))
            .context(format!("All configured RPC endpoints failed {label}")))
    }
    /// `calls` sent to endpoint `i` alone: one JSON-RPC batch where the endpoint answers batches, otherwise one call
    /// at a time. Results come back in call order.
    ///
    /// An endpoint that refuses a batch for its size (`batch_refused`) is asked the same calls again at once in chunks of
    /// half the size, halving down to single calls; the size it answered is kept for it, and later batches to it are sent
    /// in chunks of that size. A refusal is not the endpoint's failure: only a chunk that fails as anything else fails the
    /// attempt.
    pub(crate) async fn batch_at(&self, i: usize, calls: &[(&str, Value)]) -> Result<Vec<Value>> {
        let url = &self.urls[i];
        let known = self.batch_limits.lock().expect("RPC batch mutex")[i];
        let mut limit = known;
        let mut results = Vec::with_capacity(calls.len());
        while results.len() < calls.len() {
            let rest = &calls[results.len()..];
            if limit <= 1 {
                for (method, params) in rest {
                    results.push(self.at(url, method, params.clone()).await?);
                }
                break;
            }
            let chunk = &rest[..rest.len().min(limit)];
            match self.post_batch(url, chunk).await {
                Ok(Some(answered)) => results.extend(answered),
                Ok(None) => {
                    limit = 1;
                    tracing::debug!(
                        endpoint = i,
                        "RPC endpoint does not answer batches; sending calls one by one"
                    );
                }
                Err(error) if batch_refused(&error) => {
                    limit = (chunk.len() / 2).max(1);
                    tracing::debug!(endpoint = i, refused = chunk.len(), batch = limit, error = %error,
                        "RPC endpoint refused a batch; asking again in smaller chunks");
                }
                Err(error) => return Err(error),
            }
        }
        if limit < known {
            let mut limits = self.batch_limits.lock().expect("RPC batch mutex");
            if limit < limits[i] {
                limits[i] = limit;
                drop(limits);
                tracing::info!(
                    endpoint = i,
                    batch = limit,
                    "RPC endpoint takes batches of this size at most; larger ones are sent in chunks"
                );
            }
        }
        Ok(results)
    }
    /// One JSON-RPC batch of `calls` to `url`: the results in call order, or None when the endpoint answered with
    /// something other than a matching array that is no refusal of its size (it does not batch).
    async fn post_batch(&self, url: &str, calls: &[(&str, Value)]) -> Result<Option<Vec<Value>>> {
        let body: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(id, (method, params))| {
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
            })
            .collect();
        let answer = self.post(url, &Value::Array(body)).await?;
        if let Some(items) = answer.as_array().filter(|items| items.len() == calls.len()) {
            let mut results = vec![Value::Null; calls.len()];
            let mut seen = vec![false; calls.len()];
            for item in items {
                let id = item["id"]
                    .as_u64()
                    .and_then(|id| usize::try_from(id).ok())
                    .filter(|id| *id < calls.len() && !seen[*id])
                    .ok_or_else(|| anyhow::anyhow!("Invalid RPC batch response id"))?;
                seen[id] = true;
                results[id] = Self::outcome(item)?;
            }
            return Ok(Some(results));
        }
        // A single error object or a short array: this endpoint does not batch. A rate-limit error in that position is
        // still a rate limit, and an error that names batches a refusal of this batch's size.
        if answer.get("error").is_some()
            && let Err(error) = Self::outcome(&answer)
            && (is_rate_limited(&error) || batch_refused(&error))
        {
            return Err(error);
        }
        Ok(None)
    }
    /// Only an attempt that consumed a full attempt budget proves a slow endpoint.
    fn cool_slow_attempts(&self, attempts: &[(usize, tokio::time::Instant)]) {
        for (i, began) in attempts {
            if began.elapsed() >= self.attempt_budget {
                self.cool(*i);
            }
        }
    }
    /// A slow or unusable endpoint moves last in the read order for five seconds, and reads that would start at it
    /// start at the next endpoint instead. It is never dropped: a single endpoint, or every endpoint cooling at
    /// once, is still asked.
    fn cool(&self, i: usize) {
        self.cool_for(i, Duration::from_secs(5));
    }
    /// `cool` for `period`: endpoint `i` moves last in the read order until then, or until a longer cooldown it is
    /// already in ends. A soft keeper puts an endpoint that showed another block than the others agree on here for
    /// `DISAGREEMENT_COOLDOWN`; it is still asked when no other endpoint answers.
    pub fn cool_for(&self, i: usize, period: Duration) {
        let until = tokio::time::Instant::now() + period;
        let mut cooldowns = self.cooldowns.lock().expect("RPC cooldown mutex");
        cooldowns[i] = Some(cooldowns[i].map_or(until, |cooling| cooling.max(until)));
        drop(cooldowns);
        self.active
            .compare_exchange(
                i,
                (i + 1) % self.urls.len(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .ok();
    }
    /// Whether endpoint `i` is cooling down now.
    pub fn cooling(&self, i: usize) -> bool {
        self.cooldowns.lock().expect("RPC cooldown mutex")[i]
            .is_some_and(|until| until > tokio::time::Instant::now())
    }
    /// Every endpoint's view of block `number`, asked of all of them at once (those inside a rate-limit back-off
    /// excepted, and cooling ones included), each within `budget`: the block, and beside it the `finalized` header. The
    /// two are separate reads, so that an endpoint that does not serve the `finalized` tag still gives its view of the
    /// block, without a finalized number. An endpoint that answers that it has no such block gives a view with the hash
    /// `finality::ABSENT`; one that does not answer the block in time or answers it unusably gives no view. Views are in
    /// endpoint order. This is how a soft keeper asks the other endpoints
    /// about a block one of them showed with another hash than the journal's; nothing else reads this way.
    pub async fn block_views(&self, number: u64, budget: Duration) -> Vec<BlockView> {
        let reads = self.read_order().into_iter().map(|i| async move {
            let url = &self.urls[i];
            let ask = |tag: String| async move {
                tokio::time::timeout(
                    budget,
                    self.at(url, "eth_getBlockByNumber", json!([tag, false])),
                )
                .await
                .ok()
            };
            let (block, finalized) =
                tokio::join!(ask(format!("0x{number:x}")), ask("finalized".to_owned()));
            let block = block?;
            self.note_rate_limit(i, block.as_ref().err().is_some_and(is_rate_limited));
            let block = block.ok()?;
            let hash = if block.is_null() {
                crate::finality::ABSENT
            } else {
                numbered_block(&block, number).ok()?.hash
            };
            Some(BlockView {
                endpoint: i,
                hash,
                finalized: finalized
                    .and_then(Result::ok)
                    .and_then(|header| Head::from_block(&header).ok())
                    .map(|header| header.number),
            })
        });
        let mut views: Vec<BlockView> = futures_util::future::join_all(reads)
            .await
            .into_iter()
            .flatten()
            .collect();
        views.sort_by_key(|view| view.endpoint);
        views
    }
    /// Every endpoint's `finalized` header, asked of all of them at once (those inside a rate-limit back-off excepted,
    /// and cooling ones included), until `deadline`. An endpoint that does not answer by then, does not serve the tag or
    /// answers unusably is left out.
    pub async fn finalized_views(&self, deadline: tokio::time::Instant) -> Vec<Head> {
        let reads = self.read_order().into_iter().map(|i| async move {
            let answer = tokio::time::timeout_at(
                deadline,
                self.at(
                    &self.urls[i],
                    "eth_getBlockByNumber",
                    json!(["finalized", false]),
                ),
            )
            .await
            .ok()?;
            self.note_rate_limit(i, answer.as_ref().err().is_some_and(is_rate_limited));
            Head::from_block(&answer.ok()?).ok()
        });
        futures_util::future::join_all(reads)
            .await
            .into_iter()
            .flatten()
            .collect()
    }
    /// Every endpoint's hashes of the blocks `numbers` (at most MAX_BLOCK_HASHES), each endpoint's in one batch, asked of
    /// all of them at once until `deadline`, as `finalized_views` asks. An endpoint that does not answer them all, usably
    /// and by then, is left out.
    pub async fn block_hash_views(
        &self,
        numbers: &[u64],
        deadline: tokio::time::Instant,
    ) -> Vec<Vec<B256>> {
        let calls: Vec<(&str, Value)> = numbers
            .iter()
            .map(|number| {
                (
                    "eth_getBlockByNumber",
                    json!([format!("0x{number:x}"), false]),
                )
            })
            .collect();
        let calls = &calls;
        let reads = self.read_order().into_iter().map(|i| async move {
            let answer = tokio::time::timeout_at(deadline, self.batch_at(i, calls))
                .await
                .ok()?;
            self.note_rate_limit(i, answer.as_ref().err().is_some_and(is_rate_limited));
            let blocks = answer.ok()?;
            numbers
                .iter()
                .zip(&blocks)
                .map(|(number, block)| numbered_block(block, *number).ok().map(|block| block.hash))
                .collect::<Option<Vec<B256>>>()
                .filter(|hashes| hashes.len() == numbers.len())
        });
        futures_util::future::join_all(reads)
            .await
            .into_iter()
            .flatten()
            .collect()
    }
    pub async fn head(&self) -> Result<Head> {
        self.head_at("latest").await
    }
    /// The `safe` head: on an Arbitrum chain, the latest block in a batch posted to L1, which the sequencer can no longer
    /// replace.
    pub async fn safe_head(&self) -> Result<Head> {
        self.head_at("safe").await
    }
    /// Median of the per-block 50th-percentile tips over recent blocks (eth_feeHistory).
    pub async fn recent_priority_fee(&self, blocks: u64) -> Result<u128> {
        self.request_as(
            "eth_feeHistory",
            json!([format!("0x{blocks:x}"), "latest", [50]]),
            |v| {
                let mut tips = v["reward"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|row| row.get(0)?.as_str())
                    .map(|hex| u128::from_str_radix(hex.trim_start_matches("0x"), 16))
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                ensure!(!tips.is_empty(), "Fee history returned no rewards");
                tips.sort_unstable();
                Ok(tips[tips.len() / 2])
            },
        )
        .await
    }
    /// The finalized head. Whatever the mode, this reads the `finalized` tag: it is what the finality audit compares the
    /// blocks the keeper acted on against. The keeper decides on `decision_head`.
    pub async fn finalized_head(&self) -> Result<Head> {
        self.head_at("finalized").await
    }
    /// The finalized head and, when `checkpoint` names a block number, that block, both from one endpoint's answer
    /// to one batched read. An answer without a usable block for either (an endpoint that does not serve the
    /// finalized tag answers null) fails that endpoint, and the next one answers the whole read.
    pub async fn finalized_head_with(
        &self,
        checkpoint: Option<u64>,
    ) -> Result<(Head, Option<Head>)> {
        let mut calls = vec![("eth_getBlockByNumber", json!(["finalized", false]))];
        if let Some(number) = checkpoint {
            calls.push((
                "eth_getBlockByNumber",
                json!([format!("0x{number:x}"), false]),
            ));
        }
        self.batch_as(&calls, |blocks| {
            let head = Head::from_block(&blocks[0])?;
            let block = match checkpoint {
                Some(number) => Some(numbered_block(&blocks[1], number)?),
                None => None,
            };
            Ok((head, block))
        })
        .await
    }
    /// The block the keeper decides on: its time, its number and its base fee price what the tick does, and what it
    /// reads of requests and nonces it reads at this block.
    ///
    /// `Finalized`: the finalized head, today's read. `Soft`: the sequencer's latest block less the soft depth. With
    /// depth 0 that is the latest block, one request. With a depth, the latest block and the block that many below it
    /// are read in turn from one endpoint, since the second depends on the first; no other endpoint's block is mixed
    /// in, and the tick after it checks that the block it chose is still on the chain (the soft checkpoint).
    pub async fn decision_head(&self) -> Result<Head> {
        match self.finality {
            FinalityMode::Finalized => self.finalized_head().await,
            FinalityMode::Soft if self.soft_depth == 0 => self.head().await,
            FinalityMode::Soft => Ok(self.soft_blocks(None, false).await?.0),
        }
    }
    /// The decision head and, when `checkpoint` names a block number, that block, both from one endpoint's answers. An
    /// answer without a usable block for either (a finalized head the endpoint does not serve, a checkpoint block it
    /// has not reached) fails that endpoint, and the next one answers the whole read.
    ///
    /// `Finalized`: `finalized_head_with`. `Soft`: the latest block (less the soft depth) and the checkpoint block in
    /// one batch from one endpoint; with a depth, the second read of `decision_head` carries the checkpoint too.
    pub async fn decision_head_with(
        &self,
        checkpoint: Option<u64>,
    ) -> Result<(Head, Option<Head>)> {
        match self.finality {
            FinalityMode::Finalized => self.finalized_head_with(checkpoint).await,
            FinalityMode::Soft => self.soft_blocks(checkpoint, true).await,
        }
    }
    /// Soft mode: the decision head and, when `block` names a number, that block, from one endpoint's answers.
    ///
    /// With `required`, the block must be served: an endpoint that does not serve it has not reached it, and fails. A
    /// block above the decision head that the endpoint does not serve yet is otherwise `None`, which says that the
    /// endpoint has not reached it, not that the endpoint is unusable. A block at or below the decision head that is
    /// not served is unusable either way: an endpoint that has its head has all of the blocks below it.
    async fn soft_blocks(
        &self,
        block: Option<u64>,
        required: bool,
    ) -> Result<(Head, Option<Head>)> {
        let depth = self.soft_depth;
        let ask = |number: &str| ("eth_getBlockByNumber", json!([number, false]));
        let latest = ask("latest");
        let wanted = block.map(|number| ask(&format!("0x{number:x}")));
        // The batch that reads the latest block, with the checkpoint beside it when no depth needs a second read.
        let mut first = vec![latest];
        if depth == 0 {
            first.extend(wanted.clone());
        }
        let (first, wanted) = (&first, &wanted);
        self.hedged("eth_getBlockByNumber batch", move |i| async move {
            let answers = self.batch_at(i, first).await?;
            let latest = Head::from_block(&answers[0])?;
            let (decision, other) = if depth == 0 {
                (latest, answers.get(1).cloned())
            } else {
                let number = latest.number.saturating_sub(depth);
                let mut calls = vec![ask(&format!("0x{number:x}"))];
                calls.extend(wanted.clone());
                let mut answers = self.batch_at(i, &calls).await?.into_iter();
                let decision = numbered_block(&answers.next().unwrap_or(Value::Null), number)?;
                (decision, answers.next())
            };
            let found = match block {
                None => None,
                Some(number) => {
                    let value = other.ok_or_else(|| anyhow::anyhow!("Missing block answer"))?;
                    if value.is_null() && !required && number > decision.number {
                        None
                    } else {
                        Some(numbered_block(&value, number)?)
                    }
                }
            };
            Ok((decision, found))
        })
        .await
    }
    /// The block tag at which the keeper reads requests and the wallet's nonce for a tick that decided on `head`:
    /// `finalized` in finalized mode, as always, and in soft mode the number of `head`, so that everything the tick
    /// reads is read at the block it decided on.
    pub fn decision_tag(&self, head: &Head) -> String {
        match self.finality {
            FinalityMode::Finalized => "finalized".to_owned(),
            FinalityMode::Soft => format!("0x{:x}", head.number),
        }
    }
    /// `decision_tag` for a caller that holds no head. Finalized mode needs none. In soft mode the decision head is the
    /// latest block when the depth is 0, and otherwise it is read.
    pub async fn current_decision_tag(&self) -> Result<String> {
        Ok(match self.finality {
            FinalityMode::Finalized => "finalized".to_owned(),
            FinalityMode::Soft if self.soft_depth == 0 => "latest".to_owned(),
            FinalityMode::Soft => self.decision_tag(&self.decision_head().await?),
        })
    }
    /// The nonce of `address` at the block the tick decided on (`decision_tag`): the nonces the chain has consumed.
    pub async fn decision_nonce(&self, address: Address, head: &Head) -> Result<u64> {
        self.nonce(address, &self.decision_tag(head)).await
    }
    /// `call` at the block the keeper decides on rather than at the view tag: the finalized state, and in soft mode the
    /// state at the decision head.
    pub async fn call_decided<C: SolCall>(&self, to: Address, call: C) -> Result<C::Return> {
        let tag = self.current_decision_tag().await?;
        self.call_tag(to, call, &tag).await
    }
    /// The tag at which a contract view is read when the caller names no block: `finalized`, and in soft mode `latest`.
    fn view_tag(&self) -> &'static str {
        match self.finality {
            FinalityMode::Finalized => "finalized",
            FinalityMode::Soft => "latest",
        }
    }
    /// The block with this number, including its timestamp.
    pub async fn block(&self, number: u64) -> Result<Head> {
        self.request_as(
            "eth_getBlockByNumber",
            json!([format!("0x{number:x}"), false]),
            |v| numbered_block(&v, number),
        )
        .await
    }
    async fn head_at(&self, tag: &str) -> Result<Head> {
        self.request_as("eth_getBlockByNumber", json!([tag, false]), |v| {
            Head::from_block(&v)
        })
        .await
    }
    /// A contract view read at the view tag: `finalized`, and in soft mode `latest`.
    pub async fn call<C: SolCall>(&self, to: Address, call: C) -> Result<C::Return> {
        self.call_tag(to, call, self.view_tag()).await
    }
    pub async fn call_at<C: SolCall>(
        &self,
        to: Address,
        call: C,
        number: u64,
    ) -> Result<C::Return> {
        self.call_tag(to, call, &format!("0x{number:x}")).await
    }
    /// The L1 gas an Arbitrum chain charges for a transaction to `to` with `data` before it runs
    /// (`NodeInterface.gasEstimateL1Component`, 173 for an empty transaction and 732 for a fulfillment on Robinhood
    /// Chain mainnet). The precompile prices it at the chain's current L1 prices, so it is read at the latest block.
    pub async fn l1_gas(&self, to: Address, data: &[u8]) -> Result<u64> {
        let call = NodeInterface::gasEstimateL1ComponentCall {
            to,
            contractCreation: false,
            data: Bytes::copy_from_slice(data),
        };
        Ok(self
            .call_tag(NODE_INTERFACE, call, "latest")
            .await?
            .gasEstimateForL1)
    }
    pub(crate) async fn call_tag<C: SolCall>(
        &self,
        to: Address,
        call: C,
        tag: &str,
    ) -> Result<C::Return> {
        self.request_as(
            "eth_call",
            json!([{"to":to,"data":Bytes::from(call.abi_encode())},tag]),
            |v| {
                let bytes: Bytes = serde_json::from_value(v)?;
                Ok(C::abi_decode_returns(&bytes)?)
            },
        )
        .await
    }
    /// `call` at the view tag with the gas the call may use written down, for a view that needs more than an endpoint
    /// may give an `eth_call` without a limit of its own.
    pub(crate) async fn call_with_gas<C: SolCall>(
        &self,
        to: Address,
        call: C,
        gas: u64,
    ) -> Result<C::Return> {
        self.call_with_gas_tag(to, call, gas, self.view_tag()).await
    }
    /// `call_with_gas` at the block tag `tag`.
    pub(crate) async fn call_with_gas_tag<C: SolCall>(
        &self,
        to: Address,
        call: C,
        gas: u64,
        tag: &str,
    ) -> Result<C::Return> {
        self.request_as(
            "eth_call",
            json!([{"to":to,"gas":format!("0x{gas:x}"),"data":Bytes::from(call.abi_encode())},tag]),
            |v| {
                let bytes: Bytes = serde_json::from_value(v)?;
                Ok(C::abi_decode_returns(&bytes)?)
            },
        )
        .await
    }
    pub async fn nonce(&self, address: Address, tag: &str) -> Result<u64> {
        self.request_as("eth_getTransactionCount", json!([address, tag]), |v| {
            quantity(&v)
        })
        .await
    }
    /// The latest balance of `address`.
    pub async fn balance(&self, address: Address) -> Result<U256> {
        self.request_as("eth_getBalance", json!([address, "latest"]), |v| {
            wide_quantity(&v)
        })
        .await
    }
    /// The gas `tx` needs. A revert is an error a node answered (`is_node_error_response`).
    pub async fn estimate_gas(&self, tx: Value) -> Result<u64> {
        self.request_as("eth_estimateGas", json!([tx]), |v| quantity(&v))
            .await
    }
    /// The receipt of transaction `hash`, or `None` while the endpoint has none. A receipt is usable only for this
    /// transaction and with the fields reconciliation reads; any other answer fails the endpoint.
    pub async fn receipt(&self, hash: &str) -> Result<Option<Value>> {
        let expected: B256 = hash.parse()?;
        self.request_as("eth_getTransactionReceipt", json!([hash]), |v| {
            usable_receipt(v, expected)
        })
        .await
    }
    /// A receipt of any of `hashes` (one nonce's attempts) from any endpoint that serves one, with the index of its
    /// hash. `receipt` takes the first endpoint's answer, and an endpoint that has not caught up with a new block
    /// answers null; this asks every endpoint not backing off a rate limit at once, for every hash in one batch, and
    /// waits at most RECEIPT_SEARCH. Nulls, failures and unusable answers are passed over. `None` when no endpoint
    /// served a receipt in time.
    pub async fn receipt_from_any(&self, hashes: &[String]) -> Result<Option<(usize, Value)>> {
        let expected = hashes
            .iter()
            .map(|hash| hash.parse::<B256>())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let calls: Vec<(&str, Value)> = hashes
            .iter()
            .map(|hash| ("eth_getTransactionReceipt", json!([hash])))
            .collect();
        let (calls, expected) = (&calls, &expected);
        let mut answers: futures_util::stream::FuturesUnordered<_> =
            self.read_order()
                .into_iter()
                .map(|i| async move {
                    let answer = self.batch_at(i, calls).await;
                    self.note_rate_limit(i, answer.as_ref().err().is_some_and(is_rate_limited));
                    answer.ok().and_then(|values| {
                        values.into_iter().zip(expected).enumerate().find_map(
                            |(k, (value, hash))| {
                                usable_receipt(value, *hash).ok().flatten().map(|v| (k, v))
                            },
                        )
                    })
                })
                .collect();
        let found = tokio::time::timeout(self.attempt_budget.min(RECEIPT_SEARCH), async {
            while let Some(answer) = futures_util::StreamExt::next(&mut answers).await {
                if answer.is_some() {
                    return answer;
                }
            }
            None
        })
        .await;
        Ok(found.ok().flatten())
    }
    pub async fn block_hash(&self, number: u64) -> Result<B256> {
        self.request_as(
            "eth_getBlockByNumber",
            json!([format!("0x{number:x}"), false]),
            |block| {
                ensure!(
                    quantity(&block["number"])? == number,
                    "Unexpected block number"
                );
                Ok(serde_json::from_value(block["hash"].clone())?)
            },
        )
        .await
    }
    /// The hash of each of these blocks, in order, as one JSON-RPC batch answered by one endpoint: the chain's own hashes
    /// that the finality audit compares the blocks the keeper acted on against. At most `MAX_BLOCK_HASHES` blocks. An
    /// endpoint that has not reached one of them (it answers null) has not answered, and the next endpoint answers the
    /// whole batch; none of one endpoint's hashes is ever mixed with another's.
    pub async fn block_hashes(&self, numbers: &[u64]) -> Result<Vec<B256>> {
        ensure!(
            !numbers.is_empty() && numbers.len() <= MAX_BLOCK_HASHES,
            "Block hash batch size"
        );
        let calls: Vec<(&str, Value)> = numbers
            .iter()
            .map(|number| {
                (
                    "eth_getBlockByNumber",
                    json!([format!("0x{number:x}"), false]),
                )
            })
            .collect();
        let calls = &calls;
        self.hedged("eth_getBlockByNumber batch", move |i| async move {
            let blocks = self.batch_at(i, calls).await?;
            numbers
                .iter()
                .zip(&blocks)
                .map(|(number, block)| {
                    ensure!(
                        quantity(&block["number"])? == *number,
                        "Unexpected block number"
                    );
                    Ok(serde_json::from_value(block["hash"].clone())?)
                })
                .collect()
        })
        .await
    }
    /// Whether the receipt of `hash` is settled for the keeper: its block is at or below the block the keeper decides on
    /// and is the canonical block of that number.
    ///
    /// `Finalized`: `receipt_is_finalized`. `Soft`: the receipt's block is at or below the decision head, and the
    /// block of that number has the hash the receipt names, both as one endpoint says it in one batch. An endpoint
    /// that has not reached the receipt's block says it is not settled yet. Whether it is settled is the keeper's
    /// verdict on the sequencer's chain; the finality audit checks it against L1 finality later.
    pub async fn receipt_is_settled(&self, hash: &str, receipt: &Value) -> Result<bool> {
        match self.finality {
            FinalityMode::Finalized => self.receipt_is_finalized(hash, receipt).await,
            FinalityMode::Soft => {
                let number = receipt_number(hash, receipt)?;
                let expected: B256 = serde_json::from_value(receipt["blockHash"].clone())?;
                let (decision, block) = self.soft_blocks(Some(number), false).await?;
                // The decision head is itself the receipt's block when the numbers are equal: its hash must agree too.
                Ok(decision.number >= number
                    && (decision.number != number || decision.hash == expected)
                    && block.is_some_and(|block| block.hash == expected))
            }
        }
    }
    pub async fn receipt_is_finalized(&self, hash: &str, receipt: &Value) -> Result<bool> {
        let number = receipt_number(hash, receipt)?;
        let finalized = self.finalized_head().await?;
        if finalized.number < number {
            return Ok(false);
        }
        let expected: B256 = serde_json::from_value(receipt["blockHash"].clone())?;
        if finalized.number == number && finalized.hash != expected {
            return Ok(false);
        }
        Ok(self.block_hash(number).await? == expected)
    }
    pub async fn broadcast(&self, raw: &str, expected: B256) -> Result<BroadcastOutcome> {
        // A retry needs receipt reconciliation and a fresh deadline check in the worker. An endpoint
        // backing off a rate limit is passed over while another one is available.
        let i = self
            .read_order()
            .first()
            .copied()
            .unwrap_or_else(|| self.active.load(Ordering::Relaxed) % self.urls.len());
        let result = self
            .at(&self.urls[i], "eth_sendRawTransaction", json!([raw]))
            .await;
        if let Err(error) = &result {
            self.note_rate_limit(i, is_rate_limited(error));
        }
        let outcome = broadcast_result(result, expected)?;
        if matches!(
            outcome,
            BroadcastOutcome::Ambiguous | BroadcastOutcome::Rejected(_)
        ) {
            self.active
                .store((i + 1) % self.urls.len(), Ordering::Relaxed);
        }
        Ok(outcome)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    async fn serve_chain_id(listener: tokio::net::TcpListener, delay: Duration) {
        serve(
            listener,
            delay,
            r#"{"jsonrpc":"2.0","id":1,"result":"0x1"}"#,
        )
        .await
    }
    async fn serve(listener: tokio::net::TcpListener, delay: Duration, body: &'static str) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let size = socket.read(&mut buffer).await.unwrap();
                    if size == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..size]);
                    if let Some(at) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                        let length = String::from_utf8_lossy(&bytes[..at])
                            .lines()
                            .find_map(|line| {
                                let (k, v) = line.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        if bytes.len() >= at + 4 + length {
                            break;
                        }
                    }
                }
                tokio::time::sleep(delay).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            });
        }
    }
    #[tokio::test]
    async fn node_error_responses_are_delivery_failures_that_a_node_answered() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rpc = Rpc::new(vec![format!("http://{}", listener.local_addr().unwrap())]).unwrap();
        let server = tokio::spawn(serve(
            listener,
            Duration::ZERO,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"execution reverted: WrongSeed()"}}"#,
        ));
        let reverted = rpc
            .request("eth_estimateGas", json!([{}]))
            .await
            .unwrap_err();
        assert!(is_delivery_failure(&reverted));
        assert!(is_node_error_response(&reverted));
        assert!(is_revert(&reverted));
        server.abort();
        // A node that says a call reverted with code 3 and no wording, and one that refuses an estimate for its gas
        // without a revert.
        for (answer, reverts) in [
            (
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":3,"message":"0x"}}"#,
                true,
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"gas required exceeds allowance (16777216)"}}"#,
                false,
            ),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let rpc = Rpc::new(vec![format!("http://{}", listener.local_addr().unwrap())]).unwrap();
            let server = tokio::spawn(serve(listener, Duration::ZERO, answer));
            let error = rpc.estimate_gas(json!({})).await.unwrap_err();
            assert!(is_node_error_response(&error));
            assert_eq!(is_revert(&error), reverts, "{answer}");
            server.abort();
        }
        // Nobody answered on a closed port: still a delivery failure, but no node response.
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let unreachable = Rpc::new(vec![url])
            .unwrap()
            .request("eth_estimateGas", json!([{}]))
            .await
            .unwrap_err();
        assert!(is_delivery_failure(&unreachable));
        assert!(!is_node_error_response(&unreachable));
        assert!(!is_node_error_response(&anyhow::anyhow!("protocol error")));
    }
    #[tokio::test]
    async fn single_slow_endpoint_is_still_attempted_with_the_runtime_budget_while_cooling() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rpc = Rpc::new(vec![format!("http://{}", listener.local_addr().unwrap())]).unwrap();
        let server = tokio::spawn(serve_chain_id(listener, Duration::from_millis(1200)));
        // A prior full-attempt timeout must not turn the only endpoint into a hard outage.
        rpc.cooldowns.lock().unwrap()[0] =
            Some(tokio::time::Instant::now() + Duration::from_secs(5));
        let checked = rpc.for_runtime_checks();
        for _ in 0..2 {
            let started = tokio::time::Instant::now();
            assert_eq!(
                checked.request("eth_chainId", json!([])).await.unwrap(),
                json!("0x1")
            );
            assert!(started.elapsed() < Duration::from_secs(3));
        }
        server.abort();
    }
    #[tokio::test]
    async fn endpoints_slower_than_one_attempt_keep_passing_runtime_checks_in_any_position() {
        // A hung primary makes a ~1 s fallback preferred; the fallback then sits first.
        let hung = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let slow = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rpc = Rpc::new(vec![
            format!("http://{}", hung.local_addr().unwrap()),
            format!("http://{}", slow.local_addr().unwrap()),
        ])
        .unwrap();
        let primary = tokio::spawn(async move {
            let mut connections = Vec::new();
            loop {
                connections.push(hung.accept().await.unwrap().0);
            }
        });
        let fallback = tokio::spawn(serve_chain_id(slow, Duration::from_millis(1000)));
        let checked = rpc.for_runtime_checks();
        for _ in 0..3 {
            let values = tokio::time::timeout(
                Duration::from_secs(3),
                futures_util::future::try_join_all(
                    (0..7).map(|_| checked.request("eth_chainId", json!([]))),
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(values, vec![json!("0x1"); 7]);
        }
        assert_eq!(rpc.active.load(Ordering::Relaxed), 1);
        primary.abort();
        fallback.abort();
        // Three healthy endpoints at 900 ms: each is slower than one 800 ms attempt.
        let mut servers = Vec::new();
        let mut urls = Vec::new();
        for _ in 0..3 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            urls.push(format!("http://{}", listener.local_addr().unwrap()));
            servers.push(tokio::spawn(serve_chain_id(
                listener,
                Duration::from_millis(900),
            )));
        }
        let checked = Rpc::new(urls).unwrap().for_runtime_checks();
        for _ in 0..3 {
            let started = tokio::time::Instant::now();
            assert_eq!(
                checked.request("eth_chainId", json!([])).await.unwrap(),
                json!("0x1")
            );
            assert!(started.elapsed() < Duration::from_secs(3));
        }
        for server in servers {
            server.abort();
        }
    }
    #[tokio::test]
    async fn hung_primary_reaches_fallback_within_runtime_budget_and_stays_cooled() {
        let hung = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let healthy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let urls = vec![
            format!("http://{}", hung.local_addr().unwrap()),
            format!("http://{}", healthy.local_addr().unwrap()),
        ];
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let counter = primary_calls.clone();
        let primary = tokio::spawn(async move {
            let mut connections = Vec::new();
            loop {
                let (socket, _) = hung.accept().await.unwrap();
                counter.fetch_add(1, Ordering::SeqCst);
                connections.push(socket);
            }
        });
        let secondary = tokio::spawn(serve_chain_id(healthy, Duration::ZERO));
        let rpc = Rpc::new(urls).unwrap();
        let checked = rpc.for_runtime_checks();
        for _ in 0..5 {
            let values = tokio::time::timeout(Duration::from_secs(3), async {
                futures_util::future::try_join_all(
                    (0..7).map(|_| checked.request("eth_chainId", json!([]))),
                )
                .await
            })
            .await
            .unwrap()
            .unwrap();
            assert_eq!(values, vec![json!("0x1"); 7]);
        }
        assert!(primary_calls.load(Ordering::SeqCst) <= 7);
        assert_eq!(rpc.active.load(Ordering::Relaxed), 1);
        // Timeout changes the shared endpoint choice, not finality or identity validation.
        assert_eq!(
            rpc.request("eth_chainId", json!([])).await.unwrap(),
            json!("0x1")
        );
        primary.abort();
        secondary.abort();
    }
    #[tokio::test]
    async fn finalized_reads_ignore_unfinal_requests_and_orphaned_receipts() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let phase = Arc::new(AtomicUsize::new(0));
        let state = phase.clone();
        let block_a = B256::repeat_byte(0xaa);
        let block_b = B256::repeat_byte(0xbb);
        let hash = B256::repeat_byte(0x77);
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut data = Vec::new();
                let mut buf = [0; 4096];
                let body = loop {
                    let size = stream.read(&mut buf).await.unwrap();
                    if size == 0 {
                        break None;
                    }
                    data.extend_from_slice(&buf[..size]);
                    if let Some(end) = data.windows(4).position(|b| b == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&data[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (k, v) = line.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        if data.len() >= end + 4 + length {
                            break Some(
                                serde_json::from_slice::<Value>(&data[end + 4..end + 4 + length])
                                    .unwrap(),
                            );
                        }
                    }
                };
                let Some(body) = body else { continue };
                let n = state.load(Ordering::SeqCst);
                let result = match body["method"].as_str().unwrap() {
                    "eth_getBlockByNumber" => {
                        let finalized = body["params"][0] == "finalized";
                        let number = if finalized {
                            if n == 0 {
                                9
                            } else if n == 1 {
                                10
                            } else {
                                11
                            }
                        } else {
                            10
                        };
                        let h = if number == 10 && n == 2 {
                            block_b
                        } else {
                            block_a
                        };
                        json!({"number":format!("0x{number:x}"),"hash":h,"timestamp":"0x64","baseFeePerGas":"0x1"})
                    }
                    "eth_call" => {
                        let tag = body["params"][1].as_str().unwrap();
                        assert!(tag == "finalized" || tag == "0x9");
                        json!(format!(
                            "0x{:064x}",
                            if n == 0 || tag == "0x9" { 1u64 } else { 2u64 }
                        ))
                    }
                    _ => panic!("Unexpected RPC method"),
                };
                let response =
                    serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,"result":result})).unwrap();
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.len()
                );
                stream.write_all(header.as_bytes()).await.unwrap();
                stream.write_all(&response).await.unwrap();
            }
        });
        let rpc = Rpc::new(vec![url]).unwrap();
        let receipt =
            json!({"transactionHash":hash,"blockNumber":"0xa","blockHash":block_a,"status":"0x1"});
        assert!(
            !rpc.receipt_is_finalized(&hash.to_string(), &receipt)
                .await
                .unwrap()
        );
        assert_eq!(
            rpc.call(Address::ZERO, crate::abi::Coordinator::nextRequestIdCall {})
                .await
                .unwrap(),
            U256::from(1)
        );
        phase.store(1, Ordering::SeqCst);
        assert!(
            rpc.receipt_is_finalized(&hash.to_string(), &receipt)
                .await
                .unwrap()
        );
        assert_eq!(
            rpc.call(Address::ZERO, crate::abi::Coordinator::nextRequestIdCall {})
                .await
                .unwrap(),
            U256::from(2)
        );
        assert_eq!(
            rpc.call_at(
                Address::ZERO,
                crate::abi::Coordinator::nextRequestIdCall {},
                9
            )
            .await
            .unwrap(),
            U256::from(1)
        );
        phase.store(2, Ordering::SeqCst);
        assert!(
            !rpc.receipt_is_finalized(&hash.to_string(), &receipt)
                .await
                .unwrap()
        );
        assert!(
            rpc.receipt_is_finalized(&B256::ZERO.to_string(), &receipt)
                .await
                .is_err()
        );
        task.abort();
    }
    #[test]
    fn deterministic_rejections_are_node_local_and_do_not_claim_inclusion() {
        for message in [
            "insufficient funds for gas * price + value",
            "Sender doesn't have enough funds to send tx. The max upfront cost is: 100 and the sender's balance is: 0.",
            "transaction underpriced",
            "intrinsic gas too low",
            "invalid chain id",
            "invalid signature",
            "invalid sender",
            "replacement transaction underpriced",
        ] {
            let reason = rejection(message).expect("known deterministic rejection");
            assert_eq!(
                broadcast_result(
                    Err(DeliveryError {
                        rejection: Some(reason),
                        known: false,
                        known_hash: None,
                        responded: true,
                        reverted: false,
                        rate_limited: false,
                        malformed: false,
                        code: None,
                        batch_refused: false,
                        detail: "node rejection".into()
                    }
                    .into()),
                    B256::ZERO
                )
                .unwrap(),
                BroadcastOutcome::Rejected(reason)
            );
        }
        for message in [
            "nonce too low",
            "already known",
            "timeout",
            "unknown transaction",
            "not insufficient funds",
        ] {
            assert_eq!(rejection(message), None);
        }
    }
    #[test]
    fn broadcast_classification_preserves_internal_failures() {
        for message in ["already known", "Known transaction", " ALREADY KNOWN "] {
            assert!(known_transaction(message).0);
        }
        assert!(!known_transaction("unknown transaction").0);
        assert!(!known_transaction("Known transaction: 0x123").0);
        let hash_message = format!("Known transaction: {}", B256::ZERO);
        assert_eq!(known_transaction(&hash_message), (true, Some(B256::ZERO)));
        assert!(!known_transaction("Known transaction: 0xzz").0);
        for hash in [B256::ZERO, B256::repeat_byte(1)] {
            let result = broadcast_result(
                Err(DeliveryError {
                    rejection: None,
                    known: true,
                    known_hash: Some(hash),
                    responded: true,
                    reverted: false,
                    rate_limited: false,
                    malformed: false,
                    code: Some(-32000),
                    batch_refused: false,
                    detail: "JSON-RPC code -32000".into(),
                }
                .into()),
                B256::ZERO,
            );
            if hash == B256::ZERO {
                assert_eq!(result.unwrap(), BroadcastOutcome::AlreadyKnown);
            } else {
                assert!(result.is_err());
            }
        }
        assert_eq!(
            broadcast_result(
                Err(DeliveryError {
                    rejection: None,
                    known: true,
                    known_hash: None,
                    responded: true,
                    reverted: false,
                    rate_limited: false,
                    malformed: false,
                    code: Some(-32000),
                    batch_refused: false,
                    detail: "JSON-RPC code -32000".into()
                }
                .into()),
                B256::ZERO
            )
            .unwrap(),
            BroadcastOutcome::AlreadyKnown
        );
        assert_eq!(
            broadcast_result(Err(uncertain("transport").into()), B256::ZERO).unwrap(),
            BroadcastOutcome::Ambiguous
        );
        assert_eq!(
            broadcast_result(Ok(json!(B256::ZERO)), B256::ZERO).unwrap(),
            BroadcastOutcome::Acknowledged
        );
        assert!(broadcast_result(Ok(json!("invalid")), B256::ZERO).is_err());
        assert!(broadcast_result(Ok(json!(B256::repeat_byte(1))), B256::ZERO).is_err());
        assert!(broadcast_result(Err(anyhow::anyhow!("parse failure")), B256::ZERO).is_err());
    }
    type Handler = Arc<dyn Fn(&Value) -> (u16, Value) + Send + Sync>;
    /// An HTTP JSON-RPC endpoint whose every answer comes from `handler`, counting the requests it received.
    async fn endpoint(handler: Handler) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let handler = handler.clone();
                let counter = counter.clone();
                tokio::spawn(async move {
                    let mut data = Vec::new();
                    let mut buffer = [0; 4096];
                    let body = loop {
                        let Ok(size) = socket.read(&mut buffer).await else {
                            return;
                        };
                        if size == 0 {
                            return;
                        }
                        data.extend_from_slice(&buffer[..size]);
                        if let Some(end) = data.windows(4).position(|b| b == b"\r\n\r\n") {
                            let length = String::from_utf8_lossy(&data[..end])
                                .lines()
                                .find_map(|line| {
                                    let (k, v) = line.split_once(':')?;
                                    k.eq_ignore_ascii_case("content-length")
                                        .then(|| v.trim().parse::<usize>().unwrap())
                                })
                                .unwrap();
                            if data.len() >= end + 4 + length {
                                break serde_json::from_slice::<Value>(
                                    &data[end + 4..end + 4 + length],
                                )
                                .unwrap();
                            }
                        }
                    };
                    counter.fetch_add(1, Ordering::SeqCst);
                    let (status, answer) = handler(&body);
                    let answer = serde_json::to_vec(&answer).unwrap();
                    let header = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        answer.len()
                    );
                    let _ = socket.write_all(header.as_bytes()).await;
                    let _ = socket.write_all(&answer).await;
                });
            }
        });
        (url, hits, task)
    }
    fn result(call: &Value, result: Value) -> Value {
        json!({"jsonrpc":"2.0","id":call["id"],"result":result})
    }
    /// An endpoint answering every call, alone or in a batch, with the result `answer` gives it. Hits count HTTP
    /// requests, so a batch is one.
    async fn answering(
        answer: impl Fn(&Value) -> Value + Send + Sync + 'static,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let answer = Arc::new(answer);
        endpoint(Arc::new(move |body| {
            let reply = |call: &Value| result(call, answer(call));
            match body.as_array() {
                Some(calls) => (200, Value::Array(calls.iter().map(reply).collect())),
                None => (200, reply(body)),
            }
        }))
        .await
    }
    fn block(number: u64, hash: u8) -> Value {
        json!({"number":format!("0x{number:x}"),"hash":B256::repeat_byte(hash),"timestamp":"0x64","baseFeePerGas":"0x7"})
    }
    #[test]
    fn blocks_need_every_field_and_quantities_need_hex_digits() {
        let head = Head::from_block(&block(16, 1)).unwrap();
        assert_eq!((head.number, head.timestamp, head.base_fee), (16, 100, 7));
        // A missing base fee is no base fee, not zero: a send priced from zero would carry its tip alone.
        for field in ["number", "hash", "timestamp", "baseFeePerGas"] {
            let mut partial = block(16, 1);
            partial.as_object_mut().unwrap().remove(field);
            assert!(Head::from_block(&partial).is_err(), "{field}");
        }
        assert!(Head::from_block(&Value::Null).is_err());
        for (hex, value) in [("0x0", 0), ("0x1", 1), ("0x01", 1), ("0xfF", 255)] {
            assert_eq!(quantity(&json!(hex)).unwrap(), value);
        }
        for garbage in [
            json!(null),
            json!(16),
            json!(""),
            json!("0x"),
            json!("10"),
            json!("0xzz"),
            json!("0x1_0"),
            json!(" 0x1"),
            json!("0X1"),
            json!(format!("0x{}", "0".repeat(65))),
            json!("0x10000000000000000"),
        ] {
            assert!(quantity(&garbage).is_err(), "{garbage}");
        }
    }
    #[tokio::test]
    async fn an_unusable_answer_fails_its_endpoint_and_the_read_moves_on() {
        // Well-formed JSON-RPC with unusable results: null at the finalized tag, as an endpoint that does not serve
        // that tag answers, "0x" nonces, a latest block without its base fee and a null gas estimate.
        let (unusable, unusable_hits, a) = answering(|call| {
            match (call["method"].as_str().unwrap(), call["params"][0].as_str()) {
                ("eth_getBlockByNumber", Some("finalized")) => Value::Null,
                ("eth_getBlockByNumber", _) => {
                    json!({"number":"0x9","hash":B256::repeat_byte(9),"timestamp":"0x64"})
                }
                ("eth_getTransactionCount", _) => json!("0x"),
                _ => Value::Null,
            }
        })
        .await;
        let (healthy, _, b) = answering(|call| match call["method"].as_str().unwrap() {
            "eth_getBlockByNumber" => block(8, 8),
            "eth_getTransactionCount" => json!("0x5"),
            "eth_estimateGas" => json!("0x5208"),
            _ => Value::Null,
        })
        .await;
        let fresh = || Rpc::new(vec![unusable.clone(), healthy.clone()]).unwrap();
        let rpc = fresh();
        assert_eq!(rpc.finalized_head().await.unwrap().number, 8);
        // The endpoint that answered null is cooled, and reads now start at the one that answered.
        assert_eq!(rpc.active.load(Ordering::Relaxed), 1);
        assert!(
            rpc.cooldowns.lock().unwrap()[0]
                .is_some_and(|until| until > tokio::time::Instant::now())
        );
        for _ in 0..3 {
            assert_eq!(rpc.finalized_head().await.unwrap().number, 8);
        }
        assert_eq!(unusable_hits.load(Ordering::SeqCst), 1);
        // Every typed read moves on the same way; each fresh client asks the unusable endpoint first.
        assert_eq!(fresh().nonce(Address::ZERO, "latest").await.unwrap(), 5);
        assert_eq!(fresh().head().await.unwrap().base_fee, 7);
        assert_eq!(fresh().estimate_gas(json!({})).await.unwrap(), 21_000);
        assert_eq!(unusable_hits.load(Ordering::SeqCst), 4);
        // With only unusable answers the read fails as a delivery failure, neither a rate limit nor an error a node
        // answered: the caller tries again later and judges nothing about the chain from it.
        let only = Rpc::new(vec![unusable.clone()]).unwrap();
        for error in [
            only.finalized_head().await.unwrap_err(),
            only.nonce(Address::ZERO, "latest").await.unwrap_err(),
            only.head().await.unwrap_err(),
        ] {
            assert!(
                is_delivery_failure(&error)
                    && !is_rate_limited(&error)
                    && !is_node_error_response(&error),
                "{error:#}"
            );
            assert!(
                format!("{error:#}").contains("RPC answer unusable"),
                "{error:#}"
            );
        }
        // Beside a rate-limited endpoint an unusable one still fails the read as a fault, not as a rate limit.
        let (limited, _, c) = endpoint(Arc::new(|_| (429, json!({})))).await;
        let error = Rpc::new(vec![limited, unusable])
            .unwrap()
            .finalized_head()
            .await
            .unwrap_err();
        assert!(is_delivery_failure(&error) && !is_rate_limited(&error));
        for task in [a, b, c] {
            task.abort();
        }
    }
    #[tokio::test]
    async fn a_receipt_the_first_endpoint_does_not_serve_yet_comes_from_another() {
        let hash = B256::repeat_byte(7);
        let receipt = json!({"transactionHash":hash,"blockHash":B256::repeat_byte(3),"blockNumber":"0x9","status":"0x1"});
        // The first endpoint has not caught up with the receipt's block, the second answers with another
        // transaction's receipt, and the third serves it.
        let (behind, _, a) = answering(|_| Value::Null).await;
        let (other, _, b) = answering(|_| {
            json!({"transactionHash":B256::repeat_byte(8),"blockHash":B256::repeat_byte(3),"blockNumber":"0x9","status":"0x1"})
        })
        .await;
        // It knows only `hash`: every other transaction's receipt is null there.
        let served = receipt.clone();
        let (current, current_hits, c) = answering(move |call| {
            if call["params"][0] == json!(hash) {
                served.clone()
            } else {
                Value::Null
            }
        })
        .await;
        let rpc = Rpc::new(vec![behind.clone(), other.clone(), current]).unwrap();
        // A read takes the first endpoint's null as its answer.
        assert_eq!(rpc.receipt(&hash.to_string()).await.unwrap(), None);
        // Every attempt of a nonce is asked for at once: one batch per endpoint, and the index of the
        // attempt whose receipt was found.
        let attempts = [B256::repeat_byte(6).to_string(), hash.to_string()];
        assert_eq!(
            rpc.receipt_from_any(&attempts).await.unwrap(),
            Some((1, receipt))
        );
        assert_eq!(current_hits.load(Ordering::SeqCst), 1);
        // No endpoint serving a receipt is no receipt, not a failure.
        let none = Rpc::new(vec![behind, other]).unwrap();
        assert_eq!(none.receipt_from_any(&attempts).await.unwrap(), None);
        // An endpoint that does not answer is waited for only RECEIPT_SEARCH.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hung = format!("http://{}", listener.local_addr().unwrap());
        let d = tokio::spawn(serve(
            listener,
            Duration::from_secs(30),
            r#"{"jsonrpc":"2.0","id":0,"result":null}"#,
        ));
        let began = tokio::time::Instant::now();
        assert_eq!(
            Rpc::new(vec![hung])
                .unwrap()
                .receipt_from_any(&attempts)
                .await
                .unwrap(),
            None
        );
        assert!(began.elapsed() < RECEIPT_SEARCH + Duration::from_millis(500));
        for task in [a, b, c, d] {
            task.abort();
        }
    }
    #[tokio::test]
    async fn the_finalized_head_and_its_checkpoint_come_from_one_usable_answer() {
        // Each endpoint answers the checkpoint block with its own hash. The first does not serve the finalized tag,
        // the second has not served the checkpoint block yet, the third answers it with another block.
        let (unfinalized, unfinalized_hits, a) =
            answering(|call| match call["params"][0].as_str() {
                Some("finalized") => Value::Null,
                _ => block(5, 0xbb),
            })
            .await;
        let (lagging, lagging_hits, b) = answering(|call| match call["params"][0].as_str() {
            Some("finalized") => block(4, 0xcc),
            _ => Value::Null,
        })
        .await;
        let (confused, confused_hits, c) = answering(|call| match call["params"][0].as_str() {
            Some("finalized") => block(8, 0xdd),
            _ => block(6, 0xdd),
        })
        .await;
        let (healthy, healthy_hits, d) = answering(|call| match call["params"][0].as_str() {
            Some("finalized") => block(8, 0x88),
            _ => block(5, 0xaa),
        })
        .await;
        let rpc = Rpc::new(vec![
            unfinalized.clone(),
            lagging,
            confused,
            healthy.clone(),
        ])
        .unwrap();
        let (head, checkpoint) = rpc.finalized_head_with(Some(5)).await.unwrap();
        // Both blocks are the healthy endpoint's: no other endpoint's checkpoint is combined with its head.
        assert_eq!((head.number, head.hash), (8, B256::repeat_byte(0x88)));
        assert_eq!(checkpoint.unwrap().hash, B256::repeat_byte(0xaa));
        for hits in [unfinalized_hits, lagging_hits, confused_hits, healthy_hits] {
            assert_eq!(hits.load(Ordering::SeqCst), 1, "one batched request each");
        }
        let (head, checkpoint) = Rpc::new(vec![unfinalized, healthy])
            .unwrap()
            .finalized_head_with(None)
            .await
            .unwrap();
        assert_eq!((head.number, checkpoint.is_none()), (8, true));
        for task in [a, b, c, d] {
            task.abort();
        }
    }
    #[test]
    fn rate_limits_are_recognized_by_code_or_wording_only() {
        for (code, message) in [
            (429, "anything"),
            (-32005, "daily request count exceeded, request rate limited"),
            (-32029, "slow down"),
            (-32000, "Too Many Requests"),
            (
                -32000,
                "Your app has exceeded its compute units per second capacity",
            ),
            (-32603, "rate limit exceeded"),
        ] {
            assert!(rate_limit_response(code, message), "{code} {message}");
        }
        for (code, message) in [
            (-32005, "query returned more than 10000 results"),
            (-32000, "execution reverted"),
            (-32000, "header not found"),
            (-32602, "invalid argument"),
        ] {
            assert!(!rate_limit_response(code, message), "{code} {message}");
        }
        for strikes in 1..=12 {
            let backoff = rate_limit_backoff(strikes);
            let base =
                Duration::from_secs(1u64 << (strikes - 1).min(5)).min(Duration::from_secs(30));
            assert!(
                backoff >= base / 2 && backoff <= base,
                "{strikes}: {backoff:?}"
            );
        }
    }
    #[tokio::test]
    async fn a_rate_limited_endpoint_is_skipped_during_its_backoff_and_never_counts_as_a_fault() {
        let (limited, limited_hits, a) = endpoint(Arc::new(|_| {
            (
                429,
                json!({"jsonrpc":"2.0","id":1,"error":{"code":429,"message":"rate limit exceeded"}}),
            )
        }))
        .await;
        let (healthy, healthy_hits, b) =
            endpoint(Arc::new(|call| (200, result(call, json!("0x1"))))).await;
        let rpc = Rpc::new(vec![limited.clone(), healthy]).unwrap();
        for _ in 0..6 {
            assert_eq!(
                rpc.request("eth_chainId", json!([])).await.unwrap(),
                json!("0x1")
            );
        }
        // One 429 moved every later read to the other endpoint for the back-off.
        assert_eq!(limited_hits.load(Ordering::SeqCst), 1);
        assert_eq!(healthy_hits.load(Ordering::SeqCst), 6);
        // With nothing else to use, the failure says "rate limited", and a read inside the back-off sends nothing.
        let only = Rpc::new(vec![limited.clone()]).unwrap();
        let first = only.request("eth_chainId", json!([])).await.unwrap_err();
        assert!(is_rate_limited(&first) && is_delivery_failure(&first));
        let second = only.request("eth_chainId", json!([])).await.unwrap_err();
        assert!(is_rate_limited(&second));
        assert_eq!(limited_hits.load(Ordering::SeqCst), 2);
        let batch = only.batch(&[("eth_chainId", json!([]))]).await.unwrap_err();
        assert!(is_rate_limited(&batch));
        assert_eq!(limited_hits.load(Ordering::SeqCst), 2);
        // The back-off ends and the endpoint is asked again.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(is_rate_limited(
            &only.request("eth_chainId", json!([])).await.unwrap_err()
        ));
        assert_eq!(limited_hits.load(Ordering::SeqCst), 3);
        // Any other failure is not a rate limit, even next to one.
        let (broken, _, c) = endpoint(Arc::new(|_| (500, json!({})))).await;
        let mixed = Rpc::new(vec![broken, limited]).unwrap();
        let error = mixed.request("eth_chainId", json!([])).await.unwrap_err();
        assert!(!is_rate_limited(&error) && is_delivery_failure(&error));
        for task in [a, b, c] {
            task.abort();
        }
    }
    #[tokio::test]
    async fn batches_answer_in_call_order_from_one_endpoint_or_fall_back_to_single_calls() {
        let echo = |call: &Value| result(call, json!(call["params"][0]));
        // Answers a batch in reverse order: ids, not positions, place each result.
        let (batching, batching_hits, a) = endpoint(Arc::new(move |body| match body.as_array() {
            Some(calls) => (200, Value::Array(calls.iter().rev().map(echo).collect())),
            None => (200, echo(body)),
        }))
        .await;
        let calls: Vec<(&str, Value)> = (0..5).map(|i| ("eth_test", json!([i]))).collect();
        let expected: Vec<Value> = (0..5).map(|i| json!(i)).collect();
        let rpc = Rpc::new(vec![batching]).unwrap();
        assert_eq!(rpc.batch(&calls).await.unwrap(), expected);
        assert_eq!(batching_hits.load(Ordering::SeqCst), 1);
        // An endpoint that refuses batches is asked one call at a time, and that is remembered.
        let (single, single_hits, b) = endpoint(Arc::new(move |body| {
            if body.is_array() {
                (
                    200,
                    json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"batch requests are not supported"}}),
                )
            } else {
                (200, echo(body))
            }
        }))
        .await;
        let rpc = Rpc::new(vec![single]).unwrap();
        for _ in 0..2 {
            assert_eq!(rpc.batch(&calls).await.unwrap(), expected);
        }
        // The error names batches: the five are refused, then two, and the calls go one by one from then on.
        assert_eq!(single_hits.load(Ordering::SeqCst), 2 + 5 + 5);
        // One failed element fails that endpoint's whole answer; the next endpoint answers the whole batch.
        let (partial, _, c) = endpoint(Arc::new(move |body| {
            let items = body.as_array().unwrap().iter().map(|call| {
                if call["params"][0] == json!(3) {
                    json!({"jsonrpc":"2.0","id":call["id"],"error":{"code":-32000,"message":"header not found"}})
                } else {
                    echo(call)
                }
            });
            (200, Value::Array(items.collect()))
        }))
        .await;
        let (whole, _, d) = endpoint(Arc::new(move |body| {
            (
                200,
                Value::Array(body.as_array().unwrap().iter().map(echo).collect()),
            )
        }))
        .await;
        let rpc = Rpc::new(vec![partial, whole]).unwrap();
        assert_eq!(rpc.batch(&calls).await.unwrap(), expected);
        assert_eq!(rpc.active.load(Ordering::Relaxed), 1);
        for task in [a, b, c, d] {
            task.abort();
        }
    }

    #[test]
    fn provider_errors_defer_a_round_keepers_tick_and_anything_else_counts() {
        use crate::config::CoordinatorKind::{Epoch, Round};
        use crate::worker::FailedTick;
        let node = |code: i64, message: &str| {
            Rpc::outcome(&json!({"jsonrpc":"2.0","id":1,"error":{"code":code,"message":message}}))
                .unwrap_err()
        };
        let read = |error: anyhow::Error| {
            error.context("All configured RPC endpoints failed eth_call batch")
        };
        let http = |status: u16| -> anyhow::Error {
            DeliveryError {
                batch_refused: false,
                ..uncertain(format!("HTTP status {status}"))
            }
            .into()
        };
        let provider = [
            read(http(500)),
            read(http(503)),
            read(uncertain("transport").into()),
            read(uncertain("read attempt timed out").into()),
            read(node(-32000, "server busy")),
            read(node(-32000, "header not found")),
            read(node(-32000, "server busy")).context("Proxy runtime could not be verified"),
        ];
        for error in &provider {
            assert!(is_provider_failure(error), "{error:#}");
            assert_eq!(
                FailedTick::of(Round, error),
                FailedTick::ProviderUnavailable,
                "{error:#}"
            );
            // An epoch keeper counts them as it always has.
            assert_eq!(
                FailedTick::of(Epoch, error),
                FailedTick::Counted,
                "{error:#}"
            );
        }
        let counted = [
            read(malformed("eth_call answer: not hex").into()),
            read(node(3, "execution reverted")),
            read(node(-32000, "already known")),
            read(node(-32601, "method not found")),
            anyhow::anyhow!("Proxy runtime changed: implementation slot moved"),
            anyhow::anyhow!("RPC chain id 1 differs from CHAIN_ID 46630"),
        ];
        for error in &counted {
            assert!(!is_provider_failure(error), "{error:#}");
            assert_eq!(
                FailedTick::of(Round, error),
                FailedTick::Counted,
                "{error:#}"
            );
        }
        // A read that failed on rate limits is deferred as one in either mode: every endpoint backing off, or the last
        // answer a rate limit.
        for limited in [
            Rpc::all_limited_error("eth_call batch"),
            read(rate_limited("HTTP status 429").into()),
        ] {
            assert!(is_provider_failure(&limited));
            for kind in [Round, Epoch] {
                assert_eq!(FailedTick::of(kind, &limited), FailedTick::RateLimited);
            }
        }
    }

    #[tokio::test]
    async fn a_batch_an_endpoint_refuses_for_its_size_is_asked_again_in_halves_on_that_endpoint() {
        let echo = |call: &Value| result(call, json!(call["params"][0]));
        let calls: Vec<(&str, Value)> = (0..8).map(|i| ("eth_test", json!([i]))).collect();
        let expected: Vec<Value> = (0..8).map(|i| json!(i)).collect();
        // A free plan: a batch of more than three is refused with HTTP 500 and an error that names batches.
        let (free, free_hits, a) = endpoint(Arc::new(move |body| match body.as_array() {
            Some(calls) if calls.len() > 3 => (
                500,
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Batch of more than 3 requests are not allowed on free plan"}}),
            ),
            Some(calls) => (200, Value::Array(calls.iter().map(echo).collect())),
            None => (200, echo(body)),
        }))
        .await;
        let (other, other_hits, b) = answering(|call| json!(call["params"][0])).await;
        let rpc = Rpc::new(vec![free, other]).unwrap();
        // Eight are refused, then four; four chunks of two answer, on the same endpoint. The refusals are no failure: the
        // endpoint is not cooled, no rate limit is noted, and the other endpoint is not asked.
        assert_eq!(rpc.batch(&calls).await.unwrap(), expected);
        assert_eq!(free_hits.load(Ordering::SeqCst), 2 + 4);
        assert_eq!(other_hits.load(Ordering::SeqCst), 0);
        assert!(!rpc.cooling(0));
        assert_eq!(rpc.limits.lock().unwrap()[0], (0, None));
        assert_eq!(rpc.active.load(Ordering::Relaxed), 0);
        // The size it answered is kept: the next batch goes in chunks of two at once, and a batch of two whole.
        assert_eq!(rpc.batch(&calls).await.unwrap(), expected);
        assert_eq!(free_hits.load(Ordering::SeqCst), 6 + 4);
        assert_eq!(rpc.batch(&calls[..2]).await.unwrap(), expected[..2]);
        assert_eq!(free_hits.load(Ordering::SeqCst), 10 + 1);
        // So is it on a clone of the endpoints, as the runtime checks use.
        assert_eq!(
            rpc.for_runtime_checks().batch(&calls[..4]).await.unwrap(),
            expected[..4]
        );
        assert_eq!(free_hits.load(Ordering::SeqCst), 11 + 2);
        // A member that names batches in a JSON-RPC answer refuses the batch as well.
        let (members, members_hits, c) = endpoint(Arc::new(move |body| match body.as_array() {
            Some(calls) if calls.len() > 1 => (
                200,
                Value::Array(calls.iter().map(|call| json!({"jsonrpc":"2.0","id":call["id"],"error":{"code":-32000,"message":"batch too large"}})).collect()),
            ),
            Some(calls) => (200, Value::Array(calls.iter().map(echo).collect())),
            None => (200, echo(body)),
        }))
        .await;
        let rpc = Rpc::new(vec![members]).unwrap();
        assert_eq!(rpc.batch(&calls[..4]).await.unwrap(), expected[..4]);
        // Four, then two, are refused; four calls one by one.
        assert_eq!(members_hits.load(Ordering::SeqCst), 2 + 4);
        assert_eq!(rpc.batch_limits.lock().unwrap()[0], 1);
        // An HTTP error that does not name batches, and a rate limit, are the endpoint's answer to any batch: the read
        // fails over to the next endpoint without a smaller chunk being asked.
        for (status, body) in [(503, json!({})), (429, json!({}))] {
            let (down, down_hits, d) = endpoint(Arc::new(move |_| (status, body.clone()))).await;
            let (up, up_hits, e) = answering(|call| json!(call["params"][0])).await;
            let rpc = Rpc::new(vec![down, up]).unwrap();
            assert_eq!(rpc.batch(&calls).await.unwrap(), expected, "{status}");
            assert_eq!(
                (
                    down_hits.load(Ordering::SeqCst),
                    up_hits.load(Ordering::SeqCst)
                ),
                (1, 1),
                "{status}"
            );
            assert_eq!(rpc.batch_limits.lock().unwrap()[0], usize::MAX, "{status}");
            d.abort();
            e.abort();
        }
        // A revert that names a batch is the call's answer, not a refusal.
        let (reverting, reverting_hits, f) = endpoint(Arc::new(move |body| {
            (
                200,
                Value::Array(body.as_array().unwrap().iter().map(|call| json!({"jsonrpc":"2.0","id":call["id"],"error":{"code":3,"message":"execution reverted: batch member"}})).collect()),
            )
        }))
        .await;
        let rpc = Rpc::new(vec![reverting]).unwrap();
        assert!(rpc.batch(&calls).await.is_err());
        assert_eq!(reverting_hits.load(Ordering::SeqCst), 1);
        for task in [a, b, c, f] {
            task.abort();
        }
    }

    /// What a read asked of a node, in the order it asked it: `method tag`, for a block, a call and a nonce.
    type Asked = Arc<Mutex<Vec<String>>>;
    /// An endpoint serving a chain whose latest block is `latest` and whose finalized block is `finalized`. A block's
    /// hash is its number's low byte xor `salt`: endpoints on one chain agree, and a fork differs. A number past the
    /// latest block is not served yet (null). A call and a nonce answer with the number of the block they are read at,
    /// 1 at `latest` and 2 at `finalized`. Hits count HTTP requests, so a batch is one.
    async fn chain_node(
        latest: u64,
        finalized: u64,
        salt: u8,
    ) -> (String, Arc<AtomicUsize>, Asked, tokio::task::JoinHandle<()>) {
        let asked = Asked::default();
        let log = asked.clone();
        let (url, hits, task) = answering(move |call| {
            let method = call["method"].as_str().unwrap();
            let tag = match method {
                "eth_getBlockByNumber" => &call["params"][0],
                _ => &call["params"][1],
            }
            .as_str()
            .unwrap();
            log.lock().unwrap().push(format!("{method} {tag}"));
            let number = match tag {
                "latest" => Some(latest),
                "finalized" => Some(finalized),
                number => u64::from_str_radix(number.trim_start_matches("0x"), 16).ok(),
            };
            let read_at = match tag {
                "latest" => 1,
                "finalized" => 2,
                _ => number.unwrap(),
            };
            match method {
                "eth_getBlockByNumber" => number
                    .filter(|number| *number <= latest)
                    .map_or(Value::Null, |number| block(number, number as u8 ^ salt)),
                "eth_getTransactionCount" => json!(format!("0x{read_at:x}")),
                _ => json!(format!("0x{read_at:064x}")),
            }
        })
        .await;
        (url, hits, asked, task)
    }
    fn soft(url: &str, depth: u64) -> Rpc {
        Rpc::new(vec![url.to_owned()])
            .unwrap()
            .with_finality(FinalityMode::Soft, depth)
    }
    fn asked(asked: &Asked) -> Vec<String> {
        std::mem::take(&mut *asked.lock().unwrap())
    }
    #[tokio::test]
    async fn soft_mode_decides_on_the_latest_block_and_finalized_mode_on_the_finalized_one() {
        let (url, hits, log, node) = chain_node(20, 8, 0).await;
        let rpc = soft(&url, 0);
        assert_eq!(rpc.finality(), FinalityMode::Soft);
        let head = rpc.decision_head().await.unwrap();
        assert_eq!((head.number, head.hash), (20, B256::repeat_byte(20)));
        assert_eq!(asked(&log), ["eth_getBlockByNumber latest"]);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // The finalized head stays what it is in soft mode: the block the audit compares the decisions against.
        assert_eq!(rpc.finalized_head().await.unwrap().number, 8);
        assert_eq!(asked(&log), ["eth_getBlockByNumber finalized"]);

        // A client that has not been told a mode is a finalized one: exactly the finalized head, a depth means nothing.
        for finalized in [
            Rpc::new(vec![url.clone()]).unwrap(),
            Rpc::new(vec![url.clone()])
                .unwrap()
                .with_finality(FinalityMode::Finalized, 5),
        ] {
            assert_eq!(finalized.finality(), FinalityMode::Finalized);
            assert_eq!(finalized.decision_head().await.unwrap().number, 8);
            assert_eq!(asked(&log), ["eth_getBlockByNumber finalized"]);
        }
        node.abort();
    }
    #[tokio::test]
    async fn a_soft_depth_takes_the_block_below_the_latest_from_the_endpoint_that_named_the_latest()
    {
        let (url, hits, log, node) = chain_node(20, 8, 0).await;
        let head = soft(&url, 3).decision_head().await.unwrap();
        assert_eq!((head.number, head.hash), (17, B256::repeat_byte(17)));
        // The second read depends on the first, so they are two requests, by number the second.
        assert_eq!(
            asked(&log),
            ["eth_getBlockByNumber latest", "eth_getBlockByNumber 0x11"]
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        // Near the start of a chain the decision head is the first block.
        let (young, _, _, young_node) = chain_node(2, 0, 0).await;
        assert_eq!(soft(&young, 5).decision_head().await.unwrap().number, 0);

        // The two blocks are one endpoint's: one that names a latest block and then does not serve the one below it is
        // passed over whole, and nothing of it is combined with the next endpoint's answer.
        let (stuck, stuck_hits, stuck_node) = answering(|call| match call["params"][0].as_str() {
            Some("latest") => block(30, 0xa0),
            _ => Value::Null,
        })
        .await;
        let (healthy, healthy_hits, _, healthy_node) = chain_node(21, 8, 0).await;
        let rpc = Rpc::new(vec![stuck, healthy])
            .unwrap()
            .with_finality(FinalityMode::Soft, 3);
        let head = rpc.decision_head().await.unwrap();
        assert_eq!((head.number, head.hash), (18, B256::repeat_byte(18)));
        assert_eq!(stuck_hits.load(Ordering::SeqCst), 2);
        assert_eq!(healthy_hits.load(Ordering::SeqCst), 2);
        // The endpoint that answered is the one reads start at now.
        assert_eq!(rpc.active.load(Ordering::Relaxed), 1);
        for task in [node, young_node, stuck_node, healthy_node] {
            task.abort();
        }
    }
    #[tokio::test]
    async fn the_soft_decision_head_and_its_checkpoint_come_from_one_usable_answer() {
        // The first endpoint has not reached the checkpoint block, the second answers it with another block, and the
        // third is on the chain.
        let (behind, behind_hits, _, a) = chain_node(8, 3, 0).await;
        let (confused, confused_hits, b) = answering(|call| match call["params"][0].as_str() {
            Some("latest") => block(12, 0xdd),
            _ => block(6, 0xdd),
        })
        .await;
        let (healthy, healthy_hits, log, c) = chain_node(12, 3, 0).await;
        let rpc = Rpc::new(vec![behind.clone(), confused, healthy.clone()])
            .unwrap()
            .with_finality(FinalityMode::Soft, 0);
        let (head, checkpoint) = rpc.decision_head_with(Some(10)).await.unwrap();
        // Both blocks are the third endpoint's, in one batch each, and the checkpoint is the block it was asked for.
        assert_eq!((head.number, head.hash), (12, B256::repeat_byte(12)));
        let checkpoint = checkpoint.unwrap();
        assert_eq!(
            (checkpoint.number, checkpoint.hash),
            (10, B256::repeat_byte(10))
        );
        for hits in [behind_hits, confused_hits, healthy_hits.clone()] {
            assert_eq!(hits.load(Ordering::SeqCst), 1, "one batched request each");
        }
        assert_eq!(
            asked(&log),
            ["eth_getBlockByNumber latest", "eth_getBlockByNumber 0xa"]
        );
        // Without a checkpoint it is the head alone.
        let (head, checkpoint) = soft(&healthy, 0).decision_head_with(None).await.unwrap();
        assert_eq!((head.number, checkpoint.is_none()), (12, true));
        // A depth reads the decision head first and the checkpoint beside it, still from the one endpoint.
        let hits = healthy_hits.load(Ordering::SeqCst);
        asked(&log);
        let (head, checkpoint) = soft(&healthy, 2).decision_head_with(Some(9)).await.unwrap();
        assert_eq!((head.number, checkpoint.unwrap().number), (10, 9));
        assert_eq!(
            asked(&log),
            [
                "eth_getBlockByNumber latest",
                "eth_getBlockByNumber 0xa",
                "eth_getBlockByNumber 0x9"
            ]
        );
        assert_eq!(healthy_hits.load(Ordering::SeqCst) - hits, 2);
        // Every endpoint failing is a delivery failure, not an answer about the chain.
        let error = soft(&behind, 0)
            .decision_head_with(Some(10))
            .await
            .unwrap_err();
        assert!(
            is_delivery_failure(&error) && !is_rate_limited(&error),
            "{error:#}"
        );
        for task in [a, b, c] {
            task.abort();
        }
    }
    #[tokio::test]
    async fn in_finalized_mode_the_decision_head_with_its_checkpoint_is_the_finalized_read() {
        let (url, hits, log, node) = chain_node(12, 8, 0).await;
        let rpc = Rpc::new(vec![url]).unwrap();
        let (head, checkpoint) = rpc.decision_head_with(Some(5)).await.unwrap();
        assert_eq!((head.number, checkpoint.unwrap().number), (8, 5));
        // Exactly finalized_head_with: one batch of the finalized head and the checkpoint block by number.
        assert_eq!(
            asked(&log),
            ["eth_getBlockByNumber finalized", "eth_getBlockByNumber 0x5"]
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        node.abort();
    }
    #[tokio::test]
    async fn views_are_read_at_latest_in_soft_mode_and_finalized_otherwise() {
        use crate::abi::Coordinator;
        let (url, _, log, node) = chain_node(20, 8, 0).await;
        let finalized = Rpc::new(vec![url.clone()]).unwrap();
        let word = |rpc: &Rpc| {
            let rpc = rpc.clone();
            async move {
                rpc.call(Address::ZERO, Coordinator::nextRequestIdCall {})
                    .await
                    .unwrap()
            }
        };
        assert_eq!(word(&finalized).await, U256::from(2));
        assert_eq!(asked(&log), ["eth_call finalized"]);
        // Soft mode's default tag is latest, whatever the depth.
        for depth in [0, 3] {
            let rpc = soft(&url, depth);
            assert_eq!(word(&rpc).await, U256::from(1));
            assert_eq!(asked(&log), ["eth_call latest"]);
        }
        // A named block is that block in both, and the runtime-check clone keeps the mode.
        for rpc in [&finalized, &soft(&url, 0)] {
            let at = rpc
                .call_at(Address::ZERO, Coordinator::nextRequestIdCall {}, 9)
                .await
                .unwrap();
            assert_eq!(at, U256::from(9));
            assert_eq!(asked(&log), ["eth_call 0x9"]);
        }
        assert_eq!(
            soft(&url, 2).for_runtime_checks().finality(),
            FinalityMode::Soft
        );
        // A view at the block the keeper decides on: the finalized tag, the latest block when the depth is 0, and the
        // decision head's number under a depth.
        let decided = |rpc: &Rpc| {
            let rpc = rpc.clone();
            async move {
                rpc.call_decided(Address::ZERO, Coordinator::nextRequestIdCall {})
                    .await
                    .unwrap()
            }
        };
        assert_eq!(decided(&finalized).await, U256::from(2));
        assert_eq!(asked(&log), ["eth_call finalized"]);
        assert_eq!(decided(&soft(&url, 0)).await, U256::from(1));
        assert_eq!(asked(&log), ["eth_call latest"]);
        assert_eq!(decided(&soft(&url, 3)).await, U256::from(17));
        assert_eq!(
            asked(&log),
            [
                "eth_getBlockByNumber latest",
                "eth_getBlockByNumber 0x11",
                "eth_call 0x11"
            ]
        );
        node.abort();
    }
    #[tokio::test]
    async fn requests_and_nonces_are_read_at_the_block_the_tick_decided_on() {
        let (url, hits, log, node) = chain_node(20, 8, 0).await;
        let head = Head::from_block(&block(17, 0x11)).unwrap();
        let wallet = Address::repeat_byte(0x24);
        let finalized = Rpc::new(vec![url.clone()]).unwrap();
        let rpc = soft(&url, 3);
        assert_eq!(finalized.decision_tag(&head), "finalized");
        assert_eq!(rpc.decision_tag(&head), "0x11");
        // The consumed nonce: the finalized one as ever, or the one at the decision head's number.
        assert_eq!(finalized.decision_nonce(wallet, &head).await.unwrap(), 2);
        assert_eq!(asked(&log), ["eth_getTransactionCount finalized"]);
        assert_eq!(rpc.decision_nonce(wallet, &head).await.unwrap(), 17);
        assert_eq!(asked(&log), ["eth_getTransactionCount 0x11"]);
        // Without a head in hand: finalized needs no read, a depth 0 is the latest block, and a depth reads the head.
        let before = hits.load(Ordering::SeqCst);
        assert_eq!(finalized.current_decision_tag().await.unwrap(), "finalized");
        assert_eq!(
            soft(&url, 0).current_decision_tag().await.unwrap(),
            "latest"
        );
        assert_eq!(hits.load(Ordering::SeqCst), before);
        assert_eq!(rpc.current_decision_tag().await.unwrap(), "0x11");
        assert_eq!(hits.load(Ordering::SeqCst), before + 2);
        node.abort();
    }
    fn receipt(tx: B256, block: u64, hash: u8) -> Value {
        json!({"transactionHash":tx,"blockNumber":format!("0x{block:x}"),"blockHash":B256::repeat_byte(hash),"status":"0x1"})
    }
    #[tokio::test]
    async fn a_soft_receipt_is_settled_at_the_decision_head_when_its_block_is_the_chains() {
        let tx = B256::repeat_byte(0x77);
        let receipt = receipt(tx, 10, 10);
        let settled = |rpc: Rpc, receipt: Value| async move {
            rpc.receipt_is_settled(&tx.to_string(), &receipt).await
        };
        // Block 10 is the chain's, and the latest block is 12 while the finalized one is 3: settled at once in soft mode,
        // in one batch from one endpoint, and not in finalized mode.
        let (url, hits, log, a) = chain_node(12, 3, 0).await;
        assert!(settled(soft(&url, 0), receipt.clone()).await.unwrap());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(
            asked(&log),
            ["eth_getBlockByNumber latest", "eth_getBlockByNumber 0xa"]
        );
        assert!(
            !settled(Rpc::new(vec![url.clone()]).unwrap(), receipt.clone())
                .await
                .unwrap()
        );
        // A depth of 2 puts the decision head at 10, the receipt's block, and a depth of 3 below it.
        assert!(settled(soft(&url, 2), receipt.clone()).await.unwrap());
        assert!(!settled(soft(&url, 3), receipt.clone()).await.unwrap());

        // The block of that number is another block on this endpoint: the receipt is an orphan, and never settles.
        let (forked, _, _, b) = chain_node(12, 3, 0xff).await;
        assert!(!settled(soft(&forked, 0), receipt.clone()).await.unwrap());
        // An endpoint that has not reached the receipt's block says it is not settled yet, without failing.
        let (behind, behind_hits, _, c) = chain_node(9, 3, 0).await;
        assert!(!settled(soft(&behind, 0), receipt.clone()).await.unwrap());
        assert_eq!(behind_hits.load(Ordering::SeqCst), 1);
        // The receipt's block is the decision head itself: its hash is judged as well, whichever answer it came from.
        let (split, _, d) = answering(|call| match call["params"][0].as_str() {
            Some("latest") => block(10, 0xbb),
            _ => block(10, 10),
        })
        .await;
        assert!(!settled(soft(&split, 0), receipt.clone()).await.unwrap());

        // An endpoint that serves a latest block but not a block below it is unusable, not an answer: the next endpoint
        // answers, and with no other the read fails as a delivery failure.
        let (broken, _, e) = answering(|call| match call["params"][0].as_str() {
            Some("latest") => block(12, 12),
            _ => Value::Null,
        })
        .await;
        let both = Rpc::new(vec![broken.clone(), url.clone()])
            .unwrap()
            .with_finality(FinalityMode::Soft, 0);
        assert!(settled(both, receipt.clone()).await.unwrap());
        let error = settled(soft(&broken, 0), receipt.clone())
            .await
            .unwrap_err();
        assert!(is_delivery_failure(&error), "{error:#}");

        // Another transaction's receipt, and one with a status that is not a status, are errors, never a verdict.
        assert!(
            settled(soft(&url, 0), self::receipt(B256::ZERO, 10, 10))
                .await
                .is_err()
        );
        let mut odd = receipt.clone();
        odd["status"] = json!("0x2");
        assert!(settled(soft(&url, 0), odd).await.is_err());
        for task in [a, b, c, d, e] {
            task.abort();
        }
    }
    #[tokio::test]
    async fn in_finalized_mode_a_receipt_is_settled_by_exactly_the_finalized_check() {
        let tx = B256::repeat_byte(0x77);
        let (url, hits, log, node) = chain_node(12, 11, 0).await;
        let rpc = Rpc::new(vec![url]).unwrap();
        let receipt = receipt(tx, 10, 10);
        assert!(
            rpc.receipt_is_finalized(&tx.to_string(), &receipt)
                .await
                .unwrap()
        );
        let finalized = asked(&log);
        assert_eq!(
            finalized,
            ["eth_getBlockByNumber finalized", "eth_getBlockByNumber 0xa"]
        );
        let requests = hits.load(Ordering::SeqCst);
        assert!(
            rpc.receipt_is_settled(&tx.to_string(), &receipt)
                .await
                .unwrap()
        );
        // The same calls, in the same requests: the finalized head, then the block by number.
        assert_eq!(asked(&log), finalized);
        assert_eq!(hits.load(Ordering::SeqCst), 2 * requests);
        node.abort();
    }
    #[tokio::test]
    async fn block_hashes_are_one_endpoints_answer_to_one_batch_in_the_order_asked() {
        let (url, hits, log, node) = chain_node(20, 8, 0).await;
        let rpc = Rpc::new(vec![url]).unwrap();
        assert_eq!(
            rpc.block_hashes(&[12, 3, 8]).await.unwrap(),
            [
                B256::repeat_byte(12),
                B256::repeat_byte(3),
                B256::repeat_byte(8)
            ]
        );
        assert_eq!(
            asked(&log),
            [
                "eth_getBlockByNumber 0xc",
                "eth_getBlockByNumber 0x3",
                "eth_getBlockByNumber 0x8"
            ]
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // Sixty-four blocks are one batch as well, past the 32 calls of `batch`; more, or none, are refused unsent.
        let (long, long_hits, _, long_node) = chain_node(100, 90, 0).await;
        let rpc = Rpc::new(vec![long]).unwrap();
        let page: Vec<u64> = (0..MAX_BLOCK_HASHES as u64).collect();
        let hashes = rpc.block_hashes(&page).await.unwrap();
        assert_eq!(hashes.len(), 64);
        assert_eq!(hashes[63], B256::repeat_byte(63));
        assert_eq!(long_hits.load(Ordering::SeqCst), 1);
        assert!(
            rpc.block_hashes(&(0..65).collect::<Vec<u64>>())
                .await
                .is_err()
        );
        assert!(rpc.block_hashes(&[]).await.is_err());
        assert_eq!(long_hits.load(Ordering::SeqCst), 1);

        // One block that an endpoint has not reached fails that endpoint whole, and the next endpoint answers every block:
        // nothing the first one said is kept.
        let (behind, behind_hits, _, a) = chain_node(10, 3, 0).await;
        let (ahead, ahead_hits, _, b) = chain_node(20, 8, 0).await;
        let rpc = Rpc::new(vec![behind, ahead]).unwrap();
        assert_eq!(
            rpc.block_hashes(&[3, 12]).await.unwrap(),
            [B256::repeat_byte(3), B256::repeat_byte(12)]
        );
        assert_eq!(
            (
                behind_hits.load(Ordering::SeqCst),
                ahead_hits.load(Ordering::SeqCst)
            ),
            (1, 1)
        );
        // Every endpoint short of a block is a delivery failure, not an answer about the chain.
        let error = Rpc::new(vec![
            chain_node(10, 3, 0).await.0,
            chain_node(11, 3, 0).await.0,
        ])
        .unwrap()
        .block_hashes(&[3, 12])
        .await
        .unwrap_err();
        assert!(
            is_delivery_failure(&error) && !is_rate_limited(&error),
            "{error:#}"
        );
        // An endpoint on another fork is believed: whether its hashes are the keeper's is for the audit to say.
        let (fork, _, _, c) = chain_node(20, 8, 0xff).await;
        assert_eq!(
            Rpc::new(vec![fork])
                .unwrap()
                .block_hashes(&[3])
                .await
                .unwrap(),
            [B256::repeat_byte(3 ^ 0xff)]
        );
        // An answer for another block than the one asked is no answer.
        let (confused, _, d) = answering(|_| block(7, 1)).await;
        assert!(
            Rpc::new(vec![confused])
                .unwrap()
                .block_hashes(&[3])
                .await
                .is_err()
        );
        for task in [node, long_node, a, b, c, d] {
            task.abort();
        }
    }
}
