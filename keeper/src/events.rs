//! Optional push path for chain events: `eth_subscribe` to new heads and to the logs of the service contracts (an epoch
//! coordinator and its registry, or a round coordinator alone) over WebSocket. Events decide only when the worker ticks and when it re-checks its runtime pins and publishing
//! right; every decision still comes from the worker's own HTTP reads, so a missed, late or repeated event can delay
//! work but never change it. Without a WebSocket endpoint, or while every subscription is down, the worker polls.
//!
//! What a subscription follows depends on the coordinator (`Follow`). An epoch keeper follows every block and every log of
//! its two contracts, as 0.4.1 did. A round keeper runs on a chain that makes several blocks a second, and a provider
//! bills each pushed message: it follows the coordinator's requests, keeper role changes and upgrades, and new blocks only
//! while it has open work, and keeps an idle connection alive with WebSocket pings, which carry no JSON-RPC message.
use crate::rpc::{Rpc, quantity};
use alloy_primitives::{Address, B256, keccak256};
use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// While work is open and the subscription is live, a tick waits at most this long for the next pushed block or
/// event before it runs anyway: a stream that stops pushing never stalls work.
pub const BUSY_FALLBACK: Duration = Duration::from_secs(1);
/// With nothing open and a live subscription, an epoch keeper's maintenance tick still runs this often (epoch
/// preparation, health observations, operator commands). Work itself arrives as an event and wakes the worker at once. A
/// round keeper's is `IDLE_HEARTBEAT_SECONDS` (`config::IDLE_HEARTBEAT_RANGE`).
pub const IDLE_HEARTBEAT: Duration = Duration::from_secs(5);
/// A subscription that has pushed nothing for this long is replaced: the chain makes blocks every second or so. A round
/// keeper's subscription without blocks counts any frame, the answer to its ping included.
const SILENCE_LIMIT: Duration = Duration::from_secs(20);
/// A round keeper's subscription that has heard nothing for this long pings the endpoint, whose pong (or any other
/// frame) must come before SILENCE_LIMIT.
const PING_AFTER: Duration = Duration::from_secs(10);

/// What a subscription follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Follow {
    /// Every new block and every log of the service contracts, for the life of the subscription: an epoch keeper's, as
    /// 0.4.1's.
    Everything,
    /// A round keeper's: of the coordinator's logs only its requests (`RandomnessRequested`), its keeper role changes and
    /// its upgrades (`round_topics`), and new blocks only while the run loop says work is open (`Signals::want_heads`).
    Demand,
}
/// The first topics of the logs a round keeper's subscription follows: a request, which wakes the keeper; a keeper role
/// change, which has it check its publishing right; an upgrade, which has it check its runtime pins. Every other event of
/// the coordinator is of a request the keeper already has, which its ticks read while work is open.
pub fn round_topics() -> [B256; 4] {
    use crate::abi_round::RoundCoordinator as R;
    use alloy_sol_types::SolEvent;
    [
        R::RandomnessRequested::SIGNATURE_HASH,
        R::KeeperChanged::SIGNATURE_HASH,
        R::BackupKeeperSet::SIGNATURE_HASH,
        keccak256("Upgraded(address)"),
    ]
}

/// How the run loop waits for its next tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// No live subscription: plain polling, this long.
    Sleep(Duration),
    /// Open work and a live subscription: at least `spacing`, then the next pushed block or event, or `fallback` more at
    /// most.
    Blocks {
        spacing: Duration,
        fallback: Duration,
    },
    /// Nothing open and a live subscription: the next pushed event, or the heartbeat at most.
    Event(Duration),
}
/// The run loop's intervals: POLL_MS with work open, IDLE_POLL_MS with nothing open and no live subscription, and the
/// heartbeat with nothing open and a live one (IDLE_HEARTBEAT for an epoch keeper, IDLE_HEARTBEAT_SECONDS for a round
/// keeper).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cadence {
    pub poll: Duration,
    pub idle_poll: Duration,
    pub heartbeat: Duration,
}
impl Cadence {
    /// How the loop waits, with the subscription `live` or not and work `busy` or not.
    pub fn wait(&self, live: bool, busy: bool) -> Wait {
        match (live, busy) {
            (false, true) => Wait::Sleep(self.poll),
            (false, false) => Wait::Sleep(self.idle_poll),
            (true, true) => Wait::Blocks {
                spacing: self.poll,
                fallback: BUSY_FALLBACK,
            },
            (true, false) => Wait::Event(self.heartbeat),
        }
    }
}
/// A reconnect backfills up to this many missed blocks over HTTP; a longer outage forces every re-check instead.
const BACKFILL_MAX_BLOCKS: u64 = 5_000;
const BACKFILL_RANGE: u64 = 500;
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(10);
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(60);

/// What a log means for the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `Upgraded(address)` on either proxy: the runtime pins must be verified again before anything else.
    Upgrade,
    /// A change of the registry's committer or backup committers, or of a round coordinator's keeper or backup keepers:
    /// the publishing right must be checked again.
    Role,
    /// Anything else from the two service contracts: requests, fulfillments, skips, refunds, epoch publications,
    /// catalog and recipe changes. Worth a tick.
    Work,
}
pub fn classify(topic: Option<&B256>) -> Kind {
    let Some(topic) = topic else {
        return Kind::Work;
    };
    if *topic == keccak256("Upgraded(address)") {
        Kind::Upgrade
    } else if *topic == keccak256("CommitterChanged(address,address)")
        || *topic == keccak256("BackupCommitterSet(address,bool)")
        || *topic == keccak256("KeeperChanged(address,address)")
        || *topic == keccak256("BackupKeeperSet(address,bool)")
    {
        Kind::Role
    } else {
        Kind::Work
    }
}

/// When the keeper first saw a block's header, and the time the header carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sighting {
    /// The header's timestamp, in seconds.
    pub timestamp: u64,
    /// The keeper's wall clock when the header first arrived, in milliseconds.
    pub seen_ms: u64,
}
/// How many headers' first sightings are kept: the newest by number. A request is discovered within a few blocks of its
/// own, and older sightings are of no use.
const SIGHTINGS: usize = 4_096;

