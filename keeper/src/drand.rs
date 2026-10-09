//! The drand relay client, which knows nothing of the contract that verifies what it fetches. It asks the HTTP relays
//! of a drand network for one round, judges each answer, keeps a circuit per relay and takes the first well-formed
//! signature that a verifier it is given accepts. The relays are not trusted: a relay can delay a round but never change
//! it, since only a signature the verifier accepts is returned, and a signature is unique for its round.
//!
//! What a caller brings: the network (its chain hash and schedule), the round, the time the round is judged at, a verifier
//! and the table of the relay circuits in its journal. The caller decides which round to fetch and when, and what to do
//! with the signature.
use alloy_primitives::B256;
use anyhow::{Result, ensure};
use futures_util::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use serde_json::Value;
use sqlx::SqlitePool;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::task::JoinSet;

/// A failed fetch is tried again after this long while live demand waits on the round: rounds arrive every few seconds,
/// and a beacon usually has no other source to move to, so demand is never made to wait longer.
pub const RETRY_SECONDS: u64 = 2;
/// Live demand waiting on a round whose fetches have kept failing for this long is a stall, not a retry.
pub const UNAVAILABLE_SECONDS: u64 = 10;
/// One relay must answer within this long: drand relays hold a request for the next round until it exists, which took
/// 1.8 to 2.25 seconds when measured. The whole fetch, its verification included, must end within FETCH_TIMEOUT.
pub const RELAY_TIMEOUT: Duration = Duration::from_secs(4);
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(8);
/// Failed fetches are one run while none comes later than this after the one before: a fetch at its limit, the retry
/// after it and some slack. A failure later than that starts a new run, and a run that has stopped is not a stall.
pub const RUN_GAP_SECONDS: u64 = FETCH_TIMEOUT.as_secs() + RETRY_SECONDS + 5;
/// A run of failed fetches is logged at warn once for every this many seconds it goes on.
const WARN_SECONDS: u64 = 5 * 60;
/// DRAND_RELAYS lists at most this many relays: each is asked at once.
pub const MAX_RELAYS: usize = 8;
/// A round's answer is a few hundred bytes.
const ANSWER_LIMIT: usize = 16 * 1024;
const DEFAULT_RELAYS: [&str; 4] = [
    "https://api.drand.sh",
    "https://api2.drand.sh",
    "https://api3.drand.sh",
    "https://drand.cloudflare.com",
];
/// Consecutive failures of one relay (see `judge` for what counts as one) open its circuit for BREAKER_COOLDOWN_SECONDS.
/// While it is open the relay is asked only as the half-open probe of the one whose cooldown ends first (see
/// `askable`), and a verified answer closes it.
pub const BREAKER_FAILURES: i64 = 3;
pub const BREAKER_COOLDOWN_SECONDS: u64 = 120;

/// A drand network as a fetch needs it: the chain hash that names it to the relays, and its schedule. Round r is
/// scheduled at genesis + (r - 1) * period.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Network {
    pub chain_hash: B256,
    pub genesis: u64,
    pub period: u64,
}
impl Network {
    /// When a round is scheduled: genesis + (round - 1) * period.
    pub fn round_time(&self, round: u64) -> u64 {
        self.genesis
            .saturating_add(round.saturating_sub(1).saturating_mul(self.period))
    }
}
/// Whether `round` was scheduled at most two periods before `now`, the time a fetch judges it at. A relay may still be
/// waiting for such a round, so it is not held against a relay that answers "not published" or does not answer at all;
/// a relay that does not have a round older than that is at fault.
pub fn recent(network: &Network, round: u64, now: u64) -> bool {
    network
        .round_time(round)
        .saturating_add(network.period.saturating_mul(2))
        >= now
}
/// Whether a failed fetch is logged at warn: the first of its attempts, and then one for every WARN_SECONDS the failures
/// go on, told by `previous`, the time of the failure before this one. The others are for debug, since an outage that
/// lasts would otherwise fill the log.
pub fn loud(attempt: i64, previous: Option<u64>, now: u64) -> bool {
    attempt <= 1 || previous.is_none_or(|at| at / WARN_SECONDS != now / WARN_SECONDS)
}

/// The drand relays a keeper reads rounds from, as base URLs without a trailing slash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrandRelays(Vec<String>);
impl Default for DrandRelays {
    fn default() -> Self {
        Self(DEFAULT_RELAYS.map(str::to_owned).to_vec())
    }
}
impl std::fmt::Display for DrandRelays {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.join(","))
    }
}
impl DrandRelays {
    /// Comma-separated relay base URLs that replace the four public relays, at most MAX_RELAYS of them. Each is HTTPS
    /// without credentials, query or fragment, and listed once; a relay is asked for `/<chain hash>/public/<round>`
    /// under its base. `local` also admits loopback HTTP, for the local test chain only. `None` keeps the defaults.
    pub fn parse(value: Option<&str>, local: bool) -> Result<Self> {
        let Some(value) = value else {
            return Ok(Self::default());
        };
        let mut relays: Vec<String> = Vec::new();
        for entry in value.split(',') {
            let entry = entry.trim();
            ensure!(
                !entry.is_empty() && entry.len() <= 1024,
                "DRAND_RELAYS entries must be non-empty URLs of at most 1024 bytes"
            );
            let parsed = reqwest::Url::parse(entry)
                .map_err(|_| anyhow::anyhow!("DRAND_RELAYS entry is not a valid URL"))?;
            let loopback =
                local && parsed.scheme() == "http" && parsed.host_str() == Some("127.0.0.1");
            ensure!(
                (parsed.scheme() == "https" || loopback)
                    && parsed.host_str().is_some()
                    && parsed.username().is_empty()
                    && parsed.password().is_none()
                    && parsed.query().is_none()
                    && parsed.fragment().is_none(),
                "DRAND_RELAYS entries must be HTTPS URLs without credentials, query or fragment"
            );
            let base = parsed.as_str().trim_end_matches('/').to_owned();
            ensure!(
                !relays.contains(&base),
                "DRAND_RELAYS lists a relay more than once"
            );
            ensure!(
                relays.len() < MAX_RELAYS,
                "DRAND_RELAYS lists more than {MAX_RELAYS} relays"
            );
            relays.push(base);
        }
        Ok(Self(relays))
    }
    /// The relay list of a configuration: TEST_API_BASE, which only the local test chain admits, replaces it by that
    /// one base.
    pub fn configured(value: Option<&str>, test_base: Option<&str>, local: bool) -> Result<Self> {
        let relays = Self::parse(value, local)?;
        Ok(match test_base {
            Some(base) => Self(vec![base.trim_end_matches('/').to_owned()]),
            None => relays,
        })
    }
    pub fn urls(&self) -> &[String] {
        &self.0
    }
}

