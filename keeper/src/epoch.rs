//! Epoch publication is a typed maintenance lane; it never creates game jobs.
use crate::{
    abi::{ApiProof, EpochRegistry as E, EpochSelection},
    rpc::{Head, Rpc},
};
use alloy_primitives::{Address, B256, Bytes, U256, address, keccak256};
use anyhow::{Result, ensure};
use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use serde_json::{Value, json};
use sqlx::{Row, SqlitePool};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};
use tokio::task::JoinHandle;

/// A recipe as registered in EpochEntropy: the canonical request whose hash the Airnode signs, the data
/// template of its exact signed record and the JSON body posted to the provider's gateway.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredRecipe {
    pub canonical_request: String,
    pub template: Bytes,
    pub body: String,
}
/// AirnodeHub request canonicalization: the JSON of [operation, canonical(parameters)], plus
/// canonical(responseProjection) when present, where every object at any depth becomes its
/// [key, value] entries sorted by key and arrays keep their order. Its keccak256 is the request hash.
/// Keys are compared by bytes, which equals JavaScript's order for ASCII keys; a body that would
/// canonicalize differently in JavaScript fails the comparison with the registry and is refused.
pub fn canonical_request(body: &Value) -> Result<String> {
    let operation = body
        .get("operation")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Recipe operation must be a string"))?;
    let parameters = body
        .get("parameters")
        .filter(|value| value.is_object())
        .ok_or_else(|| anyhow::anyhow!("Recipe parameters must be an object"))?;
    let mut parts = vec![json!(operation), canonical(parameters)];
    if let Some(projection) = body.get("responseProjection") {
        ensure!(
            projection.is_object(),
            "Recipe responseProjection must be an object"
        );
        parts.push(canonical(projection));
    }
    Ok(serde_json::to_string(&parts)?)
}
fn canonical(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        Value::Object(entries) => {
            let mut sorted: Vec<_> = entries.iter().collect();
            sorted.sort_unstable_by(|a, b| a.0.cmp(b.0));
            Value::Array(
                sorted
                    .into_iter()
                    .map(|(key, item)| json!([key, canonical(item)]))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}
/// Attempt n publishes the source n slots after the selected one, from n × 20 blocks into the epoch.
pub const FALLBACK_DELAY_BLOCKS: u64 = 20;
/// A catalog lists 1 to MAX_SOURCES sources; the registry's sourceCountAt(epoch) bounds its attempts.
pub const MAX_SOURCES: u8 = 10;
/// One gateway request may take at most this long. A fallback window is 20 blocks, about 10 seconds on Arc, so a
/// slower response cannot win that window and would only hold the single fetch slot past the moment the next
/// source may be tried. Healthy AirnodeHub gateways answer in well under a second; a cold start retries.
pub const EPOCH_API_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
/// Consecutive transient failures (transport, timeout, HTTP 408/429/5xx, unsigned gateway errors) of one
/// Airnode's gateway open its circuit for BREAKER_COOLDOWN_SECONDS. While it is open a source of that
/// Airnode is recorded as failed without a request, so the fallback ladder moves on as soon as the next
/// window opens instead of waiting on a timeout. The first request after the cooldown probes it again.
pub const BREAKER_FAILURES: i64 = 3;
pub const BREAKER_COOLDOWN_SECONDS: u64 = 120;
/// Gateways of the Airnodes serving the built-in recipes: Hyperliquid, dRPC, TickerLayer and Nodary.
const DEFAULT_GATEWAYS: [(Address, &str); 4] = [
    (
        address!("509F4275Cbe2E2201cc5444bAc8948E3cc7c665B"),
        "https://airnode-hyperliquid.fly.dev/",
    ),
    (
        address!("511AcE8648D2f64260d50D036F8f8ce622d92137"),
        "https://airnode-drpc.fly.dev/",
    ),
    (
        address!("32f5eA20F05fdADfCD50Cb8eD920acE96D5f9f2c"),
        "https://airnode-tickerlayer.fly.dev/",
    ),
    (
        address!("E70f1e8b22a21e4Bb5188918a3033341b281E4c0"),
        "https://airnode-nodary.fly.dev/",
    ),
];
const ENDPOINTS_FORMAT: &str = "EPOCH_API_ENDPOINTS must list comma-separated AIRNODE_ADDRESS=URL pairs, each Airnode at most once";
/// Provider gateways by Airnode signer address: a catalog slot's signer selects the gateway its recipe is posted to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiEndpoints(BTreeMap<Address, String>);
impl Default for ApiEndpoints {
    fn default() -> Self {
        Self(
            DEFAULT_GATEWAYS
                .iter()
                .map(|(airnode, url)| (*airnode, (*url).to_owned()))
                .collect(),
        )
    }
}
impl ApiEndpoints {
    /// Comma-separated `AIRNODE_ADDRESS=URL` pairs that override a default gateway or add one for another
    /// Airnode. URLs are HTTPS without credentials, query or fragment, normalized to one trailing slash.
    /// `None` keeps the defaults.
    pub fn parse(value: Option<&str>) -> Result<Self> {
        let mut endpoints = Self::default();
        let Some(value) = value else {
            return Ok(endpoints);
        };
        let mut seen = Vec::new();
        for entry in value.split(',') {
            let (airnode, url) = entry
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!(ENDPOINTS_FORMAT))?;
            let airnode = airnode.trim();
            ensure!(
                airnode.len() == 42 && airnode.starts_with("0x"),
                ENDPOINTS_FORMAT
            );
            let airnode: Address = airnode
                .parse()
                .map_err(|_| anyhow::anyhow!(ENDPOINTS_FORMAT))?;
            ensure!(
                !airnode.is_zero() && !seen.contains(&airnode),
                ENDPOINTS_FORMAT
            );
            seen.push(airnode);
            let url = url.trim();
            ensure!(
                !url.is_empty() && url.len() <= 1024,
                "EPOCH_API_ENDPOINTS entries must be non-empty URLs of at most 1024 bytes"
            );
            let parsed = reqwest::Url::parse(url)
                .map_err(|_| anyhow::anyhow!("EPOCH_API_ENDPOINTS entry is not a valid URL"))?;
            ensure!(
                parsed.scheme() == "https"
                    && parsed.host_str().is_some()
                    && parsed.username().is_empty()
                    && parsed.password().is_none()
                    && parsed.query().is_none()
                    && parsed.fragment().is_none(),
                "EPOCH_API_ENDPOINTS entries must be HTTPS URLs without credentials, query or fragment"
            );
            endpoints.0.insert(
                airnode,
                format!("{}/", parsed.as_str().trim_end_matches('/')),
            );
        }
        Ok(endpoints)
    }
    pub fn url(&self, airnode: Address) -> Option<&str> {
        self.0.get(&airnode).map(String::as_str)
    }
}
pub struct Publisher {
    pub registry: Address,
    pub catalog: B256,
    endpoints: ApiEndpoints,
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
    pub fallback: u8,
    /// Sources in the epoch's catalog: attempts run from 0 to sources - 1.
    pub sources: u8,
    pub attempts: i64,
    pub retry_at: i64,
    pub last_error: Option<String>,
}
pub async fn install(pool: &SqlitePool) -> Result<()> {
    sqlx::raw_sql("CREATE TABLE IF NOT EXISTS epoch_work(key TEXT PRIMARY KEY,registry TEXT NOT NULL,catalog TEXT NOT NULL,epoch INTEGER NOT NULL,start INTEGER NOT NULL,state TEXT NOT NULL DEFAULT 'pending',api TEXT,selection TEXT,attempts INTEGER NOT NULL DEFAULT 0,retry_at INTEGER NOT NULL DEFAULT 0,last_error TEXT,fallback INTEGER NOT NULL DEFAULT 0,sources INTEGER NOT NULL DEFAULT 4); CREATE INDEX IF NOT EXISTS epoch_work_open ON epoch_work(state,start); CREATE INDEX IF NOT EXISTS epoch_work_identity ON epoch_work(registry,catalog,epoch);").execute(pool).await?;
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
    sqlx::raw_sql("CREATE TABLE IF NOT EXISTS epoch_breaker(airnode TEXT PRIMARY KEY,failures INTEGER NOT NULL,open_until INTEGER NOT NULL)")
        .execute(pool)
        .await?;
    Ok(())
}
pub async fn work(pool: &SqlitePool, key: &str) -> Result<Work> {
    let r = sqlx::query("SELECT * FROM epoch_work WHERE key=?")
        .bind(key)
        .fetch_one(pool)
        .await?;
    Ok(Work {
        key: r.get("key"),
        epoch: r.get::<i64, _>("epoch").try_into()?,
        start: r.get::<i64, _>("start").try_into()?,
        state: r.get("state"),
        api: r.get("api"),
        fallback: r.get::<i64, _>("fallback").try_into()?,
        sources: r.get::<i64, _>("sources").try_into()?,
        attempts: r.get("attempts"),
        retry_at: r.get("retry_at"),
        last_error: r.get("last_error"),
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
/// packet is never replaced, and the epoch's last source keeps its own retry or blocked outcome.
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
    let moved = sqlx::query("UPDATE epoch_work SET fallback=?,state='pending',selection=NULL,attempts=0,retry_at=0,last_error=NULL WHERE key=? AND fallback=? AND api IS NULL AND last_error IS NOT NULL AND state IN ('pending','blocked')")
        .bind(i64::from(next)).bind(&saved.key).bind(i64::from(saved.fallback)).execute(pool).await?;
    Ok(moved.rows_affected() == 1)
}
/// Keep unused immutable snapshots for 50 epochs, protecting any still-funded request or nonce.
async fn retire_idle_snapshots(pool: &SqlitePool, head: &Head) -> Result<()> {
    sqlx::query("UPDATE epoch_work SET state='expired' WHERE key IN (SELECT key FROM epoch_work WHERE start<=? AND state IN ('pending','prepared','blocked') AND NOT EXISTS(SELECT 1 FROM txs WHERE txs.job=epoch_work.key AND txs.state!='resolved') AND NOT EXISTS(SELECT 1 FROM epoch_demand JOIN jobs ON jobs.id=epoch_demand.job WHERE epoch_demand.epoch=epoch_work.epoch AND jobs.deadline>=? AND jobs.state IN ('pending','prepared','signed','submitted')) LIMIT 128)")
        .bind(i64::try_from(head.number.saturating_sub(10000))?).bind(i64::try_from(head.timestamp)?).execute(pool).await?;
    Ok(())
}
impl Publisher {
    pub fn new(registry: Address, catalog: B256, endpoints: ApiEndpoints) -> Self {
        Self {
            registry,
            catalog,
            endpoints,
            recipes: Mutex::new(HashMap::new()),
            cached: Mutex::new(None),
            fetch: Mutex::new(None),
        }
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
        let recipe = Arc::new(RegisteredRecipe {
            canonical_request: registered.canonicalRequest,
            template: registered.template,
            body: registered.body,
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
        api_override: Option<String>,
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
            || saved.retry_at > crate::health::now()? as i64
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
        let recipe = self.recipe(rpc, selection.recipe).await?;
        let now = crate::health::now()?;
        let Some(endpoint) = source_or_refuse(
            pool,
            &key,
            attempt,
            &selection,
            &recipe,
            &self.endpoints,
            api_override.is_some(),
            now,
        )
        .await?
        else {
            return Ok(None);
        };
        if let Some((failures, wait)) = breaker_open(pool, selection.airnode, now).await? {
            let message = record_failure(
                pool,
                &key,
                attempt,
                FetchFailure::Retry(
                    wait,
                    anyhow::anyhow!(
                        "Epoch gateway of Airnode {} skipped: circuit open after {failures} consecutive failures",
                        selection.airnode
                    ),
                ),
                now,
            )
            .await?;
            tracing::warn!(epoch_key=%key,error=%message,retry_in_seconds=wait,"Epoch source skipped by the provider circuit breaker");
            return Ok(None);
        }
        let client = rpc.client.clone();
        let pool = pool.clone();
        let airnode = selection.airnode;
        *self.fetch.lock().expect("epoch fetch mutex") = Some(tokio::spawn(async move {
            let result = fetch(&client, &selection, &recipe, api_override, endpoint).await;
            let now = crate::health::now()?;
            breaker_record(
                &pool,
                airnode,
                matches!(result, Err(FetchFailure::Retry(..))),
                now,
            )
            .await?;
            match result {
                Ok(api) => {
                    // First authenticated response is immutable, even when its clock is ahead.
                    sqlx::query("UPDATE epoch_work SET api=COALESCE(api,?),state=CASE WHEN state='pending' THEN 'prepared' ELSE state END,retry_at=0,last_error=NULL WHERE key=? AND state IN ('pending','prepared')")
                        .bind(serde_json::to_string(&api)?).bind(&key).execute(&pool).await?;
                    tracing::info!(epoch_key=%key,"Epoch API packet saved");
                }
                Err(failure) => {
                    let message = record_failure(&pool, &key, attempt, failure, now).await?;
                    tracing::warn!(epoch_key=%key,error=%message,"Epoch API preparation deferred");
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
    }
}
/// The selected source is fetched only when its registered recipe is consistent: the registry's selection
/// carries exactly that recipe's canonical request and query hash, the body canonicalizes to it and the
/// template is well-formed.
fn check_recipe(s: &EpochSelection, recipe: &RegisteredRecipe) -> Result<()> {
    ensure!(
        recipe.canonical_request == s.canonicalRequest
            && keccak256(recipe.canonical_request.as_bytes()) == s.queryHash,
        "Registry selection differs from registered recipe {}; refusing to fetch",
        s.recipe
    );
    let body: Value = serde_json::from_str(&recipe.body).map_err(|_| {
        anyhow::anyhow!(
            "Registered recipe {} body is not JSON; refusing to fetch",
            s.recipe
        )
    })?;
    let canonical = canonical_request(&body).map_err(|error| {
        anyhow::anyhow!(
            "Registered recipe {} body is not a gateway request ({error}); refusing to fetch",
            s.recipe
        )
    })?;
    ensure!(
        canonical == recipe.canonical_request,
        "Registered recipe {} body does not canonicalize to its canonical request; refusing to fetch",
        s.recipe
    );
    ensure!(
        crate::template::is_valid(&recipe.template),
        "Registered recipe {} has a malformed data template; refusing to fetch",
        s.recipe
    );
    Ok(())
}
/// The gateway for a consistent recipe. A refused recipe or an Airnode without a configured gateway is
/// recorded as this source's permanent failure, so the deterministic fallback ladder moves to the next
/// source when its window opens, exactly as for a rejected query.
#[allow(clippy::too_many_arguments)]
async fn source_or_refuse(
    pool: &SqlitePool,
    key: &str,
    attempt: i64,
    selection: &EpochSelection,
    recipe: &RegisteredRecipe,
    endpoints: &ApiEndpoints,
    overridden: bool,
    now: u64,
) -> Result<Option<String>> {
    let checked = check_recipe(selection, recipe).and_then(|()| {
        endpoints
            .url(selection.airnode)
            .map(str::to_owned)
            .or(overridden.then(String::new))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No gateway is configured for Airnode {}; add it to EPOCH_API_ENDPOINTS",
                    selection.airnode
                )
            })
    });
    match checked {
        Ok(endpoint) => Ok(Some(endpoint)),
        Err(error) => {
            let message =
                record_failure(pool, key, attempt, FetchFailure::Permanent(error), now).await?;
            tracing::warn!(epoch_key=%key,error=%message,"Epoch source refused");
            Ok(None)
        }
    }
}
/// Consecutive transient failures and the remaining cooldown when an Airnode's circuit is open.
async fn breaker_open(pool: &SqlitePool, airnode: Address, now: u64) -> Result<Option<(i64, u64)>> {
    let row: Option<(i64, i64)> =
        sqlx::query_as("SELECT failures,open_until FROM epoch_breaker WHERE airnode=?")
            .bind(airnode.to_string())
            .fetch_optional(pool)
            .await?;
    Ok(row.and_then(|(failures, open_until)| {
        let open_until = u64::try_from(open_until).unwrap_or(0);
        (failures >= BREAKER_FAILURES && open_until > now).then(|| (failures, open_until - now))
    }))
}
/// A transient failure counts toward the circuit and opens it at BREAKER_FAILURES; any response from the
/// gateway, valid or rejected, shows it is reachable and closes it.
async fn breaker_record(
    pool: &SqlitePool,
    airnode: Address,
    transient: bool,
    now: u64,
) -> Result<()> {
    if !transient {
        sqlx::query("DELETE FROM epoch_breaker WHERE airnode=?")
            .bind(airnode.to_string())
            .execute(pool)
            .await?;
        return Ok(());
    }
    sqlx::query("INSERT INTO epoch_breaker(airnode,failures,open_until) VALUES(?,0,0) ON CONFLICT(airnode) DO NOTHING")
        .bind(airnode.to_string())
        .execute(pool)
        .await?;
    let failures: i64 = sqlx::query_scalar("UPDATE epoch_breaker SET failures=failures+1,open_until=CASE WHEN failures+1>=?1 THEN ?2 ELSE open_until END WHERE airnode=?3 RETURNING failures")
        .bind(BREAKER_FAILURES).bind(i64::try_from(now.saturating_add(BREAKER_COOLDOWN_SECONDS))?).bind(airnode.to_string()).fetch_one(pool).await?;
    if failures == BREAKER_FAILURES {
        tracing::warn!(airnode=%airnode,failures,cooldown_seconds=BREAKER_COOLDOWN_SECONDS,"Epoch provider circuit opened");
    }
    Ok(())
}
enum FetchFailure {
    Retry(u64, anyhow::Error),
    Permanent(anyhow::Error),
}
async fn record_failure(
    pool: &SqlitePool,
    key: &str,
    attempt: i64,
    failure: FetchFailure,
    now: u64,
) -> Result<String> {
    let (retry, message) = match failure {
        FetchFailure::Retry(delay, error) => {
            let backoff = (3u64 << attempt.saturating_sub(1).clamp(0, 4) as u32).min(30);
            (Some(delay.max(backoff)), error.to_string())
        }
        FetchFailure::Permanent(error) => (None, error.to_string()),
    };
    sqlx::query("UPDATE epoch_work SET state=CASE WHEN ?=0 THEN 'blocked' ELSE state END,retry_at=?,last_error=? WHERE key=? AND api IS NULL AND state IN ('pending','prepared')")
        .bind(i64::from(retry.is_some())).bind(i64::try_from(now.saturating_add(retry.unwrap_or(0)))?).bind(&message).bind(key).execute(pool).await?;
    Ok(message)
}
fn retry_delay(header: Option<&str>, now: std::time::SystemTime) -> Result<u64> {
    let Some(value) = header.map(str::trim) else {
        return Ok(3);
    };
    let delay = if !value.is_empty() && value.bytes().all(|c| c.is_ascii_digit()) {
        value
            .parse::<u64>()
            .map_err(|_| anyhow::anyhow!("Retry-After delay exceeds supported range"))?
    } else if let Ok(until) = httpdate::parse_http_date(value) {
        let duration = until.duration_since(now).unwrap_or_default();
        duration
            .as_secs()
            .checked_add(u64::from(duration.subsec_nanos() > 0))
            .ok_or_else(|| anyhow::anyhow!("Retry-After date exceeds supported range"))?
    } else {
        3
    };
    let seconds = now.duration_since(std::time::UNIX_EPOCH)?.as_secs();
    ensure!(
        seconds
            .checked_add(delay.max(2))
            .is_some_and(|until| i64::try_from(until).is_ok()),
        "Retry-After deadline exceeds journal range"
    );
    Ok(delay.max(2))
}

async fn fetch(
    client: &reqwest::Client,
    s: &EpochSelection,
    recipe: &RegisteredRecipe,
    api_override: Option<String>,
    endpoint: String,
) -> std::result::Result<ApiProof, FetchFailure> {
    // The local-chain override keeps precedence over configured gateways and names the recipe.
    let url = match api_override {
        Some(base) => format!("{}/{}", base.trim_end_matches('/'), s.recipe),
        None => endpoint,
    };
    let mut response = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(recipe.body.clone())
        .timeout(EPOCH_API_TIMEOUT)
        .send()
        .await
        .map_err(|e| {
            FetchFailure::Retry(
                3,
                anyhow::anyhow!("Epoch API transport: {}", e.without_url()),
            )
        })?;
    let status = response.status();
    if !status.is_success() {
        let delay = retry_delay(
            response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()),
            std::time::SystemTime::now(),
        )
        .map_err(FetchFailure::Permanent)?;
        let e = anyhow::anyhow!("Epoch API HTTP {}", status.as_u16());
        return Err(
            if status.is_server_error() || matches!(status.as_u16(), 408 | 429) {
                FetchFailure::Retry(delay, e)
            } else {
                FetchFailure::Permanent(e)
            },
        );
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| {
        FetchFailure::Retry(3, anyhow::anyhow!("Epoch API body: {}", e.without_url()))
    })? {
        if bytes.len() + chunk.len() > 16384 {
            return Err(FetchFailure::Permanent(anyhow::anyhow!(
                "Epoch API response over 16KiB"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|e| FetchFailure::Retry(3, e.into()))?;
    if value.get("error").is_some()
        && ["airnode", "requestHash", "timestamp", "data", "signature"]
            .iter()
            .all(|name| value.get(*name).is_none())
    {
        return Err(FetchFailure::Retry(
            3,
            anyhow::anyhow!("Unsigned epoch gateway error"),
        ));
    }
    validate(s, recipe, value).map_err(FetchFailure::Permanent)
}
fn validate(s: &EpochSelection, recipe: &RegisteredRecipe, envelope: Value) -> Result<ApiProof> {
    ensure!(
        serde_json::from_value::<B256>(envelope["requestHash"].clone())? == s.queryHash
            && serde_json::from_value::<Address>(envelope["airnode"].clone())? == s.airnode,
        "Wrong epoch source/query"
    );
    let timestamp: U256 = envelope["timestamp"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing signed epoch timestamp"))?
        .parse()?;
    let data = if let Some(text) = envelope["data"].as_str() {
        text.as_bytes().to_vec()
    } else {
        signed_json(&envelope["data"])?.into_bytes()
    };
    let signature: Bytes = serde_json::from_value(envelope["signature"].clone())?;
    let proof = ApiProof {
        timestamp,
        data: data.into(),
        signature,
    };
    validate_attestation(s, &proof)?;
    ensure!(
        crate::template::matches(&recipe.template, &proof.data),
        "Signed data does not match the template of epoch recipe {}",
        s.recipe
    );
    Ok(proof)
}
// Airnode signs JSON.stringify(data). Preserve insertion order and ECMAScript
// binary64 formatting, including the decimal/exponent thresholds. Object keys keep
// the order the gateway sent them in, which is JavaScript's; the signature and the
// recipe's template both check the reconstructed bytes.
// serde_json's float_roundtrip feature is also required at the parsing boundary.
fn signed_json(value: &Value) -> Result<String> {
    match value {
        Value::Number(number) => {
            let number = number
                .as_f64()
                .filter(|n| n.is_finite())
                .ok_or_else(|| anyhow::anyhow!("Invalid signed JSON number"))?;
            Ok(ryu_js::Buffer::new().format_finite(number).to_owned())
        }
        Value::Array(values) => {
            let items = values.iter().map(signed_json).collect::<Result<Vec<_>>>()?;
            Ok(format!("[{}]", items.join(",")))
        }
        Value::Object(values) => {
            let items = values
                .iter()
                .map(|(key, value)| {
                    Ok(format!(
                        "{}:{}",
                        serde_json::to_string(key)?,
                        signed_json(value)?
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(format!("{{{}}}", items.join(",")))
        }
        _ => Ok(serde_json::to_string(value)?),
    }
}

/// The exact signed bytes EpochEntropy accepts for a recipe: fixed literals, key order and number grammar.
fn validate_attestation(selected: &EpochSelection, proof: &ApiProof) -> Result<()> {
    ensure!(
        !proof.data.is_empty() && proof.data.len() <= 128,
        "Response exceeds contract budget"
    );
    let signature = &proof.signature;
    ensure!(
        signature.len() == 65 && (signature[64] == 27 || signature[64] == 28),
        "Expected canonical 65-byte EIP-191 signature"
    );
    let sig = Signature::from_slice(&signature[..64])?;
    ensure!(sig.normalize_s().is_none(), "Noncanonical high-s signature");
    let mut digest_input = selected.queryHash.to_vec();
    digest_input.extend_from_slice(&proof.timestamp.to_be_bytes::<32>());
    digest_input.extend_from_slice(&proof.data);
    let digest = keccak256(digest_input);
    let mut message = b"\x19Ethereum Signed Message:\n32".to_vec();
    message.extend_from_slice(digest.as_slice());
    let key = VerifyingKey::recover_from_prehash(
        keccak256(message).as_slice(),
        &sig,
        RecoveryId::try_from(signature[64] - 27)?,
    )?;
    let point = key.to_encoded_point(false);
    let recovered = Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..]);
    ensure!(
        recovered == selected.airnode,
        "Source signature verification failed"
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn transient_failures_keep_same_epoch_retryable_and_preserve_first_packet() {
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
                FetchFailure::Retry(60, anyhow::anyhow!("temporary")),
                100,
            )
            .await
            .unwrap();
            let saved = work(&journal.pool, "retry").await.unwrap();
            assert_eq!(saved.state, "pending");
            assert!(saved.retry_at >= 160);
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
            FetchFailure::Permanent(anyhow::anyhow!("invalid signature")),
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
            FetchFailure::Permanent(anyhow::anyhow!("HTTP 400")),
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
            FetchFailure::Retry(60, anyhow::anyhow!("HTTP 429")),
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
                    FetchFailure::Permanent(anyhow::anyhow!("HTTP 400")),
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
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;

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
        }
    }
    /// A template shape from the shared data-template fixture, for listings registered after the built-ins.
    fn template(name: &str) -> Bytes {
        hex_bytes(
            fixture("epoch-data-cases.json")["templates"][name]["template"]
                .as_str()
                .unwrap(),
        )
    }
    #[test]
    fn javascript_signed_numeric_boundaries_and_string_envelopes_verify() {
        let fixture = fixture("tickerlayer-numeric-js.json");
        let canonical = fixture["canonical"].as_str().unwrap();
        let recipe = builtin(2);
        assert_eq!(recipe.canonical_request, canonical);
        for row in fixture["rows"].as_array().unwrap() {
            let mut envelope = row["envelope"].clone();
            let mut selection = selection(2, canonical);
            selection.airnode = envelope["airnode"].as_str().unwrap().parse().unwrap();
            let exact = row["exact"].as_str().unwrap();
            let proof = validate(&selection, &recipe, envelope.clone()).unwrap();
            assert_eq!(proof.data.as_ref(), exact.as_bytes());
            envelope["data"] = json!(exact);
            let raw = validate(&selection, &recipe, envelope.clone()).unwrap();
            assert_eq!(raw.data, proof.data);
            assert_eq!(raw.signature, proof.signature);
            envelope["data"] = json!(exact.replace("76439.99", "76439.98"));
            assert!(validate(&selection, &recipe, envelope).is_err());
        }
    }

    #[test]
    fn actual_tickerlayer_signatures_verify_with_the_registered_templates() {
        let fixture = fixture("tickerlayer-2026-09-15.json");
        for row in fixture["rows"].as_array().unwrap() {
            let id = if row["symbol"] == "BTCUSD" { 2 } else { 3 };
            let recipe = builtin(id);
            let mut s = selection(id, &recipe.canonical_request);
            s.airnode = fixture["signer"].as_str().unwrap().parse().unwrap();
            validate(&s, &recipe, row["envelope"].clone()).unwrap();
            // The other symbol's template refuses the same signed record.
            assert!(validate(&s, &builtin(5 - id), row["envelope"].clone()).is_err());
        }
    }
    fn signed(recipe: u8, data: Value) -> (EpochSelection, Value) {
        let signer = PrivateKeySigner::from_bytes(&B256::repeat_byte(7)).unwrap();
        let mut s = selection(recipe, "fixed-query");
        s.airnode = signer.address();
        let timestamp = U256::from(100);
        let bytes = serde_json::to_vec(&data).unwrap();
        let mut payload = s.queryHash.to_vec();
        payload.extend_from_slice(&timestamp.to_be_bytes::<32>());
        payload.extend_from_slice(&bytes);
        let signature = signer
            .sign_message_sync(keccak256(payload).as_slice())
            .unwrap();
        let envelope = json!({"requestHash":s.queryHash,"airnode":s.airnode,"timestamp":"100","data":data,"signature":format!("0x{}",hex::encode(signature.as_bytes()))});
        (s, envelope)
    }
    #[test]
    fn retry_after_never_shortens_provider_wait() {
        let date = "Sun, 06 Nov 1994 08:49:37 GMT";
        let target = httpdate::parse_http_date(date).unwrap();
        let now = target - std::time::Duration::from_secs(60);
        assert_eq!(retry_delay(Some("60"), now).unwrap(), 60);
        assert_eq!(retry_delay(Some(date), now).unwrap(), 60);
        assert_eq!(
            retry_delay(Some(date), now + std::time::Duration::from_millis(500)).unwrap(),
            60
        );
        assert_eq!(retry_delay(Some("invalid"), now).unwrap(), 3);
        assert_eq!(retry_delay(None, now).unwrap(), 3);
        assert_eq!(retry_delay(Some("0"), now).unwrap(), 2);
        assert!(retry_delay(Some("18446744073709551616"), now).is_err());
        assert!(retry_delay(Some("18446744073709551615"), now).is_err());
    }
    #[test]
    fn exact_source_packets_verify_and_mutations_fail() {
        let nodary_btc = RegisteredRecipe {
            canonical_request: r#"["latestFeeds",[["name","BTC/USD"]]]"#.into(),
            template: template("nodary-btc-usd"),
            body: r#"{"operation":"latestFeeds","parameters":{"name":"BTC/USD"}}"#.into(),
        };
        for (recipe, data) in [
            (
                builtin(0),
                json!({"symbol":"BTC","value":"4044518194.3295292854"}),
            ),
            (
                builtin(5),
                json!({"id":null,"jsonrpc":"2.0","result":"0x0123456789abcdefabcdef01234567891111111111111111222222222222222f"}),
            ),
            (
                nodary_btc,
                json!({"BTC/USD":{"value":76634.54000000001,"timestamp":1789653418094_u64,"category":"crypto"}}),
            ),
        ] {
            let (s, envelope) = signed(6, data);
            let original = validate(&s, &recipe, envelope.clone()).unwrap();
            let mut altered = envelope.clone();
            altered["timestamp"] = json!("101");
            assert!(validate(&s, &recipe, altered).is_err());
            let mut altered = envelope.clone();
            altered["requestHash"] = json!(B256::ZERO);
            assert!(validate(&s, &recipe, altered).is_err());
            let mut altered = envelope.clone();
            altered["signature"] = json!(format!("0x{}", hex::encode(&original.signature[..64])));
            assert!(validate(&s, &recipe, altered).is_err());
            let mut changed = s.clone();
            changed.airnode = Address::ZERO;
            assert!(validate(&changed, &recipe, envelope.clone()).is_err());
            // The same signed bytes checked against another recipe's template are refused.
            let other = RegisteredRecipe {
                template: builtin(2).template,
                ..recipe
            };
            assert!(validate(&s, &other, envelope).is_err());
        }
    }
    #[test]
    fn builtin_recipe_bodies_canonicalize_to_their_registered_requests() {
        let samples = fixture("airnode-recipes-2026-09-17.json");
        for id in 0..6_u8 {
            let recipe = builtin(id);
            let body: Value = serde_json::from_str(&recipe.body).unwrap();
            assert_eq!(canonical_request(&body).unwrap(), recipe.canonical_request);
            assert!(crate::template::is_valid(&recipe.template));
            check_recipe(&selection(id, &recipe.canonical_request), &recipe).unwrap();
            let listing = samples["recipes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["builtinRecipe"] == u64::from(id))
                .unwrap();
            assert_eq!(body, listing["request"]);
        }
        // Bodies keep the nested eth_call params object and array order exactly as posted.
        assert_eq!(
            builtin(5).body,
            r#"{"operation":"jsonRpc","parameters":{"network":"base","method":"eth_call","params":[{"to":"0xcA11bde05977b3631167028862bE2a173976CA11","data":"0x27e86d6e"},"latest"]}}"#
        );
    }
    #[test]
    fn canonicalization_sorts_objects_at_any_depth_and_keeps_array_order() {
        let body = json!({"operation":"op","parameters":{"b":[{"z":1,"a":{"d":2,"c":null}},"x"],"a":""},"responseProjection":{"y":"/1","x":"/0"}});
        assert_eq!(
            canonical_request(&body).unwrap(),
            r#"["op",[["a",""],["b",[[["a",[["c",null],["d",2]]],["z",1]],"x"]]],[["x","/0"],["y","/1"]]]"#
        );
        assert_eq!(
            canonical_request(&json!({"operation":"op","parameters":{"list":[3,1,2]}})).unwrap(),
            r#"["op",[["list",[3,1,2]]]]"#
        );
        for invalid in [
            json!({"parameters":{}}),
            json!({"operation":1,"parameters":{}}),
            json!({"operation":"op","parameters":[]}),
            json!({"operation":"op","parameters":{},"responseProjection":"/0"}),
        ] {
            assert!(canonical_request(&invalid).is_err());
        }
    }
    #[test]
    fn inconsistent_registered_recipes_are_refused_before_any_fetch() {
        let recipe = builtin(1);
        let matching = selection(1, &recipe.canonical_request);
        check_recipe(&matching, &recipe).unwrap();
        // Another key order in the posted body keeps the same canonical request.
        let reordered = RegisteredRecipe {
            body: r#"{"parameters":{"params":[{"data":"0x27e86d6e","to":"0xcA11bde05977b3631167028862bE2a173976CA11"},"latest"],"method":"eth_call","network":"ethereum"},"operation":"jsonRpc"}"#.into(),
            ..recipe.clone()
        };
        check_recipe(&matching, &reordered).unwrap();
        let mut wrong_hash = matching.clone();
        wrong_hash.queryHash = keccak256(builtin(2).canonical_request.as_bytes());
        let refused = [
            // The selection names another recipe's query, or a hash that is not its string.
            (selection(1, &builtin(5).canonical_request), recipe.clone()),
            (wrong_hash, recipe.clone()),
            // The body asks the gateway for something else than the registered canonical request.
            (
                matching.clone(),
                RegisteredRecipe {
                    body: recipe.body.replace("\"latest\"", "\"pending\""),
                    ..recipe.clone()
                },
            ),
            (
                matching.clone(),
                RegisteredRecipe {
                    body: "not json".into(),
                    ..recipe.clone()
                },
            ),
            (
                matching.clone(),
                RegisteredRecipe {
                    body: r#"{"operation":"jsonRpc","parameters":{},"responseProjection":"x"}"#
                        .into(),
                    ..recipe.clone()
                },
            ),
            (
                matching.clone(),
                RegisteredRecipe {
                    template: Bytes::from_static(b"\x01\x01{"),
                    ..recipe.clone()
                },
            ),
        ];
        for (selection, recipe) in refused {
            let error = check_recipe(&selection, &recipe).unwrap_err().to_string();
            assert!(error.contains("refusing to fetch"), "{error}");
        }
    }
    #[tokio::test]
    async fn refused_sources_block_only_themselves_and_the_fallback_ladder_continues() {
        let dir = tempfile::tempdir().unwrap();
        let journal = crate::journal::Journal::open(&dir.path().join("refused.sqlite"), "scope")
            .await
            .unwrap();
        let pool = &journal.pool;
        let endpoints = ApiEndpoints::default();
        sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start,attempts,sources) VALUES('refused','r','c',1,200,1,5)").execute(pool).await.unwrap();
        let recipe = builtin(1);
        let mut matching = selection(1, &recipe.canonical_request);
        matching.airnode = DEFAULT_GATEWAYS[1].0;
        assert_eq!(
            source_or_refuse(
                pool, "refused", 1, &matching, &recipe, &endpoints, false, 100
            )
            .await
            .unwrap()
            .as_deref(),
            Some("https://airnode-drpc.fly.dev/")
        );
        assert_eq!(work(pool, "refused").await.unwrap().state, "pending");
        let changed = RegisteredRecipe {
            body: recipe.body.replace("eth_call", "eth_getBlockByNumber"),
            ..recipe.clone()
        };
        assert!(
            source_or_refuse(
                pool, "refused", 1, &matching, &changed, &endpoints, false, 100
            )
            .await
            .unwrap()
            .is_none()
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
        assert!(!advance_fallback(pool, &saved, 219).await.unwrap());
        assert!(advance_fallback(pool, &saved, 220).await.unwrap());
        assert_eq!(work(pool, "refused").await.unwrap().fallback, 1);
        // An Airnode without a gateway blocks its source unless the local test override serves every recipe.
        let mut unknown = matching.clone();
        unknown.airnode = Address::repeat_byte(9);
        assert_eq!(
            source_or_refuse(pool, "refused", 1, &unknown, &recipe, &endpoints, true, 100)
                .await
                .unwrap()
                .as_deref(),
            Some("")
        );
        assert!(
            source_or_refuse(
                pool, "refused", 1, &unknown, &recipe, &endpoints, false, 100
            )
            .await
            .unwrap()
            .is_none()
        );
        let saved = work(pool, "refused").await.unwrap();
        assert_eq!((saved.state.as_str(), saved.fallback), ("blocked", 1));
        assert!(
            saved
                .last_error
                .unwrap()
                .contains("No gateway is configured for Airnode")
        );
        journal.pool.close().await;
    }
    #[test]
    fn actual_airnode_samples_verify_for_every_listing() {
        let fixture = fixture("airnode-recipes-2026-09-17.json");
        let entries = fixture["recipes"].as_array().unwrap();
        assert_eq!(entries.len(), 8);
        for entry in entries {
            let canonical = entry["canonicalRequest"].as_str().unwrap();
            // Built-in listings use the registered recipe; the others as the owner would register them later.
            let recipe = match entry["builtinRecipe"].as_u64() {
                Some(id) => builtin(u8::try_from(id).unwrap()),
                None => RegisteredRecipe {
                    canonical_request: canonical.into(),
                    template: template(entry["name"].as_str().unwrap()),
                    body: serde_json::to_string(&entry["request"]).unwrap(),
                },
            };
            assert_eq!(recipe.canonical_request, canonical);
            let mut s = selection(6, canonical);
            check_recipe(&s, &recipe).unwrap();
            assert_eq!(
                s.queryHash,
                entry["requestHash"]
                    .as_str()
                    .unwrap()
                    .parse::<B256>()
                    .unwrap()
            );
            let provider = entry["provider"].as_str().unwrap();
            let gateway = &fixture["providers"][provider];
            let signer: Address = gateway["signer"].as_str().unwrap().parse().unwrap();
            assert_eq!(
                ApiEndpoints::default().url(signer),
                gateway["endpoint"].as_str()
            );
            for sample in entry["samples"].as_array().unwrap() {
                s.airnode = sample["airnode"].as_str().unwrap().parse().unwrap();
                assert_eq!(s.airnode, signer);
                // The keeper reconstructs JSON.stringify(data) from the gateway's object envelope.
                let proof = validate(&s, &recipe, sample.clone()).unwrap();
                let mut raw = sample.clone();
                raw["data"] = json!(std::str::from_utf8(&proof.data).unwrap());
                assert_eq!(validate(&s, &recipe, raw).unwrap().data, proof.data);
                // One changed digit keeps the exact shape but breaks the provider's signature.
                let mut text = String::from_utf8(proof.data.to_vec()).unwrap();
                let at = text.rfind(|c: char| c.is_ascii_digit()).unwrap();
                let digit = (text.as_bytes()[at] - b'0' + 1) % 10;
                text.replace_range(at..=at, &digit.to_string());
                assert!(crate::template::matches(&recipe.template, text.as_bytes()));
                let mut tampered = sample.clone();
                tampered["data"] = json!(text);
                assert!(validate(&s, &recipe, tampered).is_err());
                let mut other_signer = s.clone();
                other_signer.airnode = Address::repeat_byte(1);
                assert!(validate(&other_signer, &recipe, sample.clone()).is_err());
            }
        }
    }
    #[tokio::test]
    async fn epoch_lane_resolution_is_atomic_and_separate_from_game_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("epochs.sqlite");
        let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
        let publisher = Publisher::new(
            Address::repeat_byte(1),
            B256::repeat_byte(2),
            ApiEndpoints::default(),
        );
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
                ApiEndpoints::default()
            )
            .key(1)
        );
        journal.pool.close().await;
    }
    #[test]
    fn api_endpoints_default_by_airnode_and_accept_https_overrides() {
        let defaults = ApiEndpoints::parse(None).unwrap();
        assert_eq!(defaults, ApiEndpoints::default());
        let gateways = fixture("airnode-recipes-2026-09-17.json")["providers"].clone();
        for provider in ["hyperliquid", "drpc", "tickerlayer", "nodary"] {
            let signer: Address = gateways[provider]["signer"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(
                defaults.url(signer),
                gateways[provider]["endpoint"].as_str()
            );
        }
        assert_eq!(defaults.url(Address::repeat_byte(9)), None);
        let nodary = DEFAULT_GATEWAYS[3].0;
        let extra = Address::repeat_byte(0x42);
        let custom = ApiEndpoints::parse(Some(&format!(
            " {nodary} = https://n.example/gateway// , {}=https://d.example",
            extra.to_string().to_lowercase()
        )))
        .unwrap();
        assert_eq!(custom.url(nodary), Some("https://n.example/gateway/"));
        assert_eq!(custom.url(extra), Some("https://d.example/"));
        assert_eq!(
            custom.url(DEFAULT_GATEWAYS[0].0),
            defaults.url(DEFAULT_GATEWAYS[0].0)
        );
        let drpc = DEFAULT_GATEWAYS[1].0;
        for invalid in [
            String::new(),
            "https://a.example".into(),
            "drpc=https://a.example".into(),
            format!("{drpc}=https://a.example,{drpc}=https://b.example"),
            format!("{}=https://a.example", Address::ZERO),
            format!("{drpc}="),
            format!("{drpc}=https://a.example,,{nodary}=https://b.example"),
            format!("{drpc}=http://a.example"),
            format!("{drpc}=https://user:secret@a.example"),
            format!("{drpc}=https://a.example/?key=secret"),
            format!("{drpc}=https://a.example/#secret"),
            format!("{drpc}=not a url secret"),
        ] {
            let error = ApiEndpoints::parse(Some(&invalid)).err().unwrap();
            assert!(!error.to_string().contains("secret"), "{invalid}");
        }
    }
    #[tokio::test]
    async fn provider_circuit_opens_after_repeated_transient_failures_and_closes_on_any_response() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("breaker.sqlite");
        let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
        let (airnode, other) = (DEFAULT_GATEWAYS[2].0, DEFAULT_GATEWAYS[3].0);
        for n in 1..BREAKER_FAILURES {
            breaker_record(&journal.pool, airnode, true, 1000)
                .await
                .unwrap();
            assert_eq!(
                breaker_open(&journal.pool, airnode, 1000).await.unwrap(),
                None,
                "{n}"
            );
        }
        breaker_record(&journal.pool, airnode, true, 1000)
            .await
            .unwrap();
        assert_eq!(
            breaker_open(&journal.pool, airnode, 1000).await.unwrap(),
            Some((BREAKER_FAILURES, BREAKER_COOLDOWN_SECONDS))
        );
        // Another Airnode is unaffected, and the open circuit survives a restart.
        assert_eq!(
            breaker_open(&journal.pool, other, 1000).await.unwrap(),
            None
        );
        journal.pool.close().await;
        let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
        assert_eq!(
            breaker_open(&journal.pool, airnode, 1000 + BREAKER_COOLDOWN_SECONDS - 1)
                .await
                .unwrap(),
            Some((BREAKER_FAILURES, 1))
        );
        // After the cooldown one probe goes out; its failure reopens the circuit for another cooldown.
        let probe = 1000 + BREAKER_COOLDOWN_SECONDS;
        assert_eq!(
            breaker_open(&journal.pool, airnode, probe).await.unwrap(),
            None
        );
        breaker_record(&journal.pool, airnode, true, probe)
            .await
            .unwrap();
        assert_eq!(
            breaker_open(&journal.pool, airnode, probe).await.unwrap(),
            Some((BREAKER_FAILURES + 1, BREAKER_COOLDOWN_SECONDS))
        );
        // A response, valid or rejected, closes it and restarts the count.
        breaker_record(&journal.pool, airnode, false, probe + 1)
            .await
            .unwrap();
        assert_eq!(
            breaker_open(&journal.pool, airnode, probe + 1)
                .await
                .unwrap(),
            None
        );
        breaker_record(&journal.pool, airnode, true, probe + 2)
            .await
            .unwrap();
        assert_eq!(
            breaker_open(&journal.pool, airnode, probe + 2)
                .await
                .unwrap(),
            None
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
}
