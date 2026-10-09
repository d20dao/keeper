//! Epoch publication is a typed maintenance lane; it never creates game jobs.
use crate::{
    abi::{ApiProof, EpochRegistry as E, EpochSelection},
    beacon,
    rpc::{Head, Rpc},
};
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use anyhow::{Result, ensure};
use sqlx::{Row, SqlitePool};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::task::JoinHandle;

/// A recipe as registered in EpochEntropy: the canonical request (its keccak256 is the query hash a selection names), the
/// data template of the exact record an epoch commits from it and the body that names its source. The keeper prepares
/// epochs from drand beacon recipes (see beacon.rs): a beacon's canonical request and body name a drand network, and
/// `beacon` is its registration, read with `beaconOf` for the recipes whose request says so. Any other recipe is a signed
/// API recipe, which served the epochs before the drand catalog and stays registered, and the epochs it served stay
/// verifiable; since 0.4.1 the keeper no longer prepares epochs from one (see UNSUPPORTED_RECIPE).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredRecipe {
    pub canonical_request: String,
    pub template: Bytes,
    pub body: String,
    pub beacon: Option<beacon::BeaconRegistration>,
}
/// Attempt n publishes the source n slots after the selected one, from n × 20 blocks into the epoch.
pub const FALLBACK_DELAY_BLOCKS: u64 = 20;
/// A catalog lists 1 to MAX_SOURCES sources; the registry's sourceCountAt(epoch) bounds its attempts.
pub const MAX_SOURCES: u8 = 10;
/// Consecutive failures of one drand relay open its circuit for BREAKER_COOLDOWN_SECONDS (see drand::Breaker).
pub use crate::drand::{BREAKER_COOLDOWN_SECONDS, BREAKER_FAILURES};
/// Why the keeper does not prepare an epoch whose selected recipe is not a drand beacon. The source is recorded as failed
/// for good, so a catalog with another source falls back to it when its window opens, and the epochs after it are
/// prepared as usual. It is logged as an error with this sentence and reported as the health fault
/// `epoch_recipe_unsupported` (see health::unsupported_recipe) until an epoch's selected source is supported again.
/// Epochs the registry already published are served all the same.
pub const UNSUPPORTED_RECIPE: &str = "signed API recipes are not supported since 0.4.1";
/// The registered recipe of a selected source is not a drand beacon.
#[derive(Debug)]
struct UnsupportedRecipe(u8);
impl std::fmt::Display for UnsupportedRecipe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Registered recipe {} is not a drand beacon: {UNSUPPORTED_RECIPE}; refusing to prepare an epoch from it",
            self.0
        )
    }
}
impl std::error::Error for UnsupportedRecipe {}
/// `beaconOf` reverted for a recipe whose request names drand: this registry cannot serve it as a beacon. Unlike a
/// read that failed, that does not change on the next try, so the source is recorded as failed for good and a catalog
/// with another source falls back to it, instead of the epoch being retried every tick until it is too late.
#[derive(Debug)]
struct NotABeacon(anyhow::Error);
impl std::fmt::Display for NotABeacon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The registry cannot serve this recipe as a beacon: beaconOf reverted ({})",
            self.0
        )
    }
}
impl std::error::Error for NotABeacon {}
pub struct Publisher {
    pub registry: Address,
    pub catalog: B256,
    /// The drand relays a beacon recipe's rounds are read from, and the client that asks them.
    relays: beacon::DrandRelays,
    relay_client: reqwest::Client,
    /// What fetches of beacon rounds leave running in the background, aborted when the publisher is dropped.
    stragglers: beacon::Stragglers,
    /// Registered recipes by id. A recipe never changes once registered, so each id is read once.
    recipes: Mutex<HashMap<u8, Arc<RegisteredRecipe>>>,
    cached: Mutex<Option<(u64, u64)>>,
    fetch: Mutex<Option<JoinHandle<Result<()>>>>,
}
#[derive(Debug)]
pub struct Work {
    pub key: String,
    pub epoch: u64,
    pub start: u64,
    pub state: String,
    pub api: Option<String>,
    /// The registry's selection JSON the packet was prepared for; it names the source's canonical request.
    pub selection: Option<String>,
    pub fallback: u8,
    /// Sources in the epoch's catalog: attempts run from 0 to sources - 1.
    pub sources: u8,
    pub attempts: i64,
    pub retry_at: i64,
    pub last_error: Option<String>,
    /// The run of failed beacon fetches this work is in, if it is: when it started, when its last failure was and
    /// whether that failure was the keeper's own read of the chain (see beacon::RUN_GAP_SECONDS). None for work that
    /// never failed a fetch, or whose source was refused.
    pub failing_since: Option<i64>,
    pub failed_at: Option<i64>,
    pub failed_rpc: bool,
    /// Whether a transaction for this epoch's commit is still in flight: signed or submitted, its nonce not resolved. A
    /// cancelled or replaced commit whose nonce has resolved is not, and neither is work nothing was ever signed for.
    pub in_flight: bool,
}
impl Work {
    /// When this work is next fetched: its `retry_at`, except that live paid demand on a beacon epoch whose fetches
    /// keep failing does not wait out the back-off and retries every beacon::RETRY_SECONDS after the last failure.
    pub fn retry_due(&self, live_demand: bool) -> i64 {
        match self.failed_at {
            Some(at) if live_demand => self
                .retry_at
                .min(at.saturating_add(i64::try_from(beacon::RETRY_SECONDS).unwrap_or(i64::MAX))),
            _ => self.retry_at,
        }
    }
    /// Whether the selected source of this work is a beacon.
    fn is_beacon(&self) -> bool {
        self.selection
            .as_deref()
            .and_then(|json| serde_json::from_str::<EpochSelection>(json).ok())
            .is_some_and(|selection| beacon::is_beacon_request(&selection.canonicalRequest))
    }
    /// A saved beacon packet the keeper may discard and prepare again instead of sending: it is older than
    /// BEACON_MAX_AGE at `now`, no commit transaction for it is in flight, and the work is waiting for its publication
    /// (`pending`, `prepared`) or was blocked from it (`blocked`). Whether the registry already has the epoch is for
    /// refresh_stale_beacon to ask. Anything else, and every packet of a signed recipe that an earlier release prepared,
    /// keeps its packet exactly as before.
    pub fn stale_beacon(&self, now: u64) -> bool {
        !self.in_flight
            && matches!(self.state.as_str(), "pending" | "prepared" | "blocked")
            && self
                .api
                .as_deref()
                .and_then(|json| serde_json::from_str::<ApiProof>(json).ok())
                .is_some_and(|api| {
                    U256::from(now)
                        > api
                            .timestamp
                            .saturating_add(U256::from(beacon::BEACON_MAX_AGE))
                })
            && self.is_beacon()
    }
}
pub async fn install(pool: &SqlitePool) -> Result<()> {
    sqlx::raw_sql("CREATE TABLE IF NOT EXISTS epoch_work(key TEXT PRIMARY KEY,registry TEXT NOT NULL,catalog TEXT NOT NULL,epoch INTEGER NOT NULL,start INTEGER NOT NULL,state TEXT NOT NULL DEFAULT 'pending',api TEXT,selection TEXT,attempts INTEGER NOT NULL DEFAULT 0,retry_at INTEGER NOT NULL DEFAULT 0,last_error TEXT,fallback INTEGER NOT NULL DEFAULT 0,sources INTEGER NOT NULL DEFAULT 4,failing_since INTEGER,failed_at INTEGER,failed_rpc INTEGER); CREATE INDEX IF NOT EXISTS epoch_work_open ON epoch_work(state,start); CREATE INDEX IF NOT EXISTS epoch_work_identity ON epoch_work(registry,catalog,epoch);").execute(pool).await?;
    // Journals from before variable catalogs only tracked epochs of the initial catalog, which has four sources.
    let has_sources: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('epoch_work') WHERE name='sources')",
    )
    .fetch_one(pool)
    .await?;
    if !has_sources {
        sqlx::raw_sql("ALTER TABLE epoch_work ADD COLUMN sources INTEGER NOT NULL DEFAULT 4")
            .execute(pool)
            .await?;
    }
    // Journals from before beacon epochs have no start for a run of failed fetches (see beacon::UNAVAILABLE_SECONDS),
    // and those of its first release no time and kind of the last failure (see beacon::RUN_GAP_SECONDS). The columns
    // are nullable and nothing reads them but this release, so the release before still opens the journal.
    for (column, add) in [
        (
            "failing_since",
            "ALTER TABLE epoch_work ADD COLUMN failing_since INTEGER",
        ),
        (
            "failed_at",
            "ALTER TABLE epoch_work ADD COLUMN failed_at INTEGER",
        ),
        (
            "failed_rpc",
            "ALTER TABLE epoch_work ADD COLUMN failed_rpc INTEGER",
        ),
    ] {
        let present: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('epoch_work') WHERE name=?)",
        )
        .bind(column)
        .fetch_one(pool)
        .await?;
        if !present {
            sqlx::raw_sql(add).execute(pool).await?;
        }
    }
    // A journal that releases up to 0.4.0 used also holds their `epoch_breaker` table, the circuits of the Airnode gateways
    // of signed API sources. Nothing reads or creates it any more, and nothing drops it: the journal opens as it was left,
    // and the release before creates the table itself in a journal of this release.
    sqlx::raw_sql("CREATE TABLE IF NOT EXISTS epoch_relay_breaker(url TEXT PRIMARY KEY,failures INTEGER NOT NULL,open_until INTEGER NOT NULL)")
        .execute(pool)
        .await?;
    Ok(())
}
pub async fn work(pool: &SqlitePool, key: &str) -> Result<Work> {
    let r = sqlx::query("SELECT epoch_work.*,EXISTS(SELECT 1 FROM txs WHERE txs.job=epoch_work.key AND txs.state!='resolved') AS in_flight FROM epoch_work WHERE key=?")
        .bind(key)
        .fetch_one(pool)
        .await?;
    Ok(Work {
        key: r.get("key"),
        epoch: r.get::<i64, _>("epoch").try_into()?,
        start: r.get::<i64, _>("start").try_into()?,
        state: r.get("state"),
        api: r.get("api"),
        selection: r.get("selection"),
        fallback: r.get::<i64, _>("fallback").try_into()?,
        sources: r.get::<i64, _>("sources").try_into()?,
        attempts: r.get("attempts"),
        retry_at: r.get("retry_at"),
        last_error: r.get("last_error"),
        failing_since: r.get("failing_since"),
        failed_at: r.get("failed_at"),
        failed_rpc: r.get::<Option<i64>, _>("failed_rpc") == Some(1),
        in_flight: r.get::<i64, _>("in_flight") != 0,
    })
}
pub async fn state(pool: &SqlitePool, key: &str, state: &str) -> Result<()> {
    sqlx::query("UPDATE epoch_work SET state=? WHERE key=?")
        .bind(state)
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}
/// Deterministic source fallback: work whose current source ended without a packet (a recorded
/// failure, retryable or not) moves to the next source once that attempt's window opens. A saved
/// packet is never replaced here, and the epoch's last source keeps its own retry or blocked outcome.
async fn advance_fallback(pool: &SqlitePool, saved: &Work, head: u64) -> Result<bool> {
    let next = saved.fallback.saturating_add(1);
    if saved.api.is_some()
        || saved.last_error.is_none()
        || !matches!(saved.state.as_str(), "pending" | "blocked")
        || next >= saved.sources
        || head
            < saved
                .start
                .saturating_add(u64::from(next) * FALLBACK_DELAY_BLOCKS)
    {
        return Ok(false);
    }
    let moved = sqlx::query("UPDATE epoch_work SET fallback=?,state='pending',selection=NULL,attempts=0,retry_at=0,last_error=NULL,failing_since=NULL,failed_at=NULL,failed_rpc=NULL WHERE key=? AND fallback=? AND api IS NULL AND last_error IS NOT NULL AND state IN ('pending','blocked')")
        .bind(i64::from(next)).bind(&saved.key).bind(i64::from(saved.fallback)).execute(pool).await?;
    Ok(moved.rows_affected() == 1)
}
/// The one exception to a saved packet being final: a beacon packet that aged past BEACON_MAX_AGE without being published
/// is discarded, so the epoch is prepared again with a current round (see beacon::choose_round) instead of being
/// blocked. Returns whether this call discarded it. It happens when, and only when, all of these hold:
/// - the packet is a beacon's, older than BEACON_MAX_AGE at `now`, and its work is `pending`, `prepared` or `blocked`
///   (see Work::stale_beacon). The packet of a signed recipe is never discarded here, at any age;
/// - every transaction the journal holds for the epoch is resolved, or there is none. A resolved nonce is consumed on
///   chain, so no byte string that was signed for it can be mined any more, while a commit that is signed or submitted,
///   with the fee-bumped or cancelling replacements of its nonce, is in flight and keeps its packet and its exact bytes;
/// - the registry does not have the epoch at the latest block the keeper trusts (`getEpoch` has a zero `epochHash`). An
///   epoch published by another committer, or by a commit whose receipt is not final yet, has its packet on chain, and
///   the journal keeps that one; its work is marked `committed`, which nothing else would do for blocked work, once the
///   finalized state has the epoch too. In soft mode (FINALITY_MODE=soft) the state the keeper decides on is the decision
///   head, and an epoch published at it is final enough.
///
/// It is safe because a beacon round is public and fixed by its network: unlike a signed API record, a second fetch
/// cannot return other data for the same round number, so holding a packet back protects nothing, and any current
/// round is as valid as the one it replaces. What the epoch's randomness draws on is the block after its publication,
/// whose hash does not exist while the packet is unpublished: a commit that was signed, broadcast and cancelled taught
/// nobody anything about the outcome, so preparing another round after it gives the operator nothing to choose from.
///
/// Two guards close the races. The registry is asked immediately before the swap, and a failed read leaves the packet
/// alone until the next attempt. The swap is a compare-and-swap in SQL: the row must still hold the exact packet and
/// selection that were judged, be in one of the three states and have no transaction that is not resolved, so a commit
/// signed after the snapshot was read stops it.
pub async fn refresh_stale_beacon(
    pool: &SqlitePool,
    rpc: &Rpc,
    registry: Address,
    stale: &Work,
    now: u64,
) -> Result<bool> {
    if !stale.stale_beacon(now) {
        return Ok(false);
    }
    let head = rpc.head().await?;
    let record = rpc
        .call_at(
            registry,
            E::getEpochCall {
                epochId: stale.epoch,
            },
            head.number,
        )
        .await?;
    if record.epochHash != B256::ZERO {
        // Published at the latest block, so this packet is not the one to fetch again. Once the state the keeper
        // decides on agrees, the epoch is done and its work is committed instead of blocked for good; until then it
        // stays as it is, since a block that is not final can still be replaced. On a chain that finalizes, that state
        // is the finalized one, as send_epoch reads it. In soft mode the decision head is the keeper's standard of
        // final, so an epoch published at the decision head is final enough.
        let decided = rpc
            .call_decided(
                registry,
                E::getEpochCall {
                    epochId: stale.epoch,
                },
            )
            .await?;
        if decided.epochHash != B256::ZERO {
            sqlx::query("UPDATE epoch_work SET state='committed' WHERE key=? AND state IN ('pending','prepared','blocked') AND NOT EXISTS(SELECT 1 FROM txs WHERE txs.job=epoch_work.key AND txs.state!='resolved')")
                .bind(&stale.key).execute(pool).await?;
        }
        return Ok(false);
    }
    let refreshed = sqlx::query("UPDATE epoch_work SET api=NULL,state='pending',attempts=0,retry_at=0,last_error=NULL,failing_since=NULL,failed_at=NULL,failed_rpc=NULL WHERE key=? AND api=? AND selection=? AND state IN ('pending','prepared','blocked') AND NOT EXISTS(SELECT 1 FROM txs WHERE txs.job=epoch_work.key AND txs.state!='resolved')")
        .bind(&stale.key).bind(&stale.api).bind(&stale.selection).execute(pool).await?;
    Ok(refreshed.rows_affected() == 1)
}
/// Blocked work of this registry and catalog with a saved packet, no commit in flight and live paid demand (a request
/// still open after `deadline_after`), oldest epoch first. A commit cancelled at the freshness bound resolves its epoch
/// to `blocked`, and `poll` hands `send_epoch` `prepared` work alone, so this is how refresh_stale_beacon reaches work
/// that would otherwise wait for a round nobody fetches.
pub async fn blocked_with_demand(
    pool: &SqlitePool,
    registry: Address,
    catalog: B256,
    deadline_after: u64,
) -> Result<Vec<Work>> {
    let keys: Vec<String> = sqlx::query_scalar("SELECT epoch_work.key FROM epoch_work WHERE epoch_work.registry=? AND epoch_work.catalog=? AND epoch_work.state='blocked' AND epoch_work.api IS NOT NULL AND NOT EXISTS(SELECT 1 FROM txs WHERE txs.job=epoch_work.key AND txs.state!='resolved') AND EXISTS(SELECT 1 FROM epoch_demand JOIN jobs ON jobs.id=epoch_demand.job WHERE epoch_demand.epoch=epoch_work.epoch AND jobs.deadline>? AND jobs.state IN ('pending','prepared','signed','submitted')) ORDER BY epoch_work.epoch LIMIT 8")
        .bind(registry.to_string()).bind(catalog.to_string()).bind(i64::try_from(deadline_after)?).fetch_all(pool).await?;
    let mut blocked = Vec::with_capacity(keys.len());
    for key in keys {
        blocked.push(work(pool, &key).await?);
    }
    Ok(blocked)
}
/// Keep unused immutable snapshots for 50 epochs, protecting any still-funded request or nonce.
async fn retire_idle_snapshots(pool: &SqlitePool, head: &Head) -> Result<()> {
    sqlx::query("UPDATE epoch_work SET state='expired' WHERE key IN (SELECT key FROM epoch_work WHERE start<=? AND state IN ('pending','prepared','blocked') AND NOT EXISTS(SELECT 1 FROM txs WHERE txs.job=epoch_work.key AND txs.state!='resolved') AND NOT EXISTS(SELECT 1 FROM epoch_demand JOIN jobs ON jobs.id=epoch_demand.job WHERE epoch_demand.epoch=epoch_work.epoch AND jobs.deadline>=? AND jobs.state IN ('pending','prepared','signed','submitted')) LIMIT 128)")
        .bind(i64::try_from(head.number.saturating_sub(10000))?).bind(i64::try_from(head.timestamp)?).execute(pool).await?;
    Ok(())
}
impl Publisher {
    pub fn new(registry: Address, catalog: B256, relays: beacon::DrandRelays) -> Result<Self> {
        Ok(Self {
            registry,
            catalog,
            relays,
            relay_client: beacon::relay_client()?,
            stragglers: beacon::Stragglers::default(),
            recipes: Mutex::new(HashMap::new()),
            cached: Mutex::new(None),
            fetch: Mutex::new(None),
        })
    }
    pub fn key(&self, epoch: u64) -> String {
        format!("epoch:{}:{}:{epoch}", self.registry, self.catalog)
    }
    /// A registered recipe, read from the registry the first time its id is selected and cached after that.
    async fn recipe(&self, rpc: &Rpc, id: u8) -> Result<Arc<RegisteredRecipe>> {
        if let Some(recipe) = self.recipes.lock().expect("recipe cache mutex").get(&id) {
            return Ok(recipe.clone());
        }
        let registered = rpc
            .call(self.registry, E::getRecipeCall { recipe: id })
            .await?;
        // Only a registry upgraded for beacons answers beaconOf, and only its beacon-shaped recipes can be beacons.
        let registration = if beacon::is_beacon_request(&registered.canonicalRequest) {
            match rpc
                .call(self.registry, E::beaconOfCall { recipe: id })
                .await
            {
                Ok(registration) => Some(registration.into()),
                // A node that answered that the call reverts: this registry cannot serve the recipe. Every other
                // failure of the read is the RPC's and is tried again.
                Err(error) if crate::rpc::is_revert(&error) => return Err(NotABeacon(error).into()),
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let recipe = Arc::new(RegisteredRecipe {
            canonical_request: registered.canonicalRequest,
            template: registered.template,
            body: registered.body,
            beacon: registration,
        });
        ensure!(
            keccak256(recipe.canonical_request.as_bytes()) == registered.queryHash,
            "Registry recipe {id} reports a query hash that differs from its canonical request"
        );
        self.recipes
            .lock()
            .expect("recipe cache mutex")
            .insert(id, recipe.clone());
        Ok(recipe)
    }
    /// Record an epoch's work with its start and its catalog's source count, and note a publication.
    async fn track(&self, rpc: &Rpc, pool: &SqlitePool, epoch: u64) -> Result<u64> {
        let (start, record, sources) = tokio::try_join!(
            rpc.call(self.registry, E::epochStartCall { epochId: epoch }),
            rpc.call(self.registry, E::getEpochCall { epochId: epoch }),
            rpc.call(self.registry, E::sourceCountAtCall { epochId: epoch })
        )?;
        let sources = u8::try_from(sources)
            .ok()
            .filter(|count| (1..=MAX_SOURCES).contains(count))
            .ok_or_else(|| anyhow::anyhow!("Registry reports an invalid epoch source count"))?;
        let key = self.key(epoch);
        sqlx::query("INSERT OR IGNORE INTO epoch_work(key,registry,catalog,epoch,start,sources) VALUES(?,?,?,?,?,?)")
            .bind(&key).bind(self.registry.to_string()).bind(self.catalog.to_string()).bind(i64::try_from(epoch)?).bind(i64::try_from(start)?).bind(i64::from(sources)).execute(pool).await?;
        if record.epochHash != B256::ZERO {
            state(pool, &key, "committed").await?;
        }
        Ok(start)
    }
    pub async fn finish_fetch(&self) -> Result<()> {
        let task = self.fetch.lock().expect("epoch fetch mutex").take();
        if let Some(task) = task {
            task.await??;
        }
        Ok(())
    }
    pub async fn poll(
        &self,
        rpc: &Rpc,
        pool: &SqlitePool,
        head: &Head,
        demanded_epoch: Option<u64>,
    ) -> Result<Option<Work>> {
        let finished = {
            let mut task = self.fetch.lock().expect("epoch fetch mutex");
            if task.as_ref().is_some_and(|t| t.is_finished()) {
                task.take()
            } else {
                None
            }
        };
        if let Some(task) = finished {
            task.await??;
        }
        let cached = *self.cached.lock().expect("epoch cache mutex");
        let (epoch, start) = if let Some(pair) = cached
            .filter(|(_, start)| head.number >= *start && head.number < start.saturating_add(200))
        {
            pair
        } else {
            let epoch = rpc
                .call(
                    self.registry,
                    E::nextEpochToPrepareCall {
                        number: U256::from(head.number),
                    },
                )
                .await?;
            if epoch == 0 {
                return Ok(None);
            }
            let start = self.track(rpc, pool, epoch).await?;
            *self.cached.lock().expect("epoch cache mutex") = Some((epoch, start));
            (epoch, start)
        };
        retire_idle_snapshots(pool, head).await?;
        let (epoch, start) = if let Some(demand) = demanded_epoch.filter(|d| *d != epoch) {
            (demand, self.track(rpc, pool, demand).await?)
        } else {
            (epoch, start)
        };
        let key = self.key(epoch);
        let mut saved = work(pool, &key).await?;
        if saved.state == "prepared" {
            return Ok(Some(saved));
        }
        if self.fetch.lock().expect("epoch fetch mutex").is_none()
            && advance_fallback(pool, &saved, head.number).await?
        {
            tracing::warn!(epoch_key=%key,error=saved.last_error.as_deref().unwrap_or_default(),fallback=saved.fallback + 1,"Epoch source produced no packet; moving to the next source");
            saved = work(pool, &key).await?;
        }
        if saved.state != "pending"
            || saved.api.is_some()
            || saved.retry_due(demanded_epoch == Some(epoch)) > crate::health::now()? as i64
            || head.number < start
            || (head.number >= start.saturating_add(200) && demanded_epoch != Some(epoch))
        {
            return Ok(None);
        }
        if self.fetch.lock().expect("epoch fetch mutex").is_some() {
            return Ok(None);
        }
        let selection = rpc
            .call(
                self.registry,
                E::getEpochFallbackSelectionCall {
                    epochId: epoch,
                    attempt: saved.fallback,
                },
            )
            .await?;
        let selection_json = serde_json::to_string(&selection)?;
        let prior: Option<String> =
            sqlx::query_scalar("SELECT selection FROM epoch_work WHERE key=?")
                .bind(&key)
                .fetch_one(pool)
                .await?;
        ensure!(
            prior.is_none_or(|value| value == selection_json),
            "Epoch selection changed; refusing a different source/query"
        );
        // Recheck the trusted head immediately before launching external work.
        let fresh = rpc.head().await?;
        if fresh.number < start
            || (fresh.number >= start.saturating_add(200) && demanded_epoch != Some(epoch))
        {
            return Ok(None);
        }
        if fresh.number >= start.saturating_add(200) {
            let live: i64=sqlx::query_scalar("SELECT COUNT(*) FROM epoch_demand JOIN jobs ON jobs.id=epoch_demand.job WHERE epoch_demand.epoch=? AND jobs.deadline>=? AND jobs.state IN ('pending','prepared','signed','submitted')")
                .bind(i64::try_from(epoch)?).bind(i64::try_from(fresh.timestamp)?).fetch_one(pool).await?;
            if live == 0 {
                return Ok(None);
            }
        }
        sqlx::query("UPDATE epoch_work SET selection=COALESCE(selection,?),attempts=attempts+1,retry_at=? WHERE key=? AND api IS NULL AND state IN ('pending','prepared')")
            .bind(selection_json).bind(i64::try_from(crate::health::now()?.saturating_add(3))?).bind(&key).execute(pool).await?;
        let attempt = saved.attempts.saturating_add(1);
        let recipe = match self.recipe(rpc, selection.recipe).await {
            Ok(recipe) => recipe,
            // A drand recipe the registry cannot serve as a beacon fails this source for good, like a refused recipe, once
            // a second attempt has found the same: one endpoint's revert may be a glitch, and with one source in the
            // catalog there is nothing to fall back to.
            Err(error) if attempt >= 2 && error.is::<NotABeacon>() => {
                let message = record_failure(
                    pool,
                    &key,
                    attempt,
                    FetchFailure::Permanent(error),
                    crate::health::now()?,
                )
                .await?;
                tracing::warn!(epoch_key=%key,error=%message,"Epoch source refused");
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let now = crate::health::now()?;
        if !source_or_refuse(pool, &key, attempt, &selection, &recipe, now).await? {
            return Ok(None);
        }
        // A beacon's rounds come from the drand relays, each with a circuit of its own.
        let fetcher = beacon::Fetcher {
            rpc: rpc.clone(),
            client: self.relay_client.clone(),
            pool: pool.clone(),
            relays: self.relays.urls().to_vec(),
            registry: self.registry,
            recipe_id: selection.recipe,
            recipe: recipe.clone(),
            stragglers: self.stragglers.clone(),
        };
        let pool = pool.clone();
        *self.fetch.lock().expect("epoch fetch mutex") = Some(tokio::spawn(async move {
            // The round is chosen from the epoch's start block and the chain time of this launch.
            let result = fetcher
                .fetch(start, fresh.timestamp)
                .await
                .map_err(|error| {
                    if beacon::is_chain_read(&error) {
                        FetchFailure::BeaconRpc(error)
                    } else {
                        FetchFailure::Beacon(error)
                    }
                });
            let now = crate::health::now()?;
            match result {
                Ok(api) => {
                    // First authenticated response is immutable, even when its clock is ahead.
                    sqlx::query("UPDATE epoch_work SET api=COALESCE(api,?),state=CASE WHEN state='pending' THEN 'prepared' ELSE state END,retry_at=0,last_error=NULL,failing_since=NULL,failed_at=NULL,failed_rpc=NULL WHERE key=? AND state IN ('pending','prepared')")
                        .bind(serde_json::to_string(&api)?).bind(&key).execute(&pool).await?;
                    tracing::info!(epoch_key=%key,"Epoch API packet saved");
                }
                Err(failure) => {
                    // An outage of drand that lasts is one warning to begin with and one for every five minutes of it.
                    let loud = failure_is_loud(&pool, &key, attempt, now).await?;
                    let message = record_failure(&pool, &key, attempt, failure, now).await?;
                    if loud {
                        tracing::warn!(epoch_key=%key,error=%message,"Epoch API preparation deferred");
                    } else {
                        tracing::debug!(epoch_key=%key,error=%message,"Epoch API preparation deferred");
                    }
                }
            }
            Ok(())
        }));
        Ok(None)
    }
}
impl Drop for Publisher {
    fn drop(&mut self) {
        if let Ok(task) = self.fetch.get_mut()
            && let Some(task) = task.take()
        {
            task.abort();
        }
        self.stragglers.abort();
    }
}
/// The selected source is prepared only when its registered recipe is a drand beacon and consistent: the registry's
/// selection carries exactly that recipe's canonical request and query hash, and the beacon's registration, request, body
/// and template agree (see beacon::check). Any other recipe is a signed API recipe, which is refused as UNSUPPORTED_RECIPE.
fn check_recipe(s: &EpochSelection, recipe: &RegisteredRecipe) -> Result<()> {
    if !beacon::is_beacon_request(&recipe.canonical_request) {
        return Err(UnsupportedRecipe(s.recipe).into());
    }
    ensure!(
        recipe.canonical_request == s.canonicalRequest
            && keccak256(recipe.canonical_request.as_bytes()) == s.queryHash,
        "Registry selection differs from registered recipe {}; refusing to fetch",
        s.recipe
    );
    beacon::check(s.recipe, recipe)
}
/// Whether the selected source can be prepared. A refused source is recorded as this source's permanent failure, so the
/// deterministic fallback ladder moves to the next source when its window opens, exactly as for a rejected query, and the
/// epochs after it are prepared as usual. A recipe that is no drand beacon is the refusal that is an error: the catalog in
/// force schedules something this keeper cannot serve, which is logged with UNSUPPORTED_RECIPE and reported as a health
/// fault until an epoch's selected source is supported again. Nothing else stops: published epochs are served all the same.
async fn source_or_refuse(
    pool: &SqlitePool,
    key: &str,
    attempt: i64,
    selection: &EpochSelection,
    recipe: &RegisteredRecipe,
    now: u64,
) -> Result<bool> {
    let error = match check_recipe(selection, recipe) {
        Ok(()) => {
            crate::health::supported_recipe(pool).await?;
            return Ok(true);
        }
        Err(error) => error,
    };
    let unsupported = error.is::<UnsupportedRecipe>();
    if unsupported {
        // Before the source is recorded as blocked, which nothing asks again: the poll that gets here runs under a timeout
        // (see Worker::tick), and one cut off between the two would leave a blocked source with no fault and no log. A
        // fault written ahead of a cut-off record is only written again by the next poll, which finds the source pending.
        crate::health::unsupported_recipe(pool, selection.recipe).await?;
        tracing::error!(epoch_key=%key,recipe=selection.recipe,error=%error,"Epoch source refused: {UNSUPPORTED_RECIPE}");
    }
    let message = record_failure(pool, key, attempt, FetchFailure::Permanent(error), now).await?;
    if !unsupported {
        tracing::warn!(epoch_key=%key,error=%message,"Epoch source refused");
    }
    Ok(false)
}
/// A drand relay's circuit in the epoch keeper's table (see drand::Breaker::open).
#[cfg(test)]
pub(crate) async fn relay_open(
    pool: &SqlitePool,
    relay: &str,
    now: u64,
) -> Result<Option<(i64, u64)>> {
    beacon::RELAY_BREAKER.open(pool, relay, now).await
}
/// A relay's outcome in the epoch keeper's table (see drand::Breaker::record).
#[cfg(test)]
pub(crate) async fn relay_record(
    pool: &SqlitePool,
    relay: &str,
    failed: bool,
    now: u64,
) -> Result<i64> {
    beacon::RELAY_BREAKER.record(pool, relay, failed, now).await
}
enum FetchFailure {
    /// No relay served a round (see beacon.rs): retried after beacon::backoff, or every beacon::RETRY_SECONDS while
    /// live paid demand waits. It dates a run of failures, so that a run lasting beacon::UNAVAILABLE_SECONDS shows as
    /// a stall.
    Beacon(anyhow::Error),
    /// The keeper's own read of the chain failed while it fetched a beacon round, so nothing is known about the relays
    /// (see beacon::is_chain_read). Retried and dated like `Beacon`, and reported as its own reason.
    BeaconRpc(anyhow::Error),
    Permanent(anyhow::Error),
}
async fn record_failure(
    pool: &SqlitePool,
    key: &str,
    attempt: i64,
    failure: FetchFailure,
    now: u64,
) -> Result<String> {
    // For a beacon failure, whether it was the keeper's own read of the chain that failed.
    let (retry, message, chain_read) = match failure {
        FetchFailure::Beacon(error) => (
            Some(beacon::backoff(attempt)),
            error.to_string(),
            Some(false),
        ),
        FetchFailure::BeaconRpc(error) => (
            Some(beacon::backoff(attempt)),
            error.to_string(),
            Some(true),
        ),
        FetchFailure::Permanent(error) => (None, error.to_string(), None),
    };
    // A beacon failure dates the run it belongs to: it starts one when there is none or the last failure was more than
    // RUN_GAP_SECONDS ago, so that a failure long past cannot make a fresh one look like a run (a legacy row without
    // that time is such a failure), and it leaves the time of the last failure and its kind.
    sqlx::query("UPDATE epoch_work SET state=CASE WHEN ?1=0 THEN 'blocked' ELSE state END,retry_at=?2,last_error=?3,failing_since=CASE WHEN ?4 IS NULL THEN failing_since WHEN failing_since IS NULL OR COALESCE(failed_at,0)<?5 THEN ?6 ELSE failing_since END,failed_at=CASE WHEN ?4 IS NULL THEN failed_at ELSE ?6 END,failed_rpc=CASE WHEN ?4 IS NULL THEN failed_rpc ELSE ?4 END WHERE key=?7 AND api IS NULL AND state IN ('pending','prepared')")
        .bind(i64::from(retry.is_some())).bind(i64::try_from(now.saturating_add(retry.unwrap_or(0)))?).bind(&message)
        .bind(chain_read.map(i64::from)).bind(i64::try_from(now.saturating_sub(beacon::RUN_GAP_SECONDS))?).bind(i64::try_from(now)?)
        .bind(key).execute(pool).await?;
    Ok(message)
}
/// Whether the failed fetch of a beacon epoch that is about to be recorded is worth a warning (see beacon::loud).
async fn failure_is_loud(pool: &SqlitePool, key: &str, attempt: i64, now: u64) -> Result<bool> {
    let previous: Option<i64> = sqlx::query_scalar("SELECT failed_at FROM epoch_work WHERE key=?")
        .bind(key)
        .fetch_optional(pool)
        .await?
        .flatten();
    Ok(beacon::loud(
        attempt,
        previous.and_then(|at| u64::try_from(at).ok()),
        now,
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicU64, Ordering};
    #[tokio::test]
    async fn failed_fetches_keep_the_same_epoch_retryable_and_preserve_first_packet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retry.sqlite");
        let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
        sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start,selection) VALUES('retry','r','c',1,200,'fixed-selection')").execute(&journal.pool).await.unwrap();
        for attempt in 1..=5 {
            sqlx::query("UPDATE epoch_work SET attempts=? WHERE key='retry'")
                .bind(attempt)
                .execute(&journal.pool)
                .await
                .unwrap();
            record_failure(
                &journal.pool,
                "retry",
                attempt,
                FetchFailure::Beacon(anyhow::anyhow!("temporary")),
                100,
            )
            .await
            .unwrap();
            let saved = work(&journal.pool, "retry").await.unwrap();
            assert_eq!(saved.state, "pending");
            assert_eq!(saved.retry_at, 100 + beacon::backoff(attempt) as i64);
            assert!(saved.api.is_none());
        }
        journal.pool.close().await;
        let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
        assert_eq!(work(&journal.pool, "retry").await.unwrap().state, "pending");
        sqlx::query(
            "UPDATE epoch_work SET api='first-valid-packet',state='prepared' WHERE key='retry'",
        )
        .execute(&journal.pool)
        .await
        .unwrap();
        record_failure(
            &journal.pool,
            "retry",
            6,
            FetchFailure::Permanent(anyhow::anyhow!("late error")),
            200,
        )
        .await
        .unwrap();
        let saved = work(&journal.pool, "retry").await.unwrap();
        assert_eq!(saved.api.as_deref(), Some("first-valid-packet"));
        assert_eq!(saved.state, "prepared");
        let selection: String =
            sqlx::query_scalar("SELECT selection FROM epoch_work WHERE key='retry'")
                .fetch_one(&journal.pool)
                .await
                .unwrap();
        assert_eq!(selection, "fixed-selection");
        sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start) VALUES('invalid','r','c',2,400)").execute(&journal.pool).await.unwrap();
        record_failure(
            &journal.pool,
            "invalid",
            1,
            FetchFailure::Permanent(anyhow::anyhow!("refused")),
            100,
        )
        .await
        .unwrap();
        assert_eq!(
            work(&journal.pool, "invalid").await.unwrap().state,
            "blocked"
        );
    }
    #[tokio::test]
    async fn failed_sources_fall_back_in_order_once_each_window_opens() {
        let dir = tempfile::tempdir().unwrap();
        let journal = crate::journal::Journal::open(&dir.path().join("fallback.sqlite"), "scope")
            .await
            .unwrap();
        let pool = &journal.pool;
        sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start,selection,attempts) VALUES('ladder','r','c',1,200,'primary-selection',1)").execute(pool).await.unwrap();
        // No recorded failure yet: the selected source keeps its turn even with every window open.
        let saved = work(pool, "ladder").await.unwrap();
        assert!(!advance_fallback(pool, &saved, 10_000).await.unwrap());
        record_failure(
            pool,
            "ladder",
            1,
            FetchFailure::Permanent(anyhow::anyhow!("refused")),
            100,
        )
        .await
        .unwrap();
        let saved = work(pool, "ladder").await.unwrap();
        assert_eq!((saved.state.as_str(), saved.fallback), ("blocked", 0));
        assert!(!advance_fallback(pool, &saved, 219).await.unwrap());
        assert!(advance_fallback(pool, &saved, 220).await.unwrap());
        // A stale snapshot of the same attempt cannot advance twice.
        assert!(!advance_fallback(pool, &saved, 220).await.unwrap());
        let saved = work(pool, "ladder").await.unwrap();
        assert_eq!(
            (
                saved.state.as_str(),
                saved.fallback,
                saved.attempts,
                saved.retry_at
            ),
            ("pending", 1, 0, 0)
        );
        assert!(saved.last_error.is_none());
        let selection: Option<String> =
            sqlx::query_scalar("SELECT selection FROM epoch_work WHERE key='ladder'")
                .fetch_one(pool)
                .await
                .unwrap();
        assert!(selection.is_none());
        // Retryable failures move on too, at the next window.
        record_failure(
            pool,
            "ladder",
            1,
            FetchFailure::Beacon(anyhow::anyhow!("no relay")),
            100,
        )
        .await
        .unwrap();
        let saved = work(pool, "ladder").await.unwrap();
        assert!(!advance_fallback(pool, &saved, 239).await.unwrap());
        assert!(advance_fallback(pool, &saved, 240).await.unwrap());
        for _ in 0..2 {
            record_failure(
                pool,
                "ladder",
                1,
                FetchFailure::Permanent(anyhow::anyhow!("invalid")),
                100,
            )
            .await
            .unwrap();
            let saved = work(pool, "ladder").await.unwrap();
            advance_fallback(pool, &saved, 10_000).await.unwrap();
        }
        let saved = work(pool, "ladder").await.unwrap();
        assert_eq!((saved.state.as_str(), saved.fallback), ("blocked", 3));
        assert!(!advance_fallback(pool, &saved, 10_000).await.unwrap());
        // The ladder length is the epoch's source count: eight sources walk to attempt 7 at +140 blocks, one source never moves.
        for (key, sources, last) in [("eight", 8, 7_u8), ("one", 1, 0)] {
            sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start,sources) VALUES(?,'r','c',3,600,?)")
                .bind(key).bind(sources).execute(pool).await.unwrap();
            for attempt in 0..=last {
                record_failure(
                    pool,
                    key,
                    1,
                    FetchFailure::Permanent(anyhow::anyhow!("refused")),
                    100,
                )
                .await
                .unwrap();
                let saved = work(pool, key).await.unwrap();
                assert_eq!((saved.state.as_str(), saved.fallback), ("blocked", attempt));
                let window = 600 + 20 * u64::from(attempt + 1);
                assert!(!advance_fallback(pool, &saved, window - 1).await.unwrap());
                assert_eq!(
                    advance_fallback(pool, &saved, window).await.unwrap(),
                    attempt < last
                );
            }
        }
        // A saved packet is final for its attempt, whatever happens to its transaction later.
        sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start,state,api,last_error) VALUES('packet','r','c',2,400,'blocked','first-packet','estimate reverted')").execute(pool).await.unwrap();
        let saved = work(pool, "packet").await.unwrap();
        assert!(!advance_fallback(pool, &saved, 10_000).await.unwrap());
        journal.pool.close().await;
    }
    #[tokio::test]
    async fn unused_snapshots_survive_fifty_epochs_and_paid_or_nonce_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("idle.sqlite");
        let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
        for (key, epoch) in [("idle", 1), ("paid", 2), ("nonce", 3)] {
            sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start,state,api,selection) VALUES(?,'r','c',?,200,'prepared','first-packet','first-selection')")
                .bind(key).bind(epoch).execute(&journal.pool).await.unwrap();
        }
        journal
            .discovered_epoch("game", 999, "next", Some(2))
            .await
            .unwrap();
        sqlx::query("INSERT INTO txs(job,nonce,hash,raw,kind,fee,gas,priority,payload,created) VALUES('nonce',7,'hash','exact-raw','epoch_cancel','1',21000,'1','0x',1)").execute(&journal.pool).await.unwrap();
        retire_idle_snapshots(
            &journal.pool,
            &Head {
                hash: B256::ZERO,
                number: 10199,
                timestamp: 100,
                base_fee: 1,
            },
        )
        .await
        .unwrap();
        journal.compact_history(100).await.unwrap();
        assert_eq!(
            work(&journal.pool, "idle").await.unwrap().api.as_deref(),
            Some("first-packet")
        );
        journal.pool.close().await;
        let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
        retire_idle_snapshots(
            &journal.pool,
            &Head {
                hash: B256::ZERO,
                number: 10200,
                timestamp: 101,
                base_fee: 1,
            },
        )
        .await
        .unwrap();
        journal.compact_history(160).await.unwrap();
        assert!(work(&journal.pool, "idle").await.unwrap().api.is_none());
        assert_eq!(work(&journal.pool, "idle").await.unwrap().state, "expired");
        for key in ["paid", "nonce"] {
            assert_eq!(
                work(&journal.pool, key).await.unwrap().api.as_deref(),
                Some("first-packet")
            );
            assert_eq!(work(&journal.pool, key).await.unwrap().state, "prepared");
        }
        assert_eq!(journal.unresolved().await.unwrap()[0].raw, "exact-raw");
        journal.pool.close().await;
    }
    fn selection(recipe: u8, canonical: &str) -> EpochSelection {
        EpochSelection {
            source: 0,
            recipe,
            airnode: Address::repeat_byte(9),
            selector: B256::ZERO,
            queryHash: keccak256(canonical.as_bytes()),
            canonicalRequest: canonical.into(),
        }
    }
    fn fixture(path: &str) -> Value {
        serde_json::from_str(&std::fs::read_to_string(format!("../test/fixtures/{path}")).unwrap())
            .unwrap()
    }
    fn hex_bytes(text: &str) -> Bytes {
        text.parse().unwrap()
    }
    /// The built-in recipes exactly as EpochEntropy registers them; test/EpochRecipes.test.ts checks this
    /// fixture against the contract.
    fn builtin(id: u8) -> RegisteredRecipe {
        let recipes = fixture("builtin-recipes.json");
        let entry = &recipes["recipes"][usize::from(id)];
        assert_eq!(entry["id"], u64::from(id));
        RegisteredRecipe {
            canonical_request: entry["canonicalRequest"].as_str().unwrap().into(),
            template: hex_bytes(entry["template"].as_str().unwrap()),
            body: entry["body"].as_str().unwrap().into(),
            beacon: None,
        }
    }

    #[tokio::test]
    async fn refused_sources_block_only_themselves_and_the_fallback_ladder_continues() {
        let dir = tempfile::tempdir().unwrap();
        let journal = crate::journal::Journal::open(&dir.path().join("refused.sqlite"), "scope")
            .await
            .unwrap();
        let pool = &journal.pool;
        sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start,attempts,sources) VALUES('refused','r','c',1,200,1,5)").execute(pool).await.unwrap();
        let recipe = beacon_recipe();
        let matching = selection(6, &recipe.canonical_request);
        assert!(
            source_or_refuse(pool, "refused", 1, &matching, &recipe, 100)
                .await
                .unwrap()
        );
        assert_eq!(work(pool, "refused").await.unwrap().state, "pending");
        // A beacon that disagrees with its own registration is refused like any rejected source, with a warning: it
        // blocks its source alone, and the ladder moves on when the next window opens.
        let changed = RegisteredRecipe {
            template: Bytes::from_static(&[0x04, 0x02, 0x13]),
            ..recipe.clone()
        };
        assert!(
            !source_or_refuse(pool, "refused", 1, &matching, &changed, 100)
                .await
                .unwrap()
        );
        let saved = work(pool, "refused").await.unwrap();
        assert_eq!((saved.state.as_str(), saved.fallback), ("blocked", 0));
        assert!(
            saved
                .last_error
                .as_deref()
                .unwrap()
                .contains("refusing to fetch")
        );
        assert_eq!(
            journal.meta("health:unsupported_recipe").await.unwrap(),
            None,
            "a beacon that is merely inconsistent is no unsupported recipe"
        );
        assert!(!advance_fallback(pool, &saved, 219).await.unwrap());
        assert!(advance_fallback(pool, &saved, 220).await.unwrap());
        assert_eq!(work(pool, "refused").await.unwrap().fallback, 1);
        // A selection that differs from the registered recipe is refused the same way.
        let mut wrong_hash = matching.clone();
        wrong_hash.queryHash = B256::ZERO;
        assert!(
            !source_or_refuse(pool, "refused", 1, &wrong_hash, &recipe, 100)
                .await
                .unwrap()
        );
        let saved = work(pool, "refused").await.unwrap();
        assert_eq!((saved.state.as_str(), saved.fallback), ("blocked", 1));
        assert!(
            saved
                .last_error
                .unwrap()
                .contains("Registry selection differs from registered recipe 6")
        );
        journal.pool.close().await;
    }
    #[tokio::test]
    async fn a_signed_api_recipe_is_refused_as_unsupported_reported_and_cleared_by_a_supported_source()
     {
        let dir = tempfile::tempdir().unwrap();
        let journal =
            crate::journal::Journal::open(&dir.path().join("unsupported.sqlite"), "scope")
                .await
                .unwrap();
        let pool = &journal.pool;
        let faults = |send: bool| {
            let journal = &journal;
            async move {
                crate::health::assess(journal, send, 100, 20, None, 120)
                    .await
                    .unwrap()
                    .faults
            }
        };
        assert!(faults(true).await.is_empty());
        // Every signed recipe the registry registers itself, and any other request that names no drand network, is a
        // recipe the keeper does not prepare: its source fails for good with the sentence below, as an error, and the
        // fault names the recipe, whether or not this keeper sends.
        let mut requests: Vec<(u8, RegisteredRecipe)> =
            (0..6).map(|id| (id, builtin(id))).collect();
        requests.push((
            9,
            RegisteredRecipe {
                canonical_request: r#"["passthrough","GET","/feed/latest",[],""]"#.into(),
                body: r#"["passthrough","GET","/feed/latest",[],""]"#.into(),
                ..builtin(2)
            },
        ));
        requests.push((
            10,
            RegisteredRecipe {
                canonical_request: "not a request".into(),
                body: String::new(),
                ..builtin(2)
            },
        ));
        for (id, recipe) in requests {
            let key = format!("signed-{id}");
            sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start,attempts,sources) VALUES(?,'r','c',?,200,1,2)")
                .bind(&key).bind(i64::from(id) + 1).execute(pool).await.unwrap();
            let chosen = selection(id, &recipe.canonical_request);
            assert!(
                !source_or_refuse(pool, &key, 1, &chosen, &recipe, 100)
                    .await
                    .unwrap()
            );
            let saved = work(pool, &key).await.unwrap();
            assert_eq!((saved.state.as_str(), saved.fallback), ("blocked", 0));
            let error = saved.last_error.unwrap();
            assert!(error.contains(UNSUPPORTED_RECIPE), "{error}");
            assert!(
                error.starts_with(&format!("Registered recipe {id} is not a drand beacon")),
                "{error}"
            );
            assert_eq!(
                faults(true).await,
                [format!("epoch_recipe_unsupported:{id}")]
            );
            assert_eq!(
                faults(false).await,
                [format!("epoch_recipe_unsupported:{id}")]
            );
        }
        assert_eq!(
            UNSUPPORTED_RECIPE,
            "signed API recipes are not supported since 0.4.1"
        );
        // The ladder goes on: after the window the next source is tried, and a supported source clears the fault.
        let saved = work(pool, "signed-2").await.unwrap();
        assert!(!advance_fallback(pool, &saved, 219).await.unwrap());
        assert!(advance_fallback(pool, &saved, 220).await.unwrap());
        let recipe = beacon_recipe();
        let chosen = selection(6, &recipe.canonical_request);
        assert!(
            source_or_refuse(pool, "signed-2", 1, &chosen, &recipe, 220)
                .await
                .unwrap()
        );
        assert_eq!(work(pool, "signed-2").await.unwrap().state, "pending");
        assert!(faults(true).await.is_empty());
        // A beacon whose registry has no registration is no signed recipe: it is refused as inconsistent, not as unsupported.
        let unregistered = RegisteredRecipe {
            beacon: None,
            ..recipe
        };
        sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start,attempts,sources) VALUES('unregistered','r','c',99,200,1,2)").execute(pool).await.unwrap();
        assert!(
            !source_or_refuse(pool, "unregistered", 1, &chosen, &unregistered, 220)
                .await
                .unwrap()
        );
        assert!(faults(true).await.is_empty());
        journal.pool.close().await;
    }
    /// Wakes the test that polls a future by hand.
    struct Wakeup(tokio::sync::Notify);
    impl std::task::Wake for Wakeup {
        fn wake(self: Arc<Self>) {
            self.0.notify_one();
        }
    }
    /// Polls `future` by hand `polls` times, each after its last await was answered, then drops it unfinished: what a
    /// timeout does to work that is still running. Returns its output when it finished within them.
    async fn cut_off_after<F: std::future::Future>(future: F, polls: usize) -> Option<F::Output> {
        let wakeup = Arc::new(Wakeup(tokio::sync::Notify::new()));
        let waker = std::task::Waker::from(wakeup.clone());
        let mut context = std::task::Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        for poll in 0..polls {
            if poll > 0 {
                tokio::time::timeout(std::time::Duration::from_secs(10), wakeup.0.notified())
                    .await
                    .expect("The future was never woken");
            }
            if let std::task::Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return Some(output);
            }
        }
        None
    }
    #[tokio::test]
    async fn a_refusal_cut_off_by_the_poll_timeout_never_leaves_a_blocked_source_without_its_fault()
    {
        let dir = tempfile::tempdir().unwrap();
        let journal = crate::journal::Journal::open(&dir.path().join("cut.sqlite"), "scope")
            .await
            .unwrap();
        let pool = &journal.pool;
        let recipe = builtin(2);
        let chosen = selection(2, &recipe.canonical_request);
        // The poll that refuses a signed recipe runs under a timeout. Cut it off after each of its awaits in turn: a
        // blocked source is never asked again, so it must never be found blocked while nothing reports the refusal.
        let mut finished = false;
        for polls in 1..=50usize {
            let key = format!("cut-{polls}");
            sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start,attempts,sources) VALUES(?,'r','c',?,200,1,2)")
                .bind(&key).bind(i64::try_from(polls).unwrap()).execute(pool).await.unwrap();
            crate::health::supported_recipe(pool).await.unwrap();
            let outcome = cut_off_after(
                source_or_refuse(pool, &key, 1, &chosen, &recipe, 100),
                polls,
            )
            .await;
            // The journal has one connection, so these reads queue behind whatever the cut-off future had started.
            let blocked = work(pool, &key).await.unwrap().state == "blocked";
            let reported = journal
                .meta("health:unsupported_recipe")
                .await
                .unwrap()
                .is_some();
            assert!(
                reported || !blocked,
                "cut off after {polls} polls: the source is blocked and no fault reports it"
            );
            if let Some(refused) = outcome {
                assert!(!refused.unwrap());
                assert!(blocked && reported, "an uncut refusal does both");
                finished = true;
                break;
            }
        }
        assert!(finished, "The refusal never finished");
        journal.pool.close().await;
    }
    #[tokio::test]
    async fn epoch_lane_resolution_is_atomic_and_separate_from_game_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("epochs.sqlite");
        let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
        let publisher = Publisher::new(
            Address::repeat_byte(1),
            B256::repeat_byte(2),
            beacon::DrandRelays::default(),
        )
        .unwrap();
        let key = publisher.key(1);
        sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start,api,state) VALUES(?,?,?,1,200,'first-packet','prepared')")
            .bind(&key).bind(publisher.registry.to_string()).bind(publisher.catalog.to_string()).execute(&journal.pool).await.unwrap();
        journal.discovered("1", 100, "2").await.unwrap();
        let attempt = crate::journal::Attempt {
            id: 0,
            job: key.clone(),
            nonce: 7,
            hash: "h".into(),
            raw: "raw-before-broadcast".into(),
            kind: "epoch".into(),
            fee: "1".into(),
            state: "signed".into(),
            gas: 21000,
            priority: "1".into(),
            payload: "immutable-call".into(),
            created: 1,
            broadcast: 0,
        };
        journal.signed(&attempt).await.unwrap();
        let mut conflicting = attempt.clone();
        conflicting.job = "1".into();
        conflicting.kind = "fulfill".into();
        conflicting.hash = "other".into();
        conflicting.nonce = 8;
        assert!(
            journal.signed(&conflicting).await.is_err(),
            "Concurrent game lane was accepted"
        );
        sqlx::raw_sql("CREATE TRIGGER epoch_crash BEFORE UPDATE OF state ON epoch_work WHEN NEW.state='committed' BEGIN SELECT RAISE(ABORT,'injected crash'); END;").execute(&journal.pool).await.unwrap();
        assert!(
            journal
                .resolve_nonce_epoch(7, &key, "committed")
                .await
                .is_err()
        );
        journal.pool.close().await;
        let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
        assert_eq!(journal.unresolved().await.unwrap()[0].raw, attempt.raw);
        assert_eq!(journal.nonce_floor().await.unwrap(), 0);
        assert_eq!(journal.pending().await.unwrap()[0].id, "1");
        assert_eq!(work(&journal.pool, &key).await.unwrap().state, "signed");
        sqlx::query("DROP TRIGGER epoch_crash")
            .execute(&journal.pool)
            .await
            .unwrap();
        journal
            .resolve_nonce_epoch(7, &key, "committed")
            .await
            .unwrap();
        assert_eq!(journal.nonce_floor().await.unwrap(), 8);
        assert!(journal.unresolved().await.unwrap().is_empty());
        assert_eq!(
            work(&journal.pool, &key).await.unwrap().api.as_deref(),
            Some("first-packet")
        );
        let events: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE request_id LIKE 'epoch:%'")
                .fetch_one(&journal.pool)
                .await
                .unwrap();
        assert_eq!(events, 0);
        assert_ne!(
            key,
            Publisher::new(
                Address::repeat_byte(3),
                publisher.catalog,
                beacon::DrandRelays::default()
            )
            .unwrap()
            .key(1)
        );
        journal.pool.close().await;
    }
    #[tokio::test]
    async fn journals_without_source_counts_migrate_to_the_initial_four_sources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.sqlite");
        let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
        // Rebuild the table as the previous release created it, with one unfinished epoch.
        sqlx::raw_sql("DROP TABLE epoch_work; CREATE TABLE epoch_work(key TEXT PRIMARY KEY,registry TEXT NOT NULL,catalog TEXT NOT NULL,epoch INTEGER NOT NULL,start INTEGER NOT NULL,state TEXT NOT NULL DEFAULT 'pending',api TEXT,selection TEXT,attempts INTEGER NOT NULL DEFAULT 0,retry_at INTEGER NOT NULL DEFAULT 0,last_error TEXT,fallback INTEGER NOT NULL DEFAULT 0); INSERT INTO epoch_work(key,registry,catalog,epoch,start,state,fallback,last_error) VALUES('legacy','r','c',1,200,'blocked',2,'HTTP 400');")
            .execute(&journal.pool).await.unwrap();
        journal.pool.close().await;
        let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
        let saved = work(&journal.pool, "legacy").await.unwrap();
        assert_eq!((saved.fallback, saved.sources), (2, 4));
        assert!(advance_fallback(&journal.pool, &saved, 260).await.unwrap());
        record_failure(
            &journal.pool,
            "legacy",
            1,
            FetchFailure::Permanent(anyhow::anyhow!("HTTP 400")),
            100,
        )
        .await
        .unwrap();
        let saved = work(&journal.pool, "legacy").await.unwrap();
        assert_eq!((saved.state.as_str(), saved.fallback), ("blocked", 3));
        assert!(
            !advance_fallback(&journal.pool, &saved, 10_000)
                .await
                .unwrap()
        );
        // Opening again is idempotent.
        journal.pool.close().await;
        let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
        assert_eq!(work(&journal.pool, "legacy").await.unwrap().sources, 4);
        journal.pool.close().await;
    }
    /// The registration of the beacon recipe the tests below select.
    fn beacon_registration() -> beacon::BeaconRegistration {
        beacon::BeaconRegistration {
            verifier: Address::repeat_byte(0xbb),
            genesis: 1_000_000,
            period: 3,
            chain_hash: B256::repeat_byte(0x11),
            public_key: Bytes::from(vec![7u8; 128]),
        }
    }
    /// A beacon recipe exactly as the registry registers it.
    fn beacon_recipe() -> RegisteredRecipe {
        let request = format!(r#"["drand","0x{}"]"#, "11".repeat(32));
        RegisteredRecipe {
            canonical_request: request.clone(),
            template: Bytes::from_static(&[0x04, 0x01, 0x13]),
            body: request,
            beacon: Some(beacon_registration()),
        }
    }
    #[tokio::test]
    async fn journals_without_beacon_state_gain_it_and_keep_their_work() {
        // The tables of the release before beacon epochs (0.3.0: no run of failures, no relay circuits) and of the first
        // release with them (which dated a run from its first failure only).
        let table = "CREATE TABLE epoch_work(key TEXT PRIMARY KEY,registry TEXT NOT NULL,catalog TEXT NOT NULL,epoch INTEGER NOT NULL,start INTEGER NOT NULL,state TEXT NOT NULL DEFAULT 'pending',api TEXT,selection TEXT,attempts INTEGER NOT NULL DEFAULT 0,retry_at INTEGER NOT NULL DEFAULT 0,last_error TEXT,fallback INTEGER NOT NULL DEFAULT 0,sources INTEGER NOT NULL DEFAULT 4";
        for (release, columns, dated) in [
            ("0.3.0", "", None),
            ("0.4.0", ",failing_since INTEGER", Some(77)),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("legacy-beacon.sqlite");
            let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
            sqlx::raw_sql("DROP TABLE epoch_work; DROP TABLE epoch_relay_breaker;")
                .execute(&journal.pool)
                .await
                .unwrap();
            // The statement is built from the constants above alone.
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!("{table}{columns}); INSERT INTO epoch_work(key,registry,catalog,epoch,start,state,api,selection) VALUES('legacy','r','c',1,200,'prepared','first-packet','first-selection');")))
                .execute(&journal.pool)
                .await
                .unwrap();
            if let Some(since) = dated {
                sqlx::query("UPDATE epoch_work SET failing_since=? WHERE key='legacy'")
                    .bind(since)
                    .execute(&journal.pool)
                    .await
                    .unwrap();
            }
            journal.pool.close().await;
            for _ in 0..2 {
                let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
                // Every column of a run is there, and opening again adds nothing.
                let columns: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM pragma_table_info('epoch_work') WHERE name IN ('failing_since','failed_at','failed_rpc')",
                )
                .fetch_one(&journal.pool)
                .await
                .unwrap();
                assert_eq!(columns, 3, "{release}");
                let saved = work(&journal.pool, "legacy").await.unwrap();
                assert_eq!(
                    (
                        saved.api.as_deref(),
                        saved.selection.as_deref(),
                        saved.in_flight
                    ),
                    (Some("first-packet"), Some("first-selection"), false),
                    "{release}"
                );
                // A run the earlier release dated has no time of its last failure: it counts as one that is over.
                assert_eq!(
                    (saved.failing_since, saved.failed_at, saved.failed_rpc),
                    (dated, None, false),
                    "{release}"
                );
                assert_eq!(
                    relay_open(&journal.pool, "https://relay.example", 1)
                        .await
                        .unwrap(),
                    None
                );
                journal.pool.close().await;
            }
            // Keeper 0.3.0 still opens the journal this release leaves: its statements name the columns they use, so the
            // nullable ones added here change nothing for them.
            let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
            sqlx::query("INSERT OR IGNORE INTO epoch_work(key,registry,catalog,epoch,start,sources) VALUES('next','r','c',2,400,1)")
                .execute(&journal.pool).await.unwrap();
            sqlx::query("UPDATE epoch_work SET state=CASE WHEN ?=0 THEN 'blocked' ELSE state END,retry_at=?,last_error=? WHERE key='next' AND api IS NULL AND state IN ('pending','prepared')")
                .bind(1).bind(130).bind("older failure").execute(&journal.pool).await.unwrap();
            let older = sqlx::query("SELECT epoch_work.*,EXISTS(SELECT 1 FROM txs WHERE txs.job=epoch_work.key AND txs.state!='resolved') AS in_flight FROM epoch_work WHERE key='next'")
                .fetch_one(&journal.pool).await.unwrap();
            assert_eq!(
                (
                    older.get::<String, _>("state"),
                    older.get::<i64, _>("sources"),
                    older.get::<i64, _>("retry_at"),
                    older.get::<Option<String>, _>("last_error")
                ),
                ("pending".into(), 1, 130, Some("older failure".into())),
                "{release}"
            );
            journal.pool.close().await;
        }
    }
    #[tokio::test]
    async fn relay_circuits_open_per_url_after_repeated_failures_and_close_on_a_valid_answer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay-breaker.sqlite");
        let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
        let (relay, other) = ("https://api.drand.sh", "https://api2.drand.sh");
        for n in 1..BREAKER_FAILURES {
            relay_record(&journal.pool, relay, true, 1000)
                .await
                .unwrap();
            assert_eq!(
                relay_open(&journal.pool, relay, 1000).await.unwrap(),
                None,
                "{n}"
            );
        }
        relay_record(&journal.pool, relay, true, 1000)
            .await
            .unwrap();
        assert_eq!(
            relay_open(&journal.pool, relay, 1000).await.unwrap(),
            Some((BREAKER_FAILURES, BREAKER_COOLDOWN_SECONDS))
        );
        // Another relay is unaffected, and the open circuit survives a restart.
        assert_eq!(relay_open(&journal.pool, other, 1000).await.unwrap(), None);
        journal.pool.close().await;
        let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
        assert_eq!(
            relay_open(&journal.pool, relay, 1000 + BREAKER_COOLDOWN_SECONDS - 1)
                .await
                .unwrap(),
            Some((BREAKER_FAILURES, 1))
        );
        // After the cooldown a probe goes out; its failure reopens the circuit for another cooldown.
        let probe = 1000 + BREAKER_COOLDOWN_SECONDS;
        assert_eq!(relay_open(&journal.pool, relay, probe).await.unwrap(), None);
        relay_record(&journal.pool, relay, true, probe)
            .await
            .unwrap();
        assert_eq!(
            relay_open(&journal.pool, relay, probe).await.unwrap(),
            Some((BREAKER_FAILURES + 1, BREAKER_COOLDOWN_SECONDS))
        );
        // A valid answer closes it and restarts the count.
        relay_record(&journal.pool, relay, false, probe + 1)
            .await
            .unwrap();
        assert_eq!(
            relay_open(&journal.pool, relay, probe + 1).await.unwrap(),
            None
        );
        relay_record(&journal.pool, relay, true, probe + 2)
            .await
            .unwrap();
        assert_eq!(
            relay_open(&journal.pool, relay, probe + 2).await.unwrap(),
            None
        );
        journal.pool.close().await;
    }
    #[tokio::test]
    async fn failures_and_valid_answers_that_interleave_on_one_circuit_never_fail_a_record() {
        // The failures of one fetch and the late valid answers of another are recorded by different tasks on one journal
        // connection, statement by statement: a failure that took two statements could find its circuit closed between them.
        let dir = tempfile::tempdir().unwrap();
        let journal = crate::journal::Journal::open(&dir.path().join("race.sqlite"), "scope")
            .await
            .unwrap();
        let relay = "https://relay.example";
        let mut tasks = Vec::new();
        for n in 0..96u64 {
            let pool = journal.pool.clone();
            tasks.push(tokio::spawn(async move {
                relay_record(&pool, relay, n % 4 != 0, 1000 + n).await
            }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        // Whatever the order, a circuit is never left with more failures than the tasks that failed it.
        let open = relay_open(&journal.pool, relay, 1000).await.unwrap();
        assert!(open.is_none_or(|(failures, _)| (3..=72).contains(&failures)));
        journal.pool.close().await;
    }
    #[tokio::test]
    async fn beacon_fetch_failures_back_off_and_date_the_run_they_belong_to() {
        let dir = tempfile::tempdir().unwrap();
        let journal =
            crate::journal::Journal::open(&dir.path().join("beacon-retry.sqlite"), "scope")
                .await
                .unwrap();
        let pool = &journal.pool;
        sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start) VALUES('beacon','r','c',1,200)").execute(pool).await.unwrap();
        // The run of a row: when it started, when its last failure was, whether that was a chain read, and the row's retry
        // time and state.
        let run = |key: &'static str| async move {
            let saved: (Option<i64>, Option<i64>, Option<i64>, i64, String) = sqlx::query_as(
                "SELECT failing_since,failed_at,failed_rpc,retry_at,state FROM epoch_work WHERE key=?",
            )
            .bind(key)
            .fetch_one(pool)
            .await
            .unwrap();
            saved
        };
        let fail = |attempt: i64, now: u64, chain_read: bool| async move {
            let error = anyhow::anyhow!("no relay");
            let failure = if chain_read {
                FetchFailure::BeaconRpc(error)
            } else {
                FetchFailure::Beacon(error)
            };
            record_failure(pool, "beacon", attempt, failure, now)
                .await
                .unwrap();
        };
        assert_eq!(beacon::RUN_GAP_SECONDS, 15);
        // The retry backs off from 2 seconds, doubling, to 30, and the run starts with the first failure.
        for (attempt, now, wait) in [(1, 100, 2), (2, 103, 4), (3, 110, 8), (4, 118, 16)] {
            fail(attempt, now, false).await;
            assert_eq!(
                run("beacon").await,
                (
                    Some(100),
                    Some(now as i64),
                    Some(0),
                    (now + wait) as i64,
                    "pending".into()
                ),
                "attempt {attempt}"
            );
        }
        // A failure more than 15 seconds after the one before starts a new run (here after the 16-second back-off), so
        // a run that stopped long ago is not mistaken for the one that is going on; exactly 15 seconds continues it.
        fail(5, 134, false).await;
        assert_eq!(
            run("beacon").await,
            (Some(134), Some(134), Some(0), 164, "pending".into())
        );
        fail(6, 149, false).await;
        assert_eq!(run("beacon").await.0, Some(134));
        fail(7, 165, false).await;
        assert_eq!(
            run("beacon").await,
            (Some(165), Some(165), Some(0), 195, "pending".into())
        );
        // Whatever a legacy row says of its run, it has no last failure: the next failure starts the run.
        sqlx::query("UPDATE epoch_work SET failing_since=50,failed_at=NULL,failed_rpc=NULL WHERE key='beacon'")
            .execute(pool)
            .await
            .unwrap();
        fail(8, 170, false).await;
        assert_eq!(run("beacon").await.0, Some(170));
        // The kind of the last failure is kept with it: the keeper's own chain read, or the relays.
        fail(9, 172, true).await;
        assert_eq!(
            run("beacon").await,
            (Some(170), Some(172), Some(1), 202, "pending".into())
        );
        fail(10, 174, false).await;
        assert_eq!(run("beacon").await.2, Some(0));
        // A permanent failure blocks the source like any other.
        record_failure(
            pool,
            "beacon",
            10,
            FetchFailure::Permanent(anyhow::anyhow!("refused")),
            1100,
        )
        .await
        .unwrap();
        assert_eq!(run("beacon").await.4, "blocked");
        journal.pool.close().await;
    }
    #[tokio::test]
    async fn a_failed_fetch_of_a_beacon_epoch_is_a_warning_for_its_first_attempt_and_for_every_five_minutes_of_it()
     {
        let dir = tempfile::tempdir().unwrap();
        let journal = crate::journal::Journal::open(&dir.path().join("loud.sqlite"), "scope")
            .await
            .unwrap();
        let pool = &journal.pool;
        sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start) VALUES('beacon','r','c',1,200)").execute(pool).await.unwrap();
        // A row that never failed warns at its first attempt, and at a later one too if nothing tells when the last
        // failure was, as in a journal that is older than that time.
        assert!(failure_is_loud(pool, "beacon", 1, 1000).await.unwrap());
        assert!(failure_is_loud(pool, "beacon", 2, 1000).await.unwrap());
        let mut warnings = Vec::new();
        for (attempt, now) in (1..).zip((1000..2000).step_by(31)) {
            if failure_is_loud(pool, "beacon", attempt, now).await.unwrap() {
                warnings.push(now);
            }
            record_failure(
                pool,
                "beacon",
                attempt,
                FetchFailure::Beacon(anyhow::anyhow!("no relay")),
                now,
            )
            .await
            .unwrap();
        }
        // The first attempt, then the first failure after each boundary of five minutes (1200, 1500, 1800).
        assert_eq!(warnings, [1000, 1217, 1527, 1806]);
        // Work that is not in the journal is the first attempt of its own.
        assert!(failure_is_loud(pool, "nothing", 1, 5).await.unwrap());
        journal.pool.close().await;
    }
    #[test]
    fn live_demand_pulls_a_beacon_retry_forward_to_two_seconds_after_its_last_failure() {
        let work = |retry_at, failed_at| Work {
            key: "epoch".into(),
            epoch: 1,
            start: 200,
            state: "pending".into(),
            api: None,
            selection: None,
            fallback: 0,
            sources: 1,
            attempts: 6,
            retry_at,
            last_error: Some("no relay".into()),
            failing_since: failed_at,
            failed_at,
            failed_rpc: false,
            in_flight: false,
        };
        // Without a failure time, a beacon that never failed or whose source was refused, the retry time stands.
        for demand in [false, true] {
            assert_eq!(work(500, None).retry_due(demand), 500);
        }
        // Backed off to 30 seconds after a failure at 100: without demand it waits, with demand it is due after two.
        assert_eq!(work(130, Some(100)).retry_due(false), 130);
        assert_eq!(work(130, Some(100)).retry_due(true), 102);
        // Demand never delays a retry that is already sooner.
        assert_eq!(work(101, Some(100)).retry_due(true), 101);
        assert_eq!(work(0, Some(100)).retry_due(true), 0);
        assert_eq!(work(i64::MAX, Some(i64::MAX)).retry_due(true), i64::MAX);
    }
    #[tokio::test]
    async fn a_consistent_beacon_is_prepared_and_an_inconsistent_one_is_refused_like_any_other() {
        let dir = tempfile::tempdir().unwrap();
        let journal =
            crate::journal::Journal::open(&dir.path().join("beacon-source.sqlite"), "scope")
                .await
                .unwrap();
        let pool = &journal.pool;
        sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start,attempts) VALUES('beacon','r','c',1,200,1)").execute(pool).await.unwrap();
        let recipe = beacon_recipe();
        let chosen = selection(6, &recipe.canonical_request);
        // A beacon's relays are configured on their own: nothing else is needed to prepare it.
        assert!(
            source_or_refuse(pool, "beacon", 1, &chosen, &recipe, 100)
                .await
                .unwrap()
        );
        assert_eq!(work(pool, "beacon").await.unwrap().state, "pending");
        // The registry's selection must be exactly this recipe's request.
        let mut wrong_hash = chosen.clone();
        wrong_hash.queryHash = B256::ZERO;
        for (selection, recipe) in [
            (wrong_hash, recipe.clone()),
            (selection(6, &builtin(2).canonical_request), recipe.clone()),
            (
                chosen.clone(),
                RegisteredRecipe {
                    beacon: None,
                    ..recipe.clone()
                },
            ),
            (
                chosen.clone(),
                RegisteredRecipe {
                    body: format!(r#"["drand","0x{}"]"#, "22".repeat(32)),
                    ..recipe.clone()
                },
            ),
        ] {
            let error = check_recipe(&selection, &recipe).unwrap_err().to_string();
            assert!(error.contains("refusing to fetch"), "{error}");
        }
        let mut unscheduled = beacon_registration();
        unscheduled.period = 0;
        let refused = RegisteredRecipe {
            beacon: Some(unscheduled),
            ..recipe.clone()
        };
        assert!(
            !source_or_refuse(pool, "beacon", 1, &chosen, &refused, 100)
                .await
                .unwrap()
        );
        let saved = work(pool, "beacon").await.unwrap();
        assert_eq!(saved.state, "blocked");
        assert!(saved.last_error.unwrap().contains("refusing to fetch"));
        journal.pool.close().await;
    }
    #[tokio::test]
    async fn only_beacon_recipes_are_asked_for_their_registration_and_each_recipe_is_read_once() {
        use crate::abi::Beacon;
        use alloy_sol_types::SolCall;
        use std::sync::atomic::Ordering;
        let signed = builtin(2);
        let beaconed = beacon_recipe();
        let registration = beacon_registration();
        let (get_recipe, beacon_of) = (
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        );
        let (calls, beacon_calls) = (get_recipe.clone(), beacon_of.clone());
        let (node, _) = beacon::fixture::serve({
            let (signed, beaconed, registration) =
                (signed.clone(), beaconed.clone(), registration.clone());
            move |_, body| {
                let request: Value = serde_json::from_slice(body).unwrap();
                assert_eq!(request["method"], "eth_call");
                let data: Bytes =
                    serde_json::from_value(request["params"][0]["data"].clone()).unwrap();
                let result = if data[..4] == E::getRecipeCall::SELECTOR {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let call = E::getRecipeCall::abi_decode(&data).unwrap();
                    let recipe = if call.recipe == 6 { &beaconed } else { &signed };
                    E::getRecipeCall::abi_encode_returns(&E::getRecipeReturn {
                        queryHash: keccak256(recipe.canonical_request.as_bytes()),
                        canonicalRequest: recipe.canonical_request.clone(),
                        template: recipe.template.clone(),
                        body: recipe.body.clone(),
                    })
                } else {
                    assert_eq!(data[..4], E::beaconOfCall::SELECTOR);
                    beacon_calls.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(E::beaconOfCall::abi_decode(&data).unwrap().recipe, 6);
                    E::beaconOfCall::abi_encode_returns(&Beacon {
                        verifier: registration.verifier,
                        genesis: registration.genesis,
                        period: registration.period,
                        chainHash: registration.chain_hash,
                        publicKey: registration.public_key.clone(),
                    })
                };
                beacon::fixture::answer(
                    200,
                    json!({"jsonrpc":"2.0","id":request["id"],"result":Bytes::from(result)})
                        .to_string(),
                )
            }
        })
        .await;
        let rpc = Rpc::new(vec![node]).unwrap();
        let publisher = Publisher::new(
            Address::repeat_byte(1),
            B256::repeat_byte(2),
            beacon::DrandRelays::default(),
        )
        .unwrap();
        // A signed recipe is read with getRecipe alone; a beacon-shaped one also with beaconOf.
        assert_eq!(*publisher.recipe(&rpc, 2).await.unwrap(), signed);
        assert_eq!(beacon_of.load(Ordering::SeqCst), 0);
        assert_eq!(*publisher.recipe(&rpc, 6).await.unwrap(), beaconed);
        assert_eq!(beacon_of.load(Ordering::SeqCst), 1);
        // Recipes never change once registered, so neither read is repeated.
        for id in [2, 6, 2, 6] {
            publisher.recipe(&rpc, id).await.unwrap();
        }
        assert_eq!(
            (
                get_recipe.load(Ordering::SeqCst),
                beacon_of.load(Ordering::SeqCst)
            ),
            (2, 1)
        );
    }
    /// How the registry node of `poll_node` answers `beaconOf`.
    #[derive(Clone, Copy)]
    enum BeaconOf {
        /// The node answers that the call reverts, as a registry that has no beacons does.
        Reverts,
        /// A JSON-RPC error that says nothing of the kind.
        Fails,
        /// HTTP 500.
        Down,
    }
    /// The registry node of a `poll` of epoch 1, which starts at block 200 and has `sources` sources, the first of them
    /// the drand recipe 6. It serves what `poll` reads, answers `beaconOf` as `beacon_of` says and names the calls it
    /// served, in order.
    async fn poll_node(beacon_of: BeaconOf, sources: u64) -> (Rpc, Arc<Mutex<Vec<&'static str>>>) {
        use crate::abi::EpochRecord;
        use alloy_sol_types::SolCall;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let served = calls.clone();
        let recipe = beacon_recipe();
        let (url, _) = beacon::fixture::serve(move |_, body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            let reply = |result: Value| {
                beacon::fixture::answer(
                    200,
                    json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string(),
                )
            };
            let error = |code: i64, message: &str| {
                beacon::fixture::answer(
                    200,
                    json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":code,"message":message}}).to_string(),
                )
            };
            if request["method"] == "eth_getBlockByNumber" {
                return reply(json!({"number":"0xfa","hash":B256::repeat_byte(9),"timestamp":"0x3e8","baseFeePerGas":"0x1"}));
            }
            assert_eq!(request["method"], "eth_call");
            let data: Bytes = serde_json::from_value(request["params"][0]["data"].clone()).unwrap();
            let selector = &data[..4];
            let answer = |name: &'static str, result: Vec<u8>| {
                served.lock().unwrap().push(name);
                reply(json!(Bytes::from(result)))
            };
            if selector == E::nextEpochToPrepareCall::SELECTOR {
                answer("nextEpochToPrepare", E::nextEpochToPrepareCall::abi_encode_returns(&1u64))
            } else if selector == E::epochStartCall::SELECTOR {
                answer("epochStart", E::epochStartCall::abi_encode_returns(&200u64))
            } else if selector == E::sourceCountAtCall::SELECTOR {
                answer("sourceCountAt", E::sourceCountAtCall::abi_encode_returns(&U256::from(sources)))
            } else if selector == E::getEpochCall::SELECTOR {
                answer("getEpoch", E::getEpochCall::abi_encode_returns(&EpochRecord {
                    epochHash: B256::ZERO,
                    catalogHash: B256::ZERO,
                    anchorHash: B256::ZERO,
                    source: 0,
                    queryHash: B256::ZERO,
                    dataHash: B256::ZERO,
                    attestationHash: B256::ZERO,
                    signedAt: U256::ZERO,
                    committedBlock: 0,
                }))
            } else if selector == E::getEpochFallbackSelectionCall::SELECTOR {
                answer("getEpochFallbackSelection", E::getEpochFallbackSelectionCall::abi_encode_returns(&selection(6, &recipe.canonical_request)))
            } else if selector == E::getRecipeCall::SELECTOR {
                answer("getRecipe", E::getRecipeCall::abi_encode_returns(&E::getRecipeReturn {
                    queryHash: keccak256(recipe.canonical_request.as_bytes()),
                    canonicalRequest: recipe.canonical_request.clone(),
                    template: recipe.template.clone(),
                    body: recipe.body.clone(),
                }))
            } else if selector == E::beaconOfCall::SELECTOR {
                served.lock().unwrap().push("beaconOf");
                match beacon_of {
                    BeaconOf::Reverts => error(3, "execution reverted"),
                    BeaconOf::Fails => error(-32000, "node failure"),
                    BeaconOf::Down => beacon::fixture::answer(500, ""),
                }
            } else {
                panic!("unexpected call {selector:?}")
            }
        })
        .await;
        (Rpc::new(vec![url]).unwrap(), calls)
    }
    fn poll_head() -> Head {
        Head {
            hash: B256::ZERO,
            number: 250,
            timestamp: 1000,
            base_fee: 1,
        }
    }
    fn publisher_of(registry: u8) -> Publisher {
        Publisher::new(
            Address::repeat_byte(registry),
            B256::repeat_byte(2),
            beacon::DrandRelays::default(),
        )
        .unwrap()
    }
    fn count(calls: &Mutex<Vec<&'static str>>, name: &str) -> usize {
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| **call == name)
            .count()
    }
    #[tokio::test]
    async fn a_drand_recipe_that_beaconof_reverts_for_fails_its_source_for_good_on_the_second_attempt_and_other_failures_are_retried()
     {
        let dir = tempfile::tempdir().unwrap();
        let journal =
            crate::journal::Journal::open(&dir.path().join("not-a-beacon.sqlite"), "scope")
                .await
                .unwrap();
        let pool = &journal.pool;
        // The epoch's attempt is due again: what a retry waits for.
        let due = |key: String| async move {
            sqlx::query("UPDATE epoch_work SET retry_at=0 WHERE key=?")
                .bind(key)
                .execute(pool)
                .await
                .unwrap();
        };
        // A registry that cannot hold a beacon: the first attempt is tried again, since one endpoint's revert may be a
        // glitch, and the second refuses the source and launches no fetch, so that a catalog with another source falls
        // back to it when its window opens, instead of the epoch stalling.
        let (rpc, calls) = poll_node(BeaconOf::Reverts, 2).await;
        let publisher = publisher_of(1);
        let key = publisher.key(1);
        let error = publisher
            .poll(&rpc, pool, &poll_head(), None)
            .await
            .unwrap_err();
        assert!(error.is::<NotABeacon>(), "{error}");
        let saved = work(pool, &key).await.unwrap();
        assert_eq!(
            (saved.state.as_str(), saved.attempts, saved.last_error),
            ("pending", 1, None)
        );
        due(key.clone()).await;
        assert!(
            publisher
                .poll(&rpc, pool, &poll_head(), None)
                .await
                .unwrap()
                .is_none()
        );
        let saved = work(pool, &key).await.unwrap();
        assert_eq!(
            (saved.state.as_str(), saved.attempts, saved.sources),
            ("blocked", 2, 2)
        );
        assert!(
            saved.last_error.as_deref().unwrap().starts_with(
                "The registry cannot serve this recipe as a beacon: beaconOf reverted"
            ),
            "{:?}",
            saved.last_error
        );
        assert!(publisher.fetch.lock().unwrap().is_none());
        assert_eq!(
            (count(&calls, "getRecipe"), count(&calls, "beaconOf")),
            (2, 2)
        );
        assert!(
            advance_fallback(pool, &saved, 200 + FALLBACK_DELAY_BLOCKS)
                .await
                .unwrap()
        );
        assert_eq!(work(pool, &key).await.unwrap().state, "pending");
        // The failed read is not remembered: the recipe is read again, and refused again, when it is next selected.
        let error = publisher.recipe(&rpc, 6).await.unwrap_err();
        assert!(error.is::<NotABeacon>(), "{error}");
        assert_eq!(count(&calls, "beaconOf"), 3);
        // Any other failure of the read is the RPC's, and the epoch is tried again at every tick, however often: it is
        // neither blocked nor failed for good.
        for (registry, mode) in [(2, BeaconOf::Fails), (3, BeaconOf::Down)] {
            let (rpc, calls) = poll_node(mode, 1).await;
            let publisher = publisher_of(registry);
            let key = publisher.key(1);
            for attempt in 1..=3 {
                let error = publisher
                    .poll(&rpc, pool, &poll_head(), None)
                    .await
                    .unwrap_err();
                assert!(!error.is::<NotABeacon>(), "{error}");
                let saved = work(pool, &key).await.unwrap();
                assert_eq!(
                    (
                        saved.state.as_str(),
                        saved.last_error.as_deref(),
                        saved.attempts
                    ),
                    ("pending", None, attempt)
                );
                due(key.clone()).await;
            }
            assert_eq!(count(&calls, "beaconOf"), 3);
        }
        journal.pool.close().await;
    }
    #[tokio::test]
    async fn live_demand_retries_a_backed_off_beacon_epoch_two_seconds_after_its_last_failure() {
        let dir = tempfile::tempdir().unwrap();
        let journal = crate::journal::Journal::open(&dir.path().join("demand.sqlite"), "scope")
            .await
            .unwrap();
        let pool = &journal.pool;
        let (rpc, calls) = poll_node(BeaconOf::Reverts, 1).await;
        let publisher = publisher_of(1);
        let key = publisher.key(1);
        let now = i64::try_from(crate::health::now().unwrap()).unwrap();
        // The fourth fetch failed just now and backed off for sixteen seconds.
        sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start,sources,attempts,retry_at,last_error,failing_since,failed_at,failed_rpc) VALUES(?,'r','c',1,200,1,4,?,'no relay',?,?,0)")
            .bind(&key).bind(now + 16).bind(now - 30).bind(now).execute(pool).await.unwrap();
        // Nothing waits on the epoch: the back-off stands and nothing is even read for it.
        for _ in 0..2 {
            assert!(
                publisher
                    .poll(&rpc, pool, &poll_head(), None)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(count(&calls, "getEpochFallbackSelection"), 0);
        // Live paid demand does not wait for it either, until two seconds have passed since the failure.
        assert!(
            publisher
                .poll(&rpc, pool, &poll_head(), Some(1))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(count(&calls, "getEpochFallbackSelection"), 0);
        assert_eq!(work(pool, &key).await.unwrap().attempts, 4);
        sqlx::query("UPDATE epoch_work SET failed_at=? WHERE key=?")
            .bind(now - 2)
            .bind(&key)
            .execute(pool)
            .await
            .unwrap();
        // Without demand it still waits out the back-off.
        assert!(
            publisher
                .poll(&rpc, pool, &poll_head(), None)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(count(&calls, "getEpochFallbackSelection"), 0);
        // With demand it is fetched at once, as if its last failure had backed off by two seconds.
        assert!(
            publisher
                .poll(&rpc, pool, &poll_head(), Some(1))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(count(&calls, "getEpochFallbackSelection"), 1);
        let saved = work(pool, &key).await.unwrap();
        assert_eq!(saved.attempts, 5);
        journal.pool.close().await;
    }
    /// The registry node of epochs of 200 blocks starting at 200 × the epoch, in a catalog of two sources: the signed recipe 2
    /// and the drand recipe 6. Epoch 1 selects the signed one first and the beacon as its fallback; every later epoch selects
    /// the beacon. `current` is the epoch it reports as current, and its latest block is 50 blocks into that epoch. It names
    /// the calls it served, in order.
    async fn catalog_node(current: Arc<AtomicU64>) -> (Rpc, Arc<Mutex<Vec<&'static str>>>) {
        use crate::abi::{Beacon, EpochRecord};
        use alloy_sol_types::SolCall;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let served = calls.clone();
        let (signed, beaconed, registration) = (builtin(2), beacon_recipe(), beacon_registration());
        let (url, _) = beacon::fixture::serve(move |_, body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            let epoch = current.load(Ordering::SeqCst);
            let reply = |result: Value| {
                beacon::fixture::answer(
                    200,
                    json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string(),
                )
            };
            if request["method"] == "eth_getBlockByNumber" {
                // The latest block is in the current epoch; any other is the block asked for.
                let number = match request["params"][0].as_str().unwrap() {
                    "latest" | "finalized" => format!("0x{:x}", 200 * epoch + 50),
                    asked => asked.to_owned(),
                };
                return reply(json!({"number":number,"hash":B256::repeat_byte(9),"timestamp":"0x3e8","baseFeePerGas":"0x1"}));
            }
            assert_eq!(request["method"], "eth_call");
            let data: Bytes = serde_json::from_value(request["params"][0]["data"].clone()).unwrap();
            let selector = &data[..4];
            let answer = |name: &'static str, result: Vec<u8>| {
                served.lock().unwrap().push(name);
                reply(json!(Bytes::from(result)))
            };
            if selector == E::nextEpochToPrepareCall::SELECTOR {
                answer("nextEpochToPrepare", E::nextEpochToPrepareCall::abi_encode_returns(&epoch))
            } else if selector == E::epochStartCall::SELECTOR {
                let asked = E::epochStartCall::abi_decode(&data).unwrap().epochId;
                answer("epochStart", E::epochStartCall::abi_encode_returns(&(200 * asked)))
            } else if selector == E::sourceCountAtCall::SELECTOR {
                answer("sourceCountAt", E::sourceCountAtCall::abi_encode_returns(&U256::from(2)))
            } else if selector == E::getEpochCall::SELECTOR {
                answer("getEpoch", E::getEpochCall::abi_encode_returns(&EpochRecord {
                    epochHash: B256::ZERO,
                    catalogHash: B256::ZERO,
                    anchorHash: B256::ZERO,
                    source: 0,
                    queryHash: B256::ZERO,
                    dataHash: B256::ZERO,
                    attestationHash: B256::ZERO,
                    signedAt: U256::ZERO,
                    committedBlock: 0,
                }))
            } else if selector == E::getEpochFallbackSelectionCall::SELECTOR {
                let asked = E::getEpochFallbackSelectionCall::abi_decode(&data).unwrap();
                let signed_first = asked.epochId == 1;
                let (id, recipe) = if signed_first == (asked.attempt == 0) { (2, &signed) } else { (6, &beaconed) };
                answer("getEpochFallbackSelection", E::getEpochFallbackSelectionCall::abi_encode_returns(&selection(id, &recipe.canonical_request)))
            } else if selector == E::getRecipeCall::SELECTOR {
                let recipe = if E::getRecipeCall::abi_decode(&data).unwrap().recipe == 2 { &signed } else { &beaconed };
                answer("getRecipe", E::getRecipeCall::abi_encode_returns(&E::getRecipeReturn {
                    queryHash: keccak256(recipe.canonical_request.as_bytes()),
                    canonicalRequest: recipe.canonical_request.clone(),
                    template: recipe.template.clone(),
                    body: recipe.body.clone(),
                }))
            } else if selector == E::beaconOfCall::SELECTOR {
                answer("beaconOf", E::beaconOfCall::abi_encode_returns(&Beacon {
                    verifier: registration.verifier,
                    genesis: registration.genesis,
                    period: registration.period,
                    chainHash: registration.chain_hash,
                    publicKey: registration.public_key.clone(),
                }))
            } else {
                panic!("unexpected call {selector:?}")
            }
        })
        .await;
        (Rpc::new(vec![url]).unwrap(), calls)
    }
    fn head_at(number: u64) -> Head {
        Head {
            hash: B256::ZERO,
            number,
            timestamp: 1000,
            base_fee: 1,
        }
    }
    #[tokio::test]
    async fn a_signed_recipe_in_the_catalog_blocks_its_epoch_alone_and_the_epochs_after_it_are_prepared_as_usual()
     {
        let dir = tempfile::tempdir().unwrap();
        let journal =
            crate::journal::Journal::open(&dir.path().join("signed-catalog.sqlite"), "scope")
                .await
                .unwrap();
        let pool = &journal.pool;
        let current = Arc::new(AtomicU64::new(1));
        let (rpc, calls) = catalog_node(current.clone()).await;
        // Relays that nothing listens on: the beacon source below is only ever prepared, never served.
        let relays =
            beacon::DrandRelays::parse(Some(&beacon::fixture::refused().await), true).unwrap();
        let publisher =
            Publisher::new(Address::repeat_byte(1), B256::repeat_byte(2), relays).unwrap();
        let faults = || async {
            crate::health::assess(&journal, true, 1000, 20, None, 120)
                .await
                .unwrap()
                .faults
        };
        // Epoch 1 selects the signed recipe 2: it is refused for good, no fetch is launched and the tick is not in error, however
        // often the epoch is polled. The registry is asked for the selection once, and for no registration at all.
        let key = publisher.key(1);
        for _ in 0..3 {
            assert!(
                publisher
                    .poll(&rpc, pool, &head_at(210), None)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        assert!(publisher.fetch.lock().unwrap().is_none());
        let saved = work(pool, &key).await.unwrap();
        assert_eq!(
            (saved.state.as_str(), saved.fallback, saved.attempts),
            ("blocked", 0, 1)
        );
        assert!(saved.last_error.unwrap().contains(UNSUPPORTED_RECIPE));
        assert_eq!(count(&calls, "getEpochFallbackSelection"), 1);
        assert_eq!(count(&calls, "getRecipe"), 1);
        assert_eq!(count(&calls, "beaconOf"), 0);
        assert_eq!(faults().await, ["epoch_recipe_unsupported:2"]);
        // Its fallback window is not open at block 219 and is at block 220: the beacon source is prepared then, and the
        // fault is gone.
        assert!(
            publisher
                .poll(&rpc, pool, &head_at(219), None)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(work(pool, &key).await.unwrap().fallback, 0);
        assert!(
            publisher
                .poll(&rpc, pool, &head_at(220), None)
                .await
                .unwrap()
                .is_none()
        );
        publisher.finish_fetch().await.unwrap();
        let fallback = work(pool, &key).await.unwrap();
        assert_eq!(
            (
                fallback.state.as_str(),
                fallback.fallback,
                fallback.attempts
            ),
            ("pending", 1, 1)
        );
        // The fetch ran for the beacon: this chain time is before its genesis, so there was no round to ask the relays for.
        assert!(
            fallback
                .last_error
                .unwrap()
                .contains("The beacon has no round yet")
        );
        assert_eq!(count(&calls, "beaconOf"), 1);
        assert!(faults().await.is_empty());
        // The next epoch selects the beacon and is prepared like any other, with the earlier one still blocked or not.
        current.store(2, Ordering::SeqCst);
        assert!(
            publisher
                .poll(&rpc, pool, &head_at(450), None)
                .await
                .unwrap()
                .is_none()
        );
        publisher.finish_fetch().await.unwrap();
        let next = work(pool, &publisher.key(2)).await.unwrap();
        assert_eq!(
            (next.state.as_str(), next.fallback, next.attempts),
            ("pending", 0, 1)
        );
        assert!(
            next.last_error
                .unwrap()
                .contains("The beacon has no round yet")
        );
        assert!(faults().await.is_empty());
        journal.pool.close().await;
    }
    #[tokio::test]
    async fn a_signed_packet_an_earlier_release_saved_is_handed_on_as_it_is_and_nothing_is_fetched_for_it()
     {
        let dir = tempfile::tempdir().unwrap();
        let journal =
            crate::journal::Journal::open(&dir.path().join("signed-packet.sqlite"), "scope")
                .await
                .unwrap();
        let pool = &journal.pool;
        let current = Arc::new(AtomicU64::new(1));
        let (rpc, calls) = catalog_node(current).await;
        let publisher = publisher_of(1);
        let key = publisher.key(1);
        let signed = selection(2, &builtin(2).canonical_request);
        saved_packet(pool, &key, "prepared", Some(&signed), 1000).await;
        sqlx::query("UPDATE epoch_work SET registry=?,catalog=?,sources=2,attempts=1,retry_at=0,last_error=NULL,failing_since=NULL,failed_at=NULL,failed_rpc=NULL WHERE key=?")
            .bind(publisher.registry.to_string()).bind(publisher.catalog.to_string()).bind(&key).execute(pool).await.unwrap();
        // The epoch's work is prepared, so it is handed to the sender with the packet exactly as saved: whether it is still
        // fresh enough to publish is the sender's to judge (worker::epoch_terminal), and a packet that is not is blocked there.
        let ready = publisher
            .poll(&rpc, pool, &head_at(210), None)
            .await
            .unwrap()
            .expect("prepared work is handed on");
        assert_eq!(ready.key, key);
        assert!(ready.api.as_ref().unwrap().contains("\"data\""));
        assert!(!ready.stale_beacon(u64::MAX));
        assert!(publisher.fetch.lock().unwrap().is_none());
        assert_eq!(count(&calls, "getEpochFallbackSelection"), 0);
        assert_eq!(count(&calls, "getRecipe"), 0);
        assert!(
            crate::health::assess(&journal, true, 1000, 20, None, 120)
                .await
                .unwrap()
                .healthy
        );
        journal.pool.close().await;
    }
    /// Every table of a journal, schema and rows, as text, in a fixed order.
    async fn journal_text(pool: &SqlitePool) -> Vec<String> {
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        let mut text: Vec<String> = sqlx::query_scalar(
            "SELECT type||' '||name||' '||COALESCE(sql,'') FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        for table in tables {
            let columns: Vec<String> = sqlx::query_scalar(
                "SELECT 'quote('||name||')' FROM pragma_table_info(?) ORDER BY cid",
            )
            .bind(&table)
            .fetch_all(pool)
            .await
            .unwrap();
            // The statement is built from the journal's own table and column names.
            let rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT {} FROM {table} ORDER BY rowid",
                columns.join("||'|'||")
            )))
            .fetch_all(pool)
            .await
            .unwrap();
            text.extend(rows.into_iter().map(|row| format!("{table}: {row}")));
        }
        text
    }
    #[tokio::test]
    async fn a_journal_of_release_0_4_0_opens_unchanged_and_its_signed_work_ends_without_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("release-0.4.0.sqlite");
        let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
        let pool = &journal.pool;
        // What release 0.4.0 left in its journal beside the tables this release creates: the circuits of the Airnode gateways, and
        // the work of signed API sources in every state its fetches and its sender left it in.
        sqlx::raw_sql("CREATE TABLE epoch_breaker(airnode TEXT PRIMARY KEY,failures INTEGER NOT NULL,open_until INTEGER NOT NULL); INSERT INTO epoch_breaker VALUES('0x32f5eA20F05fdADfCD50Cb8eD920acE96D5f9f2c',3,99999999999),('0x511AcE8648D2f64260d50D036F8f8ce622d92137',1,0); INSERT INTO epoch_relay_breaker VALUES('https://api.drand.sh',1,0);")
            .execute(pool).await.unwrap();
        let signed = serde_json::to_string(&selection(2, &builtin(2).canonical_request)).unwrap();
        let beaconed =
            serde_json::to_string(&selection(6, &beacon_recipe().canonical_request)).unwrap();
        let packet = r#"{"timestamp":"0x64","data":"0x7b7d","signature":"0x00"}"#;
        for (key, epoch, state, api, selection, attempts, error, fallback) in [
            (
                "signed-idle",
                11,
                "prepared",
                Some(packet),
                Some(&signed),
                1,
                None,
                0,
            ),
            (
                "signed-blocked",
                12,
                "blocked",
                None,
                Some(&signed),
                3,
                Some("Epoch API HTTP 400"),
                2,
            ),
            (
                "signed-retrying",
                13,
                "pending",
                None,
                Some(&signed),
                2,
                Some("Epoch API transport: error sending request"),
                0,
            ),
            ("signed-committed", 14, "committed", None, None, 1, None, 0),
            (
                "beacon-prepared",
                15,
                "prepared",
                Some(packet),
                Some(&beaconed),
                1,
                None,
                0,
            ),
        ] {
            sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start,state,api,selection,attempts,retry_at,last_error,fallback,sources) VALUES(?,'r','c',?,?,?,?,?,?,99999999999,?,?,5)")
                .bind(key).bind(epoch).bind(200 * epoch).bind(state).bind(api).bind(selection).bind(attempts).bind(error).bind(fallback)
                .execute(pool).await.unwrap();
        }
        let before = journal_text(pool).await;
        assert!(
            before
                .iter()
                .any(|line| line.starts_with("epoch_breaker: "))
        );
        journal.pool.close().await;
        // Opening it again, as often as a restart does, changes nothing: no table is dropped or rebuilt, no row is touched.
        for _ in 0..2 {
            let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
            assert_eq!(journal_text(&journal.pool).await, before);
            let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
                .fetch_one(&journal.pool)
                .await
                .unwrap();
            assert_eq!(integrity, "ok");
            journal.pool.close().await;
        }
        // Its signed work ends the way any unused work does, without a single error: idle and blocked packets are retired
        // after 50 epochs and then compacted, the work that was still retrying is left to its own retry, and a beacon's
        // prepared packet is as it was. The circuits of the Airnode gateways are not touched.
        let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
        retire_idle_snapshots(&journal.pool, &head_at(12_500))
            .await
            .unwrap();
        journal.compact_history(100).await.unwrap();
        let state = |key: &'static str| {
            let pool = journal.pool.clone();
            async move { work(&pool, key).await.unwrap() }
        };
        for key in ["signed-idle", "signed-blocked"] {
            let retired = state(key).await;
            assert_eq!(
                (retired.state.as_str(), retired.api, retired.selection),
                ("expired", None, None),
                "{key}"
            );
        }
        let retrying = state("signed-retrying").await;
        assert_eq!((retrying.state.as_str(), retrying.attempts), ("pending", 2));
        assert_eq!(state("signed-committed").await.state, "committed");
        let kept = state("beacon-prepared").await;
        assert_eq!(
            (kept.state.as_str(), kept.api.as_deref()),
            ("prepared", Some(packet))
        );
        let breakers: Vec<(String, i64)> =
            sqlx::query_as("SELECT airnode,failures FROM epoch_breaker ORDER BY airnode")
                .fetch_all(&journal.pool)
                .await
                .unwrap();
        assert_eq!(
            breakers,
            [
                ("0x32f5eA20F05fdADfCD50Cb8eD920acE96D5f9f2c".to_owned(), 3),
                ("0x511AcE8648D2f64260d50D036F8f8ce622d92137".to_owned(), 1)
            ]
        );
        journal.pool.close().await;
    }
    #[tokio::test]
    async fn dropping_the_publisher_aborts_what_its_fetches_left_running() {
        struct Ended(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Ended {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let publisher = publisher_of(1);
        let stragglers = publisher.stragglers.clone();
        let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard = Ended(ended.clone());
        stragglers.spawn(async move {
            let _guard = guard;
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });
        tokio::task::yield_now().await;
        assert!(!ended.load(std::sync::atomic::Ordering::SeqCst));
        drop(publisher);
        // Aborted with the publisher, not left to end after its minute.
        tokio::time::timeout(std::time::Duration::from_secs(2), stragglers.settled())
            .await
            .unwrap();
        assert!(ended.load(std::sync::atomic::Ordering::SeqCst));
    }
    /// A beacon epoch's saved work as the keeper journals it: the registry's selection and the packet fetched for it.
    async fn saved_packet(
        pool: &SqlitePool,
        key: &str,
        state: &str,
        selection: Option<&EpochSelection>,
        timestamp: u64,
    ) {
        let packet = serde_json::to_string(&ApiProof {
            timestamp: U256::from(timestamp),
            data: Bytes::from_static(b"101"),
            signature: Bytes::from(vec![7u8; 64]),
        })
        .unwrap();
        let selection = selection.map(|selection| serde_json::to_string(selection).unwrap());
        sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start,state,api,selection,attempts,retry_at,last_error,failing_since,failed_at,failed_rpc) VALUES(?,'r','c',1,200,?,?,?,4,99,'earlier failure',50,60,1)")
            .bind(key).bind(state).bind(packet).bind(selection).execute(pool).await.unwrap();
    }
    /// A transaction the keeper journaled for an epoch's commit, in the state its nonce has reached.
    async fn journal_commit(pool: &SqlitePool, key: &str, kind: &str, state: &str) {
        sqlx::query("INSERT INTO txs(job,nonce,hash,raw,kind,fee,gas,priority,payload,created,state) VALUES(?,7,?,'raw',?,'1',21000,'1','0x',1,?)")
            .bind(key).bind(format!("hash-{key}-{kind}-{state}")).bind(kind).bind(state).execute(pool).await.unwrap();
    }
    /// The keeper's registry node: `head` is its latest block, and it answers `getEpoch` with a record whose hash is
    /// `published`, at that block only. `None` answers the read with a JSON-RPC error. The count is of requests served.
    async fn registry_node(
        head: u64,
        published: Option<B256>,
    ) -> (Rpc, Arc<std::sync::atomic::AtomicUsize>) {
        registry_node_at(head, published, published.unwrap_or_default()).await
    }
    /// `registry_node` for an epoch whose hash is `latest` at the latest block and `finalized` at the finalized one,
    /// where the chain has not finalized a block yet that it has already built.
    async fn registry_node_at(
        head: u64,
        latest: Option<B256>,
        finalized: B256,
    ) -> (Rpc, Arc<std::sync::atomic::AtomicUsize>) {
        use crate::abi::EpochRecord;
        use alloy_sol_types::SolCall;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let served = Arc::new(AtomicUsize::new(0));
        let counter = served.clone();
        let (url, _) = beacon::fixture::serve(move |_, body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            counter.fetch_add(1, Ordering::SeqCst);
            let reply = |result: Value| json!({"jsonrpc":"2.0","id":request["id"],"result":result});
            let answer = match request["method"].as_str().unwrap() {
                "eth_getBlockByNumber" => {
                    assert_eq!(request["params"][0], "latest");
                    reply(json!({"number":format!("0x{head:x}"),"hash":B256::repeat_byte(9),"timestamp":"0x64","baseFeePerGas":"0x1"}))
                }
                "eth_call" => match latest {
                    Some(latest) => {
                        // The epoch is read at the block that was the latest, not at a tag that can move on, and at
                        // the finalized block.
                        let epoch_hash = if request["params"][1] == "finalized" {
                            finalized
                        } else {
                            assert_eq!(request["params"][1], format!("0x{head:x}"));
                            latest
                        };
                        let data: Bytes =
                            serde_json::from_value(request["params"][0]["data"].clone()).unwrap();
                        assert_eq!(E::getEpochCall::abi_decode(&data).unwrap().epochId, 1);
                        reply(json!(Bytes::from(E::getEpochCall::abi_encode_returns(
                            &EpochRecord {
                                epochHash: epoch_hash,
                                catalogHash: B256::ZERO,
                                anchorHash: B256::ZERO,
                                source: 0,
                                queryHash: B256::ZERO,
                                dataHash: B256::ZERO,
                                attestationHash: B256::ZERO,
                                signedAt: U256::ZERO,
                                committedBlock: 0,
                            }
                        ))))
                    }
                    None => json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"node failure"}}),
                },
                method => panic!("unexpected {method}"),
            };
            beacon::fixture::answer(200, answer.to_string())
        })
        .await;
        (Rpc::new(vec![url]).unwrap(), served)
    }
    /// Whether refresh_stale_beacon discarded `saved`'s packet at `now`, asking `rpc` for the registry's latest state.
    async fn refreshed(pool: &SqlitePool, rpc: &Rpc, saved: &Work, now: u64) -> bool {
        refresh_stale_beacon(pool, rpc, Address::repeat_byte(0xaa), saved, now)
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn a_stale_beacon_packet_is_refreshed_once_every_commit_for_it_has_resolved_and_the_epoch_is_unpublished()
     {
        let dir = tempfile::tempdir().unwrap();
        let journal = crate::journal::Journal::open(&dir.path().join("refresh.sqlite"), "scope")
            .await
            .unwrap();
        let pool = &journal.pool;
        let (rpc, _) = registry_node(500, Some(B256::ZERO)).await;
        let beaconed = selection(6, &beacon_recipe().canonical_request);
        // Nothing was ever signed for the packet, or every commit signed for it resolved: mined and reverted, or
        // cancelled or replaced by a cancellation whose nonce then resolved.
        let histories: [(&str, &[(&str, &str)]); 4] = [
            ("never", &[]),
            ("reverted", &[("epoch", "resolved")]),
            (
                "cancelled",
                &[("epoch", "resolved"), ("epoch_cancel", "resolved")],
            ),
            ("cancellation-only", &[("epoch_cancel", "resolved")]),
        ];
        assert_eq!(beacon::BEACON_MAX_AGE, 200);
        for state in ["pending", "prepared", "blocked"] {
            for (history, commits) in histories {
                let key = format!("{state}-{history}");
                saved_packet(pool, &key, state, Some(&beaconed), 1000).await;
                for (kind, tx_state) in commits {
                    journal_commit(pool, &key, kind, tx_state).await;
                }
                let saved = work(pool, &key).await.unwrap();
                assert!(saved.is_beacon() && !saved.in_flight, "{key}");
                // The packet is stale from the second after BEACON_MAX_AGE, never before.
                for (now, stale) in [
                    (0, false),
                    (1000, false),
                    (1200, false),
                    (1201, true),
                    (100_000, true),
                ] {
                    assert_eq!(saved.stale_beacon(now), stale, "{key} {now}");
                }
                assert!(!refreshed(pool, &rpc, &saved, 1200).await, "{key}");
                assert!(work(pool, &key).await.unwrap().api.is_some(), "{key}");
                assert!(refreshed(pool, &rpc, &saved, 1201).await, "{key}");
                // It is discarded for a fresh fetch of the same selection, with nothing left of the earlier attempts and
                // the transactions of the epoch as they were.
                let after = work(pool, &key).await.unwrap();
                assert_eq!(
                    (
                        after.state.as_str(),
                        after.api.as_deref(),
                        after.attempts,
                        after.retry_at,
                        after.last_error.as_deref(),
                        after.selection.clone(),
                    ),
                    ("pending", None, 0, 0, None, saved.selection.clone()),
                    "{key}"
                );
                let (since, at, chain_read, transactions): (Option<i64>, Option<i64>, Option<i64>, i64) = sqlx::query_as("SELECT failing_since,failed_at,failed_rpc,(SELECT COUNT(*) FROM txs WHERE txs.job=epoch_work.key) FROM epoch_work WHERE key=?")
                    .bind(&key)
                    .fetch_one(pool)
                    .await
                    .unwrap();
                assert_eq!(
                    (
                        since,
                        at,
                        chain_read,
                        usize::try_from(transactions).unwrap()
                    ),
                    (None, None, None, commits.len()),
                    "{key}"
                );
                assert!(!after.stale_beacon(100_000), "{key}");
                // Discarding is a compare-and-swap on the packet that was judged, so a second refresh from the same snapshot
                // does nothing.
                assert!(!refreshed(pool, &rpc, &saved, 1201).await, "{key}");
            }
        }
        journal.pool.close().await;
    }
    #[tokio::test]
    async fn a_stale_beacon_packet_stays_while_a_commit_is_in_flight_or_when_the_epoch_is_published_or_unknown()
     {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let journal = crate::journal::Journal::open(&dir.path().join("kept.sqlite"), "scope")
            .await
            .unwrap();
        let pool = &journal.pool;
        let (unpublished, unpublished_calls) = registry_node(500, Some(B256::ZERO)).await;
        let (published, published_calls) = registry_node(500, Some(B256::repeat_byte(5))).await;
        let (latest_only, _) = registry_node_at(500, Some(B256::repeat_byte(5)), B256::ZERO).await;
        let (failing, _) = registry_node(500, None).await;
        let beaconed = selection(6, &beacon_recipe().canonical_request);
        let kept = |key: &str| {
            let (pool, key) = (pool.clone(), key.to_owned());
            async move {
                let saved = work(&pool, &key).await.unwrap();
                assert!(saved.api.is_some(), "{key}");
                saved
            }
        };
        // A commit signed or submitted is bound to its packet, whatever else was resolved before it: it and its
        // replacements keep their exact bytes, and nobody asks the registry about a packet that stays anyway.
        let in_flight: [(&str, &[(&str, &str)]); 4] = [
            ("signed", &[("epoch", "signed")]),
            ("submitted", &[("epoch", "submitted")]),
            (
                "cancelling",
                &[("epoch", "submitted"), ("epoch_cancel", "signed")],
            ),
            (
                "replacing",
                &[("epoch", "resolved"), ("epoch_cancel", "submitted")],
            ),
        ];
        for state in ["pending", "prepared", "blocked"] {
            for (history, commits) in in_flight {
                let key = format!("{state}-{history}");
                saved_packet(pool, &key, state, Some(&beaconed), 1000).await;
                for (kind, tx_state) in commits {
                    journal_commit(pool, &key, kind, tx_state).await;
                }
                let saved = work(pool, &key).await.unwrap();
                assert!(saved.in_flight, "{key}");
                assert!(!saved.stale_beacon(u64::MAX), "{key}");
                assert!(
                    !refreshed(pool, &unpublished, &saved, u64::MAX).await,
                    "{key}"
                );
                assert_eq!(kept(&key).await.state, state);
            }
        }
        assert_eq!(unpublished_calls.load(Ordering::SeqCst), 0);
        // The epoch is already published at the latest block, by another committer or by a commit whose receipt is not
        // final yet: its packet is the one on chain, even with every transaction of this keeper resolved, so it stays.
        // Once the finalized state has the epoch too, the work is committed, whichever state it waited in: nothing else
        // would ever move blocked work on, and a live request for the epoch would be reported as a stall.
        for state in ["pending", "prepared", "blocked"] {
            let key = format!("published-{state}");
            saved_packet(pool, &key, state, Some(&beaconed), 1000).await;
            journal_commit(pool, &key, "epoch", "resolved").await;
            let saved = work(pool, &key).await.unwrap();
            assert!(saved.stale_beacon(2000));
            let before = published_calls.load(Ordering::SeqCst);
            assert!(!refreshed(pool, &published, &saved, 2000).await);
            // It asked for the latest block, read the epoch at that block and at the finalized one, and nothing else.
            assert_eq!(published_calls.load(Ordering::SeqCst) - before, 3);
            let after = kept(&key).await;
            assert_eq!((after.state.as_str(), after.api), ("committed", saved.api));
        }
        // Published at the latest block only: nothing is decided before it is final, and the work stays as it was until
        // the next look finds the epoch final.
        saved_packet(pool, "not-final", "blocked", Some(&beaconed), 1000).await;
        let saved = work(pool, "not-final").await.unwrap();
        assert!(!refreshed(pool, &latest_only, &saved, 2000).await);
        assert_eq!(kept("not-final").await.state, "blocked");
        assert!(!refreshed(pool, &published, &saved, 2000).await);
        assert_eq!(kept("not-final").await.state, "committed");
        // A commit signed after the snapshot was read is left to its own reconciliation, which resolves the work from
        // the registry's record: the database decides.
        saved_packet(pool, "raced-published", "blocked", Some(&beaconed), 1000).await;
        let snapshot = work(pool, "raced-published").await.unwrap();
        journal_commit(pool, "raced-published", "epoch", "signed").await;
        assert!(!refreshed(pool, &published, &snapshot, 2000).await);
        assert_eq!(kept("raced-published").await.state, "blocked");
        // A registry that cannot say leaves the packet alone and reports it, for the next attempt.
        saved_packet(pool, "unknown", "blocked", Some(&beaconed), 1000).await;
        let saved = work(pool, "unknown").await.unwrap();
        assert!(
            refresh_stale_beacon(pool, &failing, Address::repeat_byte(0xaa), &saved, 2000)
                .await
                .is_err()
        );
        assert_eq!(kept("unknown").await.state, "blocked");
        // The same packet is discarded once the registry says the epoch is unpublished.
        assert!(refreshed(pool, &unpublished, &saved, 2000).await);
        assert!(work(pool, "unknown").await.unwrap().api.is_none());
        // A transaction signed after the snapshot was read still stops the refresh: the database decides.
        saved_packet(pool, "raced", "prepared", Some(&beaconed), 1000).await;
        let snapshot = work(pool, "raced").await.unwrap();
        assert!(snapshot.stale_beacon(2000));
        journal_commit(pool, "raced", "epoch", "signed").await;
        assert!(!refreshed(pool, &unpublished, &snapshot, 2000).await);
        kept("raced").await;
        // Only work that is waiting for a publication, or was blocked from one, can be refreshed.
        let calls = unpublished_calls.load(Ordering::SeqCst);
        for state in [
            "signed",
            "submitted",
            "committed",
            "expired",
            "inconsistent",
        ] {
            let key = format!("state-{state}");
            saved_packet(pool, &key, state, Some(&beaconed), 1000).await;
            let saved = work(pool, &key).await.unwrap();
            assert!(!saved.stale_beacon(u64::MAX), "{state}");
            assert!(
                !refreshed(pool, &unpublished, &saved, u64::MAX).await,
                "{state}"
            );
            assert_eq!(kept(&key).await.state, state);
        }
        assert_eq!(unpublished_calls.load(Ordering::SeqCst), calls);
        // A packet that changed since the snapshot is not the one that was judged.
        saved_packet(pool, "replaced", "pending", Some(&beaconed), 1000).await;
        let snapshot = work(pool, "replaced").await.unwrap();
        sqlx::query("UPDATE epoch_work SET api='another-packet' WHERE key='replaced'")
            .execute(pool)
            .await
            .unwrap();
        assert!(!refreshed(pool, &unpublished, &snapshot, 2000).await);
        assert_eq!(
            work(pool, "replaced").await.unwrap().api.as_deref(),
            Some("another-packet")
        );
        // Nor is a row whose source changed since the snapshot: the packet of a signed recipe stays whatever was judged.
        saved_packet(pool, "resourced", "pending", Some(&beaconed), 1000).await;
        let snapshot = work(pool, "resourced").await.unwrap();
        let signed = serde_json::to_string(&selection(2, &builtin(2).canonical_request)).unwrap();
        sqlx::query("UPDATE epoch_work SET selection=? WHERE key='resourced'")
            .bind(&signed)
            .execute(pool)
            .await
            .unwrap();
        assert!(!refreshed(pool, &unpublished, &snapshot, 2000).await);
        assert_eq!(kept("resourced").await.selection, Some(signed));
        journal.pool.close().await;
    }
    /// The registry node of a chain whose latest block is `head` and that serves every block up to it by number. The
    /// registry has the epoch from block `published_from` on, and a read at the `latest` tag is a read at `head`. The
    /// count is of requests served, a batch being one.
    async fn registry_since(
        head: u64,
        published_from: u64,
    ) -> (Rpc, Arc<std::sync::atomic::AtomicUsize>) {
        use crate::abi::EpochRecord;
        use alloy_sol_types::SolCall;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let served = Arc::new(AtomicUsize::new(0));
        let counter = served.clone();
        let (url, _) = beacon::fixture::serve(move |_, body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            counter.fetch_add(1, Ordering::SeqCst);
            let number_of = |tag: &Value| match tag.as_str().unwrap() {
                "latest" => head,
                number => u64::from_str_radix(number.trim_start_matches("0x"), 16).unwrap(),
            };
            let reply = |call: &Value| {
                let result = match call["method"].as_str().unwrap() {
                    "eth_getBlockByNumber" => match number_of(&call["params"][0]) {
                        number if number <= head => json!({"number":format!("0x{number:x}"),"hash":B256::repeat_byte(number as u8),"timestamp":"0x64","baseFeePerGas":"0x1"}),
                        _ => Value::Null,
                    },
                    "eth_call" => {
                        let at = number_of(&call["params"][1]);
                        let data: Bytes =
                            serde_json::from_value(call["params"][0]["data"].clone()).unwrap();
                        assert_eq!(E::getEpochCall::abi_decode(&data).unwrap().epochId, 1);
                        let epoch_hash = if at >= published_from {
                            B256::repeat_byte(5)
                        } else {
                            B256::ZERO
                        };
                        json!(Bytes::from(E::getEpochCall::abi_encode_returns(
                            &EpochRecord {
                                epochHash: epoch_hash,
                                catalogHash: B256::ZERO,
                                anchorHash: B256::ZERO,
                                source: 0,
                                queryHash: B256::ZERO,
                                dataHash: B256::ZERO,
                                attestationHash: B256::ZERO,
                                signedAt: U256::ZERO,
                                committedBlock: 0,
                            }
                        )))
                    }
                    method => panic!("unexpected {method}"),
                };
                json!({"jsonrpc":"2.0","id":call["id"],"result":result})
            };
            let answer = match request.as_array() {
                Some(calls) => Value::Array(calls.iter().map(reply).collect()),
                None => reply(&request),
            };
            beacon::fixture::answer(200, answer.to_string())
        })
        .await;
        (Rpc::new(vec![url]).unwrap(), served)
    }
    #[tokio::test]
    async fn in_soft_mode_an_epoch_published_at_the_decision_head_is_final_enough_to_commit_the_work()
     {
        use crate::config::FinalityMode;
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let journal = crate::journal::Journal::open(&dir.path().join("soft.sqlite"), "scope")
            .await
            .unwrap();
        let pool = &journal.pool;
        let beaconed = selection(6, &beacon_recipe().canonical_request);
        // Published at the latest block (500), which is the decision head at depth 0: the work is committed, as it is
        // in finalized mode once the finalized state has the epoch. The registry is asked for the latest block, and for
        // the epoch at that block and at the decision head, and nothing else.
        for state in ["pending", "prepared", "blocked"] {
            let key = format!("soft-{state}");
            saved_packet(pool, &key, state, Some(&beaconed), 1000).await;
            journal_commit(pool, &key, "epoch", "resolved").await;
            let saved = work(pool, &key).await.unwrap();
            let (published, served) = registry_since(500, 500).await;
            let published = published.with_finality(FinalityMode::Soft, 0);
            assert!(!refreshed(pool, &published, &saved, 2000).await);
            assert_eq!(served.load(Ordering::SeqCst), 3);
            let after = work(pool, &key).await.unwrap();
            assert_eq!((after.state.as_str(), after.api), ("committed", saved.api));
        }
        // With a depth the decision head is below the latest block. An epoch published after it is on the chain, so the
        // packet stays, and it is not final enough: the work waits as it was until the decision head has it too.
        saved_packet(pool, "deep", "blocked", Some(&beaconed), 1000).await;
        let saved = work(pool, "deep").await.unwrap();
        let (recent, served) = registry_since(500, 499).await;
        let recent = recent.with_finality(FinalityMode::Soft, 2);
        assert!(!refreshed(pool, &recent, &saved, 2000).await);
        // The latest block, the epoch at it, the decision head in two reads from one endpoint, and the epoch at it.
        assert_eq!(served.load(Ordering::SeqCst), 5);
        assert_eq!(work(pool, "deep").await.unwrap().state, "blocked");
        let (settled, _) = registry_since(500, 498).await;
        let settled = settled.with_finality(FinalityMode::Soft, 2);
        assert!(!refreshed(pool, &settled, &saved, 2000).await);
        assert_eq!(work(pool, "deep").await.unwrap().state, "committed");
        // An epoch that is nowhere is still unpublished: the packet is discarded as in finalized mode.
        saved_packet(pool, "unpublished", "blocked", Some(&beaconed), 1000).await;
        let saved = work(pool, "unpublished").await.unwrap();
        let (unpublished, _) = registry_since(500, 501).await;
        let unpublished = unpublished.with_finality(FinalityMode::Soft, 0);
        assert!(refreshed(pool, &unpublished, &saved, 2000).await);
        assert!(work(pool, "unpublished").await.unwrap().api.is_none());
        journal.pool.close().await;
    }
    #[tokio::test]
    async fn a_signed_packet_an_earlier_release_saved_is_never_refreshed() {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let journal = crate::journal::Journal::open(&dir.path().join("signed.sqlite"), "scope")
            .await
            .unwrap();
        let pool = &journal.pool;
        let (rpc, calls) = registry_node(500, Some(B256::ZERO)).await;
        let envelope = selection(2, &builtin(2).canonical_request);
        let passthrough = selection(2, r#"["passthrough","GET","/feed/latest",[],""]"#);
        let sources = [
            ("envelope", Some(&envelope)),
            ("passthrough", Some(&passthrough)),
            ("unknown", None),
        ];
        // However old, in whatever state, with nothing in flight and the epoch unpublished: the packet a signed recipe's
        // Airnode attested for a release that prepared such epochs is the first and only one, and it is never even weighed
        // against the registry.
        for state in ["pending", "prepared", "blocked"] {
            for (name, source) in sources {
                for (history, commits) in [
                    ("never", &[][..]),
                    ("reverted", &[("epoch", "resolved")][..]),
                    (
                        "cancelled",
                        &[("epoch", "resolved"), ("epoch_cancel", "resolved")][..],
                    ),
                ] {
                    let key = format!("{state}-{name}-{history}");
                    saved_packet(pool, &key, state, source, 1000).await;
                    for (kind, tx_state) in commits {
                        journal_commit(pool, &key, kind, tx_state).await;
                    }
                    let saved = work(pool, &key).await.unwrap();
                    assert!(!saved.in_flight && saved.api.is_some(), "{key}");
                    assert!(!saved.stale_beacon(u64::MAX), "{key}");
                    assert!(!refreshed(pool, &rpc, &saved, u64::MAX).await, "{key}");
                    let after = work(pool, &key).await.unwrap();
                    assert_eq!(
                        (after.state.as_str(), after.api),
                        (state, saved.api),
                        "{key}"
                    );
                }
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        journal.pool.close().await;
    }
    #[tokio::test]
    async fn blocked_work_with_live_demand_and_nothing_in_flight_is_found_for_refreshing() {
        let dir = tempfile::tempdir().unwrap();
        let journal = crate::journal::Journal::open(&dir.path().join("blocked.sqlite"), "scope")
            .await
            .unwrap();
        let pool = &journal.pool;
        let beaconed = selection(6, &beacon_recipe().canonical_request);
        let (registry, catalog) = (Address::repeat_byte(1), B256::repeat_byte(2));
        // Epoch, state, whether a packet is saved, what its transactions are in and the deadline of the demand on it.
        let works: [(u64, &str, bool, Option<&str>, i64); 8] = [
            (5, "blocked", true, None, 300),
            (3, "blocked", true, Some("resolved"), 300),
            (4, "blocked", true, Some("submitted"), 300),
            (6, "blocked", true, None, 100),
            (7, "prepared", true, None, 300),
            (8, "blocked", false, None, 300),
            (9, "blocked", true, None, 0),
            (10, "blocked", true, Some("signed"), 300),
        ];
        for (epoch, state, packet, commit, deadline) in works {
            let key = format!("epoch:{epoch}");
            saved_packet(pool, &key, state, Some(&beaconed), 1000).await;
            sqlx::query("UPDATE epoch_work SET registry=?,catalog=?,epoch=?,api=CASE WHEN ? THEN api END WHERE key=?")
                .bind(registry.to_string()).bind(catalog.to_string()).bind(i64::try_from(epoch).unwrap()).bind(packet).bind(&key)
                .execute(pool).await.unwrap();
            if let Some(commit) = commit {
                journal_commit(pool, &key, "epoch", commit).await;
            }
            if deadline > 0 {
                journal
                    .discovered_epoch(&epoch.to_string(), deadline, "1", Some(epoch))
                    .await
                    .unwrap();
            }
        }
        // Another registry's and another catalog's work is none of this keeper's business.
        for (key, registry, catalog) in [
            ("other-registry", Address::repeat_byte(9), catalog),
            ("other-catalog", registry, B256::repeat_byte(9)),
        ] {
            saved_packet(pool, key, "blocked", Some(&beaconed), 1000).await;
            sqlx::query("UPDATE epoch_work SET registry=?,catalog=?,epoch=5 WHERE key=?")
                .bind(registry.to_string())
                .bind(catalog.to_string())
                .bind(key)
                .execute(pool)
                .await
                .unwrap();
        }
        let found = |after: u64| {
            let pool = pool.clone();
            async move {
                blocked_with_demand(&pool, registry, catalog, after)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|work| work.key)
                    .collect::<Vec<_>>()
            }
        };
        // Blocked with a saved packet, live demand and nothing in flight, oldest epoch first; a resolved commit is not in flight.
        assert_eq!(found(99).await, ["epoch:3", "epoch:5", "epoch:6"]);
        // Demand whose deadline has passed, or is that very second, is no demand.
        assert_eq!(found(100).await, ["epoch:3", "epoch:5"]);
        assert_eq!(found(299).await, ["epoch:3", "epoch:5"]);
        assert_eq!(found(300).await, Vec::<String>::new());
        for work in blocked_with_demand(pool, registry, catalog, 99)
            .await
            .unwrap()
        {
            assert_eq!(
                (work.state.as_str(), work.in_flight, work.api.is_some()),
                ("blocked", false, true)
            );
        }
        journal.pool.close().await;
    }
}