/// The client that asks relays. They are not trusted, so it follows no redirects.
pub fn relay_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

/// What one relay said about one round.
#[derive(Debug, PartialEq, Eq)]
enum Reply {
    /// A well-formed answer for the round asked: its signature, not yet verified.
    Signature([u8; 64]),
    /// The relay says it has not published the round (HTTP 425 or 404).
    NotYet,
    /// The relay answered with a server error (HTTP 5xx).
    ServerError(u16),
    /// The relay did not answer within RELAY_TIMEOUT.
    TimedOut,
    Failed(String),
}
/// What a reply comes to for the relay that gave it.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// A well-formed signature, not yet verified.
    Signature([u8; 64]),
    /// No fault of the relay: it may not have the round yet.
    Excused(String),
    Failed(String),
}
/// Judge a reply. A relay may not have a `recent` round yet (see `recent`), so "not published" and no answer at all
/// are excused for it; for a round that has been due for longer, they are that relay's failure. A server error is
/// excused the same way when `server_errors` says so (relays have answered HTTP 500 for a round not yet produced), and
/// is otherwise the relay's failure whatever the round's age.
fn judge(reply: Reply, recent: bool, server_errors: bool) -> Outcome {
    let silent = || format!("no answer within {} s", RELAY_TIMEOUT.as_secs());
    match reply {
        Reply::Signature(signature) => Outcome::Signature(signature),
        Reply::NotYet if recent => Outcome::Excused("round not published yet".into()),
        Reply::NotYet => Outcome::Failed("round already due but not served".into()),
        Reply::ServerError(status) if recent && server_errors => {
            Outcome::Excused(format!("HTTP {status}, the round may not be published yet"))
        }
        Reply::ServerError(status) => Outcome::Failed(format!("HTTP {status}")),
        Reply::TimedOut if recent => {
            Outcome::Excused(format!("{}, the round may not be published yet", silent()))
        }
        Reply::TimedOut => Outcome::Failed(silent()),
        Reply::Failed(reason) => Outcome::Failed(reason),
    }
}
/// Judge a relay's HTTP answer for `round`. The relay is not trusted: an answer for another round, malformed JSON or a
/// signature that is not 64 bytes of hex is that relay's failure, and nothing here says whether the signature is valid.
fn reply(status: u16, body: &[u8], round: u64) -> Reply {
    match status {
        200..=299 => {}
        404 | 425 => return Reply::NotYet,
        500..=599 => return Reply::ServerError(status),
        _ => return Reply::Failed(format!("HTTP {status}")),
    }
    let Ok(answer) = serde_json::from_slice::<Value>(body) else {
        return Reply::Failed("answer is not JSON".into());
    };
    match answer.get("round").and_then(Value::as_u64) {
        Some(answered) if answered == round => {}
        Some(answered) => {
            return Reply::Failed(format!("answered round {answered} for round {round}"));
        }
        None => return Reply::Failed("answer has no round number".into()),
    }
    answer
        .get("signature")
        .and_then(Value::as_str)
        .filter(|signature| signature.len() == 128)
        .and_then(|signature| hex::decode(signature).ok())
        .and_then(|bytes| <[u8; 64]>::try_from(bytes).ok())
        .map_or_else(
            || Reply::Failed("signature is not 64 bytes of hex".into()),
            Reply::Signature,
        )
}
/// Ask one relay for a round, within RELAY_TIMEOUT and ANSWER_LIMIT.
async fn ask(client: &reqwest::Client, relay: &str, chain_hash: B256, round: u64) -> Reply {
    let url = format!("{relay}/{}/public/{round}", hex::encode(chain_hash));
    let answer = async {
        let mut response = client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| format!("transport: {}", e.without_url()))?;
        let status = response.status();
        let mut body = Vec::new();
        // Only a success carries an answer; anything else is judged by its status alone.
        while status.is_success()
            && let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| format!("transport: {}", e.without_url()))?
        {
            if body.len() + chunk.len() > ANSWER_LIMIT {
                return Err(format!("answer over {ANSWER_LIMIT} bytes"));
            }
            body.extend_from_slice(&chunk);
        }
        Ok::<_, String>((status.as_u16(), body))
    };
    match tokio::time::timeout(RELAY_TIMEOUT, answer).await {
        Ok(Ok((status, body))) => reply(status, &body, round),
        Ok(Err(reason)) => Reply::Failed(reason),
        Err(_) => Reply::TimedOut,
    }
}

/// The table in the caller's journal where the relays' circuits are kept: one row per relay that has failed, with its
/// consecutive failures and the end of its cooldown once it is open. Each caller names its own table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Breaker {
    table: &'static str,
}
impl Breaker {
    /// The circuits in `table`, a fixed name of the caller's: letters, digits and underscores only.
    pub const fn new(table: &'static str) -> Self {
        let bytes = table.as_bytes();
        let mut i = 0;
        assert!(!bytes.is_empty(), "a breaker table has a name");
        while i < bytes.len() {
            assert!(
                bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_',
                "a breaker table is named by letters, digits and underscores"
            );
            i += 1;
        }
        Self { table }
    }
    pub fn table(&self) -> &'static str {
        self.table
    }
    /// Create the table if it is not there.
    pub async fn install(&self, pool: &SqlitePool) -> Result<()> {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE TABLE IF NOT EXISTS {}(url TEXT PRIMARY KEY,failures INTEGER NOT NULL,open_until INTEGER NOT NULL)",
            self.table
        )))
        .execute(pool)
        .await?;
        Ok(())
    }
    /// A relay's circuit: a relay that failed BREAKER_FAILURES times in a row is not asked for BREAKER_COOLDOWN_SECONDS,
    /// except for the half-open probe of the one that closes first (see `askable`), and a verified answer closes it.
    /// Returns the consecutive failures counted and the remaining cooldown when the circuit is open.
    pub async fn open(
        &self,
        pool: &SqlitePool,
        relay: &str,
        now: u64,
    ) -> Result<Option<(i64, u64)>> {
        let row: Option<(i64, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT failures,open_until FROM {} WHERE url=?",
            self.table
        )))
        .bind(relay)
        .fetch_optional(pool)
        .await?;
        Ok(row.and_then(|(failures, open_until)| {
            let open_until = u64::try_from(open_until).unwrap_or(0);
            (failures >= BREAKER_FAILURES && open_until > now).then(|| (failures, open_until - now))
        }))
    }
    /// A relay's failure counts toward its circuit and opens it at BREAKER_FAILURES; `failed` false closes it, since a
    /// verified answer proves it works. Returns the consecutive failures counted after this call, 0 when the circuit was
    /// closed.
    pub async fn record(
        &self,
        pool: &SqlitePool,
        relay: &str,
        failed: bool,
        now: u64,
    ) -> Result<i64> {
        if !failed {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {} WHERE url=?",
                self.table
            )))
            .bind(relay)
            .execute(pool)
            .await?;
            return Ok(0);
        }
        // One statement, so that a circuit another task closes in between (a relay's late valid answer) cannot leave the
        // failure uncounted or fail it: the first failure inserts the circuit, the others count on it.
        let failures = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("INSERT INTO {}(url,failures,open_until) VALUES(?3,1,CASE WHEN 1>=?1 THEN ?2 ELSE 0 END) ON CONFLICT(url) DO UPDATE SET failures=failures+1,open_until=CASE WHEN failures+1>=?1 THEN ?2 ELSE open_until END RETURNING failures", self.table)))
            .bind(BREAKER_FAILURES)
            .bind(i64::try_from(now.saturating_add(BREAKER_COOLDOWN_SECONDS))?)
            .bind(relay)
            .fetch_one(pool)
            .await?;
        if failures == BREAKER_FAILURES {
            tracing::warn!(
                relay,
                failures,
                cooldown_seconds = BREAKER_COOLDOWN_SECONDS,
                "drand relay circuit opened"
            );
        }
        Ok(failures)
    }
}