/// Shared between the subscription task, the main loop and the worker.
pub struct Signals {
    heads: tokio::sync::watch::Sender<u64>,
    /// Whether the run loop has work open, and so wants new blocks: what a `Follow::Demand` subscription follows them by.
    heads_wanted: tokio::sync::watch::Sender<bool>,
    /// The first sighting of each recent header: pushed by the subscription, or read by the worker.
    sightings: std::sync::Mutex<std::collections::BTreeMap<u64, Sighting>>,
    work: tokio::sync::Notify,
    upgrades: AtomicU64,
    roles: AtomicU64,
    activity: AtomicU64,
    live: AtomicBool,
}
impl Signals {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            heads: tokio::sync::watch::channel(0).0,
            heads_wanted: tokio::sync::watch::channel(false).0,
            sightings: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            work: tokio::sync::Notify::new(),
            upgrades: AtomicU64::new(0),
            roles: AtomicU64::new(0),
            activity: AtomicU64::new(0),
            live: AtomicBool::new(false),
        })
    }
    /// The run loop says whether work is open after each tick. A `Follow::Demand` subscription subscribes to new blocks
    /// while it is and unsubscribes when it is not; a `Follow::Everything` one follows every block regardless.
    pub fn want_heads(&self, wanted: bool) {
        self.heads_wanted.send_if_modified(|current| {
            let changed = *current != wanted;
            *current = wanted;
            changed
        });
    }
    /// Whether the run loop last said work is open.
    pub fn heads_wanted(&self) -> bool {
        *self.heads_wanted.borrow()
    }
    /// Bumped by every upgrade event and by every (re)subscription, whose gap may have hidden one.
    pub fn upgrades(&self) -> u64 {
        self.upgrades.load(Ordering::SeqCst)
    }
    /// Bumped by every role event and by every (re)subscription.
    pub fn roles(&self) -> u64 {
        self.roles.load(Ordering::SeqCst)
    }
    /// The highest block that carried a work event. Work stays "open" until a tick has read at least that block.
    pub fn activity(&self) -> u64 {
        self.activity.load(Ordering::SeqCst)
    }
    pub fn live(&self) -> bool {
        self.live.load(Ordering::SeqCst)
    }
    /// Tests: whether the subscription is live, without one.
    #[cfg(test)]
    pub(crate) fn set_live(&self, live: bool) {
        self.live.store(live, Ordering::SeqCst);
    }
    /// Tests: a log of `kind` in block `block`, as the subscription pushes it.
    #[cfg(test)]
    pub(crate) fn push(&self, kind: Kind, block: u64) {
        self.saw(kind, block);
    }
    pub fn heads(&self) -> tokio::sync::watch::Receiver<u64> {
        self.heads.subscribe()
    }
    fn saw(&self, kind: Kind, block: u64) {
        match kind {
            Kind::Upgrade => {
                self.upgrades.fetch_add(1, Ordering::SeqCst);
            }
            Kind::Role => {
                self.roles.fetch_add(1, Ordering::SeqCst);
            }
            Kind::Work => {}
        }
        self.activity.fetch_max(block, Ordering::SeqCst);
        self.work.notify_one();
    }
    /// The header of block `number`, of time `timestamp`, arrived at `seen_ms` by the wall clock. Only its first
    /// arrival is kept.
    pub fn header(&self, number: u64, timestamp: u64, seen_ms: u64) {
        let mut sightings = self.sightings.lock().expect("sightings mutex");
        sightings
            .entry(number)
            .or_insert(Sighting { timestamp, seen_ms });
        while sightings.len() > SIGHTINGS {
            sightings.pop_first();
        }
    }
    /// When the header of block `number` first arrived, if this process saw it.
    pub fn sighting(&self, number: u64) -> Option<Sighting> {
        self.sightings
            .lock()
            .expect("sightings mutex")
            .get(&number)
            .copied()
    }
    fn head(&self, number: u64) {
        self.heads.send_if_modified(|head| {
            let newer = number > *head;
            if newer {
                *head = number;
            }
            newer
        });
    }
    /// A new subscription cannot prove that nothing happened while there was none.
    fn resubscribed(&self) {
        self.upgrades.fetch_add(1, Ordering::SeqCst);
        self.roles.fetch_add(1, Ordering::SeqCst);
        self.live.store(true, Ordering::SeqCst);
        self.work.notify_one();
    }
    /// Resolve when the next tick is due (`Cadence::wait`). Without a live subscription this is plain polling: `poll`
    /// while work is open, `idle_poll` otherwise. With one, open work ticks on the next pushed block or event (keeping at
    /// least `poll` between ticks, and at most BUSY_FALLBACK without anything pushed), and an idle keeper ticks on the
    /// next event or after the heartbeat.
    pub async fn next_tick(
        &self,
        heads: &mut tokio::sync::watch::Receiver<u64>,
        busy: bool,
        cadence: Cadence,
    ) {
        match cadence.wait(self.live(), busy) {
            Wait::Sleep(interval) => tokio::time::sleep(interval).await,
            Wait::Blocks { spacing, fallback } => {
                tokio::time::sleep(spacing).await;
                tokio::select! {
                    _ = heads.changed() => {}
                    _ = self.work.notified() => {}
                    _ = tokio::time::sleep(fallback) => {}
                }
            }
            Wait::Event(heartbeat) => {
                tokio::select! {
                    _ = self.work.notified() => {}
                    _ = tokio::time::sleep(heartbeat) => {}
                }
            }
        }
    }
}

