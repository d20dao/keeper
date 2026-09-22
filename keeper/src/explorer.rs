//! Optional public chain-log index. Database work never participates in nonce ownership or signing.
use crate::{
    abi::{Coordinator as C, EpochRegistry as E},
    proxy::{ApprovedNext, IMPLEMENTATION_SLOT, ProxyPin, RuntimePins},
    rpc::{Rpc, quantity},
};
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_sol_types::{SolCall, SolEvent, sol};
use anyhow::{Context, Result, ensure};
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
    time::Duration,
};
use tokio_postgres::{
    Client,
    config::{ChannelBinding, SslMode},
};

mod refresh;

pub const SCHEMA: &str = include_str!("../explorer-schema.sql");
const MAX_BLOCKS: u64 = 128;
const LOOKBACK: u64 = 12;
/// Block rows are read back only for the reorg check (the last indexed block and its lookback
/// anchor), so older rows are pruned; events and requests keep their own block hashes.
const BLOCK_RETENTION: i64 = 64;
const STATUS_INTERVAL: Duration = Duration::from_secs(60);
const MAX_LOGS: usize = 4096;
const MAX_REQUESTS: usize = 1024;
sol! {
    struct MapSpec {uint8 operation;uint256 lower;uint256 upper;uint32 count;uint32 population;}
    interface PublicConfig {
        function initialFeeRecipient() external view returns(address);
        function initialMinFee() external view returns(uint256);
        function requestFeePaid(uint256 requestId) external view returns(uint256);
        function confirmationBlocks() external view returns(uint16);
        function keyHash() external view returns(bytes32);
        function firstEpochStart() external view returns(uint64);
        function hyperliquidSigner() external view returns(address);
        function ethereumBlockSigner() external view returns(address);
        function btcTradeSigner() external view returns(address);
        function ethTradeSigner() external view returns(address);
        function getMapping(uint256 requestId) external view returns(MapSpec);
    }
    event EpochCommitted(uint64 indexed epochId,bytes32 indexed epochHash,bytes packet);
    event FulfillmentEvidence(uint256 indexed requestId,bytes32 indexed transcriptHash,bytes packet);
    event RandomnessRequested(uint256 indexed requestId,address indexed consumer,bytes32 indexed keyHash,bytes32 clientSeed,uint64 requestBlock,uint32 callbackGasLimit,uint256 feePaid,address refundAddress,uint64 deadline);
}
// No Debug and no raw parse/connection errors escape this module's background task.
pub struct Settings(tokio_postgres::Config);
impl Settings {
    pub fn from_env() -> Result<Option<Self>> {
        match std::env::var("NEON_DB") {
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(_) => anyhow::bail!("Invalid explorer configuration"),
            Ok(value) => Self::parse(Some(value)),
        }
    }
    fn parse(value: Option<String>) -> Result<Option<Self>> {
        let Some(value) = value else { return Ok(None) };
        let url = reqwest::Url::parse(&value)
            .map_err(|_| anyhow::anyhow!("Invalid explorer configuration"))?;
        ensure!(
            matches!(url.scheme(), "postgres" | "postgresql")
                && url.host_str().is_some()
                && !url.username().is_empty()
                && url.password().is_some(),
            "Invalid explorer configuration"
        );
        let mut config = tokio_postgres::Config::from_str(&value)
            .map_err(|_| anyhow::anyhow!("Invalid explorer configuration"))?;
        config
            .ssl_mode(SslMode::Require)
            .channel_binding(ChannelBinding::Require)
            // Per TCP connection attempt; TLS, authentication and a serverless compute waking up are bounded by
            // CONNECT_DEADLINE instead.
            .connect_timeout(Duration::from_secs(10))
            // A silently dropped path (a NAT, proxy or pooler forgetting the session) is detected within about a
            // minute instead of the platform default of two hours of idleness, and unacknowledged writes fail
            // after 30 s where the platform supports it.
            .keepalives(true)
            .keepalives_idle(Duration::from_secs(30))
            .keepalives_interval(Duration::from_secs(10))
            .keepalives_retries(3)
            .tcp_user_timeout(Duration::from_secs(30))
            .application_name("d20dao-keeper-explorer");
        Ok(Some(Self(config)))
    }
    /// Idempotent schema-only setup using the same verified TLS and required channel binding as indexing.
    /// Errors intentionally contain no connection details or raw database responses.
    pub async fn initialize(&self) -> Result<()> {
        tokio::time::timeout(CONNECT_DEADLINE + Duration::from_secs(15), async {
            let session = self
                .connect()
                .await
                .map_err(|_| anyhow::anyhow!("Explorer database connection failed"))?;
            session
                .client
                .batch_execute(SCHEMA)
                .await
                .map_err(|_| anyhow::anyhow!("Explorer schema initialization failed"))?;
            session
                .client
                .query_one("SELECT 1", &[])
                .await
                .map_err(|_| anyhow::anyhow!("Explorer database probe failed"))?;
            Ok(())
        })
        .await
        .map_err(|_| anyhow::anyhow!("Explorer database initialization timed out"))?
    }
    async fn connect(&self) -> Result<Session> {
        #[cfg(test)]
        if let Some(roots) = tests::TEST_ROOTS.get() {
            return self.connect_with_roots(roots.clone()).await;
        }
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        self.connect_with_roots(roots).await
    }
    async fn connect_with_roots(&self, roots: rustls::RootCertStore) -> Result<Session> {
        let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let (client, connection) = tokio::time::timeout(
            CONNECT_DEADLINE,
            self.0
                .connect(tokio_postgres_rustls::MakeRustlsConnect::new(tls)),
        )
        .await
        .map_err(|_| anyhow::Error::new(Timeout("Explorer database connection")))??;
        let task = tokio::spawn(async move {
            let _ = connection.await;
        });
        let now = tokio::time::Instant::now();
        Ok(Session {
            client,
            task,
            opened: now,
            used: now,
        })
    }
}
/// One database session. It is replaced when the server or the network closed it, after SESSION_MAX_AGE, after
/// SESSION_MAX_IDLE without use, and when a round timed out inside it; never merely because a round failed.
struct Session {
    client: Client,
    task: tokio::task::JoinHandle<()>,
    opened: tokio::time::Instant,
    used: tokio::time::Instant,
}
impl Session {
    /// Why this session should not be used for the next round, if it should not.
    fn retire(&self, now: tokio::time::Instant) -> Option<&'static str> {
        if self.client.is_closed() || self.task.is_finished() {
            Some("closed")
        } else if now.saturating_duration_since(self.opened) >= SESSION_MAX_AGE {
            Some("age")
        } else if now.saturating_duration_since(self.used) >= SESSION_MAX_IDLE {
            Some("idle")
        } else {
            None
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.task.abort();
    }
}
/// TLS, SCRAM with channel binding and a suspended serverless compute resuming, together.
const CONNECT_DEADLINE: Duration = Duration::from_secs(30);
/// Healthy sessions are still replaced after this long, well inside connection-pooler and proxy lifetimes, so a
/// server-side limit is met by a planned reconnect between rounds rather than a failure inside one.
const SESSION_MAX_AGE: Duration = Duration::from_secs(15 * 60);
/// A session unused this long (for example while the chain RPC was unavailable) is replaced before its next use
/// rather than trusted: serverless computes and poolers drop idle clients.
const SESSION_MAX_IDLE: Duration = Duration::from_secs(4 * 60);
/// One round: connect if needed, register once, read one chain batch and commit it.
const ROUND_DEADLINE: Duration = Duration::from_secs(90);
/// Pause between rounds once the index has reached the chain head; shorter while catching up.
const CAUGHT_UP_PAUSE: Duration = Duration::from_secs(5);
const CATCH_UP_PAUSE: Duration = Duration::from_secs(1);
/// Indexing is reported as behind only after rounds have failed for this long. A reconnect, a rate-limited RPC or
/// a block the endpoint has not served yet resolves within a round or two and is logged at debug level only.
const BEHIND_WARN: Duration = Duration::from_secs(3 * 60);
/// While still behind, the warning is repeated at this interval so a persistent cause stays visible.
const BEHIND_REPEAT: Duration = Duration::from_secs(60 * 60);
/// A follower indexes only when the shared cursor has not moved for this long: while the primary indexes, a second
/// writer would only race it for the same cursor and double the RPC reads.
const FOLLOWER_INDEX_TAKEOVER: Duration = Duration::from_secs(60);