/// The relays worth asking: every one whose circuit is closed and, when any circuit is open, the one whose cooldown
/// ends first, as a half-open probe. A beacon usually has no other source to move to, so a relay that has recovered
/// must be found again without waiting out its whole cooldown; a valid answer closes its circuit. One probe per attempt
/// is all the load that costs.
async fn askable(
    pool: &SqlitePool,
    breaker: Breaker,
    relays: &[String],
    now: u64,
) -> Result<Vec<String>> {
    let (mut asked, mut soonest) = (Vec::new(), None::<(u64, &String)>);
    for relay in relays {
        match breaker.open(pool, relay, now).await? {
            None => asked.push(relay.clone()),
            Some((_, wait)) if soonest.is_none_or(|(least, _)| wait < least) => {
                soonest = Some((wait, relay));
            }
            Some(_) => {}
        }
    }
    if let Some((_, relay)) = soonest {
        tracing::debug!(relay = %relay, "Probing the drand relay whose circuit closes first");
        asked.push(relay.clone());
    }
    Ok(asked)
}
/// Record a relay's outcome in its circuit: `None` is a valid answer, which closes it. The first failure of a streak
/// is logged at warn and the ones that repeat it at debug; the third opens the circuit, which is logged once at warn.
/// It is bookkeeping and cannot fail a fetch: a journal that will not take it costs a circuit its count, not the round
/// the verifier has accepted.
async fn record(
    pool: &SqlitePool,
    breaker: Breaker,
    relay: &str,
    round: u64,
    failure: Option<&str>,
) {
    let failures = match crate::health::now() {
        Ok(now) => breaker.record(pool, relay, failure.is_some(), now).await,
        Err(error) => Err(error),
    };
    match (failures, failure) {
        (Err(error), _) => {
            tracing::debug!(relay, error = %error, "drand relay outcome not recorded")
        }
        (Ok(failures), Some(reason)) if failures <= 1 => {
            tracing::warn!(relay, round, reason, "drand relay gave no usable round");
        }
        (Ok(_), Some(reason)) => {
            tracing::debug!(relay, round, reason, "drand relay gave no usable round");
        }
        (Ok(_), None) => {}
    }
}

/// The requests to the relays that a fetch has sent and not yet heard from.
type Asked = FuturesUnordered<BoxFuture<'static, (String, Reply)>>;
/// What a fetch leaves behind when it returns: the answers of the relays that lost the race, settled into their
/// circuits (see `settle`). Its owner aborts what is left when it is dropped, so it can neither delay a shutdown nor
/// outlive the worker.
#[derive(Clone, Default)]
pub struct Stragglers(Arc<Mutex<JoinSet<()>>>);
impl Stragglers {
    pub(crate) fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        let mut tasks = self.0.lock().expect("stragglers mutex");
        // Finished tasks are collected here, so the set does not grow for as long as the keeper runs.
        while tasks.try_join_next().is_some() {}
        tasks.spawn(task);
    }
    /// Abort what is left.
    pub fn abort(&self) {
        self.0.lock().expect("stragglers mutex").abort_all();
    }
    /// Wait until every settling task has ended.
    #[cfg(test)]
    pub(crate) async fn settled(&self) {
        let mut tasks = std::mem::take(&mut *self.0.lock().expect("stragglers mutex"));
        while tasks.join_next().await.is_some() {}
    }
}
/// What `settle` needs of the fetch that left the relays in flight.
struct Settling {
    pool: SqlitePool,
    breaker: Breaker,
    accepted: [u8; 64],
    round: u64,
    recent: bool,
    server_errors: bool,
    verifier: &'static str,
}
/// The rest of a fetch, once its first signature is verified: the other relays are heard until each has answered or
/// RELAY_TIMEOUT has passed, and each is credited for it. A relay that gave the accepted signature served the round;
/// a signature is unique, so a different one is wrong without asking the verifier again.
async fn settle(mut asked: Asked, settling: Settling) {
    let deadline = tokio::time::Instant::now() + RELAY_TIMEOUT;
    while let Ok(Some((relay, reply))) = tokio::time::timeout_at(deadline, asked.next()).await {
        let failure = match judge(reply, settling.recent, settling.server_errors) {
            Outcome::Signature(signature) if signature == settling.accepted => None,
            Outcome::Signature(_) => Some(format!(
                "signature differs from the one {} verified",
                settling.verifier
            )),
            Outcome::Excused(_) => continue,
            Outcome::Failed(reason) => Some(reason),
        };
        record(
            &settling.pool,
            settling.breaker,
            &relay,
            settling.round,
            failure.as_deref(),
        )
        .await;
    }
}

/// A fetch that failed on the caller's own reads of the chain, not on the relays: the verifier could not say whether a
/// signature is valid, or a read the caller made for the fetch failed. Nothing is known then about what the relays did,
/// so the failure is not reported as drand being unavailable. The type marks an error; see `is_chain_read`.
#[derive(Debug)]
struct ChainRead;
impl std::fmt::Display for ChainRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("chain read failed")
    }
}
impl std::error::Error for ChainRead {}
/// An error of the caller's own read of the chain (see `ChainRead`), with this message.
pub fn chain_read(message: String) -> anyhow::Error {
    anyhow::Error::new(ChainRead).context(message)
}
/// Whether an error of a fetch is a failure of the caller's reads of the chain rather than of the relays.
pub fn is_chain_read(error: &anyhow::Error) -> bool {
    error.is::<ChainRead>()
}
/// What one fetch has learned, for its error.
#[derive(Default)]
pub struct Trace {
    /// Why each relay did not serve the round.
    pub notes: Vec<String>,
    /// A read of the chain is in flight.
    pub reading: bool,
    /// A signature could not be checked, so its relay's fault, if any, is unknown.
    pub unverified: bool,
}
impl Trace {
    /// The error of a fetch that found no round. `expired`: the fetch reached its time limit, which is the chain's
    /// doing when a read of it was what the fetch was waiting for.
    pub fn error(&self, what: String, expired: bool) -> anyhow::Error {
        let message = format!("{what}: {}", self.notes.join("; "));
        if self.unverified || (expired && self.reading) {
            chain_read(message)
        } else {
            anyhow::anyhow!(message)
        }
    }
}