pub struct Subscription(tokio::task::JoinHandle<()>);
impl Drop for Subscription {
    fn drop(&mut self) {
        self.0.abort();
    }
}
/// Start the subscription task, or nothing when no WebSocket endpoint is configured. Endpoints are tried in turn;
/// one that serves another chain is dropped for the life of the process. `addresses` are the service contracts whose logs
/// are followed: the coordinator and its registry, or a round coordinator alone. It follows everything they log and every
/// block (`Follow::Everything`).
pub fn spawn(
    urls: Vec<String>,
    rpc: Rpc,
    chain_id: u64,
    addresses: impl Into<Vec<Address>>,
    signals: Arc<Signals>,
) -> Option<Subscription> {
    spawn_following(
        urls,
        rpc,
        chain_id,
        addresses,
        signals,
        Follow::Everything,
        Keepalive::default(),
    )
}
/// `spawn`, following what `follow` says. A `Follow::Demand` session keeps `keepalive`; a `Follow::Everything` session
/// keeps the limits of 0.4.1 (`Keepalive::default`) whatever it is given.
pub fn spawn_following(
    urls: Vec<String>,
    rpc: Rpc,
    chain_id: u64,
    addresses: impl Into<Vec<Address>>,
    signals: Arc<Signals>,
    follow: Follow,
    keepalive: Keepalive,
) -> Option<Subscription> {
    spawn_task(urls, rpc, chain_id, addresses, signals, follow, keepalive)
}
/// When a `Follow::Demand` session pings a quiet endpoint, when it gives up on one that stays silent, and how far a
/// reconnect backfills the logs it missed. The default is SILENCE_LIMIT, PING_AFTER, BACKFILL_MAX_BLOCKS and
/// BACKFILL_RANGE; a round keeper takes `WS_SILENCE_SECONDS`, `WS_BACKFILL_MAX_BLOCKS` and `WS_BACKFILL_RANGE`
/// (`Keepalive::configured`).
#[derive(Clone, Copy, Debug)]
pub struct Keepalive {
    ping_after: Duration,
    silence: Duration,
    backfill_max_blocks: u64,
    backfill_range: u64,
}
impl Default for Keepalive {
    fn default() -> Self {
        Self {
            ping_after: PING_AFTER,
            silence: SILENCE_LIMIT,
            backfill_max_blocks: BACKFILL_MAX_BLOCKS,
            backfill_range: BACKFILL_RANGE,
        }
    }
}
impl Keepalive {
    /// The chain settings' silence and backfill window, with PING_AFTER (at most half the silence).
    pub fn configured(settings: &crate::config::ChainSettings) -> Self {
        let silence = Duration::from_secs(settings.ws_silence_seconds);
        Self {
            ping_after: PING_AFTER.min(silence / 2),
            silence,
            backfill_max_blocks: settings.ws_backfill_max_blocks,
            backfill_range: settings.ws_backfill_range,
        }
    }
}
fn spawn_task(
    urls: Vec<String>,
    rpc: Rpc,
    chain_id: u64,
    addresses: impl Into<Vec<Address>>,
    signals: Arc<Signals>,
    follow: Follow,
    keepalive: Keepalive,
) -> Option<Subscription> {
    if urls.is_empty() {
        return None;
    }
    let addresses: Vec<Address> = addresses.into();
    Some(Subscription(tokio::spawn(async move {
        let mut usable = vec![true; urls.len()];
        let mut last_block = None;
        let mut failures = 0u32;
        let mut next = 0;
        loop {
            let Some(index) = (0..urls.len())
                .map(|offset| (next + offset) % urls.len())
                .find(|i| usable[*i])
            else {
                tracing::error!(
                    "No usable WebSocket endpoint remains; the keeper keeps polling over HTTP"
                );
                return;
            };
            next = index + 1;
            let began = tokio::time::Instant::now();
            let ended = match follow {
                Follow::Everything => {
                    session(
                        &urls[index],
                        &rpc,
                        chain_id,
                        &addresses,
                        &signals,
                        &mut last_block,
                    )
                    .await
                }
                Follow::Demand => {
                    session_on_demand(
                        &urls[index],
                        &rpc,
                        chain_id,
                        &addresses,
                        &signals,
                        &mut last_block,
                        keepalive,
                    )
                    .await
                }
            };
            signals.live.store(false, Ordering::SeqCst);
            // The caller wakes and falls back to polling at once; the next tick re-checks everything.
            signals.work.notify_one();
            let error = ended.err();
            if let Some(Stop::WrongChain) = error.as_ref().and_then(|e| e.downcast_ref::<Stop>()) {
                tracing::error!(
                    endpoint = index,
                    "WebSocket endpoint serves another chain; it is not used"
                );
                usable[index] = false;
                continue;
            }
            if began.elapsed() > Duration::from_secs(60) {
                failures = 0;
            }
            failures = failures.saturating_add(1);
            let class = error.as_ref().map_or("closed", |e| {
                e.downcast_ref::<Stop>().map_or("transport", Stop::name)
            });
            if failures == 1 {
                tracing::warn!(
                    endpoint = index,
                    reason = class,
                    "Chain event subscription lost; polling over HTTP until it is back"
                );
            } else {
                tracing::debug!(
                    endpoint = index,
                    reason = class,
                    failures,
                    "Chain event subscription still down"
                );
            }
            let backoff = RECONNECT_MIN
                .saturating_mul(1 << failures.saturating_sub(1).min(6))
                .min(RECONNECT_MAX);
            tokio::time::sleep(backoff).await;
        }
    })))
}
/// Why a session ended, without any text from the endpoint (a WebSocket URL can carry a provider key).
#[derive(Debug)]
enum Stop {
    WrongChain,
    Handshake,
    Silent,
    Closed,
    Protocol,
}
impl Stop {
    fn name(&self) -> &'static str {
        match self {
            Self::WrongChain => "wrong_chain",
            Self::Handshake => "handshake",
            Self::Silent => "silent",
            Self::Closed => "closed",
            Self::Protocol => "protocol",
        }
    }
}
impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WebSocket session ended: {}", self.name())
    }
}
impl std::error::Error for Stop {}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
/// Open the WebSocket connection to `url` within HANDSHAKE_DEADLINE: TLS with the web's roots for `wss://`.
async fn connect(url: &str) -> Result<Socket> {
    let connector = if url.starts_with("wss://") {
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
        tokio_tungstenite::Connector::Rustls(Arc::new(tls))
    } else {
        tokio_tungstenite::Connector::Plain
    };
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(1 << 20))
        .max_frame_size(Some(1 << 20));
    let (socket, _) = tokio::time::timeout(
        HANDSHAKE_DEADLINE,
        tokio_tungstenite::connect_async_tls_with_config(url, Some(config), true, Some(connector)),
    )
    .await
    .map_err(|_| Stop::Handshake)?
    .map_err(|_| Stop::Handshake)?;
    Ok(socket)
}
/// Send one JSON-RPC call over the socket.
async fn call(socket: &mut Socket, id: u64, method: &str, params: Value) -> Result<()> {
    socket
        .send(Message::text(
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string(),
        ))
        .await
        .map_err(|_| Stop::Closed)?;
    Ok(())
}

