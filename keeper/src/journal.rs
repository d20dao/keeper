use anyhow::{Result, ensure};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use std::{path::Path, time::Duration};

// SQLite uses the txs_active partial index only when a query repeats its predicate
// verbatim, so the index and both nonce-lane reads share this single text.
macro_rules! active_txs_predicate {
    () => {
        "state IN ('signed','submitted')"
    };
}
macro_rules! unresolved_txs_sql {
    () => {
        concat!(
            "SELECT * FROM txs WHERE ",
            active_txs_predicate!(),
            " ORDER BY id"
        )
    };
}
macro_rules! nonce_lane_conflicts_sql {
    () => {
        concat!(
            "SELECT COUNT(*) FROM txs WHERE ",
            active_txs_predicate!(),
            " AND (nonce!=? OR job!=?)"
        )
    };
}
// A request may be live in at most one nonce: neither as its own single attempt nor as a
// member of a different batch. The batch's own earlier attempts (replacements) are excluded.
macro_rules! member_lane_conflicts_sql {
    () => {
        concat!(
            "SELECT COUNT(*) FROM txs WHERE ",
            active_txs_predicate!(),
            " AND job!=? AND (job=? OR job IN (SELECT job FROM batch_members WHERE request_id=?))"
        )
    };
}

/// Coordinator limit (MAX_FULFILL_BATCH); the journal refuses larger member lists outright.
pub const MAX_BATCH_MEMBERS: usize = 16;
/// Shared by the journal and the drained migration snapshot, which may copy an older journal.
pub const BATCH_MEMBERS_DDL: &str = "CREATE TABLE IF NOT EXISTS batch_members(job TEXT NOT NULL,request_id TEXT NOT NULL,position INTEGER NOT NULL,PRIMARY KEY(job,request_id)); CREATE INDEX IF NOT EXISTS batch_members_request ON batch_members(request_id);";
/// Batch attempts own a synthetic job key; their request IDs are listed in batch_members.
pub fn is_batch_job(job: &str) -> bool {
    job.starts_with("batch:")
}
/// One ordered member list has exactly one key, so an identical payload (a fee replacement,
/// or a rebroadcast after restart) always lands on the same job and member rows.
pub fn batch_key(members: &[String]) -> Result<String> {
    ensure!(
        (2..=MAX_BATCH_MEMBERS).contains(&members.len()),
        "Batch member count out of range"
    );
    let digest = alloy_primitives::keccak256(members.join(",").as_bytes());
    Ok(format!(
        "batch:{}:{}:{}",
        members[0],
        members.len(),
        hex::encode(&digest[..8])
    ))
}

