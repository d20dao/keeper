//! Optional push path for chain events: `eth_subscribe` to new heads and to the coordinator's and registry's logs
//! over WebSocket. Events decide only when the worker ticks and when it re-checks its runtime pins and publishing
//! right; every decision still comes from the worker's own HTTP reads, so a missed, late or repeated event can delay
//! work but never change it. Without a WebSocket endpoint, or while every subscription is down, the worker polls.
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
/// With nothing open and a live subscription, a maintenance tick still runs this often (epoch preparation, health
/// observations, operator commands). Work itself arrives as an event and wakes the worker at once.
pub const IDLE_HEARTBEAT: Duration = Duration::from_secs(5);
/// A subscription that has pushed nothing for this long is replaced: the chain makes blocks every second or so.
const SILENCE_LIMIT: Duration = Duration::from_secs(20);
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
    /// A change of the registry's committer or backup committers: the publishing right must be checked again.
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
    {
        Kind::Role
    } else {
        Kind::Work
    }
}

/// Shared between the subscription task, the main loop and the worker.
pub struct Signals {
    heads: tokio::sync::watch::Sender<u64>,
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
            work: tokio::sync::Notify::new(),
            upgrades: AtomicU64::new(0),
            roles: AtomicU64::new(0),
            activity: AtomicU64::new(0),
            live: AtomicBool::new(false),
        })
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
    /// Resolve when the next tick is due. Without a live subscription this is plain polling: `poll` while work is
    /// open, `idle` otherwise. With one, open work ticks on the next pushed block or event (keeping at least `poll`
    /// between ticks, and at most BUSY_FALLBACK without anything pushed), and an idle keeper ticks on the next event
    /// or after IDLE_HEARTBEAT.
    pub async fn next_tick(
        &self,
        heads: &mut tokio::sync::watch::Receiver<u64>,
        busy: bool,
        poll: Duration,
        idle: Duration,
    ) {
        if !self.live() {
            tokio::time::sleep(if busy { poll } else { idle }).await;
            return;
        }
        if busy {
            tokio::time::sleep(poll).await;
            tokio::select! {
                _ = heads.changed() => {}
                _ = self.work.notified() => {}
                _ = tokio::time::sleep(BUSY_FALLBACK) => {}
            }
        } else {
            tokio::select! {
                _ = self.work.notified() => {}
                _ = tokio::time::sleep(IDLE_HEARTBEAT) => {}
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
/// one that serves another chain is dropped for the life of the process.
pub fn spawn(
    urls: Vec<String>,
    rpc: Rpc,
    chain_id: u64,
    addresses: [Address; 2],
    signals: Arc<Signals>,
) -> Option<Subscription> {
    if urls.is_empty() {
        return None;
    }
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
            let ended = session(
                &urls[index],
                &rpc,
                chain_id,
                addresses,
                &signals,
                &mut last_block,
            )
            .await;
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

async fn session(
    url: &str,
    rpc: &Rpc,
    chain_id: u64,
    addresses: [Address; 2],
    signals: &Signals,
    last_block: &mut Option<u64>,
) -> Result<()> {
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
    let (mut socket, _) = tokio::time::timeout(
        HANDSHAKE_DEADLINE,
        tokio_tungstenite::connect_async_tls_with_config(url, Some(config), true, Some(connector)),
    )
    .await
    .map_err(|_| Stop::Handshake)?
    .map_err(|_| Stop::Handshake)?;
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
        backfill(rpc, addresses, signals, from).await;
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
            signals.head(number);
        } else if params["subscription"] == logs {
            let topic: Option<B256> = result["topics"]
                .get(0)
                .and_then(|t| serde_json::from_value(t.clone()).ok());
            let block = quantity(&result["blockNumber"]).unwrap_or(0);
            signals.saw(classify(topic.as_ref()), block);
        }
    }
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
/// Read the logs of the blocks the subscription could not see, in bounded ranges. A gap longer than
/// BACKFILL_MAX_BLOCKS, or any read failure, is left alone: the resubscription already forced every re-check and a
/// tick, and the worker's own reads are what find the work.
async fn backfill(rpc: &Rpc, addresses: [Address; 2], signals: &Signals, last: u64) {
    let head = match rpc.head().await {
        Ok(head) => head.number,
        Err(_) => return,
    };
    if head <= last || head - last > BACKFILL_MAX_BLOCKS {
        return;
    }
    let mut from = last + 1;
    while from <= head {
        let to = head.min(from + BACKFILL_RANGE - 1);
        let Ok(value) = rpc
            .request(
                "eth_getLogs",
                json!([{"address":addresses,"fromBlock":format!("0x{from:x}"),"toBlock":format!("0x{to:x}")}]),
            )
            .await
        else {
            return;
        };
        for log in value.as_array().into_iter().flatten() {
            let topic: Option<B256> = log["topics"]
                .get(0)
                .and_then(|t| serde_json::from_value(t.clone()).ok());
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
    fn log(signature: &str, block: u64) -> Value {
        json!({"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"0xlogs","result":{"topics":[topic(signature)],"blockNumber":format!("0x{block:x}")}}})
    }
    /// An HTTP endpoint for the backfill: its head is block 0x40 and one role event sits at 0x30.
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
                            json!({"number":"0x40","hash":B256::repeat_byte(1),"timestamp":"0x1"})
                        }
                        "eth_getLogs" => {
                            let from = quantity(&body["params"][0]["fromBlock"]).unwrap();
                            let to = quantity(&body["params"][0]["toBlock"]).unwrap();
                            assert!(to - from < BACKFILL_RANGE);
                            if (from..=to).contains(&0x30) {
                                json!([{"topics":[topic("BackupCommitterSet(address,bool)")],"blockNumber":"0x30"}])
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
        drop(task);
        server.abort();
        backfill.abort();
    }
    #[tokio::test]
    async fn without_a_live_subscription_the_loop_polls_by_its_intervals() {
        let signals = Signals::new();
        let mut heads = signals.heads();
        let began = tokio::time::Instant::now();
        signals
            .next_tick(
                &mut heads,
                false,
                Duration::from_millis(10),
                Duration::from_millis(150),
            )
            .await;
        assert!(began.elapsed() >= Duration::from_millis(150));
        let began = tokio::time::Instant::now();
        signals
            .next_tick(
                &mut heads,
                true,
                Duration::from_millis(10),
                Duration::from_millis(150),
            )
            .await;
        assert!(began.elapsed() < Duration::from_millis(150));
        // Live and idle: an event wakes the loop long before the heartbeat.
        signals.live.store(true, Ordering::SeqCst);
        let waker = signals.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            waker.saw(Kind::Work, 7);
        });
        let began = tokio::time::Instant::now();
        signals
            .next_tick(
                &mut heads,
                false,
                Duration::from_millis(10),
                Duration::from_millis(150),
            )
            .await;
        assert!(began.elapsed() < IDLE_HEARTBEAT);
        assert_eq!(signals.activity(), 7);
        // Live and busy: a pushed block ends the wait after the poll spacing.
        let waker = signals.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            waker.head(9);
        });
        let began = tokio::time::Instant::now();
        signals
            .next_tick(
                &mut heads,
                true,
                Duration::from_millis(20),
                Duration::from_millis(150),
            )
            .await;
        assert!(began.elapsed() >= Duration::from_millis(20) && began.elapsed() < BUSY_FALLBACK);
    }
}