/// A `Follow::Everything` session: the chain id, then every block and every log of the service contracts.
async fn session(
    url: &str,
    rpc: &Rpc,
    chain_id: u64,
    addresses: &[Address],
    signals: &Signals,
    last_block: &mut Option<u64>,
) -> Result<()> {
    let mut socket = connect(url).await?;
    for (id, method, params) in [
        (1, "eth_chainId", json!([])),
        (2, "eth_subscribe", json!(["newHeads"])),
        (3, "eth_subscribe", json!(["logs", {"address": addresses}])),
    ] {
        socket
            .send(Message::text(
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string(),
            ))
            .await
            .map_err(|_| Stop::Closed)?;
    }
    let (mut heads, mut logs, mut chain) = (None, None, None);
    let deadline = tokio::time::Instant::now() + HANDSHAKE_DEADLINE;
    while heads.is_none() || logs.is_none() || chain.is_none() {
        let message = tokio::time::timeout_at(deadline, next_json(&mut socket))
            .await
            .map_err(|_| Stop::Handshake)??;
        match message["id"].as_u64() {
            Some(1) => chain = Some(quantity(&message["result"]).map_err(|_| Stop::Protocol)?),
            Some(2) => heads = Some(subscription_id(&message)?),
            Some(3) => logs = Some(subscription_id(&message)?),
            // A notification can only follow its own subscription answer; anything else is ignored.
            _ => {}
        }
    }
    if chain != Some(chain_id) {
        return Err(Stop::WrongChain.into());
    }
    let (heads, logs) = (heads.context("heads")?, logs.context("logs")?);
    signals.resubscribed();
    tracing::info!("Chain event subscription live; ticks follow pushed blocks and events");
    // Subscribed first, backfilled second: nothing falls between the two, and overlap is harmless.
    if let Some(from) = *last_block {
        backfill(rpc, addresses, None, signals, from, Keepalive::default()).await;
    }
    loop {
        let message = match tokio::time::timeout(SILENCE_LIMIT, next_json(&mut socket)).await {
            Ok(message) => message?,
            Err(_) => return Err(Stop::Silent.into()),
        };
        if message["method"] != "eth_subscription" {
            continue;
        }
        let params = &message["params"];
        let result = &params["result"];
        if params["subscription"] == heads {
            let number = quantity(&result["number"]).map_err(|_| Stop::Protocol)?;
            *last_block = Some(last_block.map_or(number, |b| b.max(number)));
            // When the header arrived: a round keeper's sealing lag is measured from it. A header without a time is
            // still a head.
            if let Ok(timestamp) = quantity(&result["timestamp"]) {
                signals.header(number, timestamp, now_ms());
            }
            signals.head(number);
        } else if params["subscription"] == logs {
            // Endpoints do not all apply the subscription's address filter: a log from any other
            // contract would wake an idle keeper and hold it in the busy cadence for nothing.
            if !from_service(result, addresses) {
                continue;
            }
            let topic: Option<B256> = result["topics"]
                .get(0)
                .and_then(|t| serde_json::from_value(t.clone()).ok());
            let block = quantity(&result["blockNumber"]).unwrap_or(0);
            signals.saw(classify(topic.as_ref()), block);
        }
    }
}

