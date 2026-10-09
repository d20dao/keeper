//! A keeper process against the scripted chain: the real `Worker` started on a fresh journal in a temporary directory,
//! configured by `Config::load` from an environment file's worth of settings, with keys and a drand relay of its own. A
//! run is what the integration harness calls one run of the binary: a fresh `Worker` (startup) and one tick, after
//! which the node is asked what the keeper asked it and the journal is read for what it left.
//!
//! The rig is written against what every release of the keeper has: `Config::load`, `Worker::new` and `Worker::tick`.
//! It runs unchanged on the tree of keeper 0.4.1, where the golden traces are recorded (`tests/golden/record.sh`).
use crate::{
    config::Config,
    prover,
    scripted::{self, Chain, Node, render},
    worker::Worker,
};
use alloy_primitives::{Address, B256, keccak256};
use alloy_signer_local::PrivateKeySigner;
use sqlx::SqlitePool;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::PathBuf,
    sync::{Mutex, MutexGuard, OnceLock},
};

/// What a test changes in the configuration the loader built, for a setting that is not an environment variable of the
/// release under test. Nothing is changed by default, which is the keeper as an operator configures it.
pub trait Tweak: Send + Sync + 'static {
    fn apply(&self, config: &mut Config);
    /// Whether the change is a configuration that the chain policy of the rig's network would refuse at startup. The
    /// loader then reads the environment as the local test chain's, which accepts any, and the network is set after.
    fn outside_chain_policy(&self) -> bool {
        false
    }
}
impl Tweak for () {
    fn apply(&self, _: &mut Config) {}
}

/// The network that accepts every combination of the chain switches.
const LOCAL_TEST_CHAIN: u64 = 31337;
/// Serializes the tests that give the loader an environment: the process has one.
static ENVIRONMENT: Mutex<()> = Mutex::new(());
/// Every environment variable `Config::load` and `telemetry::Settings::load` read, found as the quoted names in their
/// production source: the rig clears them before it loads, so an operator's own settings never leak into a test, and
/// a setting added later is cleared too.
fn loader_names() -> &'static BTreeSet<String> {
    static NAMES: OnceLock<BTreeSet<String>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names = BTreeSet::new();
        for source in [include_str!("config.rs"), include_str!("telemetry.rs")] {
            let production = source.split("#[cfg(test)]").next().unwrap_or(source);
            for quoted in production.split('"').skip(1).step_by(2) {
                if quoted.len() > 3
                    && quoted.starts_with(|c: char| c.is_ascii_uppercase())
                    && quoted
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                {
                    names.insert(quoted.to_owned());
                }
            }
        }
        names
    })
}
/// The process environment as an operator's file would set it, for as long as this value lives; the previous values
/// come back when it is dropped.
struct Environment {
    saved: Vec<(OsString, Option<OsString>)>,
    _exclusive: MutexGuard<'static, ()>,
}
impl Environment {
    fn enter(settings: &BTreeMap<String, String>) -> Self {
        let exclusive = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
        let names: BTreeSet<&str> = loader_names()
            .iter()
            .map(String::as_str)
            .chain(settings.keys().map(String::as_str))
            .collect();
        let mut saved = Vec::new();
        for name in names {
            saved.push((name.into(), std::env::var_os(name)));
            // SAFETY: only tests that hold `ENVIRONMENT` change these names, and nothing else reads them.
            unsafe { std::env::remove_var(name) };
        }
        for (name, value) in settings {
            // SAFETY: as above.
            unsafe { std::env::set_var(name, value) };
        }
        Self {
            saved,
            _exclusive: exclusive,
        }
    }
}
impl Drop for Environment {
    fn drop(&mut self) {
        for (name, value) in self.saved.drain(..) {
            // SAFETY: still holding `ENVIRONMENT`.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(&name, value),
                    None => std::env::remove_var(&name),
                }
            }
        }
    }
}
/// `Config::load` of an environment holding exactly `settings`, as the binary would build it for `keeper run --once`.
pub fn load(settings: &BTreeMap<String, String>) -> anyhow::Result<Config> {
    let _environment = Environment::enter(settings);
    Config::load(true)
}

/// What the keeper logs at `level` or above while this value lives, on the thread that holds it: a test on its own
/// runtime reads exactly what its keeper logged.
pub struct Logs {
    text: std::sync::Arc<Mutex<Vec<u8>>>,
    _installed: tracing::subscriber::DefaultGuard,
}
#[derive(Clone)]
struct Sink(std::sync::Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Logs {
    pub fn capture(level: tracing::Level) -> Self {
        let text = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = Sink(text.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(level)
            .with_writer(move || sink.clone())
            .finish();
        Self {
            text,
            _installed: tracing::subscriber::set_default(subscriber),
        }
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.text.lock().unwrap()).into_owned()
    }
}

/// Whether the journal has this table: a round coordinator's has none of the epoch lane's.
async fn has_table(pool: &SqlitePool, name: &str) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
        > 0
}