/// The relays of one network as a fetch asks them: the HTTP client, the relay list, the circuits and where the answers
/// of the relays that lose the race are settled once the fetch has returned.
#[derive(Clone)]
pub struct Client {
    pub http: reqwest::Client,
    pub pool: SqlitePool,
    pub breaker: Breaker,
    pub relays: Vec<String>,
    pub stragglers: Stragglers,
    /// Whether a relay's server error is excused while the round is recent, as "not published" is (see `judge`).
    pub server_errors: bool,
    /// Who verifies a signature, as the relays' failures name it, such as "the coordinator".
    pub verifier: &'static str,
}
impl Client {
    /// The signature of `round` from the first relay whose answer `verify` accepts, within FETCH_TIMEOUT altogether,
    /// with `now` the time the round's age is judged at (see `recent`). Any failure to get one is retryable, and the
    /// error names each relay's reason; see `is_chain_read` for the failures that are not the relays'.
    pub async fn fetch_round<V, F>(
        &self,
        network: &Network,
        round: u64,
        now: u64,
        verify: V,
    ) -> Result<[u8; 64]>
    where
        V: Fn([u8; 64]) -> F,
        F: Future<Output = Result<bool>>,
    {
        let mut trace = Trace::default();
        let collected = tokio::time::timeout(
            FETCH_TIMEOUT,
            self.collect(network, round, now, verify, &mut trace),
        )
        .await;
        match collected {
            Ok(Ok(Some(signature))) => Ok(signature),
            Ok(Ok(None)) => Err(trace.error(format!("No drand relay served round {round}"), false)),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(trace.error(
                format!("No drand round within {} s", FETCH_TIMEOUT.as_secs()),
                true,
            )),
        }
    }
    /// Ask the relays at once and take the first answer `verify` accepts; the relays still in flight are heard
    /// afterwards, in the background (see `settle`). Each relay's outcome is recorded in its circuit: a valid answer
    /// closes it, a failure counts, and a round the relay may not have yet counts for nothing (see `judge`). `trace`
    /// collects why each other relay did not serve the round. `None` when no relay served it.
    pub async fn collect<V, F>(
        &self,
        network: &Network,
        round: u64,
        now: u64,
        verify: V,
        trace: &mut Trace,
    ) -> Result<Option<[u8; 64]>>
    where
        V: Fn([u8; 64]) -> F,
        F: Future<Output = Result<bool>>,
    {
        let recent = recent(network, round, now);
        let mut asked: Asked = askable(
            &self.pool,
            self.breaker,
            &self.relays,
            crate::health::now()?,
        )
        .await?
        .into_iter()
        .map(|relay| {
            let (client, chain_hash) = (self.http.clone(), network.chain_hash);
            async move {
                let reply = ask(&client, &relay, chain_hash, round).await;
                (relay, reply)
            }
            .boxed()
        })
        .collect();
        // The verifier's verdict on each distinct signature seen: relays that agree cost it one call.
        let mut verdicts = HashMap::new();
        while let Some((relay, reply)) = asked.next().await {
            let failure = match judge(reply, recent, self.server_errors) {
                Outcome::Excused(note) => {
                    trace.notes.push(format!("{relay}: {note}"));
                    continue;
                }
                Outcome::Failed(reason) => reason,
                Outcome::Signature(signature) => {
                    match verdict(&verify, signature, &mut verdicts, trace).await {
                        Ok(true) => {
                            record(&self.pool, self.breaker, &relay, round, None).await;
                            if !asked.is_empty() {
                                self.stragglers.spawn(settle(
                                    asked,
                                    Settling {
                                        pool: self.pool.clone(),
                                        breaker: self.breaker,
                                        accepted: signature,
                                        round,
                                        recent,
                                        server_errors: self.server_errors,
                                        verifier: self.verifier,
                                    },
                                ));
                            }
                            return Ok(Some(signature));
                        }
                        Ok(false) => "signature does not verify".to_owned(),
                        // The verifier could not be asked: no verdict on the relay.
                        Err(error) => {
                            trace
                                .notes
                                .push(format!("{relay}: signature not verified: {error}"));
                            trace.unverified = true;
                            continue;
                        }
                    }
                }
            };
            record(&self.pool, self.breaker, &relay, round, Some(&failure)).await;
            trace.notes.push(format!("{relay}: {failure}"));
        }
        Ok(None)
    }
}
/// Whether `verify` accepts this signature. A verdict is remembered for the rest of the fetch; an error is not, since it
/// says nothing about the signature.
async fn verdict<V, F>(
    verify: &V,
    signature: [u8; 64],
    verdicts: &mut HashMap<[u8; 64], bool>,
    trace: &mut Trace,
) -> Result<bool>
where
    V: Fn([u8; 64]) -> F,
    F: Future<Output = Result<bool>>,
{
    if let Some(known) = verdicts.get(&signature) {
        return Ok(*known);
    }
    trace.reading = true;
    let verified = verify(signature).await;
    trace.reading = false;
    let verified = verified?;
    verdicts.insert(signature, verified);
    Ok(verified)
}