/// The new-blocks subscription of a `Follow::Demand` session.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Blocks {
    Off,
    /// `eth_subscribe newHeads` sent with this id, its answer not come yet.
    Asked(u64),
    /// Live, with this subscription id.
    On(Value),
}
/// A `Follow::Demand` session: the chain id and the coordinator's `round_topics` logs, and new blocks while the run loop
/// has work open (`Signals::want_heads`): subscribed when work opens, unsubscribed when none is left. Without blocks the
/// endpoint may push nothing for minutes, so the session counts any frame as a sign of life and pings the endpoint after
/// PING_AFTER of quiet; nothing at all for SILENCE_LIMIT ends it, as for a `Follow::Everything` session.
async fn session_on_demand(
    url: &str,
    rpc: &Rpc,
    chain_id: u64,
    addresses: &[Address],
    signals: &Signals,
    last_block: &mut Option<u64>,
    keepalive: Keepalive,
) -> Result<()> {
    let topics = round_topics();
    let mut socket = connect(url).await?;
    call(&mut socket, 1, "eth_chainId", json!([])).await?;
    call(
        &mut socket,
        2,
        "eth_subscribe",
        json!(["logs", {"address": addresses, "topics": [topics]}]),
    )
    .await?;
    let (mut logs, mut chain) = (None, None);
    let deadline = tokio::time::Instant::now() + HANDSHAKE_DEADLINE;
    while logs.is_none() || chain.is_none() {
        let message = tokio::time::timeout_at(deadline, next_json(&mut socket))
            .await
            .map_err(|_| Stop::Handshake)??;
        match message["id"].as_u64() {
            Some(1) => chain = Some(quantity(&message["result"]).map_err(|_| Stop::Protocol)?),
            Some(2) => logs = Some(subscription_id(&message)?),
            _ => {}
        }
    }
    if chain != Some(chain_id) {
        return Err(Stop::WrongChain.into());
    }
    let logs = logs.context("logs")?;
    signals.resubscribed();
    tracing::info!(
        "Chain event subscription live; idle ticks follow pushed requests and the heartbeat, and new blocks are followed while work is open"
    );
    if let Some(from) = *last_block {
        backfill(rpc, addresses, Some(&topics), signals, from, keepalive).await;
    }
    let mut wanted = signals.heads_wanted.subscribe();
    let mut blocks = Blocks::Off;
    let mut next_id = 3u64;
    let mut heard = tokio::time::Instant::now();
    let mut pinged = false;
    wanted.borrow_and_update();
    follow_blocks(
        &mut socket,
        &mut blocks,
        signals.heads_wanted(),
        &mut next_id,
    )
    .await?;
    loop {
        let quiet = heard
            + if pinged {
                keepalive.silence
            } else {
                keepalive.ping_after
            };
        tokio::select! {
            frame = socket.next() => {
                let text = match frame {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_))) => {
                        heard = tokio::time::Instant::now();
                        pinged = false;
                        continue;
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => bail!(Stop::Closed),
                };
                heard = tokio::time::Instant::now();
                pinged = false;
                let message: Value = serde_json::from_str(text.as_str()).map_err(|_| Stop::Protocol)?;
                if let (Blocks::Asked(asked), Some(id)) = (&blocks, message["id"].as_u64())
                    && id == *asked
                {
                    blocks = Blocks::On(subscription_id(&message)?);
                    // Work may have closed while the answer was on its way.
                    follow_blocks(&mut socket, &mut blocks, signals.heads_wanted(), &mut next_id).await?;
                    continue;
                }
                if message["method"] != "eth_subscription" {
                    continue;
                }
                let params = &message["params"];
                let result = &params["result"];
                if matches!(&blocks, Blocks::On(id) if params["subscription"] == *id) {
                    let number = quantity(&result["number"]).map_err(|_| Stop::Protocol)?;
                    *last_block = Some(last_block.map_or(number, |b| b.max(number)));
                    if let Ok(timestamp) = quantity(&result["timestamp"]) {
                        signals.header(number, timestamp, now_ms());
                    }
                    signals.head(number);
                } else if params["subscription"] == logs {
                    // Neither the address filter nor the topics are applied by every endpoint.
                    let topic: Option<B256> = result["topics"]
                        .get(0)
                        .and_then(|t| serde_json::from_value(t.clone()).ok());
                    if !from_service(result, addresses)
                        || !topic.is_some_and(|topic| topics.contains(&topic))
                    {
                        continue;
                    }
                    let block = quantity(&result["blockNumber"]).unwrap_or(0);
                    signals.saw(classify(topic.as_ref()), block);
                }
            }
            changed = wanted.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let want = *wanted.borrow_and_update();
                follow_blocks(&mut socket, &mut blocks, want, &mut next_id).await?;
            }
            _ = tokio::time::sleep_until(quiet) => {
                if pinged {
                    return Err(Stop::Silent.into());
                }
                socket
                    .send(Message::Ping(Default::default()))
                    .await
                    .map_err(|_| Stop::Closed)?;
                pinged = true;
            }
        }
    }
}
/// Bring the new-blocks subscription in line with whether work is open: subscribe when it is and there is none,
/// unsubscribe when it is not and there is one. A subscription still being asked for is left alone; its answer brings it
/// in line.
async fn follow_blocks(
    socket: &mut Socket,
    blocks: &mut Blocks,
    wanted: bool,
    next_id: &mut u64,
) -> Result<()> {
    match (&*blocks, wanted) {
        (Blocks::Off, true) => {
            call(socket, *next_id, "eth_subscribe", json!(["newHeads"])).await?;
            *blocks = Blocks::Asked(*next_id);
            *next_id += 1;
        }
        (Blocks::On(id), false) => {
            call(socket, *next_id, "eth_unsubscribe", json!([id])).await?;
            *blocks = Blocks::Off;
            *next_id += 1;
        }
        _ => {}
    }
    Ok(())
}
/// The wall clock in milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}
/// Whether a log was emitted by one of the service contracts.
fn from_service(log: &Value, addresses: &[Address]) -> bool {
    serde_json::from_value::<Address>(log["address"].clone())
        .is_ok_and(|address| addresses.contains(&address))
}
fn subscription_id(message: &Value) -> Result<Value> {
    let id = &message["result"];
    ensure!(id.is_string(), Stop::Protocol);
    Ok(id.clone())
}
async fn next_json<S>(socket: &mut S) -> Result<Value>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(text.as_str()).map_err(|_| Stop::Protocol.into());
            }
            // Pings are answered by the library; pongs and binary frames carry nothing for us.
            Some(Ok(
                Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_),
            )) => {}
            Some(Ok(Message::Close(_))) | None => bail!(Stop::Closed),
            Some(Err(_)) => bail!(Stop::Closed),
        }
    }
}
/// Read the logs of the blocks the subscription could not see, in ranges of `window`'s backfill range. A gap longer
/// than its backfill maximum, or any read failure, is left alone: the resubscription already forced every re-check and a
/// tick, and the worker's own reads are what find the work. With `topics`, only logs whose first topic is one of them.
async fn backfill(
    rpc: &Rpc,
    addresses: &[Address],
    topics: Option<&[B256]>,
    signals: &Signals,
    last: u64,
    window: Keepalive,
) {
    let head = match rpc.head().await {
        Ok(head) => head.number,
        Err(_) => return,
    };
    if head <= last || head - last > window.backfill_max_blocks {
        return;
    }
    let mut from = last + 1;
    while from <= head {
        let to = head.min(from + window.backfill_range - 1);
        let mut filter = json!({"address":addresses,"fromBlock":format!("0x{from:x}"),"toBlock":format!("0x{to:x}")});
        if let Some(topics) = topics {
            filter["topics"] = json!([topics]);
        }
        let Ok(value) = rpc.request("eth_getLogs", json!([filter])).await else {
            return;
        };
        for log in value.as_array().into_iter().flatten() {
            if !from_service(log, addresses) {
                continue;
            }
            let topic: Option<B256> = log["topics"]
                .get(0)
                .and_then(|t| serde_json::from_value(t.clone()).ok());
            if topics.is_some_and(|topics| !topic.is_some_and(|topic| topics.contains(&topic))) {
                continue;
            }
            signals.saw(
                classify(topic.as_ref()),
                quantity(&log["blockNumber"]).unwrap_or(to),
            );
        }
        from = to + 1;
    }
    tracing::debug!(
        from = last + 1,
        to = head,
        "Backfilled chain events missed while unsubscribed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    fn topic(signature: &str) -> B256 {
        keccak256(signature)
    }
    #[test]
    fn upgrades_and_role_changes_are_told_apart_from_work() {
        assert_eq!(classify(Some(&topic("Upgraded(address)"))), Kind::Upgrade);
        for role in [
            "CommitterChanged(address,address)",
            "BackupCommitterSet(address,bool)",
        ] {
            assert_eq!(classify(Some(&topic(role))), Kind::Role);
        }
        for work in [
            "RandomnessRequested(uint256,address,bytes32,bytes32,uint64,uint32,uint256,address,uint64)",
            "FulfillmentSkipped(uint256,uint8)",
            "EpochCommitted(uint64,bytes32,bytes)",
            "CatalogScheduled(uint64,bytes32,uint8[],address[])",
        ] {
            assert_eq!(classify(Some(&topic(work))), Kind::Work);
        }
        assert_eq!(classify(None), Kind::Work);
    }
    /// One scripted WebSocket JSON-RPC session: answer the handshake for `chain`, push `pushes`, then close.
    async fn scripted(listener: &tokio::net::TcpListener, chain: &str, pushes: Vec<Value>) {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut answered = 0;
        while answered < 3 {
            let Some(Ok(Message::Text(text))) = socket.next().await else {
                return;
            };
            let call: Value = serde_json::from_str(text.as_str()).unwrap();
            let result = match (call["method"].as_str().unwrap(), call["params"][0].as_str()) {
                ("eth_chainId", _) => json!(chain),
                ("eth_subscribe", Some("newHeads")) => json!("0xheads"),
                ("eth_subscribe", Some("logs")) => {
                    assert_eq!(call["params"][1]["address"].as_array().unwrap().len(), 2);
                    json!("0xlogs")
                }
                other => panic!("unexpected call {other:?}"),
            };
            socket
                .send(Message::text(
                    json!({"jsonrpc":"2.0","id":call["id"],"result":result}).to_string(),
                ))
                .await
                .unwrap();
            answered += 1;
        }
        for push in pushes {
            socket.send(Message::text(push.to_string())).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = socket.close(None).await;
    }
    fn head(number: u64) -> Value {
        json!({"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"0xheads","result":{"number":format!("0x{number:x}")}}})
    }
    /// A log pushed on the logs subscription, from the first service contract.
    fn log(signature: &str, block: u64) -> Value {
        log_from(Address::repeat_byte(1), signature, block)
    }
    fn log_from(address: Address, signature: &str, block: u64) -> Value {
        json!({"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"0xlogs","result":{"address":address,"topics":[topic(signature)],"blockNumber":format!("0x{block:x}")}}})
    }
    /// An HTTP endpoint for the backfill: its head is block 0x40 and one role event of the registry sits at 0x30,
    /// followed at 0x38 by one of another contract, which an endpoint that ignores the address filter returns too.
    async fn backfill_endpoint() -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut data = Vec::new();
                    let mut buffer = [0; 4096];
                    let body: Value = loop {
                        let size = socket.read(&mut buffer).await.unwrap();
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
                                break serde_json::from_slice(&data[end + 4..end + 4 + length])
                                    .unwrap();
                            }
                        }
                    };
                    let result = match body["method"].as_str().unwrap() {
                        "eth_getBlockByNumber" => {
                            json!({"number":"0x40","hash":B256::repeat_byte(1),"timestamp":"0x1","baseFeePerGas":"0x1"})
                        }
                        "eth_getLogs" => {
                            let from = quantity(&body["params"][0]["fromBlock"]).unwrap();
                            let to = quantity(&body["params"][0]["toBlock"]).unwrap();
                            assert!(to - from < BACKFILL_RANGE);
                            if (from..=to).contains(&0x30) {
                                json!([
                                    {"address":Address::repeat_byte(2),"topics":[topic("BackupCommitterSet(address,bool)")],"blockNumber":"0x30"},
                                    {"address":Address::repeat_byte(9),"topics":[topic("BackupCommitterSet(address,bool)")],"blockNumber":"0x38"}
                                ])
                            } else {
                                json!([])
                            }
                        }
                        other => panic!("unexpected {other}"),
                    };
                    let answer =
                        json!({"jsonrpc":"2.0","id":body["id"],"result":result}).to_string();
                    let _ = socket
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                                answer.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });
        (url, task)
    }
    async fn eventually(what: &str, condition: impl Fn() -> bool) {
        let end = tokio::time::Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(tokio::time::Instant::now() < end, "{what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    #[test]
    fn a_round_keepers_keepalive_is_its_settings_and_an_epoch_keepers_is_0_4_1s() {
        let default = Keepalive::default();
        assert_eq!(
            (
                default.ping_after,
                default.silence,
                default.backfill_max_blocks,
                default.backfill_range
            ),
            (PING_AFTER, SILENCE_LIMIT, 5_000, 500)
        );
        let settings = |silence| crate::config::ChainSettings {
            ws_silence_seconds: silence,
            ws_backfill_max_blocks: 6_000,
            ws_backfill_range: 2_000,
            ..Default::default()
        };
        let configured = Keepalive::configured(&settings(60));
        assert_eq!(
            (
                configured.ping_after,
                configured.silence,
                configured.backfill_max_blocks,
                configured.backfill_range
            ),
            (PING_AFTER, Duration::from_secs(60), 6_000, 2_000)
        );
        assert_eq!(
            Keepalive::configured(&settings(5)).ping_after,
            Duration::from_millis(2_500)
        );
    }
    #[tokio::test]
    async fn pushed_events_drive_signals_and_a_reconnect_rechecks_and_backfills() {
        let wrong = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let right = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let urls = vec![
            format!("ws://{}", wrong.local_addr().unwrap()),
            format!("ws://{}", right.local_addr().unwrap()),
        ];
        let (http, backfill) = backfill_endpoint().await;
        let signals = Signals::new();
        let mut heads = signals.heads();
        let server = tokio::spawn(async move {
            // An endpoint on another chain is used once and then never again.
            scripted(&wrong, "0x1", vec![]).await;
            scripted(
                &right,
                "0x7a69",
                vec![
                    head(0x10),
                    log("Upgraded(address)", 0x10),
                    // Another contract's log, pushed by an endpoint that ignores the address filter.
                    log_from(Address::repeat_byte(9), "Upgraded(address)", 0x20),
                    log("RandomnessRequested(uint256,address,bytes32,bytes32,uint64,uint32,uint256,address,uint64)", 0x11),
                    head(0x11),
                ],
            )
            .await;
            // The second session follows a dropped first one.
            scripted(&right, "0x7a69", vec![head(0x41)]).await;
            // Keep the endpoint open without answering, so the task stays on its reconnect path.
            let _held = right.accept().await;
            std::future::pending::<()>().await;
        });
        let task = spawn(
            urls,
            Rpc::new(vec![http]).unwrap(),
            31337,
            [Address::repeat_byte(1), Address::repeat_byte(2)],
            signals.clone(),
        )
        .unwrap();
        eventually("first session delivered its events", || {
            signals.activity() == 0x11 && *heads.borrow() == 0x11
        })
        .await;
        assert!(heads.has_changed().unwrap());
        heads.borrow_and_update();
        // One resubscription plus one upgrade event; one resubscription for roles.
        assert!(signals.upgrades() >= 2);
        let roles_after_first = signals.roles();
        assert!(roles_after_first >= 1);
        // The drop is noticed, the task reconnects after its back-off, forces a fresh re-check and backfills the
        // gap over HTTP, where a role change was hiding.
        eventually("second session backfilled the gap", || {
            signals.roles() >= roles_after_first + 2 && signals.activity() >= 0x30
        })
        .await;
        eventually("second session pushed its head", || *heads.borrow() == 0x41).await;
        // Neither the pushed nor the backfilled log of the other contract counted as work.
        assert_eq!(signals.activity(), 0x30);
        drop(task);
        server.abort();
        backfill.abort();
    }
    #[tokio::test]
    async fn without_a_live_subscription_the_loop_polls_by_its_intervals() {
        let signals = Signals::new();
        let mut heads = signals.heads();
        let cadence = |poll: u64| Cadence {
            poll: Duration::from_millis(poll),
            idle_poll: Duration::from_millis(150),
            heartbeat: IDLE_HEARTBEAT,
        };
        let began = tokio::time::Instant::now();
        signals.next_tick(&mut heads, false, cadence(10)).await;
        assert!(began.elapsed() >= Duration::from_millis(150));
        let began = tokio::time::Instant::now();
        signals.next_tick(&mut heads, true, cadence(10)).await;
        assert!(began.elapsed() < Duration::from_millis(150));
        // Live and idle: an event wakes the loop long before the heartbeat.
        signals.live.store(true, Ordering::SeqCst);
        let waker = signals.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            waker.saw(Kind::Work, 7);
        });
        let began = tokio::time::Instant::now();
        signals.next_tick(&mut heads, false, cadence(10)).await;
        assert!(began.elapsed() < IDLE_HEARTBEAT);
        assert_eq!(signals.activity(), 7);
        // Live and busy: a pushed block ends the wait after the poll spacing.
        let waker = signals.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            waker.head(9);
        });
        let began = tokio::time::Instant::now();
        signals.next_tick(&mut heads, true, cadence(20)).await;
        assert!(began.elapsed() >= Duration::from_millis(20) && began.elapsed() < BUSY_FALLBACK);
        // Live and idle with nothing pushed: the heartbeat, a round keeper's own.
        let began = tokio::time::Instant::now();
        let round = Cadence {
            heartbeat: Duration::from_millis(120),
            ..cadence(10)
        };
        signals.next_tick(&mut heads, false, round).await;
        assert!(
            began.elapsed() >= Duration::from_millis(120)
                && began.elapsed() < Duration::from_millis(1_000)
        );
    }
    #[test]
    fn the_loop_waits_by_the_subscription_and_the_open_work() {
        let cadence = Cadence {
            poll: Duration::from_millis(1_000),
            idle_poll: Duration::from_millis(2_000),
            heartbeat: Duration::from_secs(10),
        };
        assert_eq!(cadence.wait(false, true), Wait::Sleep(cadence.poll));
        assert_eq!(cadence.wait(false, false), Wait::Sleep(cadence.idle_poll));
        assert_eq!(
            cadence.wait(true, true),
            Wait::Blocks {
                spacing: cadence.poll,
                fallback: BUSY_FALLBACK
            }
        );
        assert_eq!(cadence.wait(true, false), Wait::Event(cadence.heartbeat));
    }

    /// A scripted endpoint for a round keeper's subscription: every connection it accepts answers `eth_chainId` with the
    /// local chain, `eth_subscribe` with `0xlogs` or `0xheads` and `eth_unsubscribe` with true, records each call (and
    /// each ping) as a line, and pushes what the test sends it. A muted connection reads nothing more: it neither answers
    /// nor pongs.
    struct Endpoint {
        url: String,
        lines: Arc<std::sync::Mutex<Vec<String>>>,
        pushes: tokio::sync::mpsc::UnboundedSender<Value>,
        muted: Arc<AtomicBool>,
        server: tokio::task::JoinHandle<()>,
    }
    impl Endpoint {
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}", listener.local_addr().unwrap());
            let lines: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
            let (pushes, script) = tokio::sync::mpsc::unbounded_channel::<Value>();
            let script = Arc::new(tokio::sync::Mutex::new(script));
            let muted = Arc::new(AtomicBool::new(false));
            let (log, mute) = (lines.clone(), muted.clone());
            let server = tokio::spawn(async move {
                let mut session = 0;
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    session += 1;
                    mute.store(false, Ordering::SeqCst);
                    let (log, mute, script) = (log.clone(), mute.clone(), script.clone());
                    tokio::spawn(async move {
                        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                        let say =
                            |line: String| log.lock().unwrap().push(format!("{session}: {line}"));
                        loop {
                            if mute.load(Ordering::SeqCst) {
                                std::future::pending::<()>().await;
                            }
                            let mut script = script.lock().await;
                            tokio::select! {
                                frame = socket.next() => {
                                    drop(script);
                                    let text = match frame {
                                        Some(Ok(Message::Text(text))) => text,
                                        Some(Ok(Message::Ping(_))) => {
                                            say("ping".into());
                                            continue;
                                        }
                                        Some(Ok(_)) => continue,
                                        _ => return,
                                    };
                                    let call: Value = serde_json::from_str(text.as_str()).unwrap();
                                    let params = &call["params"];
                                    let (line, result) = match call["method"].as_str().unwrap() {
                                        "eth_chainId" => ("eth_chainId".to_owned(), json!("0x7a69")),
                                        "eth_subscribe" if params[0] == "logs" => {
                                            (format!("eth_subscribe logs {}", params[1]), json!("0xlogs"))
                                        }
                                        "eth_subscribe" => (
                                            format!("eth_subscribe {}", params[0].as_str().unwrap()),
                                            json!("0xheads"),
                                        ),
                                        "eth_unsubscribe" => (
                                            format!("eth_unsubscribe {}", params[0].as_str().unwrap()),
                                            json!(true),
                                        ),
                                        other => panic!("unexpected {other}"),
                                    };
                                    say(line);
                                    let answer = json!({"jsonrpc":"2.0","id":call["id"],"result":result});
                                    if socket.send(Message::text(answer.to_string())).await.is_err() {
                                        return;
                                    }
                                }
                                push = script.recv() => {
                                    let Some(push) = push else { return };
                                    if socket.send(Message::text(push.to_string())).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                    });
                }
            });
            Self {
                url,
                lines,
                pushes,
                muted,
                server,
            }
        }
        fn push(&self, value: Value) {
            self.pushes.send(value).unwrap();
        }
        fn lines(&self) -> Vec<String> {
            self.lines.lock().unwrap().clone()
        }
    }
    impl Drop for Endpoint {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    /// A round keeper's subscription (`Follow::Demand`): its handshake asks for the coordinator's requests, role changes
    /// and upgrades and no blocks; a pushed request wakes the keeper and another of the coordinator's events does not; new
    /// blocks are subscribed while the run loop says work is open and unsubscribed when it says none is; a quiet
    /// connection is pinged and stays live while it answers, and one that stops answering is replaced, which subscribes
    /// to blocks at once when work is open.
    #[tokio::test]
    async fn a_round_subscription_follows_requests_and_follows_blocks_only_while_work_is_open() {
        let endpoint = Endpoint::start().await;
        let (http, backfill) = backfill_endpoint().await;
        let signals = Signals::new();
        let mut heads = signals.heads();
        let coordinator = Address::repeat_byte(1);
        let _task = spawn_task(
            vec![endpoint.url.clone()],
            Rpc::new(vec![http]).unwrap(),
            31337,
            [coordinator],
            signals.clone(),
            Follow::Demand,
            Keepalive {
                ping_after: Duration::from_millis(300),
                silence: Duration::from_millis(900),
                ..Keepalive::default()
            },
        )
        .unwrap();
        eventually("the subscription is live", || signals.live()).await;
        let topics: Vec<String> = round_topics().iter().map(B256::to_string).collect();
        assert_eq!(
            endpoint.lines(),
            [
                "1: eth_chainId".to_owned(),
                format!(
                    "1: eth_subscribe logs {}",
                    json!({"address": [coordinator], "topics": [topics]})
                )
            ]
        );
        let requested = "RandomnessRequested(uint256,address,bytes32,bytes32,uint64,uint32,uint256,address,uint64)";
        assert_eq!(topic(requested), round_topics()[0]);
        // A request wakes the keeper; an event of a request it has is not followed, even when the endpoint pushes it.
        let roles = signals.roles();
        endpoint.push(log(requested, 0x11));
        endpoint.push(log("RoundAssigned(uint256,uint8,uint64)", 0x12));
        endpoint.push(log("KeeperChanged(address,address)", 0x11));
        eventually("the role change arrived", || signals.roles() > roles).await;
        assert_eq!(signals.activity(), 0x11);
        // Work opens: blocks are followed.
        signals.want_heads(true);
        eventually("blocks were subscribed", || {
            endpoint.lines().last().map(String::as_str) == Some("1: eth_subscribe newHeads")
        })
        .await;
        endpoint.push(head(0x13));
        eventually("the block arrived", || *heads.borrow() == 0x13).await;
        heads.borrow_and_update();
        // Work closes: blocks are unsubscribed, and one pushed after that is not followed.
        signals.want_heads(false);
        eventually("blocks were unsubscribed", || {
            endpoint.lines().last().map(String::as_str) == Some("1: eth_unsubscribe 0xheads")
        })
        .await;
        endpoint.push(head(0x14));
        // Quiet: pinged, answered, and still live past the silence limit.
        eventually("the endpoint was pinged twice", || {
            endpoint
                .lines()
                .iter()
                .filter(|line| line.ends_with("ping"))
                .count()
                >= 2
        })
        .await;
        assert!(signals.live());
        assert!(!heads.has_changed().unwrap(), "{}", *heads.borrow());
        // Work opens and the endpoint stops answering: the session is replaced, and the new one follows blocks at once.
        signals.want_heads(true);
        endpoint.muted.store(true, Ordering::SeqCst);
        eventually("the silent session was replaced", || {
            endpoint
                .lines()
                .iter()
                .any(|line| line == "2: eth_subscribe newHeads")
        })
        .await;
        let second: Vec<String> = endpoint
            .lines()
            .into_iter()
            .filter(|line| line.starts_with("2: ") && !line.ends_with("ping"))
            .collect();
        assert_eq!(second.len(), 3, "{second:#?}");
        assert_eq!(second[0], "2: eth_chainId");
        assert!(
            second[1].starts_with("2: eth_subscribe logs "),
            "{second:#?}"
        );
        backfill.abort();
    }
}