/// Typed failure classes, so a round's failure can be logged by class without its raw text (database and RPC errors
/// can carry connection details) and so the loop can tell a lost session from everything else.
#[derive(Debug)]
struct Contention(&'static str);
#[derive(Debug)]
struct ChainMoving(&'static str);
#[derive(Debug)]
struct Review(String);
/// A proxy moved to its approved next implementation. Nothing needs review: the keeper exits, and its restart
/// registers the new implementation and indexes the blocks after the move.
#[derive(Debug)]
struct Upgrading(String);
#[derive(Debug)]
struct Timeout(&'static str);
macro_rules! class_error {
    ($($t:ty),*) => {$(
        impl std::fmt::Display for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        impl std::error::Error for $t {}
    )*};
}
class_error!(Contention, ChainMoving, Review, Upgrading, Timeout);
/// Why a round did not complete. Only this class is logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cause {
    /// The database session is gone: the server, a proxy or the network closed it.
    Connection,
    /// The database answered with an error, for example a statement timeout.
    Database,
    /// Every chain RPC endpoint was rate limiting.
    RateLimited,
    /// Another chain RPC failure.
    Rpc,
    /// The endpoint had not served the block this round needed yet, or the chain moved during the round.
    ChainMoving,
    /// Another indexer advanced the shared cursor first. Its progress is this deployment's progress.
    Contention,
    /// A persistent condition needing operator review: a configuration or implementation change.
    Review,
    /// A proxy moved to its approved next implementation; the keeper's restart takes over the index. Transient.
    Upgrade,
    /// The round exceeded ROUND_DEADLINE, or connecting exceeded CONNECT_DEADLINE.
    Timeout,
    Other,
}
impl Cause {
    fn of(error: &anyhow::Error) -> Self {
        for cause in error.chain() {
            if let Some(db) = cause.downcast_ref::<tokio_postgres::Error>() {
                return if db.as_db_error().is_some() {
                    Self::Database
                } else if db.is_closed()
                    || std::error::Error::source(db).is_some_and(|s| s.is::<std::io::Error>())
                {
                    Self::Connection
                } else {
                    Self::Database
                };
            }
            if cause.is::<Contention>() {
                return Self::Contention;
            }
            if cause.is::<ChainMoving>() {
                return Self::ChainMoving;
            }
            if cause.is::<Review>() {
                return Self::Review;
            }
            if cause.is::<Upgrading>() {
                return Self::Upgrade;
            }
            if cause.is::<Timeout>() {
                return Self::Timeout;
            }
        }
        if crate::rpc::is_rate_limited(error) {
            Self::RateLimited
        } else if crate::rpc::is_delivery_failure(error) {
            Self::Rpc
        } else {
            Self::Other
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Connection => "database_connection",
            Self::Database => "database",
            Self::RateLimited => "rpc_rate_limited",
            Self::Rpc => "rpc",
            Self::ChainMoving => "chain_not_ready",
            Self::Contention => "cursor_contention",
            Self::Review => "review_required",
            Self::Upgrade => "approved_upgrade",
            Self::Timeout => "timeout",
            Self::Other => "other",
        }
    }
}
/// When indexing counts as behind and what to log about it: nothing for a round that fails and is followed by a
/// success, one warning once rounds have failed for BEHIND_WARN, a reminder every BEHIND_REPEAT, and one line when
/// it catches up after a warning. Contention is another writer's progress, not lag.
#[derive(Default)]
struct Lag {
    since: Option<tokio::time::Instant>,
    warned: Option<tokio::time::Instant>,
}
#[derive(Debug, PartialEq, Eq)]
enum LagReport {
    Quiet,
    Behind { seconds: u64 },
    CaughtUp { seconds: u64 },
}
impl Lag {
    fn failed(&mut self, now: tokio::time::Instant, cause: Cause) -> LagReport {
        if cause == Cause::Contention {
            return self.succeeded(now);
        }
        let since = *self.since.get_or_insert(now);
        let behind = now.saturating_duration_since(since);
        let due = match self.warned {
            None => behind >= BEHIND_WARN,
            Some(at) => now.saturating_duration_since(at) >= BEHIND_REPEAT,
        };
        if due {
            self.warned = Some(now);
            LagReport::Behind {
                seconds: behind.as_secs(),
            }
        } else {
            LagReport::Quiet
        }
    }
    fn succeeded(&mut self, now: tokio::time::Instant) -> LagReport {
        let since = self.since.take();
        match (self.warned.take(), since) {
            (Some(_), Some(since)) => LagReport::CaughtUp {
                seconds: now.saturating_duration_since(since).as_secs(),
            },
            _ => LagReport::Quiet,
        }
    }
}
/// Where the live status row comes from: the keeper journal's health observation and its wallet.
pub struct StatusSource {
    pub db: std::path::PathBuf,
    pub keeper: Address,
    pub role: crate::config::Role,
}
#[derive(Debug, PartialEq)]
struct KeeperStatus {
    role: &'static str,
    healthy: bool,
    send_enabled: bool,
    faults: Vec<String>,
    observed_at: u64,
    published_at: u64,
    keeper: String,
    balance: String,
    head_block: u64,
    pending_requests: i64,
    last_served_at: Option<i64>,
}
/// Fault codes without details: node rejection reasons and nonce numbers stay local.
fn public_faults(faults: &[String]) -> Vec<String> {
    faults
        .iter()
        .map(|fault| fault.split(':').next().unwrap_or_default().to_owned())
        .filter(|code| !code.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
async fn observe_status(rpc: &Rpc, source: &StatusSource) -> Result<KeeperStatus> {
    let mut health = crate::health::read(&source.db).await?;
    let now = crate::health::now()?;
    crate::health::check_freshness(&mut health, now, 30);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(1))
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&source.db)
                .read_only(true)
                .busy_timeout(Duration::from_millis(500)),
        )
        .await?;
    let counts = async {
        let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE state IN ('pending','prepared','signed','submitted') AND deadline>=?")
            .bind(i64::try_from(now)?)
            .fetch_one(&pool)
            .await?;
        let last: Option<i64> =
            sqlx::query_scalar("SELECT MAX(observed_at) FROM audit_events WHERE kind='served'")
                .fetch_one(&pool)
                .await?;
        anyhow::Ok((pending, last))
    }
    .await;
    pool.close().await;
    let (pending_requests, last_served_at) = counts?;
    let head = rpc.head().await?;
    let balance: U256 = serde_json::from_value(
        rpc.request("eth_getBalance", json!([source.keeper, "latest"]))
            .await?,
    )?;
    Ok(KeeperStatus {
        role: source.role.name(),
        healthy: health.healthy,
        send_enabled: health.send_enabled,
        faults: public_faults(&health.faults),
        observed_at: health.observed_at,
        published_at: now,
        keeper: address(source.keeper),
        balance: balance.to_string(),
        head_block: head.number,
        pending_requests,
        last_served_at,
    })
}
/// A follower replaces another keeper's status row only once it is this many seconds older than its own report,
/// so a live primary (reporting every STATUS_INTERVAL) keeps the row and the public status never alternates.
const FOLLOWER_STATUS_TAKEOVER_SECONDS: i64 = 90;
/// Replace the deployment's single status row; history is not kept. A primary always writes it. A follower writes
/// it only when the row is its own or the writer has stopped reporting, so while both run only the primary's
/// status is public and during a primary outage the follower's is.
async fn publish_status(
    pool: &Client,
    chain: &str,
    coordinator: &str,
    s: &KeeperStatus,
) -> Result<()> {
    pool.execute("INSERT INTO d20dao_explorer.keeper_status(chain_id,coordinator,keeper,healthy,send_enabled,faults,observed_at,published_at,keeper_balance,head_block,pending_requests,last_served_at,role) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) ON CONFLICT(chain_id,coordinator) DO UPDATE SET keeper=EXCLUDED.keeper,healthy=EXCLUDED.healthy,send_enabled=EXCLUDED.send_enabled,faults=EXCLUDED.faults,observed_at=EXCLUDED.observed_at,published_at=EXCLUDED.published_at,keeper_balance=EXCLUDED.keeper_balance,head_block=EXCLUDED.head_block,pending_requests=EXCLUDED.pending_requests,last_served_at=EXCLUDED.last_served_at,role=EXCLUDED.role WHERE EXCLUDED.role='primary' OR d20dao_explorer.keeper_status.keeper=EXCLUDED.keeper OR d20dao_explorer.keeper_status.published_at<EXCLUDED.published_at-$14",
        &[&chain,&coordinator,&s.keeper,&s.healthy,&s.send_enabled,&json!(s.faults),&i64::try_from(s.observed_at)?,&i64::try_from(s.published_at)?,&s.balance,&i64::try_from(s.head_block)?,&s.pending_requests,&s.last_served_at,&s.role,&FOLLOWER_STATUS_TAKEOVER_SECONDS]).await?;
    Ok(())
}
pub struct Task(tokio::task::JoinHandle<()>);
impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}
pub fn spawn(
    settings: Option<Settings>,
    rpc: Rpc,
    pins: RuntimePins,
    next: ApprovedNext,
    chain_id: u64,
    status: StatusSource,
) -> Option<Task> {
    let settings = settings?;
    Some(Task(tokio::spawn(
        Indexer::new(settings, rpc, pins, next, chain_id, status).run(),
    )))
}
/// What one successful round did: the cursor after it and whether the index reached the chain head.
struct Progress {
    cursor: u64,
    caught_up: bool,
}
/// A follower's view of the shared cursor. A follower indexes only while the cursor is its own or has not moved
/// for FOLLOWER_INDEX_TAKEOVER, so while the primary indexes the follower adds no writes and no chain reads.
struct CursorWatch {
    value: Option<u64>,
    since: tokio::time::Instant,
    mine: bool,
}
impl CursorWatch {
    fn new(now: tokio::time::Instant) -> Self {
        Self {
            value: None,
            since: now,
            mine: false,
        }
    }
    fn should_index(&mut self, cursor: u64, now: tokio::time::Instant) -> bool {
        if self.value != Some(cursor) {
            // Moved by another writer since this node last looked (this node records its own writes).
            self.value = Some(cursor);
            self.since = now;
            self.mine = false;
            return false;
        }
        self.mine || now.saturating_duration_since(self.since) >= FOLLOWER_INDEX_TAKEOVER
    }
    fn wrote(&mut self, before: u64, after: u64, now: tokio::time::Instant) {
        if after != before {
            self.value = Some(after);
            self.since = now;
            self.mine = true;
        }
    }
}
struct Indexer {
    settings: Settings,
    rpc: Rpc,
    pins: RuntimePins,
    next: ApprovedNext,
    chain_id: u64,
    status: StatusSource,
    session: Option<Session>,
    registered: Option<Deployment>,
    span: u64,
    lag: Lag,
    status_at: Option<tokio::time::Instant>,
    status_lag: Lag,
    follower: Option<CursorWatch>,
}
impl Indexer {
    fn new(
        settings: Settings,
        rpc: Rpc,
        pins: RuntimePins,
        next: ApprovedNext,
        chain_id: u64,
        status: StatusSource,
    ) -> Self {
        let follower = status
            .role
            .is_follower()
            .then(|| CursorWatch::new(tokio::time::Instant::now()));
        Self {
            settings,
            rpc,
            pins,
            next,
            chain_id,
            status,
            session: None,
            registered: None,
            span: MAX_BLOCKS,
            lag: Lag::default(),
            status_at: None,
            status_lag: Lag::default(),
            follower,
        }
    }
    async fn run(mut self) {
        loop {
            // Status is published independently of indexing progress and never blocks it.
            self.publish_status().await;
            if let Some(reason) = self
                .session
                .as_ref()
                .and_then(|session| session.retire(tokio::time::Instant::now()))
            {
                tracing::debug!(reason, "Explorer database session replaced");
                self.session = None;
            }
            let result = tokio::time::timeout(ROUND_DEADLINE, self.round())
                .await
                .unwrap_or_else(|_| Err(Timeout("Explorer round").into()));
            let now = tokio::time::Instant::now();
            let pause = match result {
                Ok(progress) => {
                    self.span = (self.span * 2).min(MAX_BLOCKS);
                    if let LagReport::CaughtUp { seconds } = self.lag.succeeded(now) {
                        tracing::info!(behind_seconds = seconds, "Public explorer index caught up");
                    }
                    if progress.caught_up {
                        CAUGHT_UP_PAUSE
                    } else {
                        CATCH_UP_PAUSE
                    }
                }
                Err(error) => {
                    let cause = Cause::of(&error);
                    // Only a lost or timed-out session is replaced. The registration survives every failure
                    // except a database error, which may mean the schema or deployment rows are gone.
                    if matches!(cause, Cause::Connection | Cause::Timeout)
                        || self
                            .session
                            .as_ref()
                            .is_some_and(|session| session.client.is_closed())
                    {
                        self.session = None;
                    }
                    if cause == Cause::Database {
                        self.registered = None;
                    }
                    if cause != Cause::Contention {
                        self.span = (self.span / 2).max(1);
                    }
                    tracing::debug!(cause = cause.name(), "Public explorer round deferred");
                    if let LagReport::Behind { seconds } = self.lag.failed(now, cause) {
                        tracing::warn!(
                            cause = cause.name(),
                            behind_seconds = seconds,
                            "Public explorer index is behind; service processing continues"
                        );
                    }
                    if cause == Cause::Contention {
                        CATCH_UP_PAUSE
                    } else {
                        CAUGHT_UP_PAUSE
                    }
                }
            };
            tokio::time::sleep(pause).await;
        }
    }
    async fn round(&mut self) -> Result<Progress> {
        if self.session.is_none() {
            self.session = Some(self.settings.connect().await?);
        }
        let Self {
            session,
            registered,
            rpc,
            pins,
            next,
            chain_id,
            span,
            follower,
            ..
        } = self;
        let session = session.as_mut().context("Explorer session")?;
        session.used = tokio::time::Instant::now();
        let pool = &mut session.client;
        if registered.is_none() {
            pool.batch_execute(SCHEMA).await?;
            *registered = Some(register(pool, rpc, *pins, *next, *chain_id).await?);
        }
        let deployment = registered.as_ref().context("Explorer registration")?;
        let before = match follower.as_mut() {
            Some(watch) => {
                let before = cursor(pool, *chain_id, *pins).await?;
                if !watch.should_index(before, tokio::time::Instant::now()) {
                    session.used = tokio::time::Instant::now();
                    return Ok(Progress {
                        cursor: before,
                        caught_up: true,
                    });
                }
                Some(before)
            }
            None => None,
        };
        let progress = scan(pool, rpc, *pins, *chain_id, deployment, *span).await?;
        let now = tokio::time::Instant::now();
        if let (Some(watch), Some(before)) = (follower.as_mut(), before) {
            watch.wrote(before, progress.cursor, now);
        }
        session.used = now;
        Ok(progress)
    }
    async fn publish_status(&mut self) {
        let now = tokio::time::Instant::now();
        if self.registered.is_none()
            || self
                .status_at
                .is_some_and(|at| at.elapsed() < STATUS_INTERVAL)
        {
            return;
        }
        // A session due for replacement is left to the next round rather than used here.
        let Some(session) = self.session.as_ref().filter(|s| s.retire(now).is_none()) else {
            return;
        };
        self.status_at = Some(now);
        let chain = self.chain_id.to_string();
        let coordinator = address(self.pins.coordinator.proxy);
        let observed = tokio::time::timeout(
            Duration::from_secs(10),
            observe_status(&self.rpc, &self.status),
        )
        .await
        .unwrap_or_else(|_| Err(Timeout("Keeper status observation").into()));
        // A publication that hangs is the session's fault; an observation that hangs is the chain RPC's.
        let mut hung = false;
        let published = match observed {
            Ok(observed) => tokio::time::timeout(
                Duration::from_secs(10),
                publish_status(&session.client, &chain, &coordinator, &observed),
            )
            .await
            .unwrap_or_else(|_| {
                hung = true;
                Err(Timeout("Keeper status publication").into())
            }),
            Err(error) => Err(error),
        };
        let now = tokio::time::Instant::now();
        match published {
            Ok(()) => {
                if let Some(session) = self.session.as_mut() {
                    session.used = now;
                }
                if let LagReport::CaughtUp { .. } = self.status_lag.succeeded(now) {
                    tracing::info!("Public keeper status published again");
                }
            }
            Err(error) => {
                let cause = Cause::of(&error);
                if cause == Cause::Connection || hung {
                    self.session = None;
                }
                tracing::debug!(cause = cause.name(), "Public keeper status not published");
                if let LagReport::Behind { seconds } = self.status_lag.failed(now, cause) {
                    tracing::warn!(
                        cause = cause.name(),
                        unpublished_seconds = seconds,
                        "Public keeper status not published; indexing continues"
                    );
                }
            }
        }
    }
}
async fn cursor(pool: &Client, chain: u64, pins: RuntimePins) -> Result<u64> {
    let next: i64 = pool
        .query_one(
            "SELECT next_block FROM d20dao_explorer.cursors WHERE chain_id=$1 AND coordinator=$2",
            &[&chain.to_string(), &address(pins.coordinator.proxy)],
        )
        .await?
        .get(0);
    Ok(u64::try_from(next)?)
}
fn address(a: Address) -> String {
    a.to_string().to_lowercase()
}
async fn at<T: SolCall>(rpc: &Rpc, to: Address, call: T, number: u64) -> Result<T::Return> {
    let value = rpc
        .request(
            "eth_call",
            json!([{"to":to,"data":Bytes::from(call.abi_encode())},format!("0x{number:x}")]),
        )
        .await?;
    let bytes: Bytes = serde_json::from_value(value)?;
    Ok(T::abi_decode_returns(&bytes)?)
}
#[derive(Clone)]
struct Header {
    number: u64,
    hash: String,
    timestamp: u64,
}
async fn header(rpc: &Rpc, number: u64) -> Result<Header> {
    let value = rpc
        .request(
            "eth_getBlockByNumber",
            json!([format!("0x{number:x}"), false]),
        )
        .await?;
    if value.is_null() {
        return Err(ChainMoving("Block not served yet").into());
    }
    if quantity(&value["number"])? != number {
        return Err(ChainMoving("Unexpected block").into());
    }
    Ok(Header {
        number,
        hash: value["hash"].as_str().context("Missing block hash")?.into(),
        timestamp: quantity(&value["timestamp"])?,
    })
}
struct Deployment {
    first: u64,
    key_hash: B256,
    /// Every implementation identity keepers started with, from the deployment row: history is attributed from it.
    approved: Vec<RuntimePins>,
    /// This keeper's approved next implementations: blocks after a move to one of them wait for its restart.
    next: ApprovedNext,
}
async fn register(
    pool: &mut Client,
    rpc: &Rpc,
    pins: RuntimePins,
    next: ApprovedNext,
    chain: u64,
) -> Result<Deployment> {
    verify_pins(rpc, pins, next).await?;
    let head = rpc.finalized_head().await?;
    let c = pins.coordinator.proxy;
    let r = pins.registry.proxy;
    let n = head.number;
    let (x, y, recipient, min_fee, confirmations, key_hash, start, catalog_hash) = tokio::try_join!(
        at(rpc, c, C::publicKeyXCall {}, n),
        at(rpc, c, C::publicKeyYCall {}, n),
        at(rpc, c, PublicConfig::initialFeeRecipientCall {}, n),
        at(rpc, c, PublicConfig::initialMinFeeCall {}, n),
        at(rpc, c, PublicConfig::confirmationBlocksCall {}, n),
        at(rpc, c, PublicConfig::keyHashCall {}, n),
        at(rpc, r, PublicConfig::firstEpochStartCall {}, n),
        at(rpc, r, E::catalogHashCall {}, n)
    )?;
    let (a, b, d, e, protocol) = tokio::try_join!(
        at(rpc, r, PublicConfig::hyperliquidSignerCall {}, n),
        at(rpc, r, PublicConfig::ethereumBlockSignerCall {}, n),
        at(rpc, r, PublicConfig::btcTradeSignerCall {}, n),
        at(rpc, r, PublicConfig::ethTradeSignerCall {}, n),
        at(rpc, c, C::protocolConfigurationHashCall {}, n)
    )?;
    let config = json!({"publicKey":[x.to_string(),y.to_string()],"feeRecipient":address(recipient),"initialMinFee":min_fee.to_string(),"confirmationBlocks":confirmations,
        "registry":address(r),"catalogHash":catalog_hash.to_string(),"firstEpochStart":start.to_string()});
    let catalog = json!({"signers":[address(a),address(b),address(d),address(e)],"registry":address(r),"chainId":chain.to_string(),"firstEpochStart":start.to_string()});
    let old:Option<(String,Value)>=(pool.query_opt("SELECT protocol_configuration_hash,implementation_pins FROM d20dao_explorer.deployments WHERE chain_id=$1 AND coordinator=$2",&[&(chain.to_string()),&(address(c))]).await?).map(|r|(r.get(0),r.get(1)));
    let mut approved: Vec<RuntimePins> = if let Some((hash, value)) = old {
        if hash != protocol.to_string() {
            return Err(Review(
                "Explorer configuration changed; explicit history review required".into(),
            )
            .into());
        }
        serde_json::from_value(value)?
    } else {
        vec![]
    };
    if !approved.contains(&pins) {
        approved.push(pins);
    }
    let first = start.checked_sub(200).context("Invalid epoch start")?;
    let tx = pool.transaction().await?;
    tx.execute("INSERT INTO d20dao_explorer.deployments(chain_id,coordinator,registry,configuration,catalog,protocol_configuration_hash,implementation_pins,first_block) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(chain_id,coordinator) DO UPDATE SET implementation_pins=EXCLUDED.implementation_pins",&[&(chain.to_string()),&(address(c)),&(address(r)),&config,&catalog,&(protocol.to_string()),&(json!(approved)),&(i64::try_from(first)?)]).await?;
    tx.execute(
        "INSERT INTO d20dao_explorer.cursors VALUES($1,$2,$3) ON CONFLICT DO NOTHING",
        &[
            &(chain.to_string()),
            &(address(c)),
            &(i64::try_from(first)?),
        ],
    )
    .await?;
    tx.commit().await?;
    Ok(Deployment {
        first,
        key_hash,
        approved,
        next,
    })
}
#[derive(Clone)]
struct Log {
    address: Address,
    block: u64,
    block_hash: String,
    tx_hash: String,
    index: u64,
    topics: Vec<B256>,
    data: Bytes,
}
fn log(value: Value) -> Result<Log> {
    if value["removed"].as_bool().unwrap_or(false) {
        return Err(ChainMoving("Removed log").into());
    }
    Ok(Log {
        address: serde_json::from_value(value["address"].clone())?,
        block: quantity(&value["blockNumber"])?,
        block_hash: value["blockHash"]
            .as_str()
            .context("Missing log block")?
            .into(),
        tx_hash: value["transactionHash"]
            .as_str()
            .context("Missing transaction")?
            .into(),
        index: quantity(&value["logIndex"])?,
        topics: serde_json::from_value(value["topics"].clone())?,
        data: serde_json::from_value(value["data"].clone())?,
    })
}
/// The implementation identity a keeper started with that was active at block `n`. A block after a move to this
/// keeper's approved next implementation (`next`) is not a review case: it waits for the restart that registers it.
async fn reviewed_pin(
    rpc: &Rpc,
    proxy: Address,
    n: u64,
    approved: &[RuntimePins],
    next: Option<B256>,
) -> Result<ProxyPin> {
    let tag = format!("0x{n:x}");
    let word: Bytes = serde_json::from_value(
        rpc.request("eth_getStorageAt", json!([proxy, IMPLEMENTATION_SLOT, tag]))
            .await?,
    )?;
    ensure!(
        word.len() == 32 && word[..12].iter().all(|b| *b == 0),
        "Invalid implementation slot"
    );
    let implementation = Address::from_slice(&word[12..]);
    let Some(pin) = approved
        .iter()
        .flat_map(|p| [p.coordinator, p.registry])
        .find(|p| p.proxy == proxy && p.implementation == implementation)
    else {
        if let Some(next) = next {
            let code: Bytes = serde_json::from_value(
                rpc.request("eth_getCode", json!([implementation, tag]))
                    .await?,
            )?;
            if keccak256(code) == next {
                return Err(Upgrading(format!(
                    "Approved next implementation at block {n}: {proxy} -> {implementation}; indexed after the keeper restarts"
                ))
                .into());
            }
        }
        return Err(Review(format!(
            "Unreviewed historical implementation at block {n}: {proxy} -> {implementation}"
        ))
        .into());
    };
    let (implementation_code, proxy_code) = tokio::try_join!(
        rpc.request("eth_getCode", json!([implementation, tag])),
        rpc.request("eth_getCode", json!([proxy, tag]))
    )?;
    let implementation_code: Bytes = serde_json::from_value(implementation_code)?;
    let proxy_code: Bytes = serde_json::from_value(proxy_code)?;
    if keccak256(implementation_code) != pin.implementation_code_hash
        || keccak256(proxy_code) != pin.proxy_code_hash
    {
        return Err(Review("Historical code mismatch".into()).into());
    }
    Ok(pin)
}
fn receipt(log: &Log, h: &Header, pin: ProxyPin) -> Value {
    json!({"blockNumber":log.block.to_string(),"blockHash":log.block_hash,"transactionHash":log.tx_hash,"logIndex":log.index,
        "timestamp":h.timestamp.to_string(),"implementations":pin,"implementationObservation":"block_end_not_transaction_execution"})
}
fn request_topic(topic: B256) -> bool {
    [
        "RandomnessRequested(uint256,address,bytes32,bytes32,uint64,uint32,uint256,address,uint64)",
        "MappingRequested(uint256,bytes32,(uint8,uint256,uint256,uint32,uint32))",
        "BlockHashStored(uint256,uint64,bytes32)",
        "RandomnessFulfilled(uint256,bytes32,address)",
        "CallbackAttempted(uint256,bool,uint32)",
        "ProofVerified(uint256,bytes32,uint256,bytes32)",
        "RequestServed(uint256,uint256)",
        "FulfillmentEvidence(uint256,bytes32,bytes)",
        "RequestRefundedTo(uint256,address,uint256,bool)",
        "RefundCallbackAttempted(uint256,address,bool,uint32)",
        "KeeperFeePaid(uint256,address,uint256,bool)",
    ]
    .iter()
    .any(|s| keccak256(s.as_bytes()) == topic)
}
#[derive(Default, Clone)]
struct Evidence {
    request: Option<Value>,
    fulfillment: Option<Value>,
    packet: Option<String>,
}
struct RequestRow {
    id: String,
    request: Value,
    mapping: Value,
    evidence: Evidence,
}
struct EpochRow {
    id: String,
    record: Value,
    packet: String,
    receipt: Value,
}
struct Batch {
    reorg: bool,
    expected_next: u64,
    start: u64,
    end: Header,
    headers: Vec<Header>,
    logs: Vec<Log>,
    requests: Vec<RequestRow>,
    epochs: Vec<EpochRow>,
}
async fn scan(
    pool: &mut Client,
    rpc: &Rpc,
    pins: RuntimePins,
    chain: u64,
    deployment: &Deployment,
    span: u64,
) -> Result<Progress> {
    let c = address(pins.coordinator.proxy);
    let chain = chain.to_string();
    // The finalized head, not latest: a load-balanced endpoint can announce a block that the backend serving
    // the next call has not imported yet, and nothing past finality can be reorganized away.
    let head = rpc.finalized_head().await?;
    let next: i64 = (pool
        .query_one(
            "SELECT next_block FROM d20dao_explorer.cursors WHERE chain_id=$1 AND coordinator=$2",
            &[&chain, &c],
        )
        .await?)
        .get(0);
    let mut next = u64::try_from(next)?;
    let mut start = next;
    let mut reorg = false;
    if next > deployment.first {
        // A finalized head never moves back, so one below the last indexed block is an endpoint (or a
        // load-balanced backend) that has not caught up yet, not a reorganization: the round is retried. Treating
        // it as one would hide the index and, beyond the lookback, rebuild it from the first block.
        if head.number < next - 1 {
            return Err(ChainMoving("Finalized head behind the index cursor").into());
        }
        let saved:Option<String>=(pool.query_opt("SELECT hash FROM d20dao_explorer.blocks WHERE chain_id=$1 AND coordinator=$2 AND number=$3",&[&chain,&c,&(i64::try_from(next-1)?)]).await?).map(|r|r.get(0));
        let canonical = Some(header(rpc, next - 1).await?.hash);
        if saved != canonical {
            reorg = true;
            start = next.saturating_sub(LOOKBACK).max(deployment.first);
            pool.execute("UPDATE d20dao_explorer.deployments SET canonical=false WHERE chain_id=$1 AND coordinator=$2",&[&chain,&c]).await?;
            let anchor = start.saturating_sub(1);
            let saved:Option<String>=(pool.query_opt("SELECT hash FROM d20dao_explorer.blocks WHERE chain_id=$1 AND coordinator=$2 AND number=$3",&[&chain,&c,&(i64::try_from(anchor)?)]).await?).map(|r|r.get(0));
            if anchor >= deployment.first
                && (anchor > head.number
                    || saved.as_deref() != Some(&header(rpc, anchor).await?.hash))
            {
                // Deep reorg: hide stale data and rebuild from initialization. No deletions.
                let tx = pool.transaction().await?;
                tx.execute("UPDATE d20dao_explorer.deployments SET canonical=false WHERE chain_id=$1 AND coordinator=$2",&[&chain,&c]).await?;
                tx.execute("UPDATE d20dao_explorer.requests SET canonical=false WHERE chain_id=$1 AND coordinator=$2",&[&chain,&c]).await?;
                tx.execute("UPDATE d20dao_explorer.events SET canonical=false WHERE chain_id=$1 AND coordinator=$2",&[&chain,&c]).await?;
                tx.execute("UPDATE d20dao_explorer.epochs SET canonical=false WHERE chain_id=$1 AND registry=$2",&[&chain,&(address(pins.registry.proxy))]).await?;
                tx.execute("UPDATE d20dao_explorer.cursors SET next_block=$3 WHERE chain_id=$1 AND coordinator=$2",&[&chain,&c,&(i64::try_from(deployment.first)?)]).await?;
                tx.commit().await?;
                start = deployment.first;
                next = deployment.first;
            }
        } else if next > head.number {
            pool.execute("UPDATE d20dao_explorer.deployments SET canonical=true WHERE chain_id=$1 AND coordinator=$2",&[&chain,&c]).await?;
            return Ok(Progress {
                cursor: next,
                caught_up: true,
            });
        }
    }
    if start > head.number {
        return Ok(Progress {
            cursor: next,
            caught_up: true,
        });
    }
    let end = (start.saturating_add(span).saturating_sub(1))
        .min(head.number)
        .max(start);
    let end = end.min(start + MAX_BLOCKS + LOOKBACK - 1);
    let end = refresh::resume_end(pool, rpc, (&chain, &c), next, start, reorg, head.number)
        .await?
        .unwrap_or(end);
    let values:Vec<Value>=serde_json::from_value(rpc.request("eth_getLogs",json!([{"address":[pins.coordinator.proxy,pins.registry.proxy],"fromBlock":format!("0x{start:x}"),"toBlock":format!("0x{end:x}")}])).await?)?;
    ensure!(values.len() <= MAX_LOGS, "Explorer log batch limit");
    let mut logs = values.into_iter().map(log).collect::<Result<Vec<_>>>()?;
    logs.sort_by_key(|l| (l.block, l.index));
    let mut needed: BTreeSet<u64> = ((end.saturating_sub(LOOKBACK)).max(start)..=end).collect();
    needed.extend(logs.iter().map(|l| l.block));
    ensure!(
        needed.iter().all(|n| *n >= start && *n <= end),
        "Log outside requested range"
    );
    let mut headers = stream::iter(needed)
        .map(|n| header(rpc, n))
        .buffer_unordered(4)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
    headers.sort_by_key(|h| h.number);
    let by_number: BTreeMap<_, _> = headers.iter().map(|h| (h.number, h.clone())).collect();
    let mut refs = BTreeMap::<String, Evidence>::new();
    let mut epochs = vec![];
    let mut reviewed = BTreeMap::new();
    for l in &logs {
        ensure!(
            l.address == pins.coordinator.proxy || l.address == pins.registry.proxy,
            "Unexpected log address"
        );
        let h = by_number
            .get(&l.block)
            .context("Log outside requested blocks")?;
        if h.hash != l.block_hash {
            return Err(ChainMoving("Log fork mismatch").into());
        }
        let topic = *l.topics.first().context("Missing log topic")?;
        if let std::collections::btree_map::Entry::Vacant(entry) =
            reviewed.entry((l.address, l.block))
        {
            entry.insert(
                reviewed_pin(
                    rpc,
                    l.address,
                    l.block,
                    &deployment.approved,
                    deployment.next.for_proxy(&pins, l.address),
                )
                .await?,
            );
        }
        let evidence = receipt(l, h, reviewed[&(l.address, l.block)]);
        if l.address == pins.registry.proxy && topic == EpochCommitted::SIGNATURE_HASH {
            let e = EpochCommitted::decode_raw_log_validate(l.topics.clone(), &l.data)?;
            ensure!(e.packet.len() <= 2048, "Epoch packet size");
            let r = at(
                rpc,
                pins.registry.proxy,
                E::getEpochCall { epochId: e.epochId },
                l.block,
            )
            .await?;
            ensure!(r.epochHash == e.epochHash, "Epoch commitment mismatch");
            // A committed epoch's catalog can no longer change, so the current registry view is read; epochs
            // published before the catalog API existed resolve to the initial catalog.
            let catalog = rpc
                .call(pins.registry.proxy, E::catalogAtCall { epochId: e.epochId })
                .await?;
            ensure!(catalog.hash == r.catalogHash, "Epoch catalog mismatch");
            let recipe = *catalog
                .recipes
                .get(usize::from(r.source))
                .context("Epoch source outside its catalog")?;
            epochs.push(EpochRow {id:e.epochId.to_string(),record:json!({"epochHash":r.epochHash.to_string(),"catalogHash":r.catalogHash.to_string(),"anchorHash":r.anchorHash.to_string(),"source":r.source,"recipe":recipe,"queryHash":r.queryHash.to_string(),"dataHash":r.dataHash.to_string(),"attestationHash":r.attestationHash.to_string(),"signedAt":r.signedAt.to_string(),"committedBlock":r.committedBlock.to_string(),
                "catalog":{"recipes":catalog.recipes,"signers":catalog.signers.iter().map(|signer|address(*signer)).collect::<Vec<_>>()}}),packet:e.packet.to_string(),receipt:evidence});
        } else if l.address == pins.coordinator.proxy && request_topic(topic) {
            let id =
                U256::from_be_bytes(l.topics.get(1).context("Missing request ID")?.0).to_string();
            let entry = refs.entry(id).or_default();
            if topic == RandomnessRequested::SIGNATURE_HASH {
                RandomnessRequested::decode_raw_log_validate(l.topics.clone(), &l.data)?;
                entry.request = Some(evidence);
            } else if topic == FulfillmentEvidence::SIGNATURE_HASH {
                let e = FulfillmentEvidence::decode_raw_log_validate(l.topics.clone(), &l.data)?;
                ensure!(e.packet.len() == 416, "Proof packet size");
                entry.packet = Some(e.packet.to_string());
                entry.fulfillment = Some(evidence);
            }
        }
    }
    // Re-read snapshots affected by a replayed/reorged suffix, including old requests with orphaned callbacks.
    let affected: Vec<String> = refs.keys().cloned().collect();
    let published: Vec<String> = epochs.iter().map(|e| e.id.clone()).collect();
    let old=pool.query("SELECT request_id,request_receipt,fulfillment_receipt,packet,request_block,request_block_hash FROM d20dao_explorer.requests WHERE chain_id=$1 AND coordinator=$2 AND request_block<=$3 AND request_id=ANY($4) LIMIT 1025",&[&chain,&c,&(i64::try_from(end)?),&affected]).await?;
    ensure!(old.len() <= MAX_REQUESTS, "Explorer request batch limit");
    merge_old(&mut refs, old, start, &by_number)?;
    ensure!(refs.len() <= MAX_REQUESTS, "Explorer request batch limit");
    if !refs.is_empty() {
        reviewed_pin(
            rpc,
            pins.coordinator.proxy,
            end,
            &deployment.approved,
            deployment.next.coordinator,
        )
        .await?;
    }
    if reorg || !published.is_empty() {
        let identity = refresh::Identity {
            next,
            start,
            end,
            hash: by_number[&end].hash.clone(),
            reorg,
        };
        let page = refresh::page(pool, &chain, &c, &identity, &affected, &published).await?;
        let mut deferred = BTreeMap::new();
        merge_old(&mut deferred, page.rows, start, &by_number)?;
        if !deferred.is_empty() {
            reviewed_pin(
                rpc,
                pins.coordinator.proxy,
                end,
                &deployment.approved,
                deployment.next.coordinator,
            )
            .await?;
        }
        let rows = read_requests(rpc, pins, &chain, &c, deployment, end, deferred).await?;
        let records = rows
            .into_iter()
            .map(|row| refresh::record(&chain, &c, row, i64::try_from(end)?))
            .collect::<Result<Vec<_>>>()?;
        if header(rpc, end).await?.hash != identity.hash {
            return Err(ChainMoving("Chain changed during refresh page").into());
        }
        refresh::stage(
            pool,
            (&chain, &c),
            &identity,
            &page.previous,
            &page.last,
            page.done,
            &records,
        )
        .await?;
        if !page.done {
            return Ok(Progress {
                cursor: next,
                caught_up: false,
            });
        }
    }
    let requests = read_requests(rpc, pins, &chain, &c, deployment, end, refs).await?;
    let final_header = header(rpc, end).await?;
    if final_header.hash != by_number[&end].hash {
        return Err(ChainMoving("Chain changed during index batch").into());
    }
    // Once per round, just before the commit: an implementation change anywhere before this point is caught
    // here, and each log was already checked against the implementation active at its own block.
    verify_pins(rpc, pins, deployment.next).await?;
    persist(
        pool,
        pins,
        chain.as_str(),
        Batch {
            reorg,
            expected_next: next,
            start,
            end: final_header,
            headers,
            logs,
            requests,
            epochs,
        },
        end == head.number,
    )
    .await?;
    Ok(Progress {
        cursor: end + 1,
        caught_up: end == head.number,
    })
}
/// The current implementations must still be the pinned ones. A mismatch needs a reviewed restart; an RPC
/// failure is only an RPC failure. A move to an approved next implementation needs no review: the keeper restarts on
/// it, and the restarted keeper registers it and indexes on.
async fn verify_pins(rpc: &Rpc, pins: RuntimePins, next: ApprovedNext) -> Result<()> {
    pins.verify(rpc, next).await.map_err(|error| {
        if crate::rpc::is_delivery_failure(&error) {
            error
        } else if let Some(upgrade) = crate::proxy::approved_upgrade(&error) {
            Upgrading(format!("Explorer pins: {upgrade}")).into()
        } else {
            Review(format!("Explorer pins: {error}")).into()
        }
    })
}
fn merge_old(
    refs: &mut BTreeMap<String, Evidence>,
    old: Vec<tokio_postgres::Row>,
    start: u64,
    by_number: &BTreeMap<u64, Header>,
) -> Result<()> {
    for row in old {
        let id: String = row.get("request_id");
        let request_block = u64::try_from(row.get::<_, i64>("request_block"))?;
        if request_block >= start
            && by_number
                .get(&request_block)
                .is_none_or(|h| h.hash != row.get::<_, String>("request_block_hash"))
        {
            continue;
        }
        let e = refs.entry(id).or_default();
        if e.request.is_none() {
            e.request = Some(row.get("request_receipt"));
        }
        if e.fulfillment.is_none() {
            let old: Option<Value> = row.get("fulfillment_receipt");
            if old
                .as_ref()
                .and_then(|v| v["blockNumber"].as_str())
                .and_then(|n| n.parse::<u64>().ok())
                .is_some_and(|n| n < start)
            {
                e.fulfillment = old;
                e.packet = row.get("packet");
            }
        }
    }
    Ok(())
}
async fn read_requests(
    rpc: &Rpc,
    pins: RuntimePins,
    chain: &str,
    c: &str,
    deployment: &Deployment,
    end: u64,
    refs: BTreeMap<String, Evidence>,
) -> Result<Vec<RequestRow>> {
    let chain_ref = chain;
    let coordinator_ref = c;
    let requests=stream::iter(refs).map(|(id,evidence)|async move {
        ensure!(evidence.request.is_some(),"Missing indexed request receipt");
        let request_id:U256=id.parse()?;
        let (r,m,fee)=tokio::try_join!(at(rpc,pins.coordinator.proxy,C::getRequestCall {id:request_id},end),at(rpc,pins.coordinator.proxy,PublicConfig::getMappingCall {requestId:request_id},end),at(rpc,pins.coordinator.proxy,PublicConfig::requestFeePaidCall {requestId:request_id},end))?;
        let mapping=json!({"operation":m.operation,"lower":m.lower.to_string(),"upper":m.upper.to_string(),"count":m.count,"population":m.population});
        ensure!(r.fulfilled==evidence.packet.is_some(),"Incomplete fulfillment evidence in index batch");
        if let Some(packet)=&evidence.packet {
            let bytes:Bytes=packet.parse()?;
            ensure!(keccak256(bytes)==r.proofHash,"Indexed proof hash mismatch");
        }
        let request=json!({"chainId":chain_ref,"coordinator":coordinator_ref,"feePaid":fee.to_string(),"requestedAt":evidence.request.as_ref().and_then(|v|v["timestamp"].as_str()),"keyHash":deployment.key_hash.to_string(),"requestId":id,"consumer":address(r.consumer),"callbackGasLimit":r.callbackGasLimit,"clientSeed":r.clientSeed.to_string(),"mappingHash":r.mappingHash.to_string(),"requestBlock":r.requestBlock.to_string(),"targetBlock":r.targetBlock.to_string(),"blockHash":r.blockHash.to_string(),"deadline":r.deadline.to_string(),"refundAddress":address(r.refundAddress),"epochId":r.epochId.to_string(),"epochHash":r.epochHash.to_string(),"fulfilled":r.fulfilled,"delivered":r.delivered,"callbackSuccess":r.delivered,"refunded":r.refunded,"randomness":r.randomness.to_string(),"proofHash":r.proofHash.to_string(),"transcriptHash":r.transcriptHash.to_string()});
        Ok::<_,anyhow::Error>(RequestRow {id,request,mapping,evidence})
    }).buffer_unordered(4).collect::<Vec<_>>().await.into_iter().collect::<Result<Vec<_>>>()?;
    Ok(requests)
}
fn n(v: &Value, key: &str) -> Result<i64> {
    Ok(v[key].as_str().context("Missing receipt number")?.parse()?)
}
async fn persist(
    pool: &mut Client,
    pins: RuntimePins,
    chain: &str,
    batch: Batch,
    caught_up: bool,
) -> Result<()> {
    let c = address(pins.coordinator.proxy);
    let r = address(pins.registry.proxy);
    let start = i64::try_from(batch.start)?;
    let end = i64::try_from(batch.end.number)?;
    let tx = pool.transaction().await?;
    tx.execute("SET LOCAL statement_timeout='5s'", &[]).await?;
    // A transaction-scoped lock prevents overlapping indexers from interleaving a suffix replacement.
    tx.execute(
        "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
        &[&(format!("{chain}:{c}"))],
    )
    .await?;
    let current:i64=(tx.query_one("SELECT next_block FROM d20dao_explorer.cursors WHERE chain_id=$1 AND coordinator=$2 FOR UPDATE",&[&chain,&c]).await?).get(0);
    if current != i64::try_from(batch.expected_next)? {
        return Err(Contention("Explorer cursor changed; retry batch").into());
    }
    if batch.reorg {
        tx.execute("UPDATE d20dao_explorer.events SET canonical=false WHERE chain_id=$1 AND coordinator=$2 AND block_number>=$3",&[&chain,&c,&start]).await?;
        tx.execute("UPDATE d20dao_explorer.requests SET canonical=false WHERE chain_id=$1 AND coordinator=$2 AND observed_block>=$3",&[&chain,&c,&start]).await?;
        tx.execute("UPDATE d20dao_explorer.epochs SET canonical=false WHERE chain_id=$1 AND registry=$2 AND block_number>=$3",&[&chain,&r,&start]).await?;
    }
    tx.execute(
        "DELETE FROM d20dao_explorer.blocks WHERE chain_id=$1 AND coordinator=$2 AND number<$3",
        &[&chain, &c, &(end - BLOCK_RETENTION)],
    )
    .await?;
    for h in batch.headers {
        tx.execute("INSERT INTO d20dao_explorer.blocks VALUES($1,$2,$3,$4,$5) ON CONFLICT(chain_id,coordinator,number) DO UPDATE SET hash=EXCLUDED.hash,timestamp=EXCLUDED.timestamp",&[&chain,&c,&(i64::try_from(h.number)?),&h.hash,&(i64::try_from(h.timestamp)?)]).await?;
    }
    for l in batch.logs {
        let topic = l.topics[0];
        let packet =
            topic == EpochCommitted::SIGNATURE_HASH || topic == FulfillmentEvidence::SIGNATURE_HASH;
        let request_id = if l.address == pins.coordinator.proxy && request_topic(topic) {
            Some(U256::from_be_bytes(l.topics[1].0).to_string())
        } else {
            None
        };
        let epoch_id = if topic == EpochCommitted::SIGNATURE_HASH {
            Some(U256::from_be_bytes(l.topics[1].0).to_string())
        } else {
            None
        };
        let payload = if packet {
            json!({"topics":l.topics,"packetStoredSeparately":true})
        } else {
            json!({"topics":l.topics,"data":l.data})
        };
        tx.execute("INSERT INTO d20dao_explorer.events VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,true) ON CONFLICT(chain_id,coordinator,block_hash,tx_hash,log_index) DO UPDATE SET canonical=true",&[&chain,&c,&(address(l.address)),&(i64::try_from(l.block)?),&l.block_hash,&l.tx_hash,&(i64::try_from(l.index)?),&(topic.to_string()),&request_id,&epoch_id,&payload]).await?;
    }
    for e in batch.epochs {
        let v = &e.receipt;
        tx.execute("INSERT INTO d20dao_explorer.epochs VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,true) ON CONFLICT(chain_id,registry,epoch_id) DO UPDATE SET record=EXCLUDED.record,packet=EXCLUDED.packet,commit_timestamp=EXCLUDED.commit_timestamp,block_number=EXCLUDED.block_number,block_hash=EXCLUDED.block_hash,tx_hash=EXCLUDED.tx_hash,log_index=EXCLUDED.log_index,receipt=EXCLUDED.receipt,canonical=true",&[&chain,&r,&e.id,&e.record,&e.packet,&(n(v,"timestamp")?),&(n(v,"blockNumber")?),&(v["blockHash"].as_str()),&(v["transactionHash"].as_str()),&(v["logIndex"].as_i64()),&e.receipt]).await?;
    }
    let identity = refresh::Identity {
        next: batch.expected_next,
        start: batch.start,
        end: batch.end.number,
        hash: batch.end.hash.clone(),
        reorg: batch.reorg,
    };
    refresh::commit_staged(&tx, chain, &c, &identity).await?;
    let records = batch
        .requests
        .into_iter()
        .map(|row| refresh::record(chain, &c, row, end))
        .collect::<Result<Vec<_>>>()?;
    refresh::upsert(&tx, &records).await?;
    tx.execute(
        "DELETE FROM d20dao_explorer.refresh_batches WHERE chain_id=$1 AND coordinator=$2",
        &[&chain, &c],
    )
    .await?;
    tx.execute("UPDATE d20dao_explorer.deployments SET last_indexed_block=$3,last_chain_timestamp=$4,canonical=canonical OR $5 WHERE chain_id=$1 AND coordinator=$2",&[&chain,&c,&end,&(i64::try_from(batch.end.timestamp)?),&caught_up]).await?;
    tx.execute(
        "UPDATE d20dao_explorer.cursors SET next_block=$3 WHERE chain_id=$1 AND coordinator=$2",
        &[&chain, &c, &(end + 1)],
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Bounded embedding hook for supervised backfill and local integration tests. No private journal is read.
/// Callers must handle errors without logging database connection details.
pub async fn index_once(
    pool: &mut Client,
    rpc: &Rpc,
    pins: RuntimePins,
    chain_id: u64,
) -> Result<()> {
    pool.batch_execute(SCHEMA).await?;
    let deployment = register(pool, rpc, pins, ApprovedNext::default(), chain_id).await?;
    scan(pool, rpc, pins, chain_id, &deployment, MAX_BLOCKS).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Roots for the local TLS PostgreSQL of the ignored tests; production builds only ever use webpki roots.
    pub(super) static TEST_ROOTS: std::sync::OnceLock<rustls::RootCertStore> =
        std::sync::OnceLock::new();
    fn install_test_roots() -> rustls::RootCertStore {
        let ca = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../.research/explorer-test-ca.der"
        ))
        .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(rustls::pki_types::CertificateDer::from(ca))
            .unwrap();
        TEST_ROOTS.get_or_init(|| roots).clone()
    }
    #[test]
    fn only_sustained_failures_are_reported_and_contention_is_progress() {
        let t0 = tokio::time::Instant::now();
        let at = |seconds: u64| t0 + Duration::from_secs(seconds);
        let mut lag = Lag::default();
        // A failure followed by a success is silent both ways, whatever its class.
        for cause in [Cause::Connection, Cause::RateLimited, Cause::ChainMoving] {
            assert_eq!(lag.failed(at(0), cause), LagReport::Quiet);
            assert_eq!(lag.succeeded(at(30)), LagReport::Quiet);
        }
        // Rounds failing for BEHIND_WARN produce one warning, then a reminder only after BEHIND_REPEAT.
        assert_eq!(lag.failed(at(100), Cause::Rpc), LagReport::Quiet);
        assert_eq!(lag.failed(at(279), Cause::Rpc), LagReport::Quiet);
        assert_eq!(
            lag.failed(at(280), Cause::Rpc),
            LagReport::Behind { seconds: 180 }
        );
        assert_eq!(lag.failed(at(290), Cause::Database), LagReport::Quiet);
        assert_eq!(
            lag.failed(at(280 + 3600), Cause::Rpc),
            LagReport::Behind { seconds: 3780 }
        );
        assert_eq!(
            lag.succeeded(at(4000)),
            LagReport::CaughtUp { seconds: 3900 }
        );
        assert_eq!(lag.succeeded(at(4001)), LagReport::Quiet);
        // Another writer advancing the cursor is progress: it never starts or extends a lag.
        for second in (5000..6000).step_by(10) {
            assert_eq!(lag.failed(at(second), Cause::Contention), LagReport::Quiet);
        }
        assert_eq!(lag.failed(at(7000), Cause::Timeout), LagReport::Quiet);
        assert_eq!(lag.failed(at(7100), Cause::Contention), LagReport::Quiet);
        assert_eq!(lag.failed(at(7200), Cause::Timeout), LagReport::Quiet);
        assert_eq!(lag.failed(at(7379), Cause::Timeout), LagReport::Quiet);
    }
    #[test]
    fn a_follower_indexes_only_its_own_or_a_stalled_cursor() {
        let t0 = tokio::time::Instant::now();
        let at = |seconds: u64| t0 + Duration::from_secs(seconds);
        let mut watch = CursorWatch::new(at(0));
        // The first look only starts the clock.
        assert!(!watch.should_index(100, at(0)));
        // The primary keeps moving it: the follower stays out.
        assert!(!watch.should_index(120, at(10)));
        assert!(!watch.should_index(140, at(20)));
        assert!(!watch.should_index(140, at(79)));
        // Stalled for the takeover window: the follower indexes, and keeps indexing its own cursor.
        assert!(watch.should_index(140, at(80)));
        watch.wrote(140, 160, at(85));
        assert!(watch.should_index(160, at(90)));
        watch.wrote(160, 160, at(91));
        assert!(watch.should_index(160, at(95)));
        watch.wrote(160, 180, at(96));
        // The primary is back and moved it: the follower yields at once.
        assert!(!watch.should_index(200, at(100)));
        assert!(!watch.should_index(200, at(159)));
        assert!(watch.should_index(200, at(160)));
    }
    #[test]
    fn failure_classes_come_from_error_types_not_text() {
        let classify = |error: anyhow::Error| Cause::of(&error);
        assert_eq!(
            classify(Contention("Explorer cursor changed; retry batch").into()),
            Cause::Contention
        );
        assert_eq!(
            classify(
                anyhow::Error::new(ChainMoving("Block not served yet")).context("while indexing")
            ),
            Cause::ChainMoving
        );
        assert_eq!(classify(Review("changed".into()).into()), Cause::Review);
        assert_eq!(classify(Upgrading("moved".into()).into()), Cause::Upgrade);
        assert_eq!(classify(Timeout("Explorer round").into()), Cause::Timeout);
        // Text that merely mentions a class is not that class.
        assert_eq!(
            classify(anyhow::anyhow!("Explorer cursor changed; retry batch")),
            Cause::Other
        );
    }
    #[tokio::test]
    async fn approved_upgrades_never_need_review_and_each_block_keeps_its_implementation() {
        // The head is past block 1_010, where the proxy will move.
        let chain = MockChain::new(2_000, Duration::ZERO);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rpc = Rpc::new(vec![format!("http://{}", listener.local_addr().unwrap())]).unwrap();
        let server = tokio::spawn(chain.clone().serve(listener));
        let before = mock_pins();
        let proxy = before.coordinator.proxy;
        let next = ApprovedNext {
            coordinator: Some(keccak256(NEXT_CODE)),
            registry: None,
        };
        verify_pins(&rpc, before, next).await.unwrap();
        // The proxy moves to the approved next implementation at block 1_010.
        let moved = Address::repeat_byte(0xc3);
        *chain.moved.lock().unwrap() = Some((proxy, moved, 1_010));
        // The keeper still running on the previous identity indexes the blocks before the move as before, and the
        // move itself is transient, since that keeper restarts: never a review.
        let history = [before];
        assert_eq!(
            reviewed_pin(&rpc, proxy, 1_009, &history, next.coordinator)
                .await
                .unwrap(),
            before.coordinator
        );
        let after = reviewed_pin(&rpc, proxy, 1_010, &history, next.coordinator)
            .await
            .unwrap_err();
        assert_eq!(Cause::of(&after), Cause::Upgrade);
        let current = verify_pins(&rpc, before, next).await.unwrap_err();
        assert_eq!(Cause::of(&current), Cause::Upgrade);
        // The same move without an approval is a review case, as before.
        let unapproved = reviewed_pin(&rpc, proxy, 1_010, &history, None)
            .await
            .unwrap_err();
        assert_eq!(Cause::of(&unapproved), Cause::Review);
        let unapproved = verify_pins(&rpc, before, ApprovedNext::default())
            .await
            .unwrap_err();
        assert_eq!(Cause::of(&unapproved), Cause::Review);
        // The restarted keeper adds its identity to the deployment row: each block is attributed to its own.
        let upgraded = RuntimePins {
            coordinator: ProxyPin {
                implementation: moved,
                implementation_code_hash: keccak256(NEXT_CODE),
                ..before.coordinator
            },
            ..before
        };
        verify_pins(&rpc, upgraded, next).await.unwrap();
        let history = [before, upgraded];
        for approval in [next.coordinator, None] {
            assert_eq!(
                reviewed_pin(&rpc, proxy, 1_009, &history, approval)
                    .await
                    .unwrap(),
                before.coordinator
            );
            assert_eq!(
                reviewed_pin(&rpc, proxy, 1_010, &history, approval)
                    .await
                    .unwrap(),
                upgraded.coordinator
            );
        }
        server.abort();
    }
    fn test_status() -> StatusSource {
        StatusSource {
            db: std::path::PathBuf::from("unused-status.sqlite"),
            role: crate::config::Role::Primary,
            keeper: Address::ZERO,
        }
    }
    #[tokio::test]
    async fn disabled_feature_creates_no_task_or_rpc() {
        assert!(Settings::parse(None).unwrap().is_none());
        let rpc = Rpc::new(vec!["http://127.0.0.1:1".into()]).unwrap();
        assert!(
            spawn(
                None,
                rpc,
                RuntimePins::default(),
                ApprovedNext::default(),
                31337,
                test_status()
            )
            .is_none()
        );
        assert!(
            Settings::parse(Some("not-a-connection-secret".into()))
                .err()
                .unwrap()
                .to_string()
                .contains("Invalid explorer configuration")
        );
    }
    #[test]
    fn connection_settings_never_downgrade_tls_or_log_unknown_parameters() {
        let settings = Settings::parse(Some(
            "postgres://user:password@example.test/db?sslmode=disable".into(),
        ))
        .unwrap()
        .unwrap();
        assert!(matches!(settings.0.get_ssl_mode(), SslMode::Require));
        let error = Settings::parse(Some(
            "postgres://user:password@example.test/db?unrecognized=SECRET".into(),
        ))
        .err()
        .unwrap();
        assert!(!error.to_string().contains("SECRET"));
        assert!(
            Settings::parse(Some(
                "postgres://user:password@example.test/db?channel_binding=require".into()
            ))
            .is_ok()
        );
    }
    #[tokio::test]
    #[ignore = "uses local TLS Postgres and generated public test CA; never NEON_DB"]
    async fn postgres_tls_requires_channel_binding_and_verified_hostname() {
        let ca = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../.research/explorer-test-ca.der"
        ))
        .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(rustls::pki_types::CertificateDer::from(ca))
            .unwrap();
        let settings=Settings::parse(Some("postgres://postgres:public-local-test@localhost:55439/explorer_test?sslmode=require&channel_binding=require".into())).unwrap().unwrap();
        assert!(matches!(
            settings.0.get_channel_binding(),
            ChannelBinding::Require
        ));
        let session = settings.connect_with_roots(roots.clone()).await.unwrap();
        let tls: bool = session
            .client
            .query_one(
                "SELECT ssl FROM pg_stat_ssl WHERE pid=pg_backend_pid()",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert!(tls);
        drop(session);
        let mut wrong=Settings::parse(Some("postgres://postgres:public-local-test@wrong.example:55439/explorer_test?sslmode=require&channel_binding=require".into())).unwrap().unwrap();
        wrong
            .0
            .hostaddr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        assert!(
            wrong.connect_with_roots(roots).await.is_err(),
            "hostname mismatch must fail"
        );
        let mut unbound = settings.0.clone();
        unbound.ssl_mode(SslMode::Disable);
        assert!(
            unbound.connect(tokio_postgres::NoTls).await.is_err(),
            "required channel binding must refuse unbound SCRAM"
        );
    }

    #[tokio::test]
    async fn stalled_database_does_not_block_caller_and_task_is_abortable() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut options = tokio_postgres::Config::new();
        options
            .host("127.0.0.1")
            .port(port)
            .user("public-test")
            .password("public-test")
            .ssl_mode(SslMode::Require)
            .channel_binding(ChannelBinding::Require);
        let began = tokio::time::Instant::now();
        let task = spawn(
            Some(Settings(options)),
            Rpc::new(vec!["http://127.0.0.1:1".into()]).unwrap(),
            RuntimePins::default(),
            ApprovedNext::default(),
            31337,
            test_status(),
        )
        .unwrap();
        assert!(began.elapsed() < Duration::from_millis(100));
        let (connection, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_millis(100), tokio::task::yield_now())
            .await
            .unwrap();
        let handle = task.0.abort_handle();
        drop(task);
        drop(connection);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(handle.is_finished());
    }
    pub(super) fn fixture_batch(next: u64, start: u64, end: u64, proof: bool) -> Batch {
        let receipt = json!({"blockNumber":"10","blockHash":"0xrequest","transactionHash":"0xtx","logIndex":0,"timestamp":"100"});
        let fulfillment = json!({"blockNumber":"11","blockHash":"0xproof","transactionHash":"0xserve","logIndex":0,"timestamp":"101"});
        Batch {
            reorg: false,
            expected_next: next,
            start,
            end: Header {
                number: end,
                hash: format!("hash-{end}"),
                timestamp: 100 + end,
            },
            headers: vec![Header {
                number: end,
                hash: format!("hash-{end}"),
                timestamp: 100 + end,
            }],
            logs: vec![],
            epochs: vec![],
            requests: vec![RequestRow {
                id: "1".into(),
                request: json!({"fulfilled":proof,"deadline":"160","epochId":"1"}),
                mapping: json!({"operation":0}),
                evidence: Evidence {
                    request: Some(receipt),
                    fulfillment: proof.then_some(fulfillment),
                    packet: proof.then(|| format!("0x{}", "00".repeat(416))),
                },
            }],
        }
    }
    #[tokio::test]
    #[ignore = "uses disposable local Postgres on port55439; never NEON_DB"]
    async fn postgres_batch_is_atomic_idempotent_and_reorg_safe() {
        let (mut pool,connection)=tokio_postgres::connect("host=127.0.0.1 port=55439 user=postgres password=public-local-test dbname=explorer_test",tokio_postgres::NoTls).await.unwrap();
        let driver = tokio::spawn(async move {
            let _ = connection.await;
        });
        pool.batch_execute(SCHEMA).await.unwrap();
        let chain = format!(
            "local-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let pins = RuntimePins::default();
        let c = address(pins.coordinator.proxy);
        pool.execute("INSERT INTO d20dao_explorer.deployments(chain_id,coordinator,registry,configuration,catalog,protocol_configuration_hash,implementation_pins,first_block) VALUES($1,$2,$2,'{}','{}','0x','[]',10)",&[&chain,&c]).await.unwrap();
        pool.execute(
            "INSERT INTO d20dao_explorer.cursors VALUES($1,$2,10)",
            &[&chain, &c],
        )
        .await
        .unwrap();
        persist(
            &mut pool,
            pins,
            &chain,
            fixture_batch(10, 10, 11, true),
            true,
        )
        .await
        .unwrap();
        assert!(
            persist(
                &mut pool,
                pins,
                &chain,
                fixture_batch(10, 10, 11, true),
                true
            )
            .await
            .is_err(),
            "stale writer must not regress cursor"
        );
        persist(
            &mut pool,
            pins,
            &chain,
            fixture_batch(12, 10, 11, true),
            true,
        )
        .await
        .unwrap();
        let count: i64 = (pool
            .query_one(
                "SELECT COUNT(*) FROM d20dao_explorer.requests WHERE chain_id=$1",
                &[&chain],
            )
            .await
            .unwrap())
        .get(0);
        assert_eq!(count, 1);
        let mut quiet = fixture_batch(12, 12, 12, false);
        quiet.requests.clear();
        persist(&mut pool, pins, &chain, quiet, true).await.unwrap();
        let retained: bool = (pool
            .query_one(
                "SELECT canonical FROM d20dao_explorer.requests WHERE chain_id=$1",
                &[&chain],
            )
            .await
            .unwrap())
        .get(0);
        assert!(retained, "quiet batches must not orphan earlier snapshots");
        let mut bad = fixture_batch(13, 10, 13, true);
        bad.requests[0].evidence.packet = Some("0xbad".into());
        assert!(persist(&mut pool, pins, &chain, bad, true).await.is_err());
        let cursor: i64 = (pool
            .query_one(
                "SELECT next_block FROM d20dao_explorer.cursors WHERE chain_id=$1",
                &[&chain],
            )
            .await
            .unwrap())
        .get(0);
        assert_eq!(cursor, 13);
        let packet: Option<String> = (pool
            .query_one(
                "SELECT packet FROM d20dao_explorer.requests WHERE chain_id=$1",
                &[&chain],
            )
            .await
            .unwrap())
        .get(0);
        assert_eq!(packet.unwrap().len(), 834);
        let mut rollback = fixture_batch(13, 10, 11, false);
        rollback.reorg = true;
        persist(&mut pool, pins, &chain, rollback, true)
            .await
            .unwrap();
        let row: (Option<String>, Value, bool) = {
            let row=pool.query_one(
            "SELECT packet,request,canonical FROM d20dao_explorer.requests WHERE chain_id=$1",&[&chain]).await.unwrap();
            (row.get(0), row.get(1), row.get(2))
        };
        assert!(row.0.is_none());
        assert_eq!(row.1["fulfilled"], false);
        assert!(row.2);
        let mut orphan = fixture_batch(12, 10, 11, false);
        orphan.reorg = true;
        orphan.requests.clear();
        persist(&mut pool, pins, &chain, orphan, true)
            .await
            .unwrap();
        let canonical: bool = (pool
            .query_one(
                "SELECT canonical FROM d20dao_explorer.requests WHERE chain_id=$1",
                &[&chain],
            )
            .await
            .unwrap())
        .get(0);
        assert!(!canonical);
        // Block rows older than the retention window are pruned; the reorg anchor window stays.
        let old: i64 = (pool
            .query_one(
                "SELECT COUNT(*) FROM d20dao_explorer.blocks WHERE chain_id=$1",
                &[&chain],
            )
            .await
            .unwrap())
        .get(0);
        assert!(old > 0);
        let far = fixture_batch(12, 12, 12 + BLOCK_RETENTION as u64 + 5, false);
        persist(&mut pool, pins, &chain, far, true).await.unwrap();
        let kept: Vec<i64> = pool
            .query(
                "SELECT number FROM d20dao_explorer.blocks WHERE chain_id=$1 ORDER BY number",
                &[&chain],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(kept, vec![12 + BLOCK_RETENTION + 5]);
        // One status row per deployment, replaced in place.
        let mut status = KeeperStatus {
            role: "primary",
            healthy: true,
            send_enabled: true,
            faults: vec![],
            observed_at: 100,
            published_at: 101,
            keeper: c.clone(),
            balance: "18000000000000000000".into(),
            head_block: 500,
            pending_requests: 2,
            last_served_at: Some(99),
        };
        publish_status(&pool, &chain, &c, &status).await.unwrap();
        status.healthy = false;
        status.faults = vec!["epoch_stalled".into()];
        status.published_at = 161;
        publish_status(&pool, &chain, &c, &status).await.unwrap();
        let rows = pool
            .query("SELECT healthy,faults,published_at,keeper_balance FROM d20dao_explorer.keeper_status WHERE chain_id=$1", &[&chain])
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].get::<_, bool>(0));
        assert_eq!(rows[0].get::<_, Value>(1), json!(["epoch_stalled"]));
        assert_eq!(rows[0].get::<_, i64>(2), 161);
        assert_eq!(rows[0].get::<_, String>(3), "18000000000000000000");
        // A follower never replaces a fresh row from another keeper, takes over a stale one and keeps its own
        // until the primary reports again.
        let follower = KeeperStatus {
            role: "follower",
            keeper: format!("0x{}", "ab".repeat(20)),
            published_at: 200,
            ..status
        };
        async fn status_row(pool: &Client, chain: &str) -> (String, String, i64) {
            let row = pool
                .query_one(
                    "SELECT keeper,role,published_at FROM d20dao_explorer.keeper_status WHERE chain_id=$1",
                    &[&chain],
                )
                .await
                .unwrap();
            (row.get(0), row.get(1), row.get(2))
        }
        publish_status(&pool, &chain, &c, &follower).await.unwrap();
        assert_eq!(
            status_row(&pool, &chain).await,
            (c.clone(), "primary".into(), 161)
        );
        let takeover = KeeperStatus {
            published_at: 252,
            ..follower
        };
        publish_status(&pool, &chain, &c, &takeover).await.unwrap();
        assert_eq!(
            status_row(&pool, &chain).await,
            (takeover.keeper.clone(), "follower".into(), 252)
        );
        let own = KeeperStatus {
            published_at: 260,
            ..takeover
        };
        publish_status(&pool, &chain, &c, &own).await.unwrap();
        assert_eq!(status_row(&pool, &chain).await.2, 260);
        let primary = KeeperStatus {
            role: "primary",
            keeper: c.clone(),
            published_at: 270,
            ..own
        };
        publish_status(&pool, &chain, &c, &primary).await.unwrap();
        assert_eq!(
            status_row(&pool, &chain).await,
            (c.clone(), "primary".into(), 270)
        );
        drop(pool);
        driver.abort();
    }
    #[test]
    fn public_status_keeps_fault_codes_only() {
        let faults = vec![
            "node_rejected:insufficient funds for gas at https://rpc.example/secret".to_owned(),
            "nonce_stalled:41".to_owned(),
            "epoch_stalled".to_owned(),
            "nonce_stalled:42".to_owned(),
        ];
        assert_eq!(
            public_faults(&faults),
            vec!["epoch_stalled", "node_rejected", "nonce_stalled"]
        );
        assert!(public_faults(&[]).is_empty());
    }
    /// A local chain for the explorer loop: one block every BLOCK_MS from `base`, public-looking pins, no logs.
    /// It answers single and batch JSON-RPC over HTTP/1.1, with a fixed latency per request and two injectable
    /// faults: a backend that has not imported the head block yet, and an endpoint answering HTTP 429.
    pub(super) struct MockChain {
        pub started: tokio::time::Instant,
        pub base: u64,
        pub latency: Duration,
        pub unserved_head: std::sync::atomic::AtomicUsize,
        /// Blocks from this number on are the ones the lagging backend has not imported.
        pub unserved_from: std::sync::atomic::AtomicU64,
        pub limited_until: std::sync::Mutex<Option<tokio::time::Instant>>,
        pub registrations: std::sync::atomic::AtomicUsize,
        pub requests: std::sync::atomic::AtomicUsize,
        /// An upgrade: from this block on, this proxy's slot names this implementation, whose code is NEXT_CODE.
        pub moved: std::sync::Mutex<Option<(Address, Address, u64)>>,
    }
    const BLOCK_MS: u64 = 500;
    const PROXY_CODE: [u8; 2] = [0x60, 0x01];
    const IMPLEMENTATION_CODE: [u8; 2] = [0x60, 0x02];
    const NEXT_CODE: [u8; 2] = [0x60, 0x03];
    pub(super) fn mock_pins() -> RuntimePins {
        let pin = |proxy: u8, implementation: u8| ProxyPin {
            proxy: Address::repeat_byte(proxy),
            proxy_code_hash: keccak256(PROXY_CODE),
            implementation: Address::repeat_byte(implementation),
            implementation_code_hash: keccak256(IMPLEMENTATION_CODE),
        };
        RuntimePins {
            coordinator: pin(0xc1, 0xc2),
            registry: pin(0xe1, 0xe2),
        }
    }
    impl MockChain {
        pub fn new(base: u64, latency: Duration) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                started: tokio::time::Instant::now(),
                base,
                latency,
                unserved_head: 0.into(),
                unserved_from: u64::MAX.into(),
                limited_until: std::sync::Mutex::new(None),
                registrations: 0.into(),
                requests: 0.into(),
                moved: std::sync::Mutex::new(None),
            })
        }
        pub fn head(&self) -> u64 {
            self.base + self.started.elapsed().as_millis() as u64 / BLOCK_MS
        }
        fn block(&self, number: u64) -> Value {
            json!({"number":format!("0x{number:x}"),"hash":format!("0x{:064x}",number*7919+1),
                "timestamp":format!("0x{:x}",1_700_000_000+number/2),"baseFeePerGas":"0x1"})
        }
        fn answer(&self, call: &Value) -> Value {
            use std::sync::atomic::Ordering::SeqCst;
            let params = &call["params"];
            let word = |value: U256| format!("0x{}", hex::encode(value.to_be_bytes::<32>()));
            let result = match call["method"].as_str().unwrap_or_default() {
                "eth_chainId" => json!("0x7a69"),
                "eth_getBlockByNumber" => {
                    let head = self.head();
                    match params[0].as_str().unwrap_or_default() {
                        "latest" | "finalized" => self.block(head),
                        tag => {
                            let number = u64::from_str_radix(tag.trim_start_matches("0x"), 16)
                                .unwrap_or(u64::MAX);
                            let unserved = number >= self.unserved_from.load(SeqCst)
                                && self
                                    .unserved_head
                                    .fetch_update(SeqCst, SeqCst, |n| n.checked_sub(1))
                                    .is_ok();
                            if number > head || unserved {
                                Value::Null
                            } else {
                                self.block(number)
                            }
                        }
                    }
                }
                "eth_getStorageAt" => {
                    let proxy: Address = serde_json::from_value(params[0].clone()).unwrap();
                    let pins = mock_pins();
                    let block = u64::from_str_radix(
                        params[2]
                            .as_str()
                            .unwrap_or_default()
                            .trim_start_matches("0x"),
                        16,
                    )
                    .unwrap_or_else(|_| self.head());
                    let implementation = match *self.moved.lock().unwrap() {
                        Some((moved, to, from)) if moved == proxy && block >= from => to,
                        _ if proxy == pins.coordinator.proxy => pins.coordinator.implementation,
                        _ => pins.registry.implementation,
                    };
                    json!(format!("0x{:0>64}", hex::encode(implementation)))
                }
                "eth_getCode" => {
                    let address: Address = serde_json::from_value(params[0].clone()).unwrap();
                    let pins = mock_pins();
                    let moved = self.moved.lock().unwrap().map(|(_, to, _)| to);
                    if address == pins.coordinator.proxy || address == pins.registry.proxy {
                        json!(format!("0x{}", hex::encode(PROXY_CODE)))
                    } else if moved == Some(address) {
                        json!(format!("0x{}", hex::encode(NEXT_CODE)))
                    } else {
                        json!(format!("0x{}", hex::encode(IMPLEMENTATION_CODE)))
                    }
                }
                "eth_getLogs" => json!([]),
                "eth_getBalance" => json!("0x1"),
                "eth_call" => {
                    let data: Bytes = serde_json::from_value(params[0]["data"].clone()).unwrap();
                    let selector: [u8; 4] = data[..4].try_into().unwrap();
                    if selector == C::publicKeyXCall::SELECTOR {
                        self.registrations.fetch_add(1, SeqCst);
                    }
                    if selector == PublicConfig::firstEpochStartCall::SELECTOR {
                        json!(word(U256::from(self.base + 200)))
                    } else {
                        json!(word(U256::ZERO))
                    }
                }
                other => panic!("Unexpected mock RPC method {other}"),
            };
            json!({"jsonrpc":"2.0","id":call["id"],"result":result})
        }
        pub async fn serve(self: std::sync::Arc<Self>, listener: tokio::net::TcpListener) {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let chain = self.clone();
                tokio::spawn(async move {
                    let mut data = Vec::new();
                    let mut buffer = [0; 8192];
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
                    chain
                        .requests
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    tokio::time::sleep(chain.latency).await;
                    let limited = chain
                        .limited_until
                        .lock()
                        .unwrap()
                        .is_some_and(|until| tokio::time::Instant::now() < until);
                    let (status, answer) = if limited {
                        ("429 Too Many Requests", json!({"error":"rate limited"}))
                    } else if let Some(calls) = body.as_array() {
                        (
                            "200 OK",
                            Value::Array(calls.iter().map(|call| chain.answer(call)).collect()),
                        )
                    } else {
                        ("200 OK", chain.answer(&body))
                    };
                    let answer = serde_json::to_vec(&answer).unwrap();
                    let header = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        answer.len()
                    );
                    let _ = socket.write_all(header.as_bytes()).await;
                    let _ = socket.write_all(&answer).await;
                });
            }
        }
    }
    /// A TCP relay to the local PostgreSQL that adds `delay` before forwarding every chunk in each direction and
    /// counts the sessions it carried.
    async fn relay(
        listener: tokio::net::TcpListener,
        delay: Duration,
        sessions: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let (inbound, _) = listener.accept().await.unwrap();
            sessions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let outbound = tokio::net::TcpStream::connect("127.0.0.1:55439")
                .await
                .unwrap();
            let (ir, iw) = inbound.into_split();
            let (or, ow) = outbound.into_split();
            for (mut from, mut to) in [
                (
                    Box::new(ir) as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
                    Box::new(ow) as Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
                ),
                (Box::new(or), Box::new(iw)),
            ] {
                tokio::spawn(async move {
                    let mut buffer = [0; 16384];
                    loop {
                        let size = match from.read(&mut buffer).await {
                            Ok(0) | Err(_) => break,
                            Ok(size) => size,
                        };
                        tokio::time::sleep(delay).await;
                        if to.write_all(&buffer[..size]).await.is_err() {
                            break;
                        }
                    }
                    let _ = to.shutdown().await;
                });
            }
        }
    }
    #[derive(Clone, Default)]
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Capture {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }
    #[tokio::test]
    #[ignore = "uses local TLS Postgres on port 55439 and the generated public test CA; never NEON_DB"]
    async fn transient_database_and_rpc_faults_are_absorbed_without_warnings() {
        use std::sync::atomic::Ordering::SeqCst;
        install_test_roots();
        let capture = Capture::default();
        let writer = capture.clone();
        let _logs = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(move || writer.clone())
                .finish(),
        );
        // Latencies in the range a remote index database and a public RPC add; override to experiment.
        let millis = |name: &str, default: u64| {
            Duration::from_millis(
                std::env::var(name)
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(default),
            )
        };
        let chain = MockChain::new(10_000, millis("SOAK_RPC_MS", 40));
        let rpc_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rpc_url = format!("http://{}", rpc_listener.local_addr().unwrap());
        let rpc_server = tokio::spawn(chain.clone().serve(rpc_listener));
        let sessions = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let db_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let db_port = db_listener.local_addr().unwrap().port();
        let db_relay = tokio::spawn(relay(
            db_listener,
            millis("SOAK_DB_MS", 20),
            sessions.clone(),
        ));
        let mut settings = Settings::parse(Some(format!(
            "postgres://postgres:public-local-test@localhost:{db_port}/explorer_test?sslmode=require&channel_binding=require"
        )))
        .unwrap()
        .unwrap();
        settings
            .0
            .hostaddr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("status.sqlite");
        let journal = crate::journal::Journal::open(&db, "soak").await.unwrap();
        crate::health::assess(&journal, true, crate::health::now().unwrap(), 20, None, 120)
            .await
            .unwrap();
        let chain_id = 900_000
            + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
                % 90_000;
        let task = spawn(
            Some(settings),
            Rpc::new(vec![rpc_url]).unwrap(),
            mock_pins(),
            ApprovedNext::default(),
            chain_id,
            StatusSource {
                db: db.clone(),
                keeper: Address::repeat_byte(0x11),
                role: crate::config::Role::Primary,
            },
        )
        .unwrap();
        let (admin, connection) = tokio_postgres::connect(
            "host=127.0.0.1 port=55439 user=postgres password=public-local-test dbname=explorer_test",
            tokio_postgres::NoTls,
        )
        .await
        .unwrap();
        let driver = tokio::spawn(async move {
            let _ = connection.await;
        });
        let cursor = || async {
            admin
                .query_opt(
                    "SELECT next_block FROM d20dao_explorer.cursors WHERE chain_id=$1",
                    &[&chain_id.to_string()],
                )
                .await
                .unwrap()
                .map_or(0, |row| row.get::<_, i64>(0) as u64)
        };
        // Recovery is the index reaching the head the chain had when the fault was injected.
        let reaches = |target: u64, limit: u64| async move {
            let end = tokio::time::Instant::now() + Duration::from_secs(limit);
            while tokio::time::Instant::now() < end {
                if cursor().await > target {
                    return tokio::time::Instant::now();
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            panic!("explorer did not reach block {target} within {limit} s");
        };
        let began = tokio::time::Instant::now();
        reaches(chain.head(), 60).await;
        let mut timeline = vec![format!("first commit after {:?}", began.elapsed())];
        // 1. The endpoint announces a head the backend serving the next call has not imported.
        let fault = tokio::time::Instant::now();
        chain.unserved_from.store(chain.head(), SeqCst);
        chain.unserved_head.store(2, SeqCst);
        let caught = reaches(chain.head() + 4, 60).await;
        timeline.push(format!("unserved head absorbed in {:?}", caught - fault));
        // 2. The server side closes the database session, as a pooler, proxy or compute restart does.
        let fault = tokio::time::Instant::now();
        let killed: i64 = admin
            .query_one(
                "SELECT COUNT(pg_terminate_backend(pid)) FROM pg_stat_activity WHERE application_name='d20dao-keeper-explorer'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(killed, 1);
        let caught = reaches(chain.head() + 4, 60).await;
        timeline.push(format!("closed session replaced in {:?}", caught - fault));
        // 3. Every endpoint rate limits for twelve seconds.
        let fault = tokio::time::Instant::now();
        *chain.limited_until.lock().unwrap() = Some(fault + Duration::from_secs(12));
        let caught = reaches(chain.head() + 30, 90).await;
        timeline.push(format!("12 s rate limit absorbed in {:?}", caught - fault));
        drop(task);
        let logs = capture.text();
        let warnings: Vec<&str> = logs
            .lines()
            .filter(|line| line.contains("WARN") && line.contains("explorer"))
            .collect();
        eprintln!(
            "{}\nregistrations={} database sessions={} rpc requests={}",
            timeline.join("\n"),
            chain.registrations.load(SeqCst),
            sessions.load(SeqCst),
            chain.requests.load(SeqCst)
        );
        assert!(warnings.is_empty(), "{warnings:#?}");
        assert!(!logs.contains("Public explorer index is behind"));
        assert_eq!(chain.registrations.load(SeqCst), 1, "registration survives");
        assert_eq!(
            sessions.load(SeqCst),
            2,
            "only the closed session is replaced"
        );
        for cause in ["chain_not_ready", "rpc_rate_limited"] {
            assert!(logs.contains(cause), "{cause} is logged at debug level");
        }
        for table in [
            "requests",
            "blocks",
            "cursors",
            "deployments",
            "keeper_status",
        ] {
            admin
                .execute(
                    &format!("DELETE FROM d20dao_explorer.{table} WHERE chain_id=$1"),
                    &[&chain_id.to_string()],
                )
                .await
                .unwrap();
        }
        driver.abort();
        rpc_server.abort();
        db_relay.abort();
        journal.pool.close().await;
    }
}