#[cfg(test)]
pub(crate) mod fixture {
    //! Loopback HTTP servers standing in for drand relays and an RPC node.
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One HTTP answer, sent after `delay`.
    pub struct Answer {
        pub status: u16,
        pub headers: Vec<(&'static str, String)>,
        pub body: Vec<u8>,
        pub delay: Duration,
    }
    pub fn answer(status: u16, body: impl Into<Vec<u8>>) -> Answer {
        Answer {
            status,
            headers: Vec::new(),
            body: body.into(),
            delay: Duration::ZERO,
        }
    }
    /// Serve `handler(request line, request body)` on a loopback port: the base URL and a count of requests answered
    /// or being answered.
    pub async fn serve(
        handler: impl Fn(&str, &[u8]) -> Answer + Send + Sync + 'static,
    ) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (handler, count) = (Arc::new(handler), Arc::new(AtomicUsize::new(0)));
        let counter = count.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let (handler, counter) = (handler.clone(), counter.clone());
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let mut buffer = [0; 4096];
                    let (head, body_at) = loop {
                        let size = socket.read(&mut buffer).await.unwrap_or(0);
                        if size == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buffer[..size]);
                        if let Some(at) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&bytes[..at]).into_owned();
                            let length = head
                                .lines()
                                .find_map(|line| {
                                    let (k, v) = line.split_once(':')?;
                                    k.eq_ignore_ascii_case("content-length")
                                        .then(|| v.trim().parse::<usize>().unwrap())
                                })
                                .unwrap_or(0);
                            if bytes.len() >= at + 4 + length {
                                break (head, at + 4);
                            }
                        }
                    };
                    counter.fetch_add(1, Ordering::SeqCst);
                    let line = head.lines().next().unwrap_or_default();
                    let reply = handler(line, &bytes[body_at..]);
                    tokio::time::sleep(reply.delay).await;
                    let headers: String = reply
                        .headers
                        .iter()
                        .map(|(name, value)| format!("{name}: {value}\r\n"))
                        .collect();
                    let head = format!(
                        "HTTP/1.1 {} X\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n",
                        reply.status,
                        reply.body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&reply.body).await;
                });
            }
        });
        (url, count)
    }
    /// The base URL of a port nothing listens on: a relay that refuses connections.
    pub async fn refused() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    }
    /// 64 bytes of `byte`, in hex: a relay's signature.
    pub fn signature(byte: u8) -> String {
        hex::encode([byte; 64])
    }
    /// A relay's answer for a round.
    pub fn round_json(round: u64, signature: &str) -> String {
        format!(r#"{{"round":{round},"randomness":"aa","signature":"{signature}"}}"#)
    }
    /// The request line a relay is asked with for a chain and round.
    pub fn asked_line(chain_hash: alloy_primitives::B256, round: u64) -> String {
        format!("GET /{}/public/{round} HTTP/1.1", hex::encode(chain_hash))
    }
}