pub struct Rig {
    dir: tempfile::TempDir,
    pub node: Node,
    /// The endpoints of the keeper's `RPC_URLS`, and the drand relays of its `DRAND_RELAYS`: the node's, and any added.
    endpoints: Vec<scripted::Endpoint>,
    relays: Vec<scripted::Endpoint>,
    tx_key: PathBuf,
    vrf_key: PathBuf,
    chain_id: u64,
    tweak: Box<dyn Tweak>,
    /// Settings of the keeper's environment beyond the ones the rig must give it, as an operator's file lists them.
    environment: BTreeMap<String, String>,
    /// The health reporting of the keeper, which only `Settings::load` on the local test chain can build.
    telemetry: Option<crate::telemetry::Settings>,
    /// How long the node holds its answers during a tick; see `scripted::hold`.
    tick_hold: std::time::Duration,
}
/// One keeper process: what it asked while starting (when the run was asked to keep it), what it asked during its
/// tick, and the journal it left.
pub struct Run {
    pub startup: Option<Vec<String>>,
    pub tick: Vec<String>,
    pub journal: String,
}
/// The lines of a tick without the re-checks that its start makes when their wall-clock intervals have passed: the
/// runtime pins every 15 seconds and the wallet's authorization every 2 or 30. A process that ticks quickly is not due
/// for them, one that ticks slowly is, and what a trace records must not depend on how long a tick took, so a process's
/// ticks start at the head they read, which is the first thing every tick does after those checks. A run, whose keeper
/// is new, is never due for them and keeps its lines.
fn unscheduled(mut tick: Vec<String>) -> Vec<String> {
    let first = tick.windows(2).position(|pair| {
        pair[0].ends_with("batch [")
            && pair[1]
                .trim_start()
                .starts_with("eth_getBlockByNumber finalized")
    });
    if let Some(first) = first {
        tick.drain(..first);
    }
    tick
}
/// A keeper process that stays up across ticks.
pub struct Process<'a> {
    rig: &'a Rig,
    worker: Worker,
    config: Config,
    /// What it asked while it started.
    pub startup: Vec<String>,
}
impl Process<'_> {
    /// From now on the Telegram events of this process wait for the test to read them.
    pub fn told(&mut self) -> crate::telegram::Captured {
        let (notifier, captured) = crate::telegram::TelegramNotifier::capture();
        self.worker.set_telegram(notifier);
        captured
    }
    /// One tick, what it asked during it, and the journal it left. An epoch fetch it started ends within the tick. The
    /// blocks the script mined since the last tick are the seconds that passed, so the backoffs the journal keeps in
    /// wall-clock time have ended, as they do between two runs.
    pub async fn tick(&self) -> anyhow::Result<Run> {
        self.rig.advance_clock().await;
        self.rig.node.take();
        let outcome = self.worker.tick().await;
        self.worker.stop_epoch_fetch().await.unwrap();
        let tick = unscheduled(render(&self.rig.node.take()));
        outcome?;
        Ok(Run {
            startup: None,
            tick,
            journal: self.rig.digest().await,
        })
    }
    /// The keeper this process is, for a test that asks it something a tick does not.
    pub fn worker(&self) -> &Worker {
        &self.worker
    }
    /// The health reporter the binary starts beside its ticks, when the configuration has one.
    pub fn telemetry(&self) -> Option<crate::telemetry::Task> {
        crate::telemetry::spawn(&self.config, &self.worker.journal)
    }
    /// The process ends: its journal closes.
    pub async fn stop(self) {
        self.worker.journal.pool.close().await;
    }
}
impl Rig {
    pub async fn new(chain_id: u64, tweak: impl Tweak) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (tx_key, vrf_key) = (dir.path().join("tx.key"), dir.path().join("vrf.key"));
        let (tx_secret, vrf_secret) = (B256::repeat_byte(0x24), B256::repeat_byte(0x42));
        std::fs::write(&tx_key, hex::encode(tx_secret)).unwrap();
        std::fs::write(&vrf_key, hex::encode(vrf_secret)).unwrap();
        // The keeper refuses a key file that group or others can read; the default umask leaves both readable on Unix.
        #[cfg(unix)]
        for key in [&tx_key, &vrf_key] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let keeper = PrivateKeySigner::from_bytes(&tx_secret).unwrap().address();
        let public_key = prover::public_key(&prover::read_key(&vrf_key).unwrap());
        let node = Node::start(Chain::new(chain_id, keeper, public_key)).await;
        let relay = node.add_relay().await;
        let endpoints = vec![node.endpoint().clone()];
        Self {
            dir,
            node,
            endpoints,
            relays: vec![relay],
            tx_key,
            vrf_key,
            chain_id,
            tweak: Box::new(tweak),
            environment: BTreeMap::new(),
            telemetry: None,
            tick_hold: scripted::hold(),
        }
    }
    /// A setting of the keeper's environment, as an operator's file lists it. It replaces what the rig would give.
    pub fn setting(mut self, name: &str, value: &str) -> Self {
        self.environment.insert(name.into(), value.into());
        self
    }
    /// Settings of the keeper's environment, in an operator's file.
    pub fn settings(mut self, pairs: &[(&str, &str)]) -> Self {
        for (name, value) in pairs {
            self.environment.insert((*name).into(), (*value).into());
        }
        self
    }
    /// A follower: it runs as an allowed backup committer beside a primary, which is the registry's committer.
    pub fn follower(self) -> Self {
        self.node.with(|chain| {
            chain.committer = Address::repeat_byte(0x91);
            chain.backups.insert(chain.keeper);
        });
        self.setting("KEEPER_ROLE", "follower")
    }
    /// The keeper reports its health to `url`, a loopback endpoint, with the key and interval of a local test.
    pub fn reporting_to(mut self, url: &str) -> Self {
        let settings = BTreeMap::from([
            ("HEALTH_API_URL".to_owned(), url.to_owned()),
            ("HEALTH_API_KEY".to_owned(), "local-test-token".to_owned()),
            ("HEALTH_INTERVAL_SECONDS".to_owned(), "1".to_owned()),
        ]);
        let _environment = Environment::enter(&settings);
        self.telemetry = crate::telemetry::Settings::load(LOCAL_TEST_CHAIN).unwrap();
        self
    }
    /// Another HTTP endpoint for the keeper's `RPC_URLS`, after the ones it has.
    pub async fn add_endpoint(&mut self) -> scripted::Endpoint {
        let endpoint = self.node.add_endpoint().await;
        self.endpoints.push(endpoint.clone());
        endpoint
    }
    /// The endpoints of the keeper's `RPC_URLS`, in order.
    pub fn endpoints(&self) -> &[scripted::Endpoint] {
        &self.endpoints
    }
    /// Another drand relay for the keeper's `DRAND_RELAYS`, after the ones it has.
    pub async fn add_relay(&mut self) -> scripted::Endpoint {
        let relay = self.node.add_relay().await;
        self.relays.push(relay.clone());
        relay
    }
    /// The drand relays of the keeper's `DRAND_RELAYS`, in order.
    pub fn relays(&self) -> &[scripted::Endpoint] {
        &self.relays
    }

    /// A rig whose node does not hold its answers, for a test that does not read the order of the calls: its runs are
    /// several times faster.
    pub fn unheld(mut self) -> Self {
        self.tick_hold = std::time::Duration::ZERO;
        self
    }
    fn db(&self) -> PathBuf {
        self.dir.path().join("keeper.sqlite")
    }
    /// The environment of the rig's keeper: what the loader requires of a keeper on a network that is not the local
    /// test chain, and every other setting at the default `Config::load` gives it.
    fn environment(&self, send: bool) -> BTreeMap<String, String> {
        let chain = self.node.chain.lock().unwrap();
        let loaded_as = if self.tweak.outside_chain_policy() {
            LOCAL_TEST_CHAIN
        } else {
            self.chain_id
        };
        let hash = |hash: B256| hash.to_string();
        let path = |path: &PathBuf| path.to_string_lossy().into_owned();
        let mut environment: BTreeMap<String, String> = [
            ("CHAIN_ID", loaded_as.to_string()),
            // The loader requires HTTPS; the rig points the endpoint at its node afterwards.
            ("RPC_URLS", "https://rpc.invalid".into()),
            ("COORDINATOR_ADDRESS", chain.coordinator.to_string()),
            ("KEEPER_DB", path(&self.db())),
            ("TX_KEY_FILE", path(&self.tx_key)),
            ("VRF_KEY_FILE", path(&self.vrf_key)),
            ("SEND_TRANSACTIONS", send.to_string()),
            ("CANCEL_MAX_FEE_PER_GAS_WEI", "150000000000".into()),
            ("EXPECTED_CODE_HASH", hash(keccak256(&chain.proxy_code))),
            ("EXPECTED_PROTOCOL_HASH", hash(chain.protocol_hash)),
            (
                "EXPECTED_IMPLEMENTATION_CODE_HASH",
                hash(chain.implementation_hash(chain.coordinator_implementation)),
            ),
            (
                "EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH",
                hash(chain.implementation_hash(chain.registry_implementation)),
            ),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect();
        environment.extend(self.environment.clone());
        environment
    }
    /// The configuration of the rig's keeper: `Config::load` of its environment, with the endpoint, the lock
    /// directory and the drand relay pointed at the rig, whose node speaks HTTP.
    pub fn config(&self, send: bool) -> Config {
        let mut config = load(&self.environment(send))
            .unwrap_or_else(|error| panic!("the rig's environment does not load: {error:#}"));
        config.chain_id = self.chain_id;
        config.rpc_urls = self.endpoints.iter().map(|e| e.url.clone()).collect();
        config.lock_dir = self.dir.path().join("locks");
        let relays: Vec<&str> = self.relays.iter().map(|e| e.url.as_str()).collect();
        config.drand_relays =
            crate::beacon::DrandRelays::parse(Some(&relays.join(",")), true).unwrap();
        if self.telemetry.is_some() {
            config.telemetry = self.telemetry.clone();
        }
        self.tweak.apply(&mut config);
        config
    }
    pub async fn journal(&self) -> SqlitePool {
        crate::sweep::open_existing(&self.db()).await.unwrap()
    }
    /// The seconds that pass between two keeper processes: backoffs that the journal keeps in wall-clock time end.
    async fn advance_clock(&self) {
        if !self.db().exists() {
            return;
        }
        let pool = self.journal().await;
        sqlx::query("UPDATE meta SET value='0' WHERE key LIKE 'prepare_retry_ms:%' OR key LIKE 'preflight_retry:%'")
            .execute(&pool)
            .await
            .unwrap();
        // A round coordinator's journal has no epoch work.
        if has_table(&pool, "epoch_work").await {
            sqlx::query("UPDATE epoch_work SET retry_at=0")
                .execute(&pool)
                .await
                .unwrap();
        }
        pool.close().await;
    }
    /// A keeper process that starts and stops without a tick.
    pub async fn start(&self) -> Vec<String> {
        self.advance_clock().await;
        self.node.take();
        let worker = Worker::new(self.config(false)).await.unwrap();
        let startup = render(&self.node.take());
        worker.journal.pool.close().await;
        startup
    }
    /// A keeper process: its startup and one tick. The startup is read only when `startup` says so: its answers are
    /// held to tell concurrent calls from dependent ones, which costs time that a run need not spend on a startup
    /// that an earlier run has recorded.
    pub async fn run(&self, send: bool, startup: bool) -> Run {
        self.run_with(send, startup, |_| {}).await.unwrap()
    }
    /// `run` with the configuration adjusted by the test, and with the error of the tick returned when it fails.
    pub async fn run_with(
        &self,
        send: bool,
        startup: bool,
        adjust: impl FnOnce(&mut Config),
    ) -> anyhow::Result<Run> {
        self.advance_clock().await;
        self.node.take();
        if !startup {
            self.node.hold(std::time::Duration::ZERO);
        }
        let mut config = self.config(send);
        adjust(&mut config);
        let worker = Worker::new(config).await.unwrap();
        let started = render(&self.node.take());
        self.node.hold(self.tick_hold);
        let outcome = worker.tick().await;
        worker.stop_epoch_fetch().await.unwrap();
        let tick = render(&self.node.take());
        worker.journal.pool.close().await;
        drop(worker);
        outcome?;
        Ok(Run {
            startup: startup.then_some(started),
            tick,
            journal: self.digest().await,
        })
    }
    /// A keeper process that stays up across ticks, as the binary does: what it has observed of a primary, and the
    /// epoch it is preparing, stay in memory from one tick to the next. Its startup is read only when `startup` says so,
    /// as for a run.
    pub async fn process(&self, send: bool, startup: bool) -> Process<'_> {
        self.advance_clock().await;
        self.node.take();
        if !startup {
            self.node.hold(std::time::Duration::ZERO);
        }
        let config = self.config(send);
        let worker = Worker::new(config.clone()).await.unwrap();
        let started = render(&self.node.take());
        self.node.hold(self.tick_hold);
        Process {
            rig: self,
            worker,
            config,
            startup: started,
        }
    }
    /// The journal as the scenario left it: request, transaction and epoch states, and the health observation.
    async fn digest(&self) -> String {
        let pool = self.journal().await;
        let jobs: Vec<(String, String)> =
            sqlx::query_as("SELECT id,state FROM jobs ORDER BY CAST(id AS INTEGER)")
                .fetch_all(&pool)
                .await
                .unwrap();
        let txs: Vec<(String, String, i64, String)> =
            sqlx::query_as("SELECT job,kind,nonce,state FROM txs ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        let epochs: Vec<(i64, String)> = if has_table(&pool, "epoch_work").await {
            sqlx::query_as("SELECT epoch,state FROM epoch_work ORDER BY epoch")
                .fetch_all(&pool)
                .await
                .unwrap()
        } else {
            Vec::new()
        };
        let sweep = crate::sweep::last(&pool).await.unwrap().map(|o| o.state);
        let queued = crate::sweep::request(&pool).await.unwrap().is_some();
        let in_flight = crate::sweep::in_flight(&pool).await.unwrap();
        let meta = |key: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, String>("SELECT value FROM meta WHERE key=?")
                    .bind(key)
                    .fetch_optional(&pool)
                    .await
                    .unwrap()
                    .unwrap_or_default()
            }
        };
        let health: serde_json::Value = serde_json::from_str(&meta("health:status").await).unwrap();
        let (cursor, floor) = (meta("cursor").await, meta("nonce_floor").await);
        pool.close().await;
        let list = |items: Vec<String>| items.join(" ");
        format!(
            "jobs [{}]\ntxs [{}]\nepochs [{}]\nsweep queued={queued} in_flight={in_flight} last={}\ncursor={cursor} nonce_floor={floor}\nhealth healthy={} faults={} send_enabled={}",
            list(
                jobs.iter()
                    .map(|(id, state)| format!("{id}={state}"))
                    .collect()
            ),
            list(
                txs.iter()
                    .map(|(job, kind, nonce, state)| format!("{job}:{kind}@{nonce}={state}"))
                    .collect()
            ),
            list(
                epochs
                    .iter()
                    .map(|(epoch, state)| format!("{epoch}={state}"))
                    .collect()
            ),
            sweep.unwrap_or_else(|| "none".into()),
            health["healthy"],
            health["faults"],
            health["send_enabled"],
        )
    }
    /// Ask the running keeper, through its journal as the `sweep` command does, to send `wei` to the fee recipient.
    pub async fn queue_sweep(&self, wei: &str) {
        let pool = self.journal().await;
        let request = crate::sweep::Request {
            mode: crate::sweep::Mode::Amount,
            wei: wei.into(),
            requested_at: 0,
        };
        crate::sweep::submit(&pool, &request).await.unwrap();
        pool.close().await;
    }
    /// Let the seconds pass that a sweep waits without a receipt before it is cancelled: its transactions were
    /// signed and sent long ago.
    pub async fn age_sweep(&self) {
        let pool = self.journal().await;
        let attempt: String =
            sqlx::query_scalar("SELECT value FROM meta WHERE key='sweep:attempt'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let mut attempt: serde_json::Value = serde_json::from_str(&attempt).unwrap();
        for tx in attempt["txs"].as_array_mut().unwrap() {
            tx["created"] = 0.into();
            tx["broadcast"] = 0.into();
        }
        sqlx::query("UPDATE meta SET value=? WHERE key='sweep:attempt'")
            .bind(attempt.to_string())
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }
    /// Run `script` on the chain.
    pub fn chain<T>(&self, script: impl FnOnce(&mut Chain) -> T) -> T {
        self.node.with(script)
    }
    /// A request from a consumer with a 100,000 gas callback, paying well above the keeper's cost.
    pub fn request(&self) -> u64 {
        self.request_with(100_000)
    }
    /// A request from a consumer with a callback of this gas limit, paying well above the keeper's cost.
    pub fn request_with(&self, callback_gas: u32) -> u64 {
        self.chain(|chain| {
            chain.request(
                Address::repeat_byte(0xa1),
                callback_gas,
                100_000_000_000_000_000,
            )
        })
    }
    /// The transactions the keeper has sent so far, as the node saw them.
    pub fn sent(&self) -> Vec<String> {
        self.chain(|chain| {
            chain
                .sends
                .iter()
                .map(|(_, what, gas, nonce)| format!("{what} gas={gas} nonce={nonce}"))
                .collect()
        })
    }
    /// Mine the transactions the keeper sent, and the blocks after them that make them final.
    pub fn settle(&self) {
        self.chain(|chain| {
            chain.include();
            chain.mine(1 + chain.finalized_lag);
        });
    }
}
