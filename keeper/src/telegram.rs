//! Optional best-effort Telegram transport. No request-path network operations.
use alloy_primitives::{Address, B256, U256};
use reqwest::Client;
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{mpsc, watch},
    task::AbortHandle,
    time::Instant,
};
use zeroize::Zeroizing;

const CAPACITY: usize = 128;
const BODY_LIMIT: usize = 65_536;
const TEXT_LIMIT: usize = 3_900;
const COOLDOWN: Duration = Duration::from_secs(300);

// Intentionally no Debug: credentials and authenticated URLs must never be logged.
pub struct Settings {
    token: Zeroizing<String>,
    chat_id: i64,
    pub low_balance_wei: U256,
    display: DisplayMetadata,
    /// Poll getUpdates for /status and /keeper. Keepers sharing one bot token compete for updates, so only one
    /// of them may poll: TELEGRAM_COMMANDS defaults to true for a primary and false for a follower.
    pub commands: bool,
}
#[derive(Clone)]
struct DisplayMetadata {
    symbol: String,
    explorer: Option<reqwest::Url>,
    /// A follower names itself in every message so its notices stand apart in a shared chat.
    follower: bool,
}
impl Default for DisplayMetadata {
    fn default() -> Self {
        Self {
            symbol: "native".into(),
            explorer: None,
            follower: false,
        }
    }
}
impl DisplayMetadata {
    fn parse(symbol: Option<String>, explorer: Option<String>) -> Result<Self, ConfigurationError> {
        let symbol = symbol.unwrap_or_else(|| "native".into());
        if symbol.is_empty()
            || symbol.len() > 24
            || !symbol
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(ConfigurationError);
        }
        let explorer = explorer
            .map(|value| {
                if value.len() > 1024 {
                    return Err(ConfigurationError);
                }
                let url = reqwest::Url::parse(&value).map_err(|_| ConfigurationError)?;
                if url.scheme() != "https"
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.query().is_some()
                    || url.fragment().is_some()
                {
                    return Err(ConfigurationError);
                }
                Ok(url)
            })
            .transpose()?;
        Ok(Self {
            symbol,
            explorer,
            follower: false,
        })
    }
}
struct Destination {
    chat_id: i64,
    display: DisplayMetadata,
}
#[derive(Debug)]
pub struct ConfigurationError;
impl std::fmt::Display for ConfigurationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "Invalid Telegram configuration; check bot/chat settings and public display metadata",
        )
    }
}
impl std::error::Error for ConfigurationError {}
impl Settings {
    pub fn from_env(role: crate::config::Role) -> Result<Option<Self>, ConfigurationError> {
        let read = |name| {
            std::env::var(name).map(Some).or_else(|e| match e {
                std::env::VarError::NotPresent => Ok(None),
                _ => Err(ConfigurationError),
            })
        };
        let mut settings = Self::parse(
            read("TELEGRAM_BOT_TOKEN")?,
            read("TELEGRAM_CHAT_ID")?,
            read("TELEGRAM_LOW_BALANCE_WEI")?,
        )?;
        if let Some(settings) = &mut settings {
            settings.display =
                DisplayMetadata::parse(read("NATIVE_CURRENCY_SYMBOL")?, read("EXPLORER_URL")?)?;
            settings.display.follower = role.is_follower();
            settings.commands = match read("TELEGRAM_COMMANDS")?.as_deref().map(str::trim) {
                None => !role.is_follower(),
                Some("true") => true,
                Some("false") => false,
                Some(_) => return Err(ConfigurationError),
            };
        }
        Ok(settings)
    }
    fn parse(
        token: Option<String>,
        chat: Option<String>,
        threshold: Option<String>,
    ) -> Result<Option<Self>, ConfigurationError> {
        let (token, chat) = match (token, chat) {
            (None, None) => return Ok(None),
            (Some(t), Some(c)) => (t, c),
            _ => return Err(ConfigurationError),
        };
        let valid = token.split_once(':').is_some_and(|(id, key)| {
            !id.is_empty()
                && id.bytes().all(|b| b.is_ascii_digit())
                && !key.is_empty()
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        });
        if !valid || token.len() > 256 {
            return Err(ConfigurationError);
        }
        let chat_id = chat.parse::<i64>().map_err(|_| ConfigurationError)?;
        if chat_id == 0 {
            return Err(ConfigurationError);
        }
        let low_balance_wei =
            U256::from_str_radix(threshold.as_deref().unwrap_or("50000000000000000"), 10)
                .map_err(|_| ConfigurationError)?;
        Ok(Some(Self {
            token: Zeroizing::new(token),
            chat_id,
            low_balance_wei,
            display: DisplayMetadata::default(),
            commands: true,
        }))
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ErrorClass {
    RpcUnavailable,
    EpochPreparation,
    TransactionSubmission,
    ReceiptRecovery,
    KeeperTick,
    /// Configured fee/cost caps below the required send. `Event::FeeBudget` carries
    /// the exceeded cap and shares this class's cooldown.
    FeeBudget,
}
#[derive(Clone, Copy, Debug)]
pub enum Health {
    Unknown,
    Healthy,
    Degraded,
}
#[derive(Clone, Copy, Debug)]
pub enum EpochState {
    Unknown,
    Local,
    Published,
    Missing,
}
#[derive(Clone, Debug)]
pub struct StatusSnapshot {
    pub observed_at_unix: Option<u64>,
    pub health: Health,
    pub pending: u64,
    pub served: u64,
    pub last_serve_unix: Option<u64>,
    pub epoch_id: Option<u64>,
    pub epoch_state: EpochState,
    pub transaction_wallet: Address,
    pub chain_id: u64,
    pub balance_wei: Option<U256>,
    pub authorized: Option<bool>,
    /// A follower's view of whether the primary is alive (committer()'s confirmed nonce recently advanced).
    pub primary_alive: Option<bool>,
}
#[derive(Clone, Debug)]
pub enum Event {
    Fulfilled {
        request_id: U256,
        tx_hash: B256,
    },
    OperationalError {
        class: ErrorClass,
    },
    /// A required send exceeds a configured cap; work is deferred, never forced.
    FeeBudget(crate::config::FeeBudget),
    LowBalance {
        wallet: Address,
        balance_wei: U256,
    },
    /// An operator sweep finished or was refused; the worker builds the text.
    Sweep(String),
    /// The transaction wallet gained or lost the right to publish in its role; the worker builds the text.
    Authorization(String),
    /// A follower published an epoch because the primary had not within its delay.
    EpochPublished {
        epoch: u64,
        tx_hash: B256,
    },
    /// Informational: the keeper restarted on an approved next implementation after an in-place upgrade and passed
    /// every startup check on it. The worker builds the text.
    Upgrade(String),
}
#[derive(Clone, Copy)]
enum Command {
    Status,
    Keeper,
}
enum Message {
    Event(Event),
    Command(Command),
}
struct Tasks {
    send: AbortHandle,
    poll: Option<AbortHandle>,
}
impl Drop for Tasks {
    fn drop(&mut self) {
        self.send.abort();
        if let Some(poll) = &self.poll {
            poll.abort();
        }
    }
}
#[derive(Clone)]
pub struct TelegramNotifier {
    tx: mpsc::Sender<Message>,
    dropped: Arc<AtomicU64>,
    status: watch::Sender<Option<StatusSnapshot>>,
    threshold: U256,
    _tasks: Arc<Tasks>,
}
fn increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
        Some(n.saturating_add(1))
    });
}
impl TelegramNotifier {
    /// Call inside the Tokio runtime. Both actors are cancelled when the final handle drops.
    pub fn start(settings: Settings) -> Result<Self, ConfigurationError> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| ConfigurationError)?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(3))
            .build()
            .map_err(|_| ConfigurationError)?;
        let (tx, rx) = mpsc::channel(CAPACITY);
        let (status, snapshot) = watch::channel(None);
        let dropped = Arc::new(AtomicU64::new(0));
        let base = format!("https://api.telegram.org/bot{}", settings.token.as_str());
        let started = Instant::now();
        let send = runtime.spawn(send_loop(
            client.clone(),
            format!("{base}/sendMessage"),
            Destination {
                chat_id: settings.chat_id,
                display: settings.display,
            },
            rx,
            snapshot,
            dropped.clone(),
            started,
        ));
        // Notification-only keepers never call getUpdates, so they cannot take another keeper's commands.
        let poll = settings.commands.then(|| {
            runtime.spawn(poll_loop(
                client,
                base,
                settings.chat_id,
                tx.clone(),
                dropped.clone(),
            ))
        });
        Ok(Self {
            tx,
            dropped,
            status,
            threshold: settings.low_balance_wei,
            _tasks: Arc::new(Tasks {
                send: send.abort_handle(),
                poll: poll.map(|task| task.abort_handle()),
            }),
        })
    }
    pub fn notify(&self, event: Event) {
        enqueue(&self.tx, Message::Event(event), &self.dropped);
    }
    /// Feed independently observed public state, preferably every 60 seconds outside the request tick.
    pub fn update_status(&self, snapshot: StatusSnapshot) {
        if let Some(balance) = snapshot.balance_wei.filter(|b| *b < self.threshold) {
            self.notify(Event::LowBalance {
                wallet: snapshot.transaction_wallet,
                balance_wei: balance,
            });
        }
        self.status.send_replace(Some(snapshot));
    }
    pub fn invalidate_status(&self) {
        self.status.send_modify(|snapshot| {
            if let Some(snapshot) = snapshot {
                snapshot.observed_at_unix = None;
                snapshot.health = Health::Unknown;
                snapshot.epoch_state = EpochState::Unknown;
                snapshot.balance_wei = None;
                snapshot.authorized = None;
            }
        });
    }
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}
fn enqueue(tx: &mpsc::Sender<Message>, value: Message, dropped: &AtomicU64) {
    if tx.try_send(value).is_err() {
        increment(dropped);
    }
}
fn amount(wei: U256) -> String {
    let unit = U256::from(1_000_000_000_000_000_000u64);
    format!("{}.{:018}", wei / unit, wei % unit)
}
fn wallet_text(s: &StatusSnapshot, display: &DisplayMetadata) -> String {
    let balance = s
        .balance_wei
        .map(amount)
        .unwrap_or_else(|| "unknown".into());
    let auth = match s.authorized {
        Some(true) => "authorized",
        Some(false) => "not authorized",
        None => "unknown",
    };
    let explorer = display
        .explorer
        .as_ref()
        .map(|base| {
            format!(
                "\n{}/address/{}",
                base.as_str().trim_end_matches('/'),
                s.transaction_wallet
            )
        })
        .unwrap_or_default();
    let role = if display.follower {
        "Backup committer"
    } else {
        "Epoch committer"
    };
    format!(
        "Keeper transaction wallet\n{}\nChain: {}\nNative balance (18 decimals): {} {}\n{role}: {}{}",
        s.transaction_wallet, s.chain_id, balance, display.symbol, auth, explorer
    )
}
fn command_text(
    command: Command,
    snapshot: Option<&StatusSnapshot>,
    uptime: u64,
    dropped: u64,
    now: u64,
    display: &DisplayMetadata,
) -> String {
    let Some(s) = snapshot else {
        return "Keeper status unavailable; public observations have not arrived yet.".into();
    };
    let age = s.observed_at_unix.and_then(|at| now.checked_sub(at));
    let mut visible = s.clone();
    if age.is_none_or(|age| age > 120) {
        visible.health = Health::Unknown;
        visible.epoch_state = EpochState::Unknown;
        visible.balance_wei = None;
        visible.authorized = None;
        visible.primary_alive = None;
    }
    let s = &visible;
    let freshness = match age {
        Some(age) if age <= 120 => format!("{age}s ago"),
        Some(age) => format!("{age}s ago (stale)"),
        None => "unknown (stale)".into(),
    };
    let role = if display.follower {
        "follower"
    } else {
        "primary"
    };
    // A follower reports whether it currently counts the primary as alive, which sets its request delay.
    let primary = if display.follower {
        match s.primary_alive {
            Some(true) => "Primary alive: yes (committer nonce advanced recently)\n",
            Some(false) => "Primary alive: no (committer nonce idle)\n",
            None => "Primary alive: unknown\n",
        }
    } else {
        ""
    };
    let content = match command {
        Command::Keeper => wallet_text(s, display),
        Command::Status => format!(
            "Keeper status ({role})\nUptime: {uptime}s\nHealth: {:?}\n{primary}Pending: {}\nServed: {}\nLast observed served receipt (Unix): {}\nEpoch: {} / {:?}\nDropped notifications: {dropped}\n{}",
            s.health,
            s.pending,
            s.served,
            s.last_serve_unix
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unknown".into()),
            s.epoch_id
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unknown".into()),
            s.epoch_state,
            wallet_text(s, display)
        ),
    };
    format!("Observation: {freshness}\n{content}")
}
fn event_text(
    event: Event,
    cooldowns: &mut HashMap<u8, Instant>,
    now: Instant,
    display: &DisplayMetadata,
) -> Option<String> {
    let key = match &event {
        Event::Fulfilled { .. } => None,
        Event::OperationalError { class } => Some(*class as u8),
        Event::FeeBudget(_) => Some(ErrorClass::FeeBudget as u8),
        Event::LowBalance { .. } => Some(255),
        Event::Sweep(_)
        | Event::EpochPublished { .. }
        | Event::Authorization(_)
        | Event::Upgrade(_) => None,
    };
    if let Some(key) = key {
        if cooldowns
            .get(&key)
            .is_some_and(|last| now.duration_since(*last) < COOLDOWN)
        {
            return None;
        }
        cooldowns.insert(key, now);
    }
    let text = match event {
        Event::Fulfilled {
            request_id,
            tx_hash,
        } if display.follower => format!(
            "Follower served request {request_id}: the primary had not served it in time\nTransaction: {tx_hash}"
        ),
        Event::Fulfilled {
            request_id,
            tx_hash,
        } => format!("Randomness served: request {request_id}\nTransaction: {tx_hash}"),
        Event::EpochPublished { epoch, tx_hash } => format!(
            "Follower published epoch {epoch}: the primary had not published it in time\nTransaction: {tx_hash}"
        ),
        Event::Authorization(text) => text.clone(),
        Event::OperationalError { class } => format!("Keeper needs attention: {class:?}"),
        Event::FeeBudget(exceeded) => format!(
            "Keeper needs attention: FeeBudget\nSends are deferred: {exceeded}.\nGas price rule: 2 x base fee + 1 gwei per gas. Raise {} in the keeper configuration and restart; no cap is bypassed to meet a deadline.",
            exceeded.cap.variable()
        ),
        Event::LowBalance {
            wallet,
            balance_wei,
        } => format!(
            "Keeper funding needed\n{wallet}\nNative balance (18 decimals): {} {}\nFund the transaction wallet to continue paying gas.",
            amount(balance_wei),
            display.symbol
        ),
        Event::Sweep(text) | Event::Upgrade(text) => text,
    };
    // Takeover notices already name the follower; its other notices carry the role as a prefix.
    Some(if display.follower && !text.starts_with("Follower ") {
        format!("[follower] {text}")
    } else {
        text
    })
}
async fn send_loop(
    client: Client,
    endpoint: String,
    destination: Destination,
    mut rx: mpsc::Receiver<Message>,
    status: watch::Receiver<Option<StatusSnapshot>>,
    dropped: Arc<AtomicU64>,
    started: Instant,
) {
    let Destination {
        chat_id: chat,
        display,
    } = destination;
    let mut next = Instant::now();
    let mut cooldowns = HashMap::new();
    while let Some(first) = rx.recv().await {
        tokio::time::sleep_until(next).await;
        let mut batch = vec![first];
        for _ in 1..16 {
            match rx.try_recv() {
                Ok(v) => batch.push(v),
                Err(_) => break,
            }
        }
        let snapshot = status.borrow().clone();
        let mut text = String::new();
        for message in batch {
            let line = match message {
                Message::Event(e) => event_text(e, &mut cooldowns, Instant::now(), &display),
                Message::Command(c) => Some(command_text(
                    c,
                    snapshot.as_ref(),
                    started.elapsed().as_secs(),
                    dropped.load(Ordering::Relaxed),
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|v| v.as_secs())
                        .unwrap_or(0),
                    &display,
                )),
            };
            if let Some(line) = line {
                if text.len() + line.len() + 2 > TEXT_LIMIT {
                    increment(&dropped);
                    continue;
                }
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&line);
            }
        }
        if text.is_empty() {
            continue;
        }
        let delay = send_text(&client, &endpoint, chat, &text).await;
        next = Instant::now() + delay;
    }
}
async fn send_text(client: &Client, endpoint: &str, chat: i64, text: &str) -> Duration {
    // Do not format/log errors: reqwest errors can contain the credential-bearing URL.
    let result = client.post(endpoint).json(&serde_json::json!({"chat_id":chat,"text":text,"link_preview_options":{"is_disabled":true}})).send().await;
    if result.as_ref().is_ok_and(|r| r.status().as_u16() == 429) {
        Duration::from_secs(30)
    } else {
        Duration::from_secs(1)
    }
}
#[derive(Deserialize)]
struct Updates {
    ok: bool,
    #[serde(default)]
    result: Vec<Update>,
}
#[derive(Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Incoming>,
}
#[derive(Deserialize)]
struct Incoming {
    date: u64,
    chat: Chat,
    text: Option<String>,
}
#[derive(Deserialize)]
struct Chat {
    id: i64,
}
#[derive(Deserialize)]
struct Identity {
    ok: bool,
    result: Option<Bot>,
}
#[derive(Deserialize)]
struct Bot {
    username: Option<String>,
}
fn parse_command(
    message: &Incoming,
    chat: i64,
    since: u64,
    username: Option<&str>,
) -> Option<Command> {
    if message.chat.id != chat || message.date < since {
        return None;
    }
    let text = message.text.as_deref()?;
    if text.len() > 80 {
        return None;
    }
    let (name, suffix) = text
        .split_once('@')
        .map_or((text, None), |(a, b)| (a, Some(b)));
    if suffix.is_some_and(|s| Some(s) != username) {
        return None;
    }
    match name {
        "/status" => Some(Command::Status),
        "/keeper" => Some(Command::Keeper),
        _ => None,
    }
}
fn dispatch_message(
    message: &Incoming,
    chat: i64,
    since: u64,
    username: Option<&str>,
    tx: &mpsc::Sender<Message>,
    dropped: &AtomicU64,
) {
    if let Some(command) = parse_command(message, chat, since, username) {
        enqueue(tx, Message::Command(command), dropped);
    }
}
async fn bounded_json<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
) -> Option<T> {
    if !response.status().is_success() {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len() + chunk.len() > BODY_LIMIT {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).ok()
}
async fn poll_loop(
    client: Client,
    base: String,
    chat: i64,
    tx: mpsc::Sender<Message>,
    dropped: Arc<AtomicU64>,
) {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut offset: i64 = -1; // Bootstrap at the newest update; discard historical command backlog.
    let mut username = None;
    loop {
        if username.is_none()
            && let Ok(r) = client.post(format!("{base}/getMe")).send().await
            && let Some(identity) = bounded_json::<Identity>(r).await.filter(|i| i.ok)
        {
            username = identity.result.and_then(|b| b.username);
        }
        let request = client.post(format!("{base}/getUpdates")).timeout(Duration::from_secs(15)).json(&serde_json::json!({"offset":offset,"timeout":10,"limit":20,"allowed_updates":["message"]}));
        let response = request.send().await;
        let updates = match response {
            Ok(r) => bounded_json::<Updates>(r).await.filter(|u| u.ok),
            Err(_) => None,
        };
        let Some(updates) = updates else {
            tokio::time::sleep(Duration::from_secs(30)).await;
            continue;
        };
        for update in updates.result.into_iter().take(20) {
            offset = offset.max(update.update_id.saturating_add(1));
            if let Some(message) = update.message.as_ref() {
                dispatch_message(message, chat, since, username.as_deref(), &tx, &dropped);
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn display_metadata_is_chain_neutral_and_rejects_secret_bearing_urls() {
        let default = DisplayMetadata::parse(None, None).unwrap();
        assert_eq!(default.symbol, "native");
        assert!(default.explorer.is_none());
        assert!(
            DisplayMetadata::parse(Some("GAS".into()), Some("https://explorer.example".into()))
                .is_ok()
        );
        for url in [
            "http://explorer.example",
            "https://secret@example.com",
            "https://example.com?key=secret",
            "https://example.com#secret",
        ] {
            let error = DisplayMetadata::parse(None, Some(url.into()))
                .err()
                .unwrap();
            assert!(!error.to_string().contains("secret"));
        }
        for symbol in ["", "TOKEN\ncommand", "https://example.com"] {
            assert!(DisplayMetadata::parse(Some(symbol.into()), None).is_err());
        }
    }

    #[test]
    fn settings_are_opt_in_and_errors_never_reveal_values() {
        assert!(Settings::parse(None, None, None).unwrap().is_none());
        let error = Settings::parse(Some("SECRET_TOKEN".into()), None, None)
            .err()
            .unwrap();
        assert!(!error.to_string().contains("SECRET_TOKEN"));
        assert!(
            Settings::parse(Some("123:test_token".into()), Some("-123".into()), None)
                .unwrap()
                .is_some()
        );
        assert!(
            Settings::parse(
                Some("123:test_token/redirect".into()),
                Some("1".into()),
                None
            )
            .is_err()
        );
        assert!(Settings::parse(Some("123:test".into()), Some("0".into()), None).is_err());
        assert!(
            Settings::parse(Some("123:test".into()), Some("1".into()), Some("-1".into())).is_err()
        );
    }
    #[test]
    fn queue_overflow_is_immediate_and_counted() {
        let (tx, _rx) = mpsc::channel(CAPACITY);
        let count = AtomicU64::new(0);
        for _ in 0..CAPACITY + 7 {
            enqueue(
                &tx,
                Message::Event(Event::OperationalError {
                    class: ErrorClass::KeeperTick,
                }),
                &count,
            );
        }
        assert_eq!(count.load(Ordering::Relaxed), 7);
        count.store(u64::MAX, Ordering::Relaxed);
        increment(&count);
        assert_eq!(count.load(Ordering::Relaxed), u64::MAX);
    }
    #[test]
    fn repeated_errors_and_low_balance_have_bounded_cooldowns() {
        let mut cooldowns = HashMap::new();
        let now = Instant::now();
        let event = Event::OperationalError {
            class: ErrorClass::RpcUnavailable,
        };
        let display = DisplayMetadata::default();
        assert!(event_text(event.clone(), &mut cooldowns, now, &display).is_some());
        assert!(
            event_text(
                event.clone(),
                &mut cooldowns,
                now + Duration::from_secs(299),
                &display
            )
            .is_none()
        );
        assert!(event_text(event, &mut cooldowns, now + COOLDOWN, &display).is_some());
        let low = Event::LowBalance {
            wallet: Address::ZERO,
            balance_wei: U256::ZERO,
        };
        assert!(event_text(low.clone(), &mut cooldowns, now, &display).is_some());
        assert!(event_text(low, &mut cooldowns, now + Duration::from_secs(1), &display).is_none());
        assert_eq!(cooldowns.len(), 2);
    }
    #[test]
    fn an_approved_upgrade_notice_is_informational_and_never_held_back() {
        let mut cooldowns = HashMap::new();
        let now = Instant::now();
        let notice =
            || Event::Upgrade("Keeper restarted on the approved coordinator implementation".into());
        let display = DisplayMetadata::default();
        for _ in 0..2 {
            assert_eq!(
                event_text(notice(), &mut cooldowns, now, &display).as_deref(),
                Some("Keeper restarted on the approved coordinator implementation")
            );
        }
        assert!(cooldowns.is_empty(), "not an error class and no cooldown");
        let follower = DisplayMetadata {
            follower: true,
            ..DisplayMetadata::default()
        };
        assert!(
            event_text(notice(), &mut cooldowns, now, &follower)
                .unwrap()
                .starts_with("[follower] Keeper restarted")
        );
    }
    #[test]
    fn fee_budget_names_the_cap_and_shares_its_class_cooldown() {
        use crate::config::{FeeBudget, FeeCap};
        let mut cooldowns = HashMap::new();
        let now = Instant::now();
        let display = DisplayMetadata::default();
        let exceeded = FeeBudget {
            cap: FeeCap::MaxFeePerGas,
            required: 503_000_000_000,
            limit: 100_000_000_000,
        };
        let text = event_text(Event::FeeBudget(exceeded), &mut cooldowns, now, &display).unwrap();
        assert!(text.starts_with("Keeper needs attention: FeeBudget"));
        assert!(text.contains("required 503000000000 exceeds MAX_FEE_PER_GAS_WEI=100000000000"));
        assert!(text.contains("Raise MAX_FEE_PER_GAS_WEI"));
        assert!(text.contains("2 x base fee + 1 gwei"));
        assert!(text.len() < TEXT_LIMIT);
        let other = FeeBudget {
            cap: FeeCap::MaxTxCost,
            ..exceeded
        };
        assert!(
            event_text(
                Event::FeeBudget(other),
                &mut cooldowns,
                now + Duration::from_secs(299),
                &display
            )
            .is_none()
        );
        assert!(
            event_text(
                Event::OperationalError {
                    class: ErrorClass::FeeBudget
                },
                &mut cooldowns,
                now + Duration::from_secs(299),
                &display
            )
            .is_none()
        );
        assert!(
            event_text(
                Event::FeeBudget(other),
                &mut cooldowns,
                now + COOLDOWN,
                &display
            )
            .unwrap()
            .contains("Raise MAX_TX_COST_WEI")
        );
        assert_eq!(cooldowns.len(), 1);
    }
    #[test]
    fn commands_require_exact_text_chat_and_current_message() {
        let make = |id, date, text: &str| Incoming {
            chat: Chat { id },
            date,
            text: Some(text.into()),
        };
        assert!(matches!(
            parse_command(&make(42, 100, "/status"), 42, 100, Some("my_bot")),
            Some(Command::Status)
        ));
        assert!(matches!(
            parse_command(&make(42, 100, "/keeper@my_bot"), 42, 100, Some("my_bot")),
            Some(Command::Keeper)
        ));
        for message in [
            make(41, 100, "/status"),
            make(42, 99, "/status"),
            make(42, 100, "/status extra"),
            make(42, 100, "/status@other"),
            make(42, 100, "/withdraw"),
        ] {
            assert!(parse_command(&message, 42, 100, Some("my_bot")).is_none());
        }
    }
    #[test]
    fn unauthorized_mock_update_never_enqueues_even_a_rejection() {
        let updates: Updates = serde_json::from_str(r#"{"ok":true,"result":[{"update_id":5,"message":{"date":100,"chat":{"id":999},"text":"/keeper"}}]}"#).unwrap();
        let (tx, mut rx) = mpsc::channel(1);
        let dropped = AtomicU64::new(0);
        for update in updates.result {
            dispatch_message(
                &update.message.unwrap(),
                42,
                100,
                Some("my_bot"),
                &tx,
                &dropped,
            );
        }
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        dispatch_message(
            &Incoming {
                date: 100,
                chat: Chat { id: 42 },
                text: Some("/keeper".into()),
            },
            42,
            100,
            Some("my_bot"),
            &tx,
            &dropped,
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(Message::Command(Command::Keeper))
        ));
    }
    #[test]
    fn snapshots_expose_only_typed_public_information() {
        let snapshot = StatusSnapshot {
            observed_at_unix: Some(100),
            health: Health::Healthy,
            pending: 2,
            served: 3,
            last_serve_unix: Some(100),
            epoch_id: Some(5),
            epoch_state: EpochState::Local,
            transaction_wallet: Address::ZERO,
            chain_id: 31337,
            balance_wei: Some(U256::from(50_000_000_000_000_000u64)),
            authorized: Some(true),
            primary_alive: None,
        };
        let display = DisplayMetadata::parse(
            Some("TOK".into()),
            Some("https://explorer.example/base".into()),
        )
        .unwrap();
        let text = command_text(Command::Status, Some(&snapshot), 60, 4, 110, &display);
        assert!(text.contains("0.050000000000000000 TOK"));
        let generic = wallet_text(&snapshot, &DisplayMetadata::default());
        assert!(generic.contains("0.050000000000000000 native"));
        assert!(!generic.contains("https://"));
        assert!(text.contains("Pending: 2"));
        assert!(text.contains("Epoch: 5 / Local"));
        assert!(text.contains("explorer.example/base/address/"));
        assert!(text.len() < TEXT_LIMIT);
        assert!(text.contains("Observation: 10s ago"));
        let stale = command_text(Command::Status, Some(&snapshot), 600, 4, 221, &display);
        assert!(stale.contains("121s ago (stale)"));
        assert!(stale.contains("Health: Unknown"));
        assert!(stale.contains("Epoch committer: unknown"));
        assert!(!stale.contains("0.050000000000000000"));
        let stale_wallet = command_text(Command::Keeper, Some(&snapshot), 600, 4, 221, &display);
        assert!(stale_wallet.contains("121s ago (stale)"));
        assert!(stale_wallet.contains("Epoch committer: unknown"));
        let future = command_text(Command::Status, Some(&snapshot), 60, 4, 99, &display);
        assert!(future.contains("unknown (stale)"));
        assert!(!text.contains("Primary alive"));
        // A follower's status says whether it counts the primary as alive.
        let follower = DisplayMetadata {
            follower: true,
            ..display.clone()
        };
        let alive = StatusSnapshot {
            primary_alive: Some(true),
            ..snapshot.clone()
        };
        assert!(
            command_text(Command::Status, Some(&alive), 60, 4, 110, &follower)
                .contains("Primary alive: yes")
        );
        let idle = StatusSnapshot {
            primary_alive: Some(false),
            ..snapshot.clone()
        };
        assert!(
            command_text(Command::Status, Some(&idle), 60, 4, 110, &follower)
                .contains("Primary alive: no")
        );
        assert!(
            command_text(Command::Status, Some(&idle), 600, 4, 221, &follower)
                .contains("Primary alive: unknown")
        );
    }
    #[tokio::test]
    async fn stalled_delivery_does_not_block_producers() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/sendMessage", listener.local_addr().unwrap());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut buffer = [0; 4096];
            let _ = socket.read(&mut buffer).unwrap();
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        });
        let client = Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let (tx, rx) = mpsc::channel(CAPACITY);
        let (_status, snapshot) = watch::channel(None);
        let dropped = Arc::new(AtomicU64::new(0));
        let actor = tokio::spawn(send_loop(
            client,
            endpoint,
            Destination {
                chat_id: 42,
                display: DisplayMetadata::default(),
            },
            rx,
            snapshot,
            dropped.clone(),
            Instant::now(),
        ));
        enqueue(&tx, Message::Command(Command::Status), &dropped);
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        // HTTP is still stalled. Producers fill a bounded queue without waiting for it.
        for _ in 0..CAPACITY + 1 {
            enqueue(&tx, Message::Command(Command::Keeper), &dropped);
        }
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        release_tx.send(()).unwrap();
        server.join().unwrap();
        actor.abort();
    }
    #[tokio::test]
    async fn local_http_send_is_json_and_429_only_defers_background() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/sendMessage", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut data = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let n = stream.read(&mut buffer).unwrap();
                data.extend_from_slice(&buffer[..n]);
                if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data[..at]);
                    let len: usize = headers
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap();
                    if data.len() >= at + 4 + len {
                        break;
                    }
                }
            }
            stream.write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            String::from_utf8(data).unwrap()
        });
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        assert_eq!(
            send_text(&client, &endpoint, 42, "Randomness served").await,
            Duration::from_secs(30)
        );
        let request = server.join().unwrap();
        assert!(request.contains("\"chat_id\":42"));
        assert!(request.contains("Randomness served"));
    }
}