#[cfg(test)]
mod tests {
    use super::{fixture::*, *};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const GENESIS: u64 = 1_000_000;
    fn network() -> Network {
        Network {
            chain_hash: B256::repeat_byte(0x11),
            genesis: GENESIS,
            period: 3,
        }
    }
    /// The circuits of these tests, in a table of their own.
    const BREAKER: Breaker = Breaker::new("test_relay_breaker");
    async fn circuits(dir: &tempfile::TempDir) -> SqlitePool {
        let pool = SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(dir.path().join("circuits.sqlite"))
                .create_if_missing(true),
        )
        .await
        .unwrap();
        BREAKER.install(&pool).await.unwrap();
        pool
    }
    async fn failures(pool: &SqlitePool, relay: &str) -> Option<i64> {
        sqlx::query_scalar("SELECT failures FROM test_relay_breaker WHERE url=?")
            .bind(relay)
            .fetch_optional(pool)
            .await
            .unwrap()
    }
    /// A client of the test network's relays that excuses server errors when `server_errors` says so.
    fn client(pool: &SqlitePool, relays: Vec<String>, server_errors: bool) -> Client {
        Client {
            http: relay_client().unwrap(),
            pool: pool.clone(),
            breaker: BREAKER,
            relays,
            stragglers: Stragglers::default(),
            server_errors,
            verifier: "the verifier",
        }
    }
    const ROUND: u64 = 101;
    const ROUND_TIME: u64 = GENESIS + 3 * (ROUND - 1);
    #[test]
    fn round_times_follow_genesis_plus_rounds_minus_one_periods() {
        let network = network();
        assert_eq!(network.round_time(1), GENESIS);
        assert_eq!(network.round_time(11), GENESIS + 30);
        assert_eq!(network.round_time(0), GENESIS);
        assert_eq!(
            Network {
                period: u64::MAX,
                ..network
            }
            .round_time(3),
            u64::MAX
        );
        // evmnet: round 1 at its genesis, one round every 3 seconds.
        let evmnet = Network {
            genesis: 1_727_521_075,
            ..network
        };
        assert_eq!(evmnet.round_time(2), 1_727_521_078);
    }
    #[tokio::test]
    async fn the_first_signature_the_verifier_accepts_wins_and_a_wrong_one_moves_on_to_the_next_relay()
     {
        let dir = tempfile::tempdir().unwrap();
        let pool = circuits(&dir).await;
        let line = asked_line(network().chain_hash, ROUND);
        // The relay that answers first gives a well-formed signature that does not verify; the next one, the right one.
        let (wrong, _) = serve({
            let (line, body) = (line.clone(), round_json(ROUND, &signature(0x22)));
            move |request, _| {
                assert_eq!(request, line);
                answer(200, body.clone())
            }
        })
        .await;
        let (right, _) = serve({
            let body = round_json(ROUND, &signature(0x11));
            move |request, _| {
                assert_eq!(request, line);
                Answer {
                    delay: Duration::from_millis(300),
                    ..answer(200, body.clone())
                }
            }
        })
        .await;
        let asked = AtomicUsize::new(0);
        let verify = |signature: [u8; 64]| {
            asked.fetch_add(1, Ordering::SeqCst);
            async move { Ok(signature == [0x11; 64]) }
        };
        let relays = client(&pool, vec![wrong.clone(), right.clone()], true);
        let signature = relays
            .fetch_round(&network(), ROUND, ROUND_TIME + 1, verify)
            .await
            .unwrap();
        relays.stragglers.settled().await;
        assert_eq!(signature, [0x11; 64]);
        // The verifier was asked about both signatures, the wrong one first, and the relay that gave it is at fault.
        assert_eq!(asked.load(Ordering::SeqCst), 2);
        assert_eq!(failures(&pool, &wrong).await, Some(1));
        assert_eq!(failures(&pool, &right).await, None);
        // A signature no relay gives that verifies is no round, and each relay's reason is named.
        let error = relays
            .fetch_round(&network(), ROUND, ROUND_TIME + 1, |_| async { Ok(false) })
            .await
            .unwrap_err();
        assert!(!is_chain_read(&error));
        let error = error.to_string();
        assert!(
            error.starts_with("No drand relay served round 101: ")
                && error.contains(&format!("{wrong}: signature does not verify"))
                && error.contains(&format!("{right}: signature does not verify")),
            "{error}"
        );
        // A verifier that cannot be asked blames no relay: the failure is the caller's read of the chain.
        let error = client(&pool, vec![right.clone()], true)
            .fetch_round(&network(), ROUND, ROUND_TIME + 1, |_| async {
                Err(anyhow::anyhow!("node failure"))
            })
            .await
            .unwrap_err();
        assert!(is_chain_read(&error), "{error}");
        assert_eq!(failures(&pool, &right).await, Some(1));
        pool.close().await;
    }
    #[tokio::test]
    async fn a_server_error_is_excused_while_the_round_is_recent_only_when_the_caller_excuses_it() {
        let dir = tempfile::tempdir().unwrap();
        let pool = circuits(&dir).await;
        let (down, hits) = serve(|_, _| answer(500, "")).await;
        let never = |_: [u8; 64]| async { Ok::<_, anyhow::Error>(true) };
        // Up to two periods after its time the round may not be produced yet: the error counts for nothing.
        let excusing = client(&pool, vec![down.clone()], true);
        for now in [ROUND_TIME - 1, ROUND_TIME, ROUND_TIME + 6] {
            let error = excusing
                .fetch_round(&network(), ROUND, now, never)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(&format!(
                    "{down}: HTTP 500, the round may not be published yet"
                )),
                "{error}"
            );
        }
        assert_eq!(failures(&pool, &down).await, None);
        // Once the round is due, it is the relay's failure.
        let error = excusing
            .fetch_round(&network(), ROUND, ROUND_TIME + 7, never)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(&format!("{down}: HTTP 500")), "{error}");
        assert_eq!(failures(&pool, &down).await, Some(1));
        // A caller that does not excuse it counts it whatever the round's age.
        client(&pool, vec![down.clone()], false)
            .fetch_round(&network(), ROUND, ROUND_TIME, never)
            .await
            .unwrap_err();
        assert_eq!(failures(&pool, &down).await, Some(2));
        assert_eq!(hits.load(Ordering::SeqCst), 5);
        pool.close().await;
    }
    #[tokio::test]
    async fn each_breaker_keeps_its_circuits_in_its_own_table_and_is_named_plainly() {
        let dir = tempfile::tempdir().unwrap();
        let pool = circuits(&dir).await;
        let other = Breaker::new("other_relay_breaker");
        other.install(&pool).await.unwrap();
        assert_eq!(other.table(), "other_relay_breaker");
        let relay = "https://relay.example";
        for _ in 0..BREAKER_FAILURES {
            BREAKER.record(&pool, relay, true, 1000).await.unwrap();
        }
        assert_eq!(
            BREAKER.open(&pool, relay, 1000).await.unwrap(),
            Some((BREAKER_FAILURES, BREAKER_COOLDOWN_SECONDS))
        );
        assert_eq!(other.open(&pool, relay, 1000).await.unwrap(), None);
        assert_eq!(other.record(&pool, relay, true, 1000).await.unwrap(), 1);
        assert_eq!(failures(&pool, relay).await, Some(BREAKER_FAILURES));
        // A verified answer closes the circuit of its own table.
        BREAKER.record(&pool, relay, false, 1001).await.unwrap();
        assert_eq!(failures(&pool, relay).await, None);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT failures FROM other_relay_breaker WHERE url=?")
                .bind(relay)
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
        // A table is named by letters, digits and underscores, and nothing else reaches the SQL.
        for name in ["", "relay breaker", "x;DROP TABLE jobs", "a-b", "t\""] {
            assert!(
                std::panic::catch_unwind(|| Breaker::new(Box::leak(name.into()))).is_err(),
                "{name}"
            );
        }
        pool.close().await;
    }
    #[test]
    fn relays_default_to_the_four_public_ones_and_are_validated_strictly() {
        let defaults = DrandRelays::parse(None, false).unwrap();
        assert_eq!(defaults, DrandRelays::default());
        assert_eq!(
            defaults.urls(),
            [
                "https://api.drand.sh",
                "https://api2.drand.sh",
                "https://api3.drand.sh",
                "https://drand.cloudflare.com"
            ]
        );
        assert_eq!(
            defaults.to_string(),
            "https://api.drand.sh,https://api2.drand.sh,https://api3.drand.sh,https://drand.cloudflare.com"
        );
        // Whitespace and trailing slashes are normalized away, and a path stays.
        let custom = DrandRelays::parse(
            Some(" https://relay.example// , https://other.example/drand/ ,https://third.example:8443"),
            false,
        )
        .unwrap();
        assert_eq!(
            custom.urls(),
            [
                "https://relay.example",
                "https://other.example/drand",
                "https://third.example:8443"
            ]
        );
        // Loopback HTTP is for the local test chain alone.
        let local = "http://127.0.0.1:8545/base/";
        assert_eq!(
            DrandRelays::parse(Some(local), true).unwrap().urls(),
            ["http://127.0.0.1:8545/base"]
        );
        for invalid in [
            String::new(),
            " ".into(),
            ",".into(),
            "https://a.example,,https://b.example".into(),
            "https://a.example,".into(),
            "a.example".into(),
            "http://a.example".into(),
            "ftp://a.example".into(),
            "https://user:secret@a.example".into(),
            "https://:secret@a.example".into(),
            "https://a.example/?key=secret".into(),
            "https://a.example/#secret".into(),
            "https://a.example,https://a.example/".into(),
            "https://A.EXAMPLE,https://a.example".into(),
            format!("https://a.example/{}", "x".repeat(1025)),
            local.into(),
            "http://localhost:8545".into(),
        ] {
            let error = DrandRelays::parse(Some(&invalid), false).err().unwrap();
            assert!(!error.to_string().contains("secret"), "{invalid}");
        }
        for invalid in [
            "http://localhost:8545",
            "http://10.0.0.1:8545",
            "http://127.0.0.1:1?a=b",
            "https://a.example,https://a.example",
        ] {
            assert!(
                DrandRelays::parse(Some(invalid), true).is_err(),
                "{invalid}"
            );
        }
    }
    #[test]
    fn at_most_eight_relays_are_accepted_and_more_is_a_clear_configuration_error() {
        assert_eq!(MAX_RELAYS, 8);
        let list = |count: usize| {
            (1..=count)
                .map(|n| format!("https://relay{n}.example"))
                .collect::<Vec<_>>()
                .join(",")
        };
        let relays = DrandRelays::parse(Some(&list(8)), false).unwrap();
        assert_eq!(relays.urls().len(), 8);
        assert_eq!(relays.urls()[7], "https://relay8.example");
        for count in [9, 20] {
            let error = DrandRelays::parse(Some(&list(count)), false)
                .unwrap_err()
                .to_string();
            assert_eq!(error, "DRAND_RELAYS lists more than 8 relays");
        }
        // The limit counts distinct relays, and the local override is one relay whatever the list holds.
        assert!(
            DrandRelays::parse(Some(&format!("{},https://relay1.example", list(8))), false)
                .is_err()
        );
        assert_eq!(
            DrandRelays::configured(Some(&list(8)), Some("http://127.0.0.1:9"), true)
                .unwrap()
                .urls(),
            ["http://127.0.0.1:9"]
        );
        assert!(DrandRelays::configured(Some(&list(9)), Some("http://127.0.0.1:9"), true).is_err());
    }
    #[test]
    fn test_api_base_replaces_the_relay_list_by_that_one_base() {
        let list = "https://a.example,https://b.example";
        assert_eq!(
            DrandRelays::configured(Some(list), None, false)
                .unwrap()
                .urls(),
            ["https://a.example", "https://b.example"]
        );
        assert_eq!(
            DrandRelays::configured(None, None, true).unwrap(),
            DrandRelays::default()
        );
        for value in [None, Some(list)] {
            assert_eq!(
                DrandRelays::configured(value, Some("http://127.0.0.1:9/api/"), true)
                    .unwrap()
                    .urls(),
                ["http://127.0.0.1:9/api"]
            );
        }
        // A malformed list is still refused, so a misconfiguration does not hide behind the local override.
        assert!(
            DrandRelays::configured(Some("nonsense"), Some("http://127.0.0.1:9"), true).is_err()
        );
    }
    fn failed(reply: Reply) -> String {
        match reply {
            Reply::Failed(reason) => reason,
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn relay_answers_are_judged_for_the_round_asked_and_nothing_more() {
        let good = round_json(7, &signature(0x11));
        assert_eq!(reply(200, good.as_bytes(), 7), Reply::Signature([0x11; 64]));
        // Hex case does not matter, and nothing beyond round and signature is read.
        let upper = round_json(7, &signature(0xab).to_uppercase());
        assert_eq!(
            reply(200, upper.as_bytes(), 7),
            Reply::Signature([0xab; 64])
        );
        let bare = format!(
            r#"{{"round":7,"signature":"{}","extra":[1,2]}}"#,
            signature(1)
        );
        assert_eq!(reply(200, bare.as_bytes(), 7), Reply::Signature([1; 64]));
        // A round the relay says it does not have: `judge` decides by the round's age whether that is its fault.
        for status in [425, 404] {
            assert_eq!(reply(status, b"", 7), Reply::NotYet);
            assert_eq!(reply(status, good.as_bytes(), 7), Reply::NotYet);
        }
        // A server error is told apart, since a caller may excuse it while the round is recent (see `judge`).
        for status in [500, 502, 503, 504, 599] {
            assert_eq!(
                reply(status, good.as_bytes(), 7),
                Reply::ServerError(status)
            );
        }
        for status in [429, 408, 403, 400, 302, 199, 300, 600] {
            assert_eq!(
                failed(reply(status, good.as_bytes(), 7)),
                format!("HTTP {status}")
            );
        }
        // Another round than asked for, however valid its signature may be.
        assert_eq!(
            failed(reply(200, round_json(8, &signature(0x11)).as_bytes(), 7)),
            "answered round 8 for round 7"
        );
        let no_round = [
            r#"{"signature":"00"}"#.to_owned(),
            format!(r#"{{"round":"7","signature":"{}"}}"#, signature(1)),
            format!(r#"{{"round":7.5,"signature":"{}"}}"#, signature(1)),
            format!(r#"{{"round":-7,"signature":"{}"}}"#, signature(1)),
            format!(r#"{{"round":null,"signature":"{}"}}"#, signature(1)),
            "[]".to_owned(),
        ];
        for body in no_round {
            assert_eq!(
                failed(reply(200, body.as_bytes(), 7)),
                "answer has no round number",
                "{body}"
            );
        }
        for body in ["", "<html>"] {
            assert_eq!(failed(reply(200, body.as_bytes(), 7)), "answer is not JSON");
        }
        assert_eq!(failed(reply(204, b"", 7)), "answer is not JSON");
        // The signature is exactly 64 bytes of hex: no prefix, no other length, no other characters, no other type.
        let bad = [
            signature(1)[..127].to_owned(),
            signature(1) + "0",
            signature(1) + "00",
            signature(1).replacen('0', "g", 1),
            "0x".to_owned() + &signature(1)[..126],
            "z".repeat(128),
            "a".to_owned(),
            String::new(),
        ];
        for signature in bad {
            assert_eq!(
                failed(reply(200, round_json(7, &signature).as_bytes(), 7)),
                "signature is not 64 bytes of hex",
                "{signature}"
            );
        }
        for body in [
            r#"{"round":7}"#,
            r#"{"round":7,"signature":null}"#,
            r#"{"round":7,"signature":[1,2]}"#,
            r#"{"round":7,"signature":12345}"#,
        ] {
            assert_eq!(
                failed(reply(200, body.as_bytes(), 7)),
                "signature is not 64 bytes of hex",
                "{body}"
            );
        }
    }
    #[test]
    fn a_relay_is_excused_for_a_recent_round_it_lacks_and_at_fault_for_one_that_is_due() {
        let beacon = network();
        // A round is recent while it was scheduled at most two periods (6 seconds) before the fetch's chain time,
        // whether or not that time has come.
        let round = 101;
        let time = beacon.round_time(round);
        for (now, recent_round) in [
            (0, true),
            (time - 1, true),
            (time, true),
            (time + 5, true),
            (time + 6, true),
            (time + 7, false),
            (time + 60, false),
            (u64::MAX, false),
        ] {
            assert_eq!(recent(&beacon, round, now), recent_round, "{now}");
        }
        // The rule is in periods, not seconds.
        let slow = Network {
            period: 30,
            ..network()
        };
        assert!(recent(&slow, 3, slow.round_time(3) + 60));
        assert!(!recent(&slow, 3, slow.round_time(3) + 61));
        // Neither an answer of "not published" nor silence counts against a relay for a recent round; both do for a
        // round that is due, and the other replies do not depend on age.
        let silent = format!("no answer within {} s", RELAY_TIMEOUT.as_secs());
        for server_errors in [false, true] {
            for status in [404, 425] {
                assert_eq!(
                    judge(reply(status, b"", 7), true, server_errors),
                    Outcome::Excused("round not published yet".into())
                );
                assert_eq!(
                    judge(reply(status, b"", 7), false, server_errors),
                    Outcome::Failed("round already due but not served".into())
                );
            }
            assert_eq!(
                judge(Reply::TimedOut, true, server_errors),
                Outcome::Excused(format!("{silent}, the round may not be published yet"))
            );
            assert_eq!(
                judge(Reply::TimedOut, false, server_errors),
                Outcome::Failed(silent.clone())
            );
        }
        // A server error is the relay's failure whatever the round's age, unless the caller excuses it for a recent
        // round: relays have answered HTTP 500 for a round that is not produced yet.
        for status in [500, 503] {
            for recent in [true, false] {
                assert_eq!(
                    judge(Reply::ServerError(status), recent, false),
                    Outcome::Failed(format!("HTTP {status}"))
                );
            }
            assert_eq!(
                judge(Reply::ServerError(status), true, true),
                Outcome::Excused(format!("HTTP {status}, the round may not be published yet"))
            );
            assert_eq!(
                judge(Reply::ServerError(status), false, true),
                Outcome::Failed(format!("HTTP {status}"))
            );
        }
        assert_eq!(RELAY_TIMEOUT, Duration::from_secs(4));
        assert!(RELAY_TIMEOUT < FETCH_TIMEOUT && FETCH_TIMEOUT == Duration::from_secs(8));
        for (recent, server_errors) in [(true, false), (false, false), (true, true), (false, true)]
        {
            assert_eq!(
                judge(Reply::Signature([9; 64]), recent, server_errors),
                Outcome::Signature([9; 64])
            );
            assert_eq!(
                judge(Reply::Failed("HTTP 403".into()), recent, server_errors),
                Outcome::Failed("HTTP 403".into())
            );
        }
    }
    #[tokio::test]
    async fn a_relay_is_asked_for_its_chain_and_round_and_bounded_in_time_size_and_redirects() {
        let chain = network().chain_hash;
        let client = relay_client().unwrap();
        let good = round_json(5, &signature(0x11));
        let (relay, count) = serve({
            let (line, good) = (asked_line(chain, 5), good.clone());
            move |request, _| {
                if request == line {
                    answer(200, good.clone())
                } else {
                    answer(404, "")
                }
            }
        })
        .await;
        assert_eq!(
            ask(&client, &relay, chain, 5).await,
            Reply::Signature([0x11; 64])
        );
        // A round the relay has not published.
        assert_eq!(ask(&client, &relay, chain, 6).await, Reply::NotYet);
        assert_eq!(count.load(Ordering::SeqCst), 2);
        let (relay, _) = serve(|_, _| answer(425, "Too early")).await;
        assert_eq!(ask(&client, &relay, chain, 5).await, Reply::NotYet);
        let (relay, _) = serve(|_, _| answer(503, "down")).await;
        assert_eq!(
            ask(&client, &relay, chain, 5).await,
            Reply::ServerError(503)
        );
        // An answer past the size limit is refused, however the rest of it looks.
        let big = format!(
            r#"{{"round":5,"signature":"{}","pad":"{}"}}"#,
            signature(1),
            "x".repeat(ANSWER_LIMIT)
        );
        let (relay, _) = serve(move |_, _| answer(200, big.clone())).await;
        assert_eq!(
            failed(ask(&client, &relay, chain, 5).await),
            format!("answer over {ANSWER_LIMIT} bytes")
        );
        // A redirect is not followed: its target is never asked.
        let (target, hits) = serve(move |_, _| answer(200, good.clone())).await;
        let (relay, _) = serve(move |_, _| {
            let mut redirect = answer(302, "");
            redirect.headers.push(("Location", target.clone()));
            redirect
        })
        .await;
        assert_eq!(failed(ask(&client, &relay, chain, 5).await), "HTTP 302");
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        // A relay that refuses the connection is a transport failure that does not name its address (a network stack that
        // is slow to refuse it may run into the time limit first).
        match ask(&client, &refused().await, chain, 5).await {
            Reply::Failed(reason) => {
                assert!(reason.starts_with("transport: "), "{reason}");
                assert!(!reason.contains("127.0.0.1"), "{reason}");
            }
            other => assert_eq!(other, Reply::TimedOut),
        }
    }

    #[tokio::test]
    async fn a_relay_that_holds_a_request_is_waited_for_up_to_the_time_limit_and_not_beyond() {
        let (chain, client) = (network().chain_hash, relay_client().unwrap());
        // Relays hold a request for a round that is about to be published, which took 1.8 to 2.25 seconds when measured:
        // one that holds it for three seconds answers in time, one that never answers is timed out at the limit.
        let (holding, _) = serve(|_, _| Answer {
            delay: Duration::from_secs(3),
            ..answer(200, round_json(5, &signature(0x11)))
        })
        .await;
        let (silent, _) = serve(|_, _| Answer {
            delay: Duration::from_secs(30),
            ..answer(200, "")
        })
        .await;
        let started = tokio::time::Instant::now();
        let (held, gone) = tokio::join!(
            ask(&client, &holding, chain, 5),
            ask(&client, &silent, chain, 5)
        );
        assert_eq!(held, Reply::Signature([0x11; 64]));
        assert_eq!(gone, Reply::TimedOut);
        assert!(
            started.elapsed() >= RELAY_TIMEOUT
                && started.elapsed() < RELAY_TIMEOUT + Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn askable_lists_every_closed_relay_and_the_open_one_that_closes_first() {
        let dir = tempfile::tempdir().unwrap();
        let pool = &circuits(&dir).await;
        let relays: Vec<String> = [
            "https://a.example",
            "https://b.example",
            "https://c.example",
        ]
        .map(str::to_owned)
        .into();
        let trip = |relay: &str, at: u64| {
            let (pool, relay) = (pool.clone(), relay.to_owned());
            async move {
                for _ in 0..3 {
                    BREAKER.record(&pool, &relay, true, at).await.unwrap();
                }
            }
        };
        let of = |wanted: &[usize]| -> Vec<String> {
            wanted.iter().map(|at| relays[*at].clone()).collect()
        };
        assert_eq!(askable(pool, BREAKER, &relays, 1000).await.unwrap(), relays);
        // b failed last and c earliest: with a still closed, a is asked and, as the probe, c, whose cooldown ends first.
        trip(&relays[1], 1010).await;
        trip(&relays[2], 1000).await;
        assert_eq!(
            askable(pool, BREAKER, &relays, 1011).await.unwrap(),
            of(&[0, 2])
        );
        // With a open too, the probe is still c, which opened first, and it is asked alone.
        trip(&relays[0], 1005).await;
        assert_eq!(
            askable(pool, BREAKER, &relays, 1011).await.unwrap(),
            of(&[2])
        );
        // Once its cooldown has passed c is simply closed, and the probe is the one that closes next.
        let cooldown = BREAKER_COOLDOWN_SECONDS;
        assert_eq!(
            askable(pool, BREAKER, &relays, 1000 + cooldown)
                .await
                .unwrap(),
            of(&[2, 0])
        );
        assert_eq!(
            askable(pool, BREAKER, &relays, 1005 + cooldown)
                .await
                .unwrap(),
            of(&[0, 2, 1])
        );
        assert_eq!(
            askable(pool, BREAKER, &relays, 1010 + cooldown)
                .await
                .unwrap(),
            relays
        );
        assert!(askable(pool, BREAKER, &[], 1011).await.unwrap().is_empty());
        // A verified answer closes a circuit at once, and the probe moves on to the next.
        BREAKER.record(pool, &relays[2], false, 1012).await.unwrap();
        assert_eq!(
            askable(pool, BREAKER, &relays, 1013).await.unwrap(),
            of(&[2, 0])
        );
        pool.close().await;
    }
}