pub struct Journal {
    pub pool: SqlitePool,
}
#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub deadline: i64,
    pub state: String,
    pub proof: Option<String>,
    pub call: Option<String>,
}
#[derive(Debug, Clone)]
pub struct Attempt {
    pub id: i64,
    pub job: String,
    pub nonce: i64,
    pub hash: String,
    pub raw: String,
    pub kind: String,
    pub fee: String,
    pub state: String,
    pub gas: i64,
    pub priority: String,
    pub payload: String,
    pub created: i64,
    pub broadcast: i64,
}
pub(crate) fn job_from_row(r: sqlx::sqlite::SqliteRow) -> Job {
    Job {
        id: r.get("id"),
        deadline: r.get("deadline"),
        state: r.get("state"),
        proof: r.get("proof"),
        call: r.get("call"),
    }
}
async fn checkpoint(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    number: u64,
    hash: &str,
) -> Result<()> {
    let old: Option<String> =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='finalized_checkpoint'")
            .fetch_optional(&mut **tx)
            .await?;
    if let Some(old) = old {
        let (previous, previous_hash): (u64, String) = serde_json::from_str(&old)?;
        if previous > number {
            return Ok(());
        }
        ensure!(
            previous != number || previous_hash == hash,
            "Finalized checkpoint conflict"
        );
        if previous == number {
            return Ok(());
        }
    }
    sqlx::query("INSERT INTO meta(key,value) VALUES('finalized_checkpoint',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value").bind(serde_json::to_string(&(number,hash))?).execute(&mut **tx).await?;
    Ok(())
}
impl Journal {
    pub async fn open(path: &Path, scope: &str) -> Result<Self> {
        crate::migration::ensure_startable(path)?;
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        sqlx::raw_sql("CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT NOT NULL); CREATE TABLE IF NOT EXISTS jobs(id TEXT PRIMARY KEY,deadline INTEGER NOT NULL,state TEXT NOT NULL DEFAULT 'pending',proof TEXT,call TEXT); CREATE TABLE IF NOT EXISTS txs(id INTEGER PRIMARY KEY,job TEXT NOT NULL,nonce INTEGER NOT NULL,hash TEXT NOT NULL UNIQUE,raw TEXT NOT NULL,kind TEXT NOT NULL,fee TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'signed',gas INTEGER NOT NULL,priority TEXT NOT NULL,payload TEXT NOT NULL,created INTEGER NOT NULL,broadcast INTEGER NOT NULL DEFAULT 0); CREATE INDEX IF NOT EXISTS jobs_open ON jobs(state,deadline);").execute(&pool).await?;
        sqlx::query("INSERT OR IGNORE INTO meta(key,value) VALUES('schema_version','1')")
            .execute(&pool)
            .await?;
        let schema: String =
            sqlx::query_scalar("SELECT value FROM meta WHERE key='schema_version'")
                .fetch_one(&pool)
                .await?;
        ensure!(
            schema == "1",
            "Unsupported journal schema; explicit migration required"
        );
        sqlx::query("INSERT OR IGNORE INTO meta(key,value) VALUES('scope',?)")
            .bind(scope)
            .execute(&pool)
            .await?;
        let actual: String = sqlx::query_scalar("SELECT value FROM meta WHERE key='scope'")
            .fetch_one(&pool)
            .await?;
        ensure!(
            actual == scope,
            "Journal belongs to a different chain/coordinator/sender"
        );
        // Backfill existing journals too: never allocate a nonce already resolved here.
        sqlx::query("INSERT INTO meta(key,value) SELECT 'nonce_floor',CAST(COALESCE(MAX(nonce)+1,0) AS TEXT) FROM txs WHERE state='resolved' ON CONFLICT(key) DO UPDATE SET value=CAST(MAX(CAST(meta.value AS INTEGER),CAST(excluded.value AS INTEGER)) AS TEXT)")
            .execute(&pool).await?;
        // Database identity is committed before the scope lock can bind to it.
        sqlx::query("INSERT OR IGNORE INTO meta(key,value) VALUES('instance_id',lower(hex(randomblob(32))))")
            .execute(&pool).await?;
        crate::audit::install(&pool).await?;
        crate::epoch::install(&pool).await?;
        sqlx::raw_sql("CREATE TABLE IF NOT EXISTS finalized_receipts(hash TEXT PRIMARY KEY,block_number INTEGER NOT NULL,block_hash TEXT NOT NULL,status INTEGER NOT NULL);").execute(&pool).await?;
        sqlx::raw_sql("CREATE TABLE IF NOT EXISTS epoch_demand(job TEXT PRIMARY KEY,epoch INTEGER NOT NULL); CREATE INDEX IF NOT EXISTS epoch_demand_epoch ON epoch_demand(epoch,job);").execute(&pool).await?;
        sqlx::raw_sql(BATCH_MEMBERS_DDL).execute(&pool).await?;
        sqlx::raw_sql(concat!(
            "CREATE INDEX IF NOT EXISTS txs_active ON txs(id) WHERE ",
            active_txs_predicate!(),
            ";"
        ))
        .execute(&pool)
        .await?;
        sqlx::raw_sql("CREATE INDEX IF NOT EXISTS txs_job_state ON txs(job,state); CREATE INDEX IF NOT EXISTS txs_nonce_state ON txs(nonce,state); CREATE INDEX IF NOT EXISTS jobs_compact ON jobs(id) WHERE state IN ('served','callback_failed','refunded','expired','ignored') AND (proof IS NOT NULL OR call IS NOT NULL); CREATE INDEX IF NOT EXISTS txs_compact ON txs(id) WHERE state='resolved' AND (raw!='' OR payload!=''); CREATE INDEX IF NOT EXISTS epochs_compact ON epoch_work(key) WHERE state IN ('committed','expired') AND (api IS NOT NULL OR selection IS NOT NULL);").execute(&pool).await?;
        Ok(Self { pool })
    }
    /// Retain receipt lookup metadata, removing only replay data that can no longer be used.
    /// A persisted cadence and small indexed batches keep maintenance off the hot path.
    /// Expired unpublished epochs have no chain evidence: their raw packet is intentionally
    /// discarded, while identity, status, timing and last error remain for diagnosis.
    pub async fn compact_history(&self, now: u64) -> Result<()> {
        let now = i64::try_from(now)?;
        let mut tx = self.pool.begin().await?;
        let due: i64 = sqlx::query_scalar("SELECT CAST(COALESCE((SELECT value FROM meta WHERE key='history:compact_after'),'0') AS INTEGER)")
            .fetch_one(&mut *tx).await?;
        if now < due {
            return Ok(());
        }
        // A member of a live batch keeps its proof/calldata exactly like a job with its own live attempt.
        sqlx::query("UPDATE jobs SET proof=NULL,call=NULL WHERE id IN (SELECT id FROM jobs WHERE state IN ('served','callback_failed','refunded','expired','ignored') AND (proof IS NOT NULL OR call IS NOT NULL) AND NOT EXISTS(SELECT 1 FROM txs WHERE txs.job=jobs.id AND txs.state!='resolved') AND NOT EXISTS(SELECT 1 FROM batch_members JOIN txs ON txs.job=batch_members.job WHERE batch_members.request_id=jobs.id AND txs.state!='resolved') LIMIT 128)")
            .execute(&mut *tx).await?;
        sqlx::query("UPDATE epoch_work SET api=NULL,selection=NULL WHERE key IN (SELECT key FROM epoch_work WHERE state IN ('committed','expired') AND (api IS NOT NULL OR selection IS NOT NULL) AND NOT EXISTS(SELECT 1 FROM txs WHERE txs.job=epoch_work.key AND txs.state!='resolved') LIMIT 128)")
            .execute(&mut *tx).await?;
        sqlx::query("UPDATE txs SET raw='',payload='' WHERE id IN (SELECT candidate.id FROM txs AS candidate WHERE candidate.state='resolved' AND (candidate.raw!='' OR candidate.payload!='') AND NOT EXISTS(SELECT 1 FROM txs AS live WHERE live.nonce=candidate.nonce AND live.state!='resolved') AND NOT EXISTS(SELECT 1 FROM txs AS live WHERE live.job=candidate.job AND live.state!='resolved') LIMIT 128)")
            .execute(&mut *tx).await?;
        sqlx::query("INSERT INTO meta(key,value) VALUES('history:compact_after',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
            .bind(now.saturating_add(60).to_string()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT value FROM meta WHERE key=?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?)
    }
    /// Revisit pilot-era excluded requests once without replacing the journal or nonce history.
    pub async fn enable_public_service(&self, now: u64) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let current: Option<String> =
            sqlx::query_scalar("SELECT value FROM meta WHERE key='consumer_access'")
                .fetch_optional(&mut *tx)
                .await?;
        if current.as_deref() != Some("public") {
            sqlx::query("UPDATE jobs SET state=CASE WHEN call IS NULL THEN 'pending' ELSE 'prepared' END WHERE state='ignored' AND deadline>? AND NOT EXISTS(SELECT 1 FROM txs WHERE txs.job=jobs.id AND txs.state!='resolved')")
                .bind(i64::try_from(now)?).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO meta(key,value) VALUES('cursor','1') ON CONFLICT(key) DO UPDATE SET value='1'")
                .execute(&mut *tx).await?;
            sqlx::query("INSERT INTO meta(key,value) VALUES('consumer_access','public') ON CONFLICT(key) DO UPDATE SET value='public'")
                .execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn finalized_checkpoint(&self, number: u64, hash: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        checkpoint(&mut tx, number, hash).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn finalized_receipt(
        &self,
        hash: &str,
        number: u64,
        block_hash: &str,
        status: u64,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO finalized_receipts(hash,block_number,block_hash,status) VALUES(?,?,?,?) ON CONFLICT DO NOTHING").bind(hash).bind(i64::try_from(number)?).bind(block_hash).bind(i64::try_from(status)?).execute(&mut *tx).await?;
        let actual: (i64, String, i64) = sqlx::query_as(
            "SELECT block_number,block_hash,status FROM finalized_receipts WHERE hash=?",
        )
        .bind(hash)
        .fetch_one(&mut *tx)
        .await?;
        ensure!(
            actual
                == (
                    i64::try_from(number)?,
                    block_hash.to_string(),
                    i64::try_from(status)?
                ),
            "Finalized receipt conflict"
        );
        checkpoint(&mut tx, number, block_hash).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn discovered(&self, id: &str, deadline: i64, next: &str) -> Result<()> {
        self.discovered_epoch(id, deadline, next, None).await
    }
    pub async fn discovered_epoch(
        &self,
        id: &str,
        deadline: i64,
        next: &str,
        epoch: Option<u64>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT OR IGNORE INTO jobs(id,deadline) VALUES(?,?)")
            .bind(id)
            .bind(deadline)
            .execute(&mut *tx)
            .await?;
        if let Some(epoch) = epoch {
            sqlx::query("INSERT INTO epoch_demand(job,epoch) VALUES(?,?) ON CONFLICT(job) DO UPDATE SET epoch=excluded.epoch")
                .bind(id)
                .bind(i64::try_from(epoch)?)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("INSERT INTO meta(key,value) VALUES('cursor',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value").bind(next).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn cursor(&self, next: &str) -> Result<()> {
        sqlx::query("INSERT INTO meta(key,value) VALUES('cursor',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value").bind(next).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn pending(&self) -> Result<Vec<Job>> {
        let rows=sqlx::query("SELECT * FROM jobs WHERE state IN ('pending','prepared','signed','submitted') ORDER BY deadline,id LIMIT 256").fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(job_from_row).collect())
    }
    /// Claim a fair, bounded preparation batch before any RPC work starts.
    /// Failed/unpublished jobs cannot repeatedly monopolize the queue prefix after restart.
    /// A follower claims from the newest end (`tail_first`), the end it sends from: claiming from the head like
    /// the primary would prepare, and then send, exactly the requests the primary is serving at that moment.
    pub async fn claim_preparation(
        &self,
        wall_now: u64,
        eligible_after: u64,
        excluded: &[String],
        tail_first: bool,
    ) -> Result<Vec<Job>> {
        ensure!(
            excluded.len() <= 8,
            "Preparation exclusion exceeds one batch"
        );
        let mut tx = self.pool.begin().await?;
        // Two fixed statements, never a built string: the head and the tail of the same queue.
        const HEAD: &str = "SELECT jobs.* FROM jobs LEFT JOIN meta ON meta.key='prepare_retry_ms:'||jobs.id WHERE jobs.state='pending' AND jobs.call IS NULL AND jobs.deadline>? AND CAST(COALESCE(meta.value,'0') AS INTEGER)<=? ORDER BY CAST(COALESCE(meta.value,'0') AS INTEGER),jobs.deadline,LENGTH(jobs.id),jobs.id LIMIT 16";
        const TAIL: &str = "SELECT jobs.* FROM jobs LEFT JOIN meta ON meta.key='prepare_retry_ms:'||jobs.id WHERE jobs.state='pending' AND jobs.call IS NULL AND jobs.deadline>? AND CAST(COALESCE(meta.value,'0') AS INTEGER)<=? ORDER BY CAST(COALESCE(meta.value,'0') AS INTEGER),jobs.deadline DESC,LENGTH(jobs.id) DESC,jobs.id DESC LIMIT 16";
        let rows = sqlx::query(if tail_first { TAIL } else { HEAD })
            .bind(i64::try_from(eligible_after)?)
            .bind(i64::try_from(wall_now)?)
            .fetch_all(&mut *tx)
            .await?;
        let jobs: Vec<_> = rows
            .into_iter()
            .map(job_from_row)
            .filter(|job| !excluded.contains(&job.id))
            .take(8)
            .collect();
        for job in &jobs {
            sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
                .bind(format!("prepare_retry_ms:{}",job.id)).bind(wall_now.saturating_add(250).to_string()).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(jobs)
    }
    pub async fn job(&self, id: &str) -> Result<Option<Job>> {
        Ok(sqlx::query("SELECT * FROM jobs WHERE id=?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .map(job_from_row))
    }
    /// At most this many prepared jobs are considered in one tick, from whichever end of the queue the caller works.
    pub const QUEUE_PAGE: usize = 256;
    /// Prepared jobs whose preflight backoff has passed, earliest deadline first: the head of the queue.
    pub async fn prepared_due(&self, now: u64) -> Result<Vec<Job>> {
        self.prepared_page(now, false).await
    }
    /// The same page taken from the newest end, for a follower that joins the queue tail first.
    pub async fn prepared_due_tail(&self, now: u64) -> Result<Vec<Job>> {
        self.prepared_page(now, true).await
    }
    async fn prepared_page(&self, now: u64, tail: bool) -> Result<Vec<Job>> {
        // Two fixed statements, never a built string: the head page and the tail page of the same queue.
        const HEAD: &str = "SELECT jobs.* FROM jobs LEFT JOIN meta ON meta.key='preflight_retry:'||jobs.id WHERE jobs.state='prepared' AND jobs.call IS NOT NULL AND CAST(COALESCE(meta.value,'0') AS INTEGER)<=? ORDER BY jobs.deadline,LENGTH(jobs.id),jobs.id LIMIT 256";
        const TAIL: &str = "SELECT jobs.* FROM jobs LEFT JOIN meta ON meta.key='preflight_retry:'||jobs.id WHERE jobs.state='prepared' AND jobs.call IS NOT NULL AND CAST(COALESCE(meta.value,'0') AS INTEGER)<=? ORDER BY jobs.deadline DESC,LENGTH(jobs.id) DESC,jobs.id DESC LIMIT 256";
        let rows = sqlx::query(if tail { TAIL } else { HEAD })
            .bind(i64::try_from(now)?)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(job_from_row).collect())
    }
    /// The oldest open jobs with nothing of this node's own in flight for them, oldest deadline first and at least
    /// `min_age` seconds old. A follower re-reads these few from chain before it judges the queue, because the
    /// primary serves the oldest first and those are exactly the rows a follower learns about last.
    pub async fn pending_oldest(&self, now: u64, min_age: u64, limit: u32) -> Result<Vec<Job>> {
        const SQL: &str = "SELECT jobs.* FROM jobs WHERE jobs.state IN ('pending','prepared') AND jobs.deadline>? AND jobs.deadline<=? AND NOT EXISTS(SELECT 1 FROM txs WHERE txs.job=jobs.id AND txs.state!='resolved') AND NOT EXISTS(SELECT 1 FROM batch_members JOIN txs ON txs.job=batch_members.job WHERE batch_members.request_id=jobs.id AND txs.state!='resolved') ORDER BY jobs.deadline,LENGTH(jobs.id),jobs.id LIMIT ?";
        let rows = sqlx::query(SQL)
            .bind(i64::try_from(now)?)
            .bind(i64::try_from(
                now.saturating_add(crate::config::RESPONSE_TIMEOUT_SECONDS)
                    .saturating_sub(min_age),
            )?)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(job_from_row).collect())
    }
    /// The unserved queue this node can see: how many requests are still open and when the oldest was created.
    /// `now` is the finalized chain time; a request whose deadline has passed is no longer queue pressure.
    pub async fn pending_queue(&self, now: u64) -> Result<(u64, Option<u64>)> {
        let row: (i64, Option<i64>) = sqlx::query_as(
            "SELECT COUNT(*),MIN(deadline) FROM jobs WHERE state IN ('pending','prepared') AND deadline>?",
        )
        .bind(i64::try_from(now)?)
        .fetch_one(&self.pool)
        .await?;
        Ok((u64::try_from(row.0)?, row.1.map(u64::try_from).transpose()?))
    }
    pub async fn preflight_next(&self, id: Option<&str>) -> Result<()> {
        if let Some(id) = id {
            sqlx::query("INSERT INTO meta(key,value) VALUES('preflight_next',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
                .bind(id).execute(&self.pool).await?;
        } else {
            sqlx::query("DELETE FROM meta WHERE key='preflight_next'")
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }
    pub async fn preflight_backoff(&self, id: &str, until: u64) -> Result<()> {
        sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
            .bind(format!("preflight_retry:{id}")).bind(until.to_string()).execute(&self.pool).await?;
        Ok(())
    }
    /// One commit for every candidate of a batch preflight, recorded before any RPC work.
    pub async fn preflight_backoff_many(&self, ids: &[String], until: u64) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for id in ids {
            sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
                .bind(format!("preflight_retry:{id}")).bind(until.to_string()).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    /// A request whose own preflight the node rejected keeps its single-path retries but
    /// is left out of batches, so one bad proof cannot force every batch back to single sends.
    pub async fn exclude_from_batches(&self, id: &str) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO meta(key,value) VALUES(?,'1')")
            .bind(format!("batch_exclude:{id}"))
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub async fn batch_excluded(&self, id: &str) -> Result<bool> {
        Ok(self.meta(&format!("batch_exclude:{id}")).await?.is_some())
    }
    /// Ordered member request IDs of a batch job; empty when the key is unknown.
    pub async fn batch_members(&self, job: &str) -> Result<Vec<String>> {
        Ok(
            sqlx::query_scalar(
                "SELECT request_id FROM batch_members WHERE job=? ORDER BY position",
            )
            .bind(job)
            .fetch_all(&self.pool)
            .await?,
        )
    }
    /// Move every member of a batch together (broadcast bookkeeping only; terminal states
    /// are set through resolve_nonce_batch).
    pub async fn members_state(&self, job: &str, state: &str) -> Result<()> {
        sqlx::query("UPDATE jobs SET state=? WHERE id IN (SELECT request_id FROM batch_members WHERE job=?)")
            .bind(state)
            .bind(job)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    /// Drop stale unstarted work in one indexed database operation instead of spending RPC calls
    /// on each expired historical job. Submitted nonce lanes are reconciled separately.
    pub async fn expire_unstarted(&self, now: i64) -> Result<()> {
        sqlx::query("UPDATE jobs SET state='expired' WHERE deadline<? AND state IN ('pending','prepared','blocked')")
            .bind(now).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn state(&self, id: &str, state: &str) -> Result<()> {
        sqlx::query("UPDATE jobs SET state=? WHERE id=?")
            .bind(state)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub async fn prepared(&self, id: &str, proof: &str, call: &str) -> Result<()> {
        sqlx::query("UPDATE jobs SET proof=COALESCE(proof,?),call=COALESCE(call,?),state='prepared' WHERE id=?").bind(proof).bind(call).bind(id).execute(&self.pool).await?;
        Ok(())
    }
    /// Signed bytes and nonce are committed BEFORE any network broadcast.
    pub async fn signed(&self, a: &Attempt) -> Result<()> {
        ensure!(
            matches!(
                a.kind.as_str(),
                "fulfill" | "cancel" | "epoch" | "epoch_cancel"
            ),
            "Unknown transaction work kind"
        );
        ensure!(
            !(a.kind == "fulfill" && is_batch_job(&a.job)),
            "A single fulfillment cannot target a batch job"
        );
        let mut tx = self.pool.begin().await?;
        Self::claim_lane(&mut tx, a).await?;
        if a.kind.starts_with("epoch") {
            let changed = sqlx::query("UPDATE epoch_work SET state='signed' WHERE key=?")
                .bind(&a.job)
                .execute(&mut *tx)
                .await?;
            ensure!(
                changed.rows_affected() == 1,
                "Epoch work missing from journal"
            );
        } else if is_batch_job(&a.job) {
            // A nonce cancellation of a batch keeps every member with the lane it already owns.
            let members = Self::members_in(&mut tx, &a.job).await?;
            ensure!(!members.is_empty(), "Batch members missing from journal");
            Self::mark_members_signed(&mut tx, &members).await?;
        } else {
            let changed = sqlx::query("UPDATE jobs SET state='signed' WHERE id=?")
                .bind(&a.job)
                .execute(&mut *tx)
                .await?;
            ensure!(
                changed.rows_affected() == 1,
                "Game work missing from journal"
            );
        }
        tx.commit().await?;
        Ok(())
    }
    /// A batch attempt: the lane rule, the tx row, the member list and every member's
    /// `signed` state commit together or not at all. The key must be derived from the
    /// ordered members, and no member may be live under any other nonce.
    pub async fn signed_batch(&self, a: &Attempt, members: &[String]) -> Result<()> {
        ensure!(a.kind == "fulfill_batch", "Not a batch attempt");
        ensure!(
            a.job == batch_key(members)?,
            "Batch key does not match its members"
        );
        ensure!(
            members
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == members.len(),
            "Duplicate batch member"
        );
        let mut tx = self.pool.begin().await?;
        for id in members {
            let live: i64 = sqlx::query_scalar(member_lane_conflicts_sql!())
                .bind(&a.job)
                .bind(id)
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
            ensure!(
                live == 0,
                "Request {id} is already live in another nonce; refusing a second attempt"
            );
        }
        Self::claim_lane(&mut tx, a).await?;
        let existing = Self::members_in(&mut tx, &a.job).await?;
        if existing.is_empty() {
            for (position, id) in members.iter().enumerate() {
                sqlx::query("INSERT INTO batch_members(job,request_id,position) VALUES(?,?,?)")
                    .bind(&a.job)
                    .bind(id)
                    .bind(i64::try_from(position)?)
                    .execute(&mut *tx)
                    .await?;
            }
        } else {
            // A replacement carries the identical payload, hence the identical member list.
            ensure!(
                existing == members,
                "Batch member list changed between attempts"
            );
        }
        Self::mark_members_signed(&mut tx, members).await?;
        tx.commit().await?;
        Ok(())
    }
    async fn claim_lane(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, a: &Attempt) -> Result<()> {
        let conflicting: i64 = sqlx::query_scalar(nonce_lane_conflicts_sql!())
            .bind(a.nonce)
            .bind(&a.job)
            .fetch_one(&mut **tx)
            .await?;
        ensure!(
            conflicting == 0,
            "Another game or epoch already owns the nonce lane"
        );
        let sweeping: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM meta WHERE key=?")
            .bind(crate::sweep::ATTEMPT_KEY)
            .fetch_one(&mut **tx)
            .await?;
        ensure!(sweeping == 0, "An operator sweep owns the nonce lane");
        sqlx::query("INSERT INTO txs(job,nonce,hash,raw,kind,fee,gas,priority,payload,created) VALUES(?,?,?,?,?,?,?,?,?,?)").bind(&a.job).bind(a.nonce).bind(&a.hash).bind(&a.raw).bind(&a.kind).bind(&a.fee).bind(a.gas).bind(&a.priority).bind(&a.payload).bind(a.created).execute(&mut **tx).await?;
        Ok(())
    }
    async fn members_in(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        job: &str,
    ) -> Result<Vec<String>> {
        Ok(
            sqlx::query_scalar(
                "SELECT request_id FROM batch_members WHERE job=? ORDER BY position",
            )
            .bind(job)
            .fetch_all(&mut **tx)
            .await?,
        )
    }
    // Members enter a batch prepared and stay signed/submitted until the nonce resolves;
    // any other state means the journal and the attempt disagree, so the signing fails.
    async fn mark_members_signed(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        members: &[String],
    ) -> Result<()> {
        for id in members {
            let changed = sqlx::query("UPDATE jobs SET state='signed' WHERE id=? AND state IN ('prepared','signed','submitted')")
                .bind(id)
                .execute(&mut **tx)
                .await?;
            ensure!(
                changed.rows_affected() == 1,
                "Batch member {id} is not prepared or live in the journal"
            );
        }
        Ok(())
    }
    pub async fn unresolved(&self) -> Result<Vec<Attempt>> {
        let rows = sqlx::query(unresolved_txs_sql!())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| Attempt {
                id: r.get("id"),
                job: r.get("job"),
                nonce: r.get("nonce"),
                hash: r.get("hash"),
                raw: r.get("raw"),
                kind: r.get("kind"),
                fee: r.get("fee"),
                state: r.get("state"),
                gas: r.get("gas"),
                priority: r.get("priority"),
                payload: r.get("payload"),
                created: r.get("created"),
                broadcast: r.get("broadcast"),
            })
            .collect())
    }
    pub async fn tx_state(&self, id: i64, state: &str) -> Result<()> {
        sqlx::query("UPDATE txs SET state=? WHERE id=?")
            .bind(state)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub async fn resolve_nonce(&self, nonce: i64) -> Result<()> {
        let next = nonce
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Nonce overflow"))?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE txs SET state='resolved' WHERE nonce=?")
            .bind(nonce)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO meta(key,value) VALUES('nonce_floor',?) ON CONFLICT(key) DO UPDATE SET value=CAST(MAX(CAST(meta.value AS INTEGER),CAST(excluded.value AS INTEGER)) AS TEXT)")
            .bind(next.to_string()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    /// Receipt outcome, every replacement attempt, and nonce floor commit together.
    pub async fn resolve_nonce_job(&self, nonce: i64, job: &str, state: &str) -> Result<()> {
        let next = nonce
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Nonce overflow"))?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE txs SET state='resolved' WHERE nonce=?")
            .bind(nonce)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO meta(key,value) VALUES('nonce_floor',?) ON CONFLICT(key) DO UPDATE SET value=CAST(MAX(CAST(meta.value AS INTEGER),CAST(excluded.value AS INTEGER)) AS TEXT)")
            .bind(next.to_string()).execute(&mut *tx).await?;
        let updated = sqlx::query("UPDATE jobs SET state=? WHERE id=?")
            .bind(state)
            .bind(job)
            .execute(&mut *tx)
            .await?;
        ensure!(
            updated.rows_affected() == 1,
            "Receipt job missing from journal"
        );
        tx.commit().await?;
        Ok(())
    }
    pub async fn resolve_nonce_epoch(&self, nonce: i64, key: &str, state: &str) -> Result<()> {
        let next = nonce
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Nonce overflow"))?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE txs SET state='resolved' WHERE nonce=?")
            .bind(nonce)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO meta(key,value) VALUES('nonce_floor',?) ON CONFLICT(key) DO UPDATE SET value=CAST(MAX(CAST(meta.value AS INTEGER),CAST(excluded.value AS INTEGER)) AS TEXT)").bind(next.to_string()).execute(&mut *tx).await?;
        let changed = sqlx::query("UPDATE epoch_work SET state=? WHERE key=?")
            .bind(state)
            .bind(key)
            .execute(&mut *tx)
            .await?;
        ensure!(
            changed.rows_affected() == 1,
            "Epoch receipt missing from journal"
        );
        tx.commit().await?;
        Ok(())
    }
    /// Receipt outcome for a batch nonce: every attempt of the nonce, the nonce floor and
    /// each member's terminal state commit together. The caller must account for exactly
    /// the journaled member list; a partial resolution is refused.
    pub async fn resolve_nonce_batch(
        &self,
        nonce: i64,
        key: &str,
        states: &[(String, &str)],
    ) -> Result<()> {
        let next = nonce
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Nonce overflow"))?;
        let mut tx = self.pool.begin().await?;
        let members = Self::members_in(&mut tx, key).await?;
        ensure!(!members.is_empty(), "Batch members missing from journal");
        let accounted: std::collections::BTreeSet<&str> =
            states.iter().map(|(id, _)| id.as_str()).collect();
        ensure!(
            accounted.len() == states.len()
                && accounted.len() == members.len()
                && members.iter().all(|id| accounted.contains(id.as_str())),
            "Batch resolution does not account for every member exactly once"
        );
        sqlx::query("UPDATE txs SET state='resolved' WHERE nonce=?")
            .bind(nonce)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO meta(key,value) VALUES('nonce_floor',?) ON CONFLICT(key) DO UPDATE SET value=CAST(MAX(CAST(meta.value AS INTEGER),CAST(excluded.value AS INTEGER)) AS TEXT)")
            .bind(next.to_string()).execute(&mut *tx).await?;
        for (id, state) in states {
            let updated = sqlx::query("UPDATE jobs SET state=? WHERE id=?")
                .bind(state)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            ensure!(
                updated.rows_affected() == 1,
                "Batch member {id} missing from journal"
            );
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn nonce_floor(&self) -> Result<u64> {
        Ok(self
            .meta("nonce_floor")
            .await?
            .unwrap_or_else(|| "0".into())
            .parse()?)
    }
    pub async fn broadcast_attempt(&self, id: i64, now: i64) -> Result<()> {
        sqlx::query("UPDATE txs SET broadcast=? WHERE id=?")
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn active_nonce_lane_reads_do_not_scan_resolved_history() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(&dir.path().join("active.sqlite"), "scope")
            .await
            .unwrap();
        // Plans the exact texts executed by unresolved() and signed().
        for explain in [
            sqlx::query(concat!("EXPLAIN QUERY PLAN ", unresolved_txs_sql!())),
            sqlx::query(concat!("EXPLAIN QUERY PLAN ", nonce_lane_conflicts_sql!()))
                .bind(0i64)
                .bind("job"),
        ] {
            let plan: Vec<String> = explain
                .fetch_all(&j.pool)
                .await
                .unwrap()
                .iter()
                .map(|row| row.get("detail"))
                .collect();
            assert!(
                plan.iter().any(|detail| detail.contains("txs_active")),
                "{plan:?}"
            );
        }
        // The per-member ownership read of signed_batch() is keyed by job, so SQLite may
        // prefer the (job,state) index over the active-lane index; either way it must
        // seek by index and never scan the txs table.
        let plan: Vec<String> =
            sqlx::query(concat!("EXPLAIN QUERY PLAN ", member_lane_conflicts_sql!()))
                .bind("batch:1:2:00")
                .bind("1")
                .bind("1")
                .fetch_all(&j.pool)
                .await
                .unwrap()
                .iter()
                .map(|row| row.get("detail"))
                .collect();
        assert!(
            plan.iter()
                .any(|detail| detail.contains("SEARCH txs USING"))
                && !plan.iter().any(|detail| detail.starts_with("SCAN txs")),
            "{plan:?}"
        );
    }
    #[tokio::test]
    async fn public_admission_revisits_excluded_requests_once_without_losing_nonce_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("public.sqlite");
        let j = Journal::open(&path, "scope").await.unwrap();
        for (id, deadline) in [("1", 200), ("2", 99), ("3", 200), ("4", 200), ("5", 200)] {
            j.discovered(id, deadline, "999").await.unwrap();
            j.state(id, if id == "5" { "served" } else { "ignored" })
                .await
                .unwrap();
        }
        sqlx::query("UPDATE jobs SET proof='fixed-proof',call='fixed-call' WHERE id='3'")
            .execute(&j.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO txs(job,nonce,hash,raw,kind,fee,gas,priority,payload,created) VALUES('4',8,'h','signed-bytes','fulfill','1',1,'1','fixed-payload',1)").execute(&j.pool).await.unwrap();
        sqlx::query("INSERT INTO meta(key,value) VALUES('nonce_floor','8') ON CONFLICT(key) DO UPDATE SET value='8'").execute(&j.pool).await.unwrap();
        sqlx::raw_sql("CREATE TRIGGER refuse_public BEFORE INSERT ON meta WHEN NEW.key='consumer_access' BEGIN SELECT RAISE(ABORT,'injected crash'); END;").execute(&j.pool).await.unwrap();
        assert!(j.enable_public_service(100).await.is_err());
        assert_eq!(j.meta("cursor").await.unwrap().as_deref(), Some("999"));
        assert_eq!(j.job("1").await.unwrap().unwrap().state, "ignored");
        sqlx::query("DROP TRIGGER refuse_public")
            .execute(&j.pool)
            .await
            .unwrap();
        j.enable_public_service(100).await.unwrap();
        assert_eq!(j.meta("cursor").await.unwrap().as_deref(), Some("1"));
        for (id, state) in [
            ("1", "pending"),
            ("2", "ignored"),
            ("3", "prepared"),
            ("4", "ignored"),
            ("5", "served"),
        ] {
            assert_eq!(j.job(id).await.unwrap().unwrap().state, state);
        }
        assert_eq!(
            j.job("3").await.unwrap().unwrap().call.as_deref(),
            Some("fixed-call")
        );
        assert_eq!(j.unresolved().await.unwrap()[0].raw, "signed-bytes");
        assert_eq!(j.nonce_floor().await.unwrap(), 8);
        j.cursor("54").await.unwrap();
        j.pool.close().await;
        let j = Journal::open(&path, "scope").await.unwrap();
        j.enable_public_service(150).await.unwrap();
        assert_eq!(j.meta("cursor").await.unwrap().as_deref(), Some("54"));
        assert_eq!(j.nonce_floor().await.unwrap(), 8);
    }
    #[tokio::test]
    async fn discovery_commits_epoch_demand_with_job_and_cursor_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demand.sqlite");
        let j = Journal::open(&path, "scope").await.unwrap();
        sqlx::raw_sql("CREATE TRIGGER demand_crash BEFORE INSERT ON epoch_demand BEGIN SELECT RAISE(ABORT,'injected crash'); END;").execute(&j.pool).await.unwrap();
        assert!(j.discovered_epoch("1", 100, "2", Some(7)).await.is_err());
        j.pool.close().await;
        let j = Journal::open(&path, "scope").await.unwrap();
        assert!(j.job("1").await.unwrap().is_none());
        assert!(j.meta("cursor").await.unwrap().is_none());
        sqlx::query("DROP TRIGGER demand_crash")
            .execute(&j.pool)
            .await
            .unwrap();
        j.discovered_epoch("1", 100, "2", Some(7)).await.unwrap();
        j.pool.close().await;
        let j = Journal::open(&path, "scope").await.unwrap();
        let epoch: i64 = sqlx::query_scalar("SELECT epoch FROM epoch_demand WHERE job='1'")
            .fetch_one(&j.pool)
            .await
            .unwrap();
        assert_eq!(epoch, 7);
        assert_eq!(j.meta("cursor").await.unwrap().as_deref(), Some("2"));
        assert!(j.job("1").await.unwrap().is_some());
        j.pool.close().await;
    }

    #[tokio::test]
    async fn history_compaction_preserves_live_recovery_and_audit_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("compact.sqlite");
        let j = Journal::open(&path, "scope").await.unwrap();
        for (id, state) in [
            ("done", "served"),
            ("live", "prepared"),
            ("guarded", "expired"),
        ] {
            j.discovered(id, 100, "next").await.unwrap();
            j.prepared(id, "proof", "call").await.unwrap();
            j.state(id, state).await.unwrap();
        }
        sqlx::raw_sql("INSERT INTO epoch_work(key,registry,catalog,epoch,start,state,api,selection) VALUES('past','registry','catalog',1,200,'committed','packet','selection'),('future','registry','catalog',2,400,'prepared','first-packet','selection'),('guarded-epoch','registry','catalog',3,600,'expired','packet','selection'); INSERT INTO telemetry_outbox VALUES(1,'report','exact-payload',7,2);").execute(&j.pool).await.unwrap();
        for (job, nonce, hash, state) in [
            ("done", 1, "done-hash", "resolved"),
            ("guarded", 2, "old-hash", "resolved"),
            ("guarded", 2, "replacement-hash", "signed"),
            ("guarded-epoch", 3, "cancel-hash", "submitted"),
        ] {
            sqlx::query("INSERT INTO txs(job,nonce,hash,raw,kind,fee,state,gas,priority,payload,created) VALUES(?,?,?,'signed-bytes','fulfill','1',?,1,'1','payload',9)")
                .bind(job).bind(nonce).bind(hash).bind(state).execute(&j.pool).await.unwrap();
        }
        let audit_before: Vec<(i64, Option<String>, String, String, i64)> =
            sqlx::query_as("SELECT * FROM audit_events ORDER BY cursor")
                .fetch_all(&j.pool)
                .await
                .unwrap();
        j.compact_history(100).await.unwrap();
        assert!(j.job("done").await.unwrap().unwrap().proof.is_none());
        for id in ["live", "guarded"] {
            assert_eq!(
                j.job(id).await.unwrap().unwrap().proof.as_deref(),
                Some("proof")
            );
        }
        let rows: Vec<(String, String, String)> =
            sqlx::query_as("SELECT hash,raw,payload FROM txs ORDER BY id")
                .fetch_all(&j.pool)
                .await
                .unwrap();
        assert_eq!(rows[0], ("done-hash".into(), "".into(), "".into()));
        assert!(
            rows[1..]
                .iter()
                .all(|r| r.1 == "signed-bytes" && r.2 == "payload")
        );
        let epochs: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT key,api FROM epoch_work ORDER BY key")
                .fetch_all(&j.pool)
                .await
                .unwrap();
        assert_eq!(
            epochs,
            vec![
                ("future".into(), Some("first-packet".into())),
                ("guarded-epoch".into(), Some("packet".into())),
                ("past".into(), None)
            ]
        );
        j.pool.close().await;
        let j = Journal::open(&path, "scope").await.unwrap();
        j.compact_history(101).await.unwrap();
        j.compact_history(160).await.unwrap();
        let audit_after: Vec<(i64, Option<String>, String, String, i64)> =
            sqlx::query_as("SELECT * FROM audit_events ORDER BY cursor")
                .fetch_all(&j.pool)
                .await
                .unwrap();
        assert_eq!(audit_before, audit_after);
        let outbox: (i64, String, String, i64, i64) =
            sqlx::query_as("SELECT * FROM telemetry_outbox")
                .fetch_one(&j.pool)
                .await
                .unwrap();
        assert_eq!(outbox, (1, "report".into(), "exact-payload".into(), 7, 2));
        assert_eq!(
            crate::epoch::work(&j.pool, "future")
                .await
                .unwrap()
                .api
                .as_deref(),
            Some("first-packet")
        );
        j.pool.close().await;
    }

    #[tokio::test]
    async fn history_compaction_rolls_back_payloads_and_cadence_together() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("compact-crash.sqlite");
        let j = Journal::open(&path, "scope").await.unwrap();
        j.discovered("done", 1, "next").await.unwrap();
        j.prepared("done", "proof", "call").await.unwrap();
        j.state("done", "served").await.unwrap();
        sqlx::raw_sql("CREATE TRIGGER compact_crash BEFORE INSERT ON meta WHEN NEW.key='history:compact_after' BEGIN SELECT RAISE(ABORT,'injected crash'); END;").execute(&j.pool).await.unwrap();
        assert!(j.compact_history(100).await.is_err());
        j.pool.close().await;
        let j = Journal::open(&path, "scope").await.unwrap();
        assert_eq!(
            j.job("done").await.unwrap().unwrap().proof.as_deref(),
            Some("proof")
        );
        assert!(j.meta("history:compact_after").await.unwrap().is_none());
        sqlx::query("DROP TRIGGER compact_crash")
            .execute(&j.pool)
            .await
            .unwrap();
        j.compact_history(100).await.unwrap();
        assert!(j.job("done").await.unwrap().unwrap().proof.is_none());
        j.pool.close().await;
    }

    #[tokio::test]
    async fn history_compaction_batches_and_cadence_are_durable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("batch.sqlite");
        let j = Journal::open(&path, "scope").await.unwrap();
        sqlx::raw_sql("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<300) INSERT INTO jobs(id,deadline,state,proof,call) SELECT CAST(x AS TEXT),1,'expired','proof','call' FROM n;").execute(&j.pool).await.unwrap();
        j.compact_history(100).await.unwrap();
        j.pool.close().await;
        let j = Journal::open(&path, "scope").await.unwrap();
        j.compact_history(159).await.unwrap();
        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE proof IS NOT NULL")
                .fetch_one(&j.pool)
                .await
                .unwrap();
        assert_eq!(remaining, 172);
        j.compact_history(160).await.unwrap();
        j.compact_history(220).await.unwrap();
        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE proof IS NOT NULL")
                .fetch_one(&j.pool)
                .await
                .unwrap();
        assert_eq!(remaining, 0);
        let retained: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs")
            .fetch_one(&j.pool)
            .await
            .unwrap();
        assert_eq!(retained, 300);
        j.pool.close().await;
    }

    #[tokio::test]
    async fn prepared_backoff_survives_restart_and_does_not_hide_urgent_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edf.sqlite");
        let j = Journal::open(&path, "scope").await.unwrap();
        for (id, deadline) in [("1", 110), ("2", 120)] {
            j.discovered(id, deadline, "3").await.unwrap();
            j.prepared(id, "proof", "call").await.unwrap();
        }
        j.preflight_backoff("1", 102).await.unwrap();
        j.pool.close().await;
        let j = Journal::open(&path, "scope").await.unwrap();
        assert_eq!(
            j.prepared_due(101)
                .await
                .unwrap()
                .iter()
                .map(|j| j.id.as_str())
                .collect::<Vec<_>>(),
            ["2"]
        );
        assert_eq!(
            j.prepared_due(102)
                .await
                .unwrap()
                .iter()
                .map(|j| j.id.as_str())
                .collect::<Vec<_>>(),
            ["1", "2"]
        );
        assert_eq!(
            j.job("1").await.unwrap().unwrap().proof.as_deref(),
            Some("proof")
        );
        j.pool.close().await;
    }
    #[tokio::test]
    async fn finalized_receipts_and_checkpoints_survive_restart_and_conflicts_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("finality.sqlite");
        let journal = Journal::open(&path, "scope").await.unwrap();
        journal
            .finalized_receipt("tx1", 10, "block10", 1)
            .await
            .unwrap();
        journal.pool.close().await;
        let journal = Journal::open(&path, "scope").await.unwrap();
        let saved = journal.meta("finalized_checkpoint").await.unwrap();
        assert_eq!(
            saved,
            Some(serde_json::to_string(&(10, "block10")).unwrap())
        );
        assert!(
            journal
                .finalized_receipt("tx1", 20, "other", 1)
                .await
                .is_err()
        );
        assert_eq!(journal.meta("finalized_checkpoint").await.unwrap(), saved);
        sqlx::raw_sql("CREATE TRIGGER refuse_checkpoint BEFORE INSERT ON finalized_receipts WHEN NEW.hash='tx2' BEGIN SELECT RAISE(ABORT,'injected failure'); END;").execute(&journal.pool).await.unwrap();
        assert!(
            journal
                .finalized_receipt("tx2", 20, "block20", 1)
                .await
                .is_err()
        );
        assert_eq!(journal.meta("finalized_checkpoint").await.unwrap(), saved);
        journal.finalized_checkpoint(9, "old").await.unwrap();
        assert_eq!(journal.meta("finalized_checkpoint").await.unwrap(), saved);
        assert!(journal.finalized_checkpoint(10, "changed").await.is_err());
        assert_eq!(
            journal.nonce_floor().await.unwrap(),
            0,
            "checkpoint does not resolve a nonce by itself"
        );
    }
    #[tokio::test]
    async fn unavailable_preparation_prefix_cannot_hide_ready_work_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preparation.sqlite");
        let journal = Journal::open(&path, "scope").await.unwrap();
        for id in 1..=9 {
            journal
                .discovered(&id.to_string(), 160, "10")
                .await
                .unwrap();
        }
        let first = journal
            .claim_preparation(100_000, 105, &[], false)
            .await
            .unwrap();
        assert_eq!(
            first.iter().map(|j| j.id.as_str()).collect::<Vec<_>>(),
            ["1", "2", "3", "4", "5", "6", "7", "8"]
        );
        let attempted = first.into_iter().map(|j| j.id).collect::<Vec<_>>();
        journal.pool.close().await;
        let journal = Journal::open(&path, "scope").await.unwrap();
        let second = journal
            .claim_preparation(100_000, 105, &attempted, false)
            .await
            .unwrap();
        assert_eq!(
            second.iter().map(|j| j.id.as_str()).collect::<Vec<_>>(),
            ["9"]
        );
        journal.prepared("9", "proof", "call").await.unwrap();
        assert_eq!(journal.prepared_due(100).await.unwrap()[0].id, "9");
        assert!(
            journal
                .claim_preparation(100_100, 106, &[], false)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            journal
                .claim_preparation(100_250, 107, &[], false)
                .await
                .unwrap()
                .len(),
            8
        );
        assert!(
            journal
                .claim_preparation(160_000, 165, &[], false)
                .await
                .unwrap()
                .is_empty()
        );
    }
    #[tokio::test]
    async fn preparation_fairness_is_not_limited_to_the_first_256_pending_rows() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("large.sqlite"), "scope")
            .await
            .unwrap();
        for id in 1..=300 {
            journal
                .discovered(&id.to_string(), 160, "301")
                .await
                .unwrap();
        }
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..38 {
            for job in journal
                .claim_preparation(100, 105, &[], false)
                .await
                .unwrap()
            {
                assert!(seen.insert(job.id));
            }
        }
        assert_eq!(seen.len(), 300);
        assert!(seen.contains("300"));
    }
    #[tokio::test]
    async fn a_follower_claims_preparation_from_the_newest_end_of_the_queue() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("tail.sqlite"), "scope")
            .await
            .unwrap();
        // A burst opens every request within a second or two: the deadlines barely differ, so id order decides.
        for id in 1..=40 {
            journal
                .discovered(&id.to_string(), 160 + i64::from(id > 20), "41")
                .await
                .unwrap();
        }
        let ids = |jobs: Vec<Job>| jobs.into_iter().map(|job| job.id).collect::<Vec<_>>();
        let head = ids(journal
            .claim_preparation(100, 105, &[], false)
            .await
            .unwrap());
        assert_eq!(head, (1..=8).map(|id| id.to_string()).collect::<Vec<_>>());
        let tail = ids(journal
            .claim_preparation(100, 105, &[], true)
            .await
            .unwrap());
        assert_eq!(
            tail,
            (33..=40).rev().map(|id| id.to_string()).collect::<Vec<_>>()
        );
        // The retry stamp still rotates the claimed prefix away, at either end.
        let next = ids(journal
            .claim_preparation(100, 105, &[], true)
            .await
            .unwrap());
        assert_eq!(
            next,
            (25..=32).rev().map(|id| id.to_string()).collect::<Vec<_>>()
        );
    }
    #[tokio::test]
    async fn receipt_resolution_rolls_back_and_reopens_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipt.sqlite");
        let j = Journal::open(&path, "scope").await.unwrap();
        j.discovered("1", 100, "2").await.unwrap();
        sqlx::query("INSERT INTO txs(job,nonce,hash,raw,kind,fee,gas,priority,payload,created) VALUES('1',7,'h','r','fulfill','1',1,'1','p',1)")
            .execute(&j.pool).await.unwrap();
        sqlx::raw_sql("CREATE TRIGGER reject_terminal BEFORE UPDATE OF state ON jobs WHEN NEW.state='served' BEGIN SELECT RAISE(ABORT,'injected crash'); END;")
            .execute(&j.pool).await.unwrap();
        assert!(j.resolve_nonce_job(7, "1", "served").await.is_err());
        j.pool.close().await;
        let j = Journal::open(&path, "scope").await.unwrap();
        assert_eq!(j.unresolved().await.unwrap().len(), 1);
        assert_eq!(j.nonce_floor().await.unwrap(), 0);
        assert_eq!(j.pending().await.unwrap()[0].state, "pending");
        sqlx::query("DROP TRIGGER reject_terminal")
            .execute(&j.pool)
            .await
            .unwrap();
        j.resolve_nonce_job(7, "1", "served").await.unwrap();
        j.pool.close().await;
        let j = Journal::open(&path, "scope").await.unwrap();
        assert!(j.unresolved().await.unwrap().is_empty());
        assert!(j.pending().await.unwrap().is_empty());
        assert_eq!(j.nonce_floor().await.unwrap(), 8);
        let state: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id='1'")
            .fetch_one(&j.pool)
            .await
            .unwrap();
        assert_eq!(state, "served");
        j.pool.close().await;
    }
    fn batch_attempt(job: &str, nonce: i64, hash: &str, kind: &str) -> Attempt {
        Attempt {
            id: 0,
            job: job.into(),
            nonce,
            hash: hash.into(),
            raw: "raw".into(),
            kind: kind.into(),
            fee: "10".into(),
            state: "signed".into(),
            gas: 21000,
            priority: "1".into(),
            payload: "0x".into(),
            created: 1,
            broadcast: 0,
        }
    }
    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn batch_key_is_deterministic_ordered_and_bounded() {
        let key = batch_key(&ids(&["7", "9"])).unwrap();
        assert_eq!(key, batch_key(&ids(&["7", "9"])).unwrap());
        assert!(key.starts_with("batch:7:2:"));
        assert_eq!(key.len(), "batch:7:2:".len() + 16);
        assert!(is_batch_job(&key));
        assert!(!is_batch_job("7"));
        assert_ne!(key, batch_key(&ids(&["9", "7"])).unwrap());
        assert_ne!(key, batch_key(&ids(&["7", "9", "11"])).unwrap());
        assert!(batch_key(&ids(&["7"])).is_err());
        let sixteen: Vec<String> = (1..=16).map(|n| n.to_string()).collect();
        assert!(batch_key(&sixteen).is_ok());
        let seventeen: Vec<String> = (1..=17).map(|n| n.to_string()).collect();
        assert!(batch_key(&seventeen).is_err());
    }
    #[tokio::test]
    async fn batch_signing_commits_lane_members_and_states_together_or_not_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("batch-sign.sqlite");
        let j = Journal::open(&path, "scope").await.unwrap();
        for id in ["1", "2", "3"] {
            j.discovered(id, 100, "4").await.unwrap();
            j.prepared(id, "proof", "call").await.unwrap();
        }
        let members = ids(&["1", "2"]);
        let key = batch_key(&members).unwrap();
        // The last member's state change is the last write: an abort there must undo everything.
        sqlx::raw_sql("CREATE TRIGGER refuse_last BEFORE UPDATE OF state ON jobs WHEN NEW.id='2' AND NEW.state='signed' BEGIN SELECT RAISE(ABORT,'injected crash'); END;")
            .execute(&j.pool).await.unwrap();
        assert!(
            j.signed_batch(&batch_attempt(&key, 7, "h1", "fulfill_batch"), &members)
                .await
                .is_err()
        );
        j.pool.close().await;
        let j = Journal::open(&path, "scope").await.unwrap();
        assert!(j.unresolved().await.unwrap().is_empty());
        assert!(j.batch_members(&key).await.unwrap().is_empty());
        assert_eq!(j.job("1").await.unwrap().unwrap().state, "prepared");
        sqlx::query("DROP TRIGGER refuse_last")
            .execute(&j.pool)
            .await
            .unwrap();
        // Kind, key derivation, size and duplicates are refused before anything is written.
        assert!(
            j.signed_batch(&batch_attempt(&key, 7, "h1", "fulfill"), &members)
                .await
                .is_err()
        );
        assert!(
            j.signed_batch(
                &batch_attempt("batch:1:2:0000000000000000", 7, "h1", "fulfill_batch"),
                &members
            )
            .await
            .is_err()
        );
        let duplicate = ids(&["1", "1"]);
        assert!(
            j.signed_batch(
                &batch_attempt(&batch_key(&duplicate).unwrap(), 7, "h1", "fulfill_batch"),
                &duplicate
            )
            .await
            .is_err()
        );
        // A member that is not prepared (still pending) cannot be signed into a batch.
        let unprepared = ids(&["1", "9"]);
        j.discovered("9", 100, "10").await.unwrap();
        assert!(
            j.signed_batch(
                &batch_attempt(&batch_key(&unprepared).unwrap(), 7, "h1", "fulfill_batch"),
                &unprepared
            )
            .await
            .is_err()
        );
        assert!(j.unresolved().await.unwrap().is_empty());
        j.signed_batch(&batch_attempt(&key, 7, "h1", "fulfill_batch"), &members)
            .await
            .unwrap();
        j.pool.close().await;
        let j = Journal::open(&path, "scope").await.unwrap();
        assert_eq!(j.batch_members(&key).await.unwrap(), members);
        for id in ["1", "2"] {
            assert_eq!(j.job(id).await.unwrap().unwrap().state, "signed");
        }
        assert_eq!(j.job("3").await.unwrap().unwrap().state, "prepared");
        let live = j.unresolved().await.unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(
            (live[0].job.as_str(), live[0].kind.as_str()),
            (key.as_str(), "fulfill_batch")
        );
        // A replacement of the same batch: identical members are accepted, any change is refused.
        j.members_state(&key, "submitted").await.unwrap();
        assert!(
            j.signed_batch(
                &batch_attempt(&key, 7, "h2", "fulfill_batch"),
                &ids(&["1", "3"])
            )
            .await
            .is_err()
        );
        j.signed_batch(&batch_attempt(&key, 7, "h2", "fulfill_batch"), &members)
            .await
            .unwrap();
        assert_eq!(j.unresolved().await.unwrap().len(), 2);
        assert_eq!(j.job("1").await.unwrap().unwrap().state, "signed");
        // The nonce cancellation of a batch job keeps every member on the lane it already owns.
        j.signed(&batch_attempt(&key, 7, "h3", "cancel"))
            .await
            .unwrap();
        assert_eq!(j.unresolved().await.unwrap().len(), 3);
        assert!(
            j.signed(&batch_attempt(&key, 7, "h4", "fulfill"))
                .await
                .is_err(),
            "a single fulfillment must never target a batch key"
        );
        j.pool.close().await;
    }
    #[tokio::test]
    async fn batch_signing_refuses_members_live_under_another_nonce() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(&dir.path().join("batch-lane.sqlite"), "scope")
            .await
            .unwrap();
        for id in ["1", "2", "3", "4"] {
            j.discovered(id, 100, "5").await.unwrap();
            j.prepared(id, "proof", "call").await.unwrap();
        }
        // Member "1" has its own unresolved single attempt on nonce 7.
        j.signed(&batch_attempt("1", 7, "single", "fulfill"))
            .await
            .unwrap();
        let members = ids(&["1", "2"]);
        let error = j
            .signed_batch(
                &batch_attempt(&batch_key(&members).unwrap(), 7, "b1", "fulfill_batch"),
                &members,
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("Request 1 is already live"),
            "{error}"
        );
        j.resolve_nonce_job(7, "1", "served").await.unwrap();
        // Members "2","3" are live in one batch; a second batch containing "3" is refused
        // even under the same nonce, and so is the member's own single attempt.
        let first = ids(&["2", "3"]);
        j.signed_batch(
            &batch_attempt(&batch_key(&first).unwrap(), 8, "b2", "fulfill_batch"),
            &first,
        )
        .await
        .unwrap();
        let second = ids(&["3", "4"]);
        let error = j
            .signed_batch(
                &batch_attempt(&batch_key(&second).unwrap(), 8, "b3", "fulfill_batch"),
                &second,
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("Request 3 is already live"),
            "{error}"
        );
        assert!(
            j.signed(&batch_attempt("3", 8, "s3", "fulfill"))
                .await
                .is_err()
        );
        assert_eq!(j.unresolved().await.unwrap().len(), 1);
        assert_eq!(j.job("4").await.unwrap().unwrap().state, "prepared");
        j.pool.close().await;
    }
    #[tokio::test]
    async fn batch_resolution_is_atomic_accounts_for_every_member_and_advances_the_floor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("batch-resolve.sqlite");
        let j = Journal::open(&path, "scope").await.unwrap();
        for id in ["1", "2", "3"] {
            j.discovered(id, 100, "4").await.unwrap();
            j.prepared(id, "proof", "call").await.unwrap();
        }
        let members = ids(&["1", "2", "3"]);
        let key = batch_key(&members).unwrap();
        j.signed_batch(&batch_attempt(&key, 7, "h1", "fulfill_batch"), &members)
            .await
            .unwrap();
        j.signed_batch(&batch_attempt(&key, 7, "h2", "fulfill_batch"), &members)
            .await
            .unwrap();
        fn served(list: &[(&str, &'static str)]) -> Vec<(String, &'static str)> {
            list.iter()
                .map(|(id, state)| (id.to_string(), *state))
                .collect()
        }
        // Partial, duplicated and foreign member lists are refused before any write.
        for states in [
            served(&[("1", "served"), ("2", "served")]),
            served(&[("1", "served"), ("2", "served"), ("2", "expired")]),
            served(&[("1", "served"), ("2", "served"), ("9", "served")]),
        ] {
            assert!(j.resolve_nonce_batch(7, &key, &states).await.is_err());
        }
        assert!(
            j.resolve_nonce_batch(7, "batch:unknown", &served(&[("1", "served")]))
                .await
                .is_err()
        );
        assert_eq!(j.unresolved().await.unwrap().len(), 2);
        assert_eq!(j.nonce_floor().await.unwrap(), 0);
        sqlx::raw_sql("CREATE TRIGGER reject_last BEFORE UPDATE OF state ON jobs WHEN NEW.id='3' AND NEW.state='expired' BEGIN SELECT RAISE(ABORT,'injected crash'); END;")
            .execute(&j.pool).await.unwrap();
        let outcome = served(&[("1", "served"), ("2", "callback_failed"), ("3", "expired")]);
        assert!(j.resolve_nonce_batch(7, &key, &outcome).await.is_err());
        j.pool.close().await;
        let j = Journal::open(&path, "scope").await.unwrap();
        assert_eq!(j.unresolved().await.unwrap().len(), 2);
        assert_eq!(j.nonce_floor().await.unwrap(), 0);
        for id in ["1", "2", "3"] {
            assert_eq!(j.job(id).await.unwrap().unwrap().state, "signed");
        }
        sqlx::query("DROP TRIGGER reject_last")
            .execute(&j.pool)
            .await
            .unwrap();
        j.resolve_nonce_batch(7, &key, &outcome).await.unwrap();
        j.pool.close().await;
        let j = Journal::open(&path, "scope").await.unwrap();
        assert!(j.unresolved().await.unwrap().is_empty());
        assert_eq!(j.nonce_floor().await.unwrap(), 8);
        for (id, state) in [("1", "served"), ("2", "callback_failed"), ("3", "expired")] {
            assert_eq!(j.job(id).await.unwrap().unwrap().state, state);
        }
        assert_eq!(j.batch_members(&key).await.unwrap(), members);
        // History compaction now treats the members like any resolved terminal job.
        j.compact_history(100).await.unwrap();
        for id in ["1", "2", "3"] {
            assert!(j.job(id).await.unwrap().unwrap().proof.is_none());
        }
        let bodies: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM txs WHERE state='resolved' AND (raw!='' OR payload!='')",
        )
        .fetch_one(&j.pool)
        .await
        .unwrap();
        assert_eq!(bodies, 0);
        j.pool.close().await;
    }
    #[tokio::test]
    async fn history_compaction_keeps_members_of_a_live_batch() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(&dir.path().join("batch-compact.sqlite"), "scope")
            .await
            .unwrap();
        for id in ["1", "2"] {
            j.discovered(id, 100, "3").await.unwrap();
            j.prepared(id, "proof", "call").await.unwrap();
        }
        let members = ids(&["1", "2"]);
        let key = batch_key(&members).unwrap();
        j.signed_batch(&batch_attempt(&key, 7, "h1", "fulfill_batch"), &members)
            .await
            .unwrap();
        // Even a member whose journal state were terminal keeps its body while its batch is live.
        sqlx::query("UPDATE jobs SET state='served' WHERE id='2'")
            .execute(&j.pool)
            .await
            .unwrap();
        j.compact_history(100).await.unwrap();
        for id in ["1", "2"] {
            assert_eq!(
                j.job(id).await.unwrap().unwrap().proof.as_deref(),
                Some("proof")
            );
        }
        assert_eq!(j.unresolved().await.unwrap()[0].raw, "raw");
        j.pool.close().await;
    }
    #[tokio::test]
    async fn expired_backlog_does_not_hide_new_work_or_discard_a_nonce_lane() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("jobs.sqlite"), "scope")
            .await
            .unwrap();
        sqlx::raw_sql("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000) INSERT INTO jobs(id,deadline) SELECT CAST(x AS TEXT),10 FROM n;").execute(&journal.pool).await.unwrap();
        journal.discovered("1001", 100, "1002").await.unwrap();
        journal.state("1", "submitted").await.unwrap();
        journal.expire_unstarted(50).await.unwrap();
        let jobs = journal.pending().await.unwrap();
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().any(|j| j.id == "1001"));
        assert!(jobs.iter().any(|j| j.id == "1"));
    }
    #[tokio::test]
    async fn durable_signed_bytes_and_scope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.sqlite");
        let j = Journal::open(&path, "chain:contract:sender").await.unwrap();
        j.discovered("1", 100, "2").await.unwrap();
        j.prepared("1", "proof", "call").await.unwrap();
        j.signed(&Attempt {
            id: 0,
            job: "1".into(),
            nonce: 7,
            hash: "hash".into(),
            raw: "raw".into(),
            kind: "fulfill".into(),
            fee: "10".into(),
            state: "signed".into(),
            gas: 21000,
            priority: "1".into(),
            payload: "0x".into(),
            created: 1,
            broadcast: 0,
        })
        .await
        .unwrap();
        j.preflight_next(Some("2")).await.unwrap();
        j.pool.close().await;
        let j = Journal::open(&path, "chain:contract:sender").await.unwrap();
        assert_eq!(
            j.meta("preflight_next").await.unwrap().as_deref(),
            Some("2")
        );
        j.preflight_next(None).await.unwrap();
        assert_eq!(j.meta("preflight_next").await.unwrap(), None);
        assert_eq!(j.unresolved().await.unwrap()[0].raw, "raw");
        assert_eq!(
            j.pending().await.unwrap()[0].proof.as_deref(),
            Some("proof")
        );
        assert_eq!(j.meta("cursor").await.unwrap().as_deref(), Some("2"));
        j.resolve_nonce(7).await.unwrap();
        assert_eq!(j.nonce_floor().await.unwrap(), 8);
        j.resolve_nonce(3).await.unwrap();
        assert_eq!(j.nonce_floor().await.unwrap(), 8, "nonce floor regressed");
        // An old journal without this metadata must recover its floor from resolved attempts.
        sqlx::query("DELETE FROM meta WHERE key='nonce_floor'")
            .execute(&j.pool)
            .await
            .unwrap();
        j.pool.close().await;
        let j = Journal::open(&path, "chain:contract:sender").await.unwrap();
        assert_eq!(j.nonce_floor().await.unwrap(), 8);
        j.pool.close().await;
        assert!(Journal::open(&path, "other-chain").await.is_err());
    }
}
