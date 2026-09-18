use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_sol_types::SolCall;
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

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
    /// The endpoint refused the call for its request rate (HTTP 429 or an equivalent provider error).
    rate_limited: bool,
    detail: String,
}
impl std::fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RPC delivery uncertain: {}", self.detail)
    }
}
impl std::error::Error for DeliveryError {}
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
        rate_limited: false,
        detail: detail.into(),
    }
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
#[derive(Clone)]
pub struct Rpc {
    pub client: reqwest::Client,
    pub urls: Vec<String>,
    active: Arc<AtomicUsize>,
    cooldowns: Arc<Mutex<Vec<Option<tokio::time::Instant>>>>,
    /// Per endpoint: consecutive rate-limited answers and the end of the back-off they earned. An endpoint inside
    /// its back-off is not asked at all, so a limit is never answered with more traffic.
    limits: Arc<Mutex<Vec<RateLimit>>>,
    /// Endpoints that answered a JSON-RPC batch with something other than a matching array.
    unbatched: Arc<Mutex<Vec<bool>>>,
    read_budget: Duration,
    attempt_budget: Duration,
}
#[derive(Clone, Debug)]
pub struct Head {
    pub number: u64,
    pub hash: B256,
    pub timestamp: u64,
    pub base_fee: u128,
}
impl Head {
    /// A block object as eth_getBlockByNumber returns it.
    pub fn from_block(v: &Value) -> Result<Self> {
        Ok(Self {
            number: quantity(&v["number"])?,
            hash: serde_json::from_value(v["hash"].clone())?,
            timestamp: quantity(&v["timestamp"])?,
            base_fee: U256::from_str_radix(
                v["baseFeePerGas"]
                    .as_str()
                    .unwrap_or("0x0")
                    .trim_start_matches("0x"),
                16,
            )?
            .try_into()?,
        })
    }
}
pub fn quantity(v: &Value) -> Result<u64> {
    Ok(U256::from_str_radix(
        v.as_str()
            .ok_or_else(|| anyhow::anyhow!("Expected RPC hex quantity"))?
            .trim_start_matches("0x"),
        16,
    )?
    .try_into()?)
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
            unbatched: Arc::new(Mutex::new(vec![false; count])),
            read_budget: Duration::from_secs(8u64.saturating_mul(count as u64)),
            attempt_budget: Duration::from_secs(8),
        })
    }
    /// Endpoints in the order a read tries them: the last one that answered first, slow ones last, and the ones
    /// inside a rate-limit back-off not at all. Empty when every endpoint is backing off.
    fn read_order(&self) -> Vec<usize> {
        let count = self.urls.len();
        let start = self.active.load(Ordering::Relaxed) % count;
        let now = tokio::time::Instant::now();
        let limits = self.limits.lock().expect("RPC rate-limit mutex");
        let mut order: Vec<usize> = (0..count)
            .map(|offset| (start + offset) % count)
            .filter(|i| limits[*i].1.is_none_or(|until| until <= now))
            .collect();
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
        if !response.status().is_success() {
            return Err(uncertain(format!("HTTP status {}", response.status().as_u16())).into());
        }
        let mut response = response;
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
                rate_limited: rate_limit_response(code, message),
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
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.hedged(method, |i| {
            let params = params.clone();
            async move { self.at(&self.urls[i], method, params).await }
        })
        .await
    }
    /// Independent reads sent as one JSON-RPC batch per endpoint attempt, with the same endpoint order, hedging
    /// and rate-limit back-off as `request`. Results come back in call order and always from one endpoint. An
    /// endpoint that answers a batch with anything but a matching array is asked the same calls one by one from
    /// then on.
    pub async fn batch(&self, calls: &[(&str, Value)]) -> Result<Vec<Value>> {
        ensure!(!calls.is_empty() && calls.len() <= 32, "RPC batch size");
        let label = format!("{} batch", calls[0].0);
        self.hedged(&label, |i| self.batch_at(i, calls)).await
    }
    /// Run one idempotent read against the endpoints until one answers.
    async fn hedged<T, F, Fut>(&self, label: &str, attempt: F) -> Result<T>
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
        let mut protocol_error = None;
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
                            tracing::warn!(method=label,error=%e,"RPC attempt failed");
                            if !is_delivery_failure(&e) {
                                protocol_error = Some(e);
                            } else {
                                last_error = Some(e);
                            }
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
        Err(protocol_error
            .or(last_error)
            .or(last_limited)
            .unwrap_or_else(|| anyhow::anyhow!("No configured RPC endpoints"))
            .context(format!("All configured RPC endpoints failed {label}")))
    }
    async fn batch_at(&self, i: usize, calls: &[(&str, Value)]) -> Result<Vec<Value>> {
        let url = &self.urls[i];
        if !self.unbatched.lock().expect("RPC batch mutex")[i] {
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
                return Ok(results);
            }
            // A single error object or a short array: this endpoint does not batch. A rate-limit error in that
            // position is still a rate limit, not a lack of batch support.
            if answer.get("error").is_some()
                && let Err(error) = Self::outcome(&answer)
                && is_rate_limited(&error)
            {
                return Err(error);
            }
            self.unbatched.lock().expect("RPC batch mutex")[i] = true;
            tracing::debug!(
                endpoint = i,
                "RPC endpoint does not answer batches; sending calls one by one"
            );
        }
        let mut results = Vec::with_capacity(calls.len());
        for (method, params) in calls {
            results.push(self.at(url, method, params.clone()).await?);
        }
        Ok(results)
    }
    /// Only an attempt that consumed a full attempt budget proves a slow endpoint.
    fn cool_slow_attempts(&self, attempts: &[(usize, tokio::time::Instant)]) {
        for (i, began) in attempts {
            if began.elapsed() < self.attempt_budget {
                continue;
            }
            self.cooldowns.lock().expect("RPC cooldown mutex")[*i] =
                Some(tokio::time::Instant::now() + Duration::from_secs(5));
            self.active
                .compare_exchange(
                    *i,
                    (*i + 1) % self.urls.len(),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .ok();
        }
    }
    pub async fn head(&self) -> Result<Head> {
        self.head_at("latest").await
    }
    /// Median of the per-block 50th-percentile tips over recent blocks (eth_feeHistory).
    pub async fn recent_priority_fee(&self, blocks: u64) -> Result<u128> {
        let v = self
            .request(
                "eth_feeHistory",
                json!([format!("0x{blocks:x}"), "latest", [50]]),
            )
            .await?;
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
    }
    pub async fn finalized_head(&self) -> Result<Head> {
        self.head_at("finalized").await
    }
    /// The block with this number, including its timestamp.
    pub async fn block(&self, number: u64) -> Result<Head> {
        let block = self.head_at(&format!("0x{number:x}")).await?;
        ensure!(block.number == number, "Unexpected block number");
        Ok(block)
    }
    async fn head_at(&self, tag: &str) -> Result<Head> {
        let v = self
            .request("eth_getBlockByNumber", json!([tag, false]))
            .await?;
        Head::from_block(&v)
    }
    pub async fn call<C: SolCall>(&self, to: Address, call: C) -> Result<C::Return> {
        self.call_tag(to, call, "finalized").await
    }
    pub async fn call_at<C: SolCall>(
        &self,
        to: Address,
        call: C,
        number: u64,
    ) -> Result<C::Return> {
        self.call_tag(to, call, &format!("0x{number:x}")).await
    }
    async fn call_tag<C: SolCall>(&self, to: Address, call: C, tag: &str) -> Result<C::Return> {
        let v = self
            .request(
                "eth_call",
                json!([{"to":to,"data":Bytes::from(call.abi_encode())},tag]),
            )
            .await?;
        let bytes: Bytes = serde_json::from_value(v)?;
        Ok(C::abi_decode_returns(&bytes)?)
    }
    pub async fn nonce(&self, address: Address, tag: &str) -> Result<u64> {
        quantity(
            &self
                .request("eth_getTransactionCount", json!([address, tag]))
                .await?,
        )
    }
    pub async fn receipt(&self, hash: &str) -> Result<Option<Value>> {
        let v = self
            .request("eth_getTransactionReceipt", json!([hash]))
            .await?;
        Ok(if v.is_null() { None } else { Some(v) })
    }
    pub async fn block_hash(&self, number: u64) -> Result<B256> {
        let block = self
            .request(
                "eth_getBlockByNumber",
                json!([format!("0x{number:x}"), false]),
            )
            .await?;
        ensure!(
            quantity(&block["number"])? == number,
            "Unexpected block number"
        );
        Ok(serde_json::from_value(block["hash"].clone())?)
    }
    pub async fn receipt_is_finalized(&self, hash: &str, receipt: &Value) -> Result<bool> {
        let actual: B256 = serde_json::from_value(receipt["transactionHash"].clone())?;
        ensure!(
            actual == hash.parse::<B256>()?,
            "Unexpected receipt transaction"
        );
        ensure!(quantity(&receipt["status"])? <= 1, "Invalid receipt status");
        let number = quantity(&receipt["blockNumber"])?;
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
        server.abort();
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
                        rate_limited: false,
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
                    rate_limited: false,
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
                    rate_limited: false,
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
        assert_eq!(single_hits.load(Ordering::SeqCst), 1 + 5 + 5);
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
}
