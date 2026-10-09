use crate::config::{CoordinatorKind, FinalityMode};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
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
/// The epoch each request of an epoch coordinator waits on, as 0.4.1 has it.
const EPOCH_DEMAND_DDL: &str = "CREATE TABLE IF NOT EXISTS epoch_demand(job TEXT PRIMARY KEY,epoch INTEGER NOT NULL); CREATE INDEX IF NOT EXISTS epoch_demand_epoch ON epoch_demand(epoch,job);";
/// The marks of the blocks a soft keeper acted on (`Mark`).
const SOFT_MARKS_DDL: &str = "CREATE TABLE IF NOT EXISTS soft_marks(number INTEGER NOT NULL,hash TEXT NOT NULL,kind TEXT NOT NULL,ref TEXT NOT NULL,created INTEGER NOT NULL,status INTEGER,PRIMARY KEY(number,kind,ref));";
/// The round each request of a round coordinator is bound to (COORDINATOR_KIND=round), one row per job: its beacon and
/// round, the fingerprint of the fields its seed binds that a reorg could change (`round::fingerprint`), and when this
/// keeper first saw it. Only a round coordinator's journal has it.
pub const ROUND_DEMAND_DDL: &str = "CREATE TABLE IF NOT EXISTS round_demand(job TEXT PRIMARY KEY,beacon INTEGER NOT NULL,round INTEGER NOT NULL,fingerprint TEXT NOT NULL,sealing_lag_ms INTEGER,seen_at INTEGER NOT NULL); CREATE INDEX IF NOT EXISTS round_demand_round ON round_demand(beacon,round,job);";
/// What discovery journals of a round coordinator's request beside its job: see `ROUND_DEMAND_DDL`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundAssignment {
    pub beacon: u8,
    pub round: u64,
    /// `0x`-prefixed hex of the 32-byte fingerprint.
    pub fingerprint: String,
    /// The keeper's wall clock at first sight less the request block's time, in milliseconds; informational.
    pub sealing_lag_ms: i64,
    /// The keeper's wall clock at first sight, in Unix seconds.
    pub seen_at: u64,
}
/// A round that live requests wait on: its beacon and round, how many requests, and the earliest of their deadlines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DemandedRound {
    pub beacon: u8,
    pub round: u64,
    pub requests: u64,
    pub earliest_deadline: u64,
}
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
/// A block the keeper holds the chain to on every tick, under a meta key: the highest block it has relied on. The
/// finalized checkpoint is the highest finalized block, which no endpoint may contradict. The soft checkpoint is the
/// highest block a soft tick decided on, which the next tick checks is still the chain's.
#[derive(Clone, Copy)]
struct Checkpoint {
    key: &'static str,
    conflict: &'static str,
}
const FINALIZED_CHECKPOINT: Checkpoint = Checkpoint {
    key: "finalized_checkpoint",
    conflict: "Finalized checkpoint conflict",
};
const SOFT_CHECKPOINT: Checkpoint = Checkpoint {
    key: "soft_checkpoint",
    conflict: "Soft checkpoint conflict",
};
/// Advance the checkpoint to the block `number` with `hash`. It never moves down, and another hash for its own number
/// is a conflict, not an update.
async fn checkpoint(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    which: Checkpoint,
    number: u64,
    hash: &str,
) -> Result<()> {
    let old: Option<String> = sqlx::query_scalar("SELECT value FROM meta WHERE key=?")
        .bind(which.key)
        .fetch_optional(&mut **tx)
        .await?;
    if let Some(old) = old {
        let (previous, previous_hash): (u64, String) = serde_json::from_str(&old)?;
        if previous > number {
            return Ok(());
        }
        ensure!(
            previous != number || previous_hash == hash,
            "{}",
            which.conflict
        );
        if previous == number {
            return Ok(());
        }
    }
    sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value").bind(which.key).bind(serde_json::to_string(&(number,hash))?).execute(&mut **tx).await?;
    Ok(())
}

/// What a soft mark records, and so what it justifies (`FINALITY_MODE=soft`). A mark is a block the keeper acted on,
/// written in the same transaction as the action, so that the finality audit can check the block against L1 finality.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkKind {
    /// A settled receipt: the reference is the transaction hash and the block is the receipt's.
    Receipt,
    /// The decision head of a tick. One row, replaced as the soft checkpoint moves, like the checkpoint itself.
    Head,
    /// The decision head a transaction was signed at: the reference is the transaction hash.
    Sign,
    /// The decision head at which a nonce was found consumed, when no endpoint served the receipt that used it: the
    /// reference is the nonce. A nonce is never resolved on the sequencer's word without a mark.
    Nonce,
}
impl MarkKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Receipt => "receipt",
            Self::Head => "head",
            Self::Sign => "sign",
            Self::Nonce => "nonce",
        }
    }
    fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "receipt" => Self::Receipt,
            "head" => Self::Head,
            "sign" => Self::Sign,
            "nonce" => Self::Nonce,
            other => anyhow::bail!("Unknown soft mark kind {other}"),
        })
    }
}
/// A row of `soft_marks`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    pub kind: MarkKind,
    /// The block the keeper acted on, as the sequencer's chain had it then.
    pub number: u64,
    pub hash: String,
    /// What the mark is about, by kind: a transaction hash, a nonce, or nothing for the head.
    pub reference: String,
    /// Wall-clock seconds when the keeper wrote it: the audit lag is measured from here.
    pub created: u64,
    /// A receipt's status, which `finalized_receipts` records when the audit moves the receipt there. The other kinds
    /// have none. (A column beside the five of the design: a finalized receipt names its status, and a mark is the
    /// only place that still knows it by the time the block is final.)
    pub status: Option<u64>,
}
/// A block the keeper acted on that is not the chain's. Found by the finality audit (the mark's kind) or by the soft
/// checkpoint (`soft_checkpoint`), as one endpoint showed it; recorded durably under `finality:mismatch` only once at
/// least two endpoints agree that the block has another hash (`finality::verdict`), or once an operator has said so
/// (`finality --acknowledge`). The record is the incident: from it the keeper recovers by itself, and starts no new work
/// until it has.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mismatch {
    /// The mark's kind (`receipt`, `sign`, `head`, `nonce`), `soft_checkpoint` for the block the previous tick decided
    /// on, or `finalized_receipt` / `finalized_checkpoint` for a mark that agrees with the chain but contradicts a
    /// finalized record the journal already holds.
    pub kind: String,
    pub number: u64,
    /// The mark's reference: a transaction hash, a nonce, or empty.
    pub reference: String,
    /// The hash the keeper recorded for the block, and the one the chain has for it.
    pub expected: String,
    pub actual: String,
    /// Wall-clock seconds when it was found.
    pub detected_at: u64,
}
impl Mismatch {
    /// The operator's handle on this mismatch: twelve hex digits of the hash of everything in the record. The
    /// acknowledgement names it, so that a command written down for one incident cannot acknowledge another.
    pub fn id(&self) -> String {
        let digest = alloy_primitives::keccak256(format!(
            "{}|{}|{}|{}|{}|{}",
            self.kind, self.number, self.reference, self.expected, self.actual, self.detected_at
        ));
        hex::encode(&digest[..6])
    }
}
/// An operator's word on a mismatch, written by `d20dao-keeper finality --acknowledge`, durably, under `finality:ack`.
/// The keeper does not wait for one: it recovers from a mismatch on record by itself. Acknowledging the mismatch on
/// record only says that someone has looked; acknowledging one that the keeper suspects and could not have confirmed
/// by two endpoints (`Suspected`) records it, so that the keeper recovers from it as from a confirmed one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Acknowledgement {
    pub id: String,
    pub acknowledged_at: u64,
}
/// A mismatch one endpoint showed and the endpoints have neither confirmed nor refuted (no two of them agree on the
/// block), as the keeper last left it under `finality:suspected` for `health` and `finality --status`. It is a note
/// and not the hold: the keeper holds its sends on the suspicion it keeps in memory, finds it again from the journal
/// and the chain after a restart (or does not, when the endpoint shows the journal's block again), and never reads this
/// note back for a decision. The keeper deletes it once the suspicion is settled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suspected {
    pub mismatch: Mismatch,
    /// How many times the endpoints were asked, each time without two of them agreeing.
    pub checks: u32,
    /// How many endpoints the keeper has, and how many of them answered the last time.
    pub endpoints: usize,
    pub answered: usize,
}
/// Where a soft keeper stands with the finality of what it has acted on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinalityState {
    /// No mismatch is on record.
    Clear,
    /// A mismatch is on record, and the keeper is recovering from it by itself: it takes the chain as it is now, puts
    /// the nonce lane and the jobs right, and clears the record. It starts no new work meanwhile. The acknowledgement is
    /// an operator's, when there is one.
    Recovering(Mismatch, Option<Acknowledgement>),
}
/// The meta key under which the first mismatch is kept. While it is set the audit does nothing.
pub const MISMATCH_KEY: &str = "finality:mismatch";
/// The acknowledgement of the mismatch on record, and what the recovery keeps while it works. All of them go with the
/// incident, in the transaction that clears the record.
pub const ACK_KEY: &str = "finality:ack";
/// The id of the mismatch about which the owner was asked to act, because the recovery from it kept failing, so that
/// a restart does not ask again.
pub const ALERTED_KEY: &str = "finality:alerted";
/// The id of the suspected mismatch about which the owner was asked to act, because no two endpoints settled it, so that
/// a restart does not ask again. Apart from `ALERTED_KEY`: a suspicion the owner acknowledged, or the endpoints confirmed,
/// keeps its id, and the recovery from it is paged on its own when it keeps failing.
pub const UNCONFIRMED_ALERTED_KEY: &str = "finality:unconfirmed_alerted";
/// The note of a suspected mismatch (`Suspected`).
pub const SUSPECTED_KEY: &str = "finality:suspected";
/// The last incident that was recovered from, with what the recovery did. It stays until the next one replaces it.
pub const LAST_RECOVERY_KEY: &str = "finality:last_recovery";
/// Prefix of the recovery's own progress keys (`finality:recovery:` with `confirmed`, `scan`, `fill`, `since`, `jobs`,
/// `rewind` and `stats`).
pub const RECOVERY_PREFIX: &str = "finality:recovery:";
/// What an audit of the marks found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Audit {
    /// A mismatch is already on record: nothing is audited until the recovery from it has cleared the record.
    Stopped(Mismatch),
    /// No mark lies at or below the finalized head.
    Idle,
    /// Every mark of these blocks is the chain's: its receipts are finalized receipts now, the finalized checkpoint is
    /// the highest of the blocks, and the marks are gone.
    Audited {
        blocks: usize,
        receipts: usize,
        checkpoint: (u64, String),
    },
    /// A mark is not the chain's as the endpoint that answered tells it (or contradicts a finalized record). Nothing
    /// was moved and nothing is recorded: the keeper asks the other endpoints first.
    Mismatch(Mismatch),
}
/// Forget a job's preparation and preflight backoffs and its exclusion from batches.
async fn clear_backoffs(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, job: &str) -> Result<()> {
    sqlx::query("DELETE FROM meta WHERE key IN (?,?,?)")
        .bind(format!("prepare_retry_ms:{job}"))
        .bind(format!("preflight_retry:{job}"))
        .bind(format!("batch_exclude:{job}"))
        .execute(&mut **tx)
        .await?;
    Ok(())
}
/// The state a job takes when its nonce resolves. `reprove` is a round coordinator's request that moved under a
/// cancelled fulfillment (review M2): it is `pending` again, its proof, calldata and backoffs gone, to be proved for the
/// request the chain has now. Any other state is set as it is.
async fn set_resolved_state(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    job: &str,
    state: &str,
) -> Result<sqlx::sqlite::SqliteQueryResult> {
    if state != "reprove" {
        return Ok(sqlx::query("UPDATE jobs SET state=? WHERE id=?")
            .bind(state)
            .bind(job)
            .execute(&mut **tx)
            .await?);
    }
    clear_backoffs(tx, job).await?;
    Ok(
        sqlx::query("UPDATE jobs SET state='pending',proof=NULL,call=NULL WHERE id=?")
            .bind(job)
            .execute(&mut **tx)
            .await?,
    )
}
/// Record `mark` in the transaction of the action it justifies. The same mark again changes nothing; another hash or
/// status for the same block, kind and reference is a conflict, never an update, so that evidence is not overwritten.
pub(crate) async fn write_mark(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    mark: &Mark,
) -> Result<()> {
    ensure!(
        mark.kind != MarkKind::Head,
        "A head mark is written with the soft checkpoint"
    );
    let number = i64::try_from(mark.number)?;
    let status = mark.status.map(i64::try_from).transpose()?;
    sqlx::query("INSERT INTO soft_marks(number,hash,kind,ref,created,status) VALUES(?,?,?,?,?,?) ON CONFLICT(number,kind,ref) DO NOTHING")
        .bind(number)
        .bind(&mark.hash)
        .bind(mark.kind.name())
        .bind(&mark.reference)
        .bind(i64::try_from(mark.created)?)
        .bind(status)
        .execute(&mut **tx)
        .await?;
    let stored: (String, Option<i64>) =
        sqlx::query_as("SELECT hash,status FROM soft_marks WHERE number=? AND kind=? AND ref=?")
            .bind(number)
            .bind(mark.kind.name())
            .bind(&mark.reference)
            .fetch_one(&mut **tx)
            .await?;
    ensure!(stored == (mark.hash.clone(), status), "Soft mark conflict");
    Ok(())
}
/// A row of `soft_marks` (number, hash, kind, ref, created, status) as a `Mark`.
fn mark_from_row(row: sqlx::sqlite::SqliteRow) -> Result<Mark> {
    Ok(Mark {
        kind: MarkKind::parse(&row.get::<String, _>("kind"))?,
        number: u64::try_from(row.get::<i64, _>("number"))?,
        hash: row.get("hash"),
        reference: row.get("ref"),
        created: u64::try_from(row.get::<i64, _>("created"))?,
        status: row
            .get::<Option<i64>, _>("status")
            .map(u64::try_from)
            .transpose()?,
    })
}
/// What `Journal::reopen_nonce` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reopened {
    /// The attempts of the nonce are in the lane again, and they are the only ones.
    Lane,
    /// The journal holds no signed bytes for the nonce, so the lane cannot be refilled. Nothing was changed.
    NoBytes,
}
/// The tick's `head` mark: the one row of its kind, replaced when the decision head moves up and kept when it does not,
/// like the soft checkpoint it is written with.
async fn head_mark(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    number: u64,
    hash: &str,
    created: u64,
) -> Result<()> {
    sqlx::query("DELETE FROM soft_marks WHERE kind='head' AND number<=?")
        .bind(i64::try_from(number)?)
        .execute(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO soft_marks(number,hash,kind,ref,created) SELECT ?,?,'head','',? WHERE NOT EXISTS(SELECT 1 FROM soft_marks WHERE kind='head')")
        .bind(i64::try_from(number)?)
        .bind(hash)
        .bind(i64::try_from(created)?)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
/// The statement with which `compact_history` of 0.4.1 blanks the signed bytes of resolved transactions, and so what
/// finalized mode runs.
const COMPACT_TXS: &str = "UPDATE txs SET raw='',payload='' WHERE id IN (SELECT candidate.id FROM txs AS candidate WHERE candidate.state='resolved' AND (candidate.raw!='' OR candidate.payload!='') AND NOT EXISTS(SELECT 1 FROM txs AS live WHERE live.nonce=candidate.nonce AND live.state!='resolved') AND NOT EXISTS(SELECT 1 FROM txs AS live WHERE live.job=candidate.job AND live.state!='resolved') LIMIT 128)";
/// Soft mode blanks a resolved transaction only once the audit has made its receipt a finalized one: until then a
/// reorged transaction must be re-broadcast byte for byte.
const COMPACT_TXS_AUDITED: &str = "UPDATE txs SET raw='',payload='' WHERE id IN (SELECT candidate.id FROM txs AS candidate WHERE candidate.state='resolved' AND (candidate.raw!='' OR candidate.payload!='') AND NOT EXISTS(SELECT 1 FROM txs AS live WHERE live.nonce=candidate.nonce AND live.state!='resolved') AND NOT EXISTS(SELECT 1 FROM txs AS live WHERE live.job=candidate.job AND live.state!='resolved') AND candidate.hash IN (SELECT hash FROM finalized_receipts) LIMIT 128)";
async fn recorded_mismatch(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> Result<Option<Mismatch>> {
    let saved: Option<String> = sqlx::query_scalar("SELECT value FROM meta WHERE key=?")
        .bind(MISMATCH_KEY)
        .fetch_optional(&mut **tx)
        .await?;
    saved
        .map(|saved| serde_json::from_str(&saved))
        .transpose()
        .map_err(Into::into)
}
/// The meta key a round coordinator's journal names its kind under. An epoch coordinator's journal has none, as every
/// journal of 0.4.1 has none.
pub const KIND_KEY: &str = "coordinator_kind";
/// The meta key of the observation `request_moved` (`Journal::request_moved`): how many requests a replaced block has
/// moved under this keeper, and the last one, with when.
pub const REQUEST_MOVED_KEY: &str = "observation:request_moved";
impl Journal {
    /// The journal of an epoch coordinator's keeper with the tables of either finality mode (`open_for` in soft mode),
    /// for tests alone. It adds the soft keeper's `soft_marks` to any journal it opens, which an Arc keeper's journal must
    /// never gain, so nothing but a test may open a journal this way: the keeper, its commands and any tool name the kind
    /// and the mode (`open_for`), or open an existing journal without creating anything (`sweep::open_existing`).
    #[cfg(test)]
    pub async fn open(path: &Path, scope: &str) -> Result<Self> {
        Self::open_for(path, scope, CoordinatorKind::Epoch, FinalityMode::Soft).await
    }
    /// The journal of a keeper of the coordinator `kind` deciding on `mode`. The tables of the jobs, the nonce lane and
    /// the audit are the same for both kinds. An epoch coordinator's journal also has the epoch lane's tables
    /// (`epoch::install`, `epoch_demand`), and in finalized mode, as an Arc keeper runs, it is exactly the journal of 0.4.1:
    /// no table, index or trigger more. A round coordinator's has none of the epoch lane's tables and has the round
    /// lane's instead (`round::install`, `round_demand`); it names its kind in meta (`KIND_KEY`), and a journal of the
    /// other kind is refused by each. The marks of soft finality (`soft_marks`) are a soft keeper's: a journal gains the
    /// table the first time a keeper in soft mode opens it, and keeps it.
    pub async fn open_for(
        path: &Path,
        scope: &str,
        kind: CoordinatorKind,
        mode: FinalityMode,
    ) -> Result<Self> {
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
        let epoch_lane: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='epoch_work')",
        )
        .fetch_one(&pool)
        .await?;
        let named: Option<String> = sqlx::query_scalar("SELECT value FROM meta WHERE key=?")
            .bind(KIND_KEY)
            .fetch_optional(&pool)
            .await?;
        match kind {
            CoordinatorKind::Epoch => {
                ensure!(
                    named.is_none(),
                    "Journal belongs to a round coordinator's keeper, not an epoch coordinator's"
                );
                crate::epoch::install(&pool).await?;
            }
            CoordinatorKind::Round => {
                ensure!(
                    !epoch_lane,
                    "Journal belongs to an epoch coordinator's keeper, not a round coordinator's"
                );
                sqlx::query("INSERT OR IGNORE INTO meta(key,value) VALUES(?,'round')")
                    .bind(KIND_KEY)
                    .execute(&pool)
                    .await?;
                crate::round::install(&pool).await?;
            }
        }
        sqlx::raw_sql("CREATE TABLE IF NOT EXISTS finalized_receipts(hash TEXT PRIMARY KEY,block_number INTEGER NOT NULL,block_hash TEXT NOT NULL,status INTEGER NOT NULL);").execute(&pool).await?;
        // The blocks a soft-finality keeper acted on, until the finality audit has checked them against L1 finality. A
        // table of its own beside the others, which only a soft keeper's journal has: 0.4.1 opens a journal that has it
        // and ignores it.
        if mode == FinalityMode::Soft {
            sqlx::raw_sql(SOFT_MARKS_DDL).execute(&pool).await?;
        }
        // The requests' demand: the epoch each waits on, or the round.
        sqlx::raw_sql(match kind {
            CoordinatorKind::Epoch => EPOCH_DEMAND_DDL,
            CoordinatorKind::Round => ROUND_DEMAND_DDL,
        })
        .execute(&pool)
        .await?;
        sqlx::raw_sql(BATCH_MEMBERS_DDL).execute(&pool).await?;
        sqlx::raw_sql(concat!(
            "CREATE INDEX IF NOT EXISTS txs_active ON txs(id) WHERE ",
            active_txs_predicate!(),
            ";"
        ))
        .execute(&pool)
        .await?;
        sqlx::raw_sql("CREATE INDEX IF NOT EXISTS txs_job_state ON txs(job,state); CREATE INDEX IF NOT EXISTS txs_nonce_state ON txs(nonce,state); CREATE INDEX IF NOT EXISTS jobs_compact ON jobs(id) WHERE state IN ('served','callback_failed','refunded','expired','ignored') AND (proof IS NOT NULL OR call IS NOT NULL); CREATE INDEX IF NOT EXISTS txs_compact ON txs(id) WHERE state='resolved' AND (raw!='' OR payload!='');").execute(&pool).await?;
        if kind == CoordinatorKind::Epoch {
            sqlx::raw_sql("CREATE INDEX IF NOT EXISTS epochs_compact ON epoch_work(key) WHERE state IN ('committed','expired') AND (api IS NOT NULL OR selection IS NOT NULL);").execute(&pool).await?;
        }
        Ok(Self { pool })
    }
    /// Retain receipt lookup metadata, removing only replay data that can no longer be used.
    /// A persisted cadence and small indexed batches keep maintenance off the hot path.
    /// Expired unpublished epochs have no chain evidence: their raw packet is intentionally
    /// discarded, while identity, status, timing and last error remain for diagnosis.
    pub async fn compact_history(&self, now: u64) -> Result<()> {
        self.compact_history_for(now, FinalityMode::Finalized).await
    }
    /// `compact_history` for a keeper deciding on `mode`. In soft mode a resolved transaction keeps its signed bytes and
    /// payload until the audit has made its receipt a finalized one (`hash IN finalized_receipts`): until the block is
    /// final the sequencer can replace it, and the bytes are what re-broadcasts the transaction exactly. A transaction
    /// that never had a receipt (a replaced attempt of the nonce, or a nonce found consumed without one) never enters
    /// `finalized_receipts` and so keeps its bytes.
    pub async fn compact_history_for(&self, now: u64, mode: FinalityMode) -> Result<()> {
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
        // A round coordinator's journal has no epoch work.
        let epochs: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='epoch_work')",
        )
        .fetch_one(&mut *tx)
        .await?;
        if epochs {
            sqlx::query("UPDATE epoch_work SET api=NULL,selection=NULL WHERE key IN (SELECT key FROM epoch_work WHERE state IN ('committed','expired') AND (api IS NOT NULL OR selection IS NOT NULL) AND NOT EXISTS(SELECT 1 FROM txs WHERE txs.job=epoch_work.key AND txs.state!='resolved') LIMIT 128)")
                .execute(&mut *tx).await?;
        }
        sqlx::query(match mode {
            FinalityMode::Finalized => COMPACT_TXS,
            FinalityMode::Soft => COMPACT_TXS_AUDITED,
        })
        .execute(&mut *tx)
        .await?;
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
        checkpoint(&mut tx, FINALIZED_CHECKPOINT, number, hash).await?;
        tx.commit().await?;
        Ok(())
    }
    /// The block a soft tick decided on (`FINALITY_MODE=soft`), saved for the next tick to check it is still on the
    /// chain. Only the finality auditor writes `finalized_checkpoint` in soft mode, so the two keys never mix.
    pub async fn soft_checkpoint(&self, number: u64, hash: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        checkpoint(&mut tx, SOFT_CHECKPOINT, number, hash).await?;
        tx.commit().await?;
        Ok(())
    }
    /// A soft tick's decision head: `soft_checkpoint` and, in the same transaction, the `head` mark, so that the block
    /// the tick acted on is also one the finality audit checks. Like the checkpoint it keeps only the highest block.
    pub async fn soft_decision(&self, number: u64, hash: &str, created: u64) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        checkpoint(&mut tx, SOFT_CHECKPOINT, number, hash).await?;
        head_mark(&mut tx, number, hash, created).await?;
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
        checkpoint(&mut tx, FINALIZED_CHECKPOINT, number, block_hash).await?;
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
    /// A round coordinator's request found by discovery: its job, the round it is bound to and the cursor past it, in one
    /// commit. A job that is new gets its round row afresh, so that a row left by an earlier coordinator's request of the
    /// same id (a migration to another coordinator drops the jobs) does not lend it its first sight. So does a `vanished`
    /// job, which is `pending` again with nothing of its old request kept: a replaced block took that request, and the
    /// chain now has a live one under the id (review H1). A job that was already known otherwise keeps the first sight and
    /// its lag, and takes the round and fingerprint the chain has now. True when the job is new or revived.
    pub async fn discovered_round(
        &self,
        id: &str,
        deadline: i64,
        next: &str,
        assignment: &RoundAssignment,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let new = sqlx::query("INSERT INTO jobs(id,deadline) VALUES(?,?) ON CONFLICT(id) DO UPDATE SET state='pending',proof=NULL,call=NULL,deadline=excluded.deadline WHERE jobs.state='vanished'")
            .bind(id)
            .bind(deadline)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 1;
        if new {
            clear_backoffs(&mut tx, id).await?;
        }
        let statement = if new {
            "INSERT INTO round_demand(job,beacon,round,fingerprint,sealing_lag_ms,seen_at) VALUES(?,?,?,?,?,?) ON CONFLICT(job) DO UPDATE SET beacon=excluded.beacon,round=excluded.round,fingerprint=excluded.fingerprint,sealing_lag_ms=excluded.sealing_lag_ms,seen_at=excluded.seen_at"
        } else {
            "INSERT INTO round_demand(job,beacon,round,fingerprint,sealing_lag_ms,seen_at) VALUES(?,?,?,?,?,?) ON CONFLICT(job) DO UPDATE SET beacon=excluded.beacon,round=excluded.round,fingerprint=excluded.fingerprint"
        };
        sqlx::query(statement)
            .bind(id)
            .bind(i64::from(assignment.beacon))
            .bind(i64::try_from(assignment.round)?)
            .bind(&assignment.fingerprint)
            .bind(assignment.sealing_lag_ms)
            .bind(i64::try_from(assignment.seen_at)?)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO meta(key,value) VALUES('cursor',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value").bind(next).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(new)
    }
    /// The round a job is bound to, as discovery journaled it.
    pub async fn round_assignment(&self, id: &str) -> Result<Option<RoundAssignment>> {
        let row: Option<(i64, i64, String, Option<i64>, i64)> = sqlx::query_as(
            "SELECT beacon,round,fingerprint,sealing_lag_ms,seen_at FROM round_demand WHERE job=?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(beacon, round, fingerprint, lag, seen_at)| {
            Ok(RoundAssignment {
                beacon: u8::try_from(beacon)?,
                round: u64::try_from(round)?,
                fingerprint,
                sealing_lag_ms: lag.unwrap_or_default(),
                seen_at: u64::try_from(seen_at)?,
            })
        })
        .transpose()
    }
    /// Round demand: the rounds that live requests wait on, those in `pending`, `prepared`, `signed` or `submitted` with a
    /// deadline after `after`, the earliest deadline first. The round lane (task K2) fetches these rounds.
    pub async fn round_demand(&self, after: u64) -> Result<Vec<DemandedRound>> {
        let rows: Vec<(i64, i64, i64, i64)> = sqlx::query_as("SELECT round_demand.beacon,round_demand.round,COUNT(*),MIN(jobs.deadline) FROM round_demand JOIN jobs ON jobs.id=round_demand.job WHERE jobs.state IN ('pending','prepared','signed','submitted') AND jobs.deadline>? GROUP BY round_demand.beacon,round_demand.round ORDER BY MIN(jobs.deadline),round_demand.beacon,round_demand.round")
            .bind(i64::try_from(after)?)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|(beacon, round, requests, deadline)| {
                Ok(DemandedRound {
                    beacon: u8::try_from(beacon)?,
                    round: u64::try_from(round)?,
                    requests: u64::try_from(requests)?,
                    earliest_deadline: u64::try_from(deadline)?,
                })
            })
            .collect()
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
    /// `claim_preparation` of a round coordinator's keeper: only the jobs whose round, as discovery journaled it, the
    /// round lane has verified, so that a request is read again and proved only once its round's signature is in hand.
    pub async fn claim_round_preparation(
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
        const HEAD: &str = "SELECT jobs.* FROM jobs JOIN round_demand ON round_demand.job=jobs.id JOIN round_work ON round_work.beacon=round_demand.beacon AND round_work.round=round_demand.round AND round_work.state='verified' LEFT JOIN meta ON meta.key='prepare_retry_ms:'||jobs.id WHERE jobs.state='pending' AND jobs.call IS NULL AND jobs.deadline>? AND CAST(COALESCE(meta.value,'0') AS INTEGER)<=? ORDER BY CAST(COALESCE(meta.value,'0') AS INTEGER),jobs.deadline,LENGTH(jobs.id),jobs.id LIMIT 16";
        const TAIL: &str = "SELECT jobs.* FROM jobs JOIN round_demand ON round_demand.job=jobs.id JOIN round_work ON round_work.beacon=round_demand.beacon AND round_work.round=round_demand.round AND round_work.state='verified' LEFT JOIN meta ON meta.key='prepare_retry_ms:'||jobs.id WHERE jobs.state='pending' AND jobs.call IS NULL AND jobs.deadline>? AND CAST(COALESCE(meta.value,'0') AS INTEGER)<=? ORDER BY CAST(COALESCE(meta.value,'0') AS INTEGER),jobs.deadline DESC,LENGTH(jobs.id) DESC,jobs.id DESC LIMIT 16";
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
    /// A round coordinator's request whose fields at the decision head are not the ones the journal holds for it: a
    /// replaced block moved it (design C, 3.7). In one commit its round row takes the beacon, round and fingerprint the
    /// chain has now; a proof made for the old fields is dropped and the job is `pending` again, to be proved for the new
    /// ones, with its backoffs and its exclusion from batches cleared; and the observation `request_moved` is recorded
    /// (`REQUEST_MOVED_KEY`: how many, and the last). A job with a transaction in flight is left to reconciliation.
    pub async fn request_moved(
        &self,
        id: &str,
        assignment: &RoundAssignment,
        now: u64,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE round_demand SET beacon=?,round=?,fingerprint=? WHERE job=?")
            .bind(i64::from(assignment.beacon))
            .bind(i64::try_from(assignment.round)?)
            .bind(&assignment.fingerprint)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE jobs SET proof=NULL,call=NULL,state='pending' WHERE id=? AND state IN ('pending','prepared') AND NOT EXISTS(SELECT 1 FROM txs WHERE txs.job=jobs.id AND txs.state!='resolved') AND NOT EXISTS(SELECT 1 FROM batch_members JOIN txs ON txs.job=batch_members.job WHERE batch_members.request_id=jobs.id AND txs.state!='resolved')")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM meta WHERE key IN (?,?,?)")
            .bind(format!("prepare_retry_ms:{id}"))
            .bind(format!("preflight_retry:{id}"))
            .bind(format!("batch_exclude:{id}"))
            .execute(&mut *tx)
            .await?;
        let count: i64 = sqlx::query_scalar(
            "SELECT COALESCE(CAST(json_extract(value,'$.count') AS INTEGER),0) FROM meta WHERE key=?",
        )
        .bind(REQUEST_MOVED_KEY)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(0);
        sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
            .bind(REQUEST_MOVED_KEY)
            .bind(serde_json::json!({"count":count+1,"request":id,"at":now}).to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    /// A `vanished` job whose request the chain has again, live, under its id (review H1): `pending` again with the
    /// request's deadline, nothing of its old proof, calldata or backoffs kept, and its round row afresh, in one commit.
    /// True when the job was vanished.
    pub async fn revive_round(
        &self,
        id: &str,
        deadline: i64,
        assignment: &RoundAssignment,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let revived = sqlx::query("UPDATE jobs SET state='pending',proof=NULL,call=NULL,deadline=? WHERE id=? AND state='vanished'")
            .bind(deadline)
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 1;
        if revived {
            clear_backoffs(&mut tx, id).await?;
            sqlx::query("INSERT INTO round_demand(job,beacon,round,fingerprint,sealing_lag_ms,seen_at) VALUES(?,?,?,?,?,?) ON CONFLICT(job) DO UPDATE SET beacon=excluded.beacon,round=excluded.round,fingerprint=excluded.fingerprint,sealing_lag_ms=excluded.sealing_lag_ms,seen_at=excluded.seen_at")
                .bind(id)
                .bind(i64::from(assignment.beacon))
                .bind(i64::try_from(assignment.round)?)
                .bind(&assignment.fingerprint)
                .bind(assignment.sealing_lag_ms)
                .bind(i64::try_from(assignment.seen_at)?)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(revived)
    }
    /// The lowest id of a `vanished` job, if any: discovery keeps its cursor at or below it, since the chain as it is can
    /// reuse the id for a new request (review H1).
    pub async fn lowest_vanished(&self) -> Result<Option<u64>> {
        let id: Option<i64> =
            sqlx::query_scalar("SELECT MIN(CAST(id AS INTEGER)) FROM jobs WHERE state='vanished'")
                .fetch_one(&self.pool)
                .await?;
        Ok(id.map(u64::try_from).transpose()?)
    }
    /// A round coordinator's request that the coordinator does not have any more (`UnknownRequest`): a replaced block
    /// took it, and nothing of it was escrowed on the chain as it is (design C, 3.7). Its proof and calldata go. `vanished`
    /// is not final: a live request under the id later makes the job `pending` again (`discovered_round`, `revive_round`,
    /// review H1).
    pub async fn vanished(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE jobs SET state='vanished',proof=NULL,call=NULL WHERE id=?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
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
    /// Every prepared request left out of batches, in one read.
    pub async fn batch_excluded_prepared(&self) -> Result<std::collections::HashSet<String>> {
        Ok(sqlx::query_scalar("SELECT jobs.id FROM jobs JOIN meta ON meta.key='batch_exclude:'||jobs.id WHERE jobs.state='prepared' AND jobs.call IS NOT NULL")
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .collect())
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
        self.signed_marked(a, None).await
    }
    /// `signed`, with the `sign` mark of a soft keeper: the decision head the transaction was signed at, committed
    /// with the signed bytes or not at all.
    pub async fn signed_marked(&self, a: &Attempt, mark: Option<&Mark>) -> Result<()> {
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
        if let Some(mark) = mark {
            write_mark(&mut tx, mark).await?;
        }
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
        self.signed_batch_marked(a, members, None).await
    }
    /// `signed_batch`, with the `sign` mark of a soft keeper in the same commit.
    pub async fn signed_batch_marked(
        &self,
        a: &Attempt,
        members: &[String],
        mark: Option<&Mark>,
    ) -> Result<()> {
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
        if let Some(mark) = mark {
            write_mark(&mut tx, mark).await?;
        }
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
        self.resolve_nonce_job_marked(nonce, job, state, None).await
    }
    /// `resolve_nonce_job`, with the soft mark that justifies it (a `receipt`, or a `nonce` mark when no receipt was
    /// served) in the same commit: a soft keeper never moves the nonce floor on the sequencer's word without one.
    pub async fn resolve_nonce_job_marked(
        &self,
        nonce: i64,
        job: &str,
        state: &str,
        mark: Option<&Mark>,
    ) -> Result<()> {
        let next = nonce
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Nonce overflow"))?;
        let mut tx = self.pool.begin().await?;
        if let Some(mark) = mark {
            write_mark(&mut tx, mark).await?;
        }
        sqlx::query("UPDATE txs SET state='resolved' WHERE nonce=?")
            .bind(nonce)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO meta(key,value) VALUES('nonce_floor',?) ON CONFLICT(key) DO UPDATE SET value=CAST(MAX(CAST(meta.value AS INTEGER),CAST(excluded.value AS INTEGER)) AS TEXT)")
            .bind(next.to_string()).execute(&mut *tx).await?;
        let updated = set_resolved_state(&mut tx, job, state).await?;
        ensure!(
            updated.rows_affected() == 1,
            "Receipt job missing from journal"
        );
        tx.commit().await?;
        Ok(())
    }
    pub async fn resolve_nonce_epoch(&self, nonce: i64, key: &str, state: &str) -> Result<()> {
        self.resolve_nonce_epoch_marked(nonce, key, state, None)
            .await
    }
    /// `resolve_nonce_epoch` with its soft mark in the same commit; see `resolve_nonce_job_marked`.
    pub async fn resolve_nonce_epoch_marked(
        &self,
        nonce: i64,
        key: &str,
        state: &str,
        mark: Option<&Mark>,
    ) -> Result<()> {
        let next = nonce
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Nonce overflow"))?;
        let mut tx = self.pool.begin().await?;
        if let Some(mark) = mark {
            write_mark(&mut tx, mark).await?;
        }
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
    /// each member's state commit together. The caller must account for exactly the journaled
    /// member list; a partial resolution is refused. A member resolved to `prepared` (a live
    /// member of a reverted batch) keeps its journaled single calldata, is left out of later
    /// batches and loses its preflight backoff in the same commit, so it is resent one at a time
    /// at once and never re-enters a batch; one without calldata returns to `pending` to be
    /// proven again.
    pub async fn resolve_nonce_batch(
        &self,
        nonce: i64,
        key: &str,
        states: &[(String, &str)],
    ) -> Result<()> {
        self.resolve_nonce_batch_marked(nonce, key, states, None)
            .await
    }
    /// `resolve_nonce_batch` with its soft mark in the same commit; see `resolve_nonce_job_marked`.
    pub async fn resolve_nonce_batch_marked(
        &self,
        nonce: i64,
        key: &str,
        states: &[(String, &str)],
        mark: Option<&Mark>,
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
        if let Some(mark) = mark {
            write_mark(&mut tx, mark).await?;
        }
        sqlx::query("UPDATE txs SET state='resolved' WHERE nonce=?")
            .bind(nonce)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO meta(key,value) VALUES('nonce_floor',?) ON CONFLICT(key) DO UPDATE SET value=CAST(MAX(CAST(meta.value AS INTEGER),CAST(excluded.value AS INTEGER)) AS TEXT)")
            .bind(next.to_string()).execute(&mut *tx).await?;
        for (id, state) in states {
            let updated = if *state == "prepared" {
                sqlx::query("UPDATE jobs SET state=CASE WHEN call IS NULL THEN 'pending' ELSE 'prepared' END WHERE id=?")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?
            } else {
                set_resolved_state(&mut tx, id, state).await?
            };
            ensure!(
                updated.rows_affected() == 1,
                "Batch member {id} missing from journal"
            );
            if *state == "prepared" {
                sqlx::query("INSERT OR IGNORE INTO meta(key,value) VALUES(?,'1')")
                    .bind(format!("batch_exclude:{id}"))
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM meta WHERE key=?")
                    .bind(format!("preflight_retry:{id}"))
                    .execute(&mut *tx)
                    .await?;
            }
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
    /// Every soft mark, lowest block first: what the keeper has acted on and the audit has not yet checked.
    pub async fn soft_marks(&self) -> Result<Vec<Mark>> {
        let rows = sqlx::query(
            "SELECT number,hash,kind,ref,created,status FROM soft_marks ORDER BY number,kind,ref",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(mark_from_row).collect()
    }
    /// The distinct blocks of the marks at or above `from`, lowest first, at most `limit`: one page of the recovery's
    /// walk through the marks.
    pub async fn mark_numbers_from(&self, from: u64, limit: usize) -> Result<Vec<u64>> {
        let numbers: Vec<i64> = sqlx::query_scalar(
            "SELECT DISTINCT number FROM soft_marks WHERE number>=? ORDER BY number LIMIT ?",
        )
        .bind(i64::try_from(from)?)
        .bind(i64::try_from(limit)?)
        .fetch_all(&self.pool)
        .await?;
        numbers
            .into_iter()
            .map(|number| Ok(u64::try_from(number)?))
            .collect()
    }
    /// The marks at the blocks `first` to `last`, lowest first.
    pub async fn marks_between(&self, first: u64, last: u64) -> Result<Vec<Mark>> {
        let rows = sqlx::query("SELECT number,hash,kind,ref,created,status FROM soft_marks WHERE number>=? AND number<=? ORDER BY number,kind,ref")
            .bind(i64::try_from(first)?)
            .bind(i64::try_from(last)?)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(mark_from_row).collect()
    }
    /// Delete these marks (by block, kind and reference), and write `fresh` in their place when it is given, in one
    /// transaction. Only the recovery from a mismatch on record does this: a mark is evidence, and is otherwise deleted
    /// by the audit alone, once it has checked it.
    pub async fn replace_marks(&self, stale: &[Mark], fresh: Option<&Mark>) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for mark in stale {
            sqlx::query("DELETE FROM soft_marks WHERE number=? AND kind=? AND ref=?")
                .bind(i64::try_from(mark.number)?)
                .bind(mark.kind.name())
                .bind(&mark.reference)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(fresh) = fresh {
            write_mark(&mut tx, fresh).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    /// An operator acknowledges a mismatch, named by its `id`. Nothing the keeper does waits for it.
    ///
    /// - The mismatch on record: the acknowledgement is written beside it, and nothing else is touched. The keeper is
    ///   recovering from it already.
    /// - With none on record, the mismatch of the note `finality:suspected`: one endpoint showed it and no two endpoints
    ///   agreed on the block, so the keeper holds its sends and asks again. The operator's word stands in for the
    ///   second endpoint: the mismatch is recorded and acknowledged, and the note goes, in one transaction. The keeper
    ///   recovers from it at its next tick.
    ///
    /// Any other id acknowledges nothing. The same acknowledgement again changes nothing.
    pub async fn acknowledge_finality(&self, id: &str, now: u64) -> Result<Acknowledgement> {
        let mut tx = self.pool.begin().await?;
        let id = id.trim().to_ascii_lowercase();
        let recorded = match recorded_mismatch(&mut tx).await? {
            Some(recorded) => recorded,
            None => {
                let noted: Option<String> =
                    sqlx::query_scalar("SELECT value FROM meta WHERE key=?")
                        .bind(SUSPECTED_KEY)
                        .fetch_optional(&mut *tx)
                        .await?;
                let suspected = noted
                    .and_then(|noted| serde_json::from_str::<Suspected>(&noted).ok())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "No finality mismatch is on record or suspected; there is nothing to acknowledge"
                        )
                    })?;
                ensure!(
                    suspected.mismatch.id() == id,
                    "The suspected finality mismatch has id {}, not {id}; read it with `finality --status` and acknowledge that one",
                    suspected.mismatch.id(),
                );
                sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO NOTHING")
                    .bind(MISMATCH_KEY)
                    .bind(serde_json::to_string(&suspected.mismatch)?)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM meta WHERE key=?")
                    .bind(SUSPECTED_KEY)
                    .execute(&mut *tx)
                    .await?;
                suspected.mismatch
            }
        };
        ensure!(
            recorded.id() == id,
            "The finality mismatch on record has id {}, not {id}; read it with `finality --status` and acknowledge that one",
            recorded.id(),
        );
        let saved: Option<String> = sqlx::query_scalar("SELECT value FROM meta WHERE key=?")
            .bind(ACK_KEY)
            .fetch_optional(&mut *tx)
            .await?;
        let acknowledged = saved
            .and_then(|saved| serde_json::from_str::<Acknowledgement>(&saved).ok())
            .filter(|ack| ack.id == recorded.id());
        let ack = match acknowledged {
            Some(ack) => ack,
            None => {
                let ack = Acknowledgement {
                    id: recorded.id(),
                    acknowledged_at: now,
                };
                sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
                    .bind(ACK_KEY)
                    .bind(serde_json::to_string(&ack)?)
                    .execute(&mut *tx)
                    .await?;
                ack
            }
        };
        tx.commit().await?;
        Ok(ack)
    }
    /// Where the keeper stands: no mismatch on record, or one it is recovering from, with the operator's
    /// acknowledgement of it when there is one. An acknowledgement of another mismatch than the one on record counts
    /// for nothing, and neither does one that cannot be read.
    pub async fn finality_state(&self) -> Result<FinalityState> {
        let Some(mismatch) = self.finality_mismatch().await? else {
            return Ok(FinalityState::Clear);
        };
        let ack = self
            .meta(ACK_KEY)
            .await?
            .and_then(|saved| serde_json::from_str::<Acknowledgement>(&saved).ok())
            .filter(|ack| ack.id == mismatch.id());
        Ok(FinalityState::Recovering(mismatch, ack))
    }
    /// Keep `suspected` as the note of the mismatch the keeper suspects (`Suspected`), replacing the one before.
    pub async fn note_suspicion(&self, suspected: &Suspected) -> Result<()> {
        self.set_meta(SUSPECTED_KEY, &serde_json::to_string(suspected)?)
            .await
    }
    /// The note of a suspected mismatch, if there is one.
    pub async fn suspicion_note(&self) -> Result<Option<Suspected>> {
        Ok(self
            .meta(SUSPECTED_KEY)
            .await?
            .and_then(|noted| serde_json::from_str(&noted).ok()))
    }
    /// Delete the note of a suspected mismatch.
    pub async fn clear_suspicion_note(&self) -> Result<()> {
        sqlx::query("DELETE FROM meta WHERE key=?")
            .bind(SUSPECTED_KEY)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    /// At least two endpoints agree that the chain changed: in one transaction, `mismatch` is recorded unless one is
    /// on record already (the first one stays), `evidence` is kept under `key` beside it, and the note of the suspicion
    /// goes. True when this call recorded it.
    pub async fn confirm_finality_mismatch(
        &self,
        mismatch: &Mismatch,
        key: &str,
        evidence: &serde_json::Value,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let recorded =
            sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO NOTHING")
                .bind(MISMATCH_KEY)
                .bind(serde_json::to_string(mismatch)?)
                .execute(&mut *tx)
                .await?
                .rows_affected()
                == 1;
        if recorded {
            sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
                .bind(key)
                .bind(evidence.to_string())
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM meta WHERE key=?")
            .bind(SUSPECTED_KEY)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(recorded)
    }
    /// Set a meta key, replacing what it held.
    pub async fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
            .bind(key)
            .bind(value)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    /// The soft checkpoint and the `head` mark set to this block, whatever they were. The endpoints agree that the chain
    /// changed, and the chain they serve now is the one to go on from, so unlike `soft_decision` this moves the
    /// checkpoint down as well as up, and replaces a hash for its own number.
    pub async fn rebase_soft_decision(&self, number: u64, hash: &str, created: u64) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
            .bind(SOFT_CHECKPOINT.key)
            .bind(serde_json::to_string(&(number, hash))?)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM soft_marks WHERE kind='head'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO soft_marks(number,hash,kind,ref,created) VALUES(?,?,'head','',?)")
            .bind(i64::try_from(number)?)
            .bind(hash)
            .bind(i64::try_from(created)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    /// Take the transactions of `nonce` back into the nonce lane, for the recovery from a mismatch on record: the chain
    /// no longer has their receipt, or has it in another block, and the keeper must settle the nonce again from the
    /// chain as it is now.
    ///
    /// In one transaction: every attempt of the nonce goes back to `submitted`, stamped with `chain_time` (the time
    /// attempts carry is the chain's, the time of the head they were signed at) and unbroadcast, so that the keeper
    /// broadcasts the bytes it kept before it replaces them; the marks of their receipts and of the nonce are deleted,
    /// since the settlement writes the ones of the block it finds (another hash for the same block would be a conflict,
    /// and another block's mark would be a second row for the audit to find stale); the request, the batch members or the
    /// epoch they served go back to `submitted`; and every other live attempt is parked (`parked_signed`,
    /// `parked_submitted`) so that the nonce is the only one in the lane, which is what reconciliation requires. Parked
    /// attempts return with `unpark_lanes`. The age of the lane that health measures starts at `wall_time`.
    ///
    /// `NoBytes` when the journal has no signed transaction for the nonce, or the last attempt's bytes are gone: the
    /// lane cannot be refilled and nothing was changed.
    pub async fn reopen_nonce(
        &self,
        nonce: u64,
        chain_time: u64,
        wall_time: u64,
    ) -> Result<Reopened> {
        let nonce = i64::try_from(nonce)?;
        let now = i64::try_from(chain_time)?;
        let mut tx = self.pool.begin().await?;
        let rows: Vec<(String, String, String, String)> =
            sqlx::query_as("SELECT job,kind,hash,raw FROM txs WHERE nonce=? ORDER BY id")
                .bind(nonce)
                .fetch_all(&mut *tx)
                .await?;
        if rows.last().is_none_or(|(_, _, _, raw)| raw.is_empty()) {
            return Ok(Reopened::NoBytes);
        }
        sqlx::query("UPDATE txs SET state='parked_'||state WHERE state IN ('signed','submitted') AND nonce!=?")
            .bind(nonce)
            .execute(&mut *tx)
            .await?;
        for (_, _, hash, _) in &rows {
            sqlx::query("DELETE FROM soft_marks WHERE kind='receipt' AND ref=?")
                .bind(hash)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM soft_marks WHERE kind='nonce' AND ref=?")
            .bind(nonce.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE txs SET state='submitted',created=?,broadcast=0 WHERE nonce=?")
            .bind(now)
            .bind(nonce)
            .execute(&mut *tx)
            .await?;
        let jobs: std::collections::BTreeSet<(&str, bool)> = rows
            .iter()
            .map(|(job, kind, _, _)| (job.as_str(), kind.starts_with("epoch")))
            .collect();
        for (job, epoch) in jobs {
            if epoch {
                sqlx::query("UPDATE epoch_work SET state='submitted' WHERE key=?")
                    .bind(job)
                    .execute(&mut *tx)
                    .await?;
            } else if is_batch_job(job) {
                sqlx::query("UPDATE jobs SET state='submitted' WHERE id IN (SELECT request_id FROM batch_members WHERE job=?)")
                    .bind(job)
                    .execute(&mut *tx)
                    .await?;
            } else {
                sqlx::query("UPDATE jobs SET state='submitted' WHERE id=?")
                    .bind(job)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        // The lane starts its age over: a nonce that was settled long ago is not stalled.
        sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
            .bind(format!("nonce_started:{nonce}"))
            .bind(wall_time.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Reopened::Lane)
    }
    /// Start the recovery's fill of a nonce, kept as `value` under `key`: in one transaction every live attempt is
    /// parked (`parked_signed`, `parked_submitted`), so that the nonce the fill takes is the only one in the lane, as
    /// `reopen_nonce` does for the nonce it reopens, and the fill is saved.
    pub async fn begin_fill(&self, key: &str, value: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE txs SET state='parked_'||state WHERE state IN ('signed','submitted')")
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
            .bind(key)
            .bind(value)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    /// Set the meta `key` to `value` and write `mark` beside it, in one transaction: a transaction the recovery signed
    /// to fill a nonce, journaled with the `sign` mark of the head it was signed on.
    pub async fn set_meta_marked(&self, key: &str, value: &str, mark: Option<&Mark>) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
            .bind(key)
            .bind(value)
            .execute(&mut *tx)
            .await?;
        if let Some(mark) = mark {
            write_mark(&mut tx, mark).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    /// The recovery's fill of `nonce` is settled: in one transaction the receipt marks of `hashes` (the attempts of the
    /// nonce) and its nonce marks, which the chain no longer has, are deleted, `fresh` is written for the transaction
    /// that used the nonce, and the fill kept under `key` goes. The nonce floor does not move.
    pub async fn settle_fill(
        &self,
        key: &str,
        nonce: u64,
        hashes: &[String],
        fresh: Option<&Mark>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for hash in hashes {
            sqlx::query("DELETE FROM soft_marks WHERE kind='receipt' AND ref=?")
                .bind(hash)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM soft_marks WHERE kind='nonce' AND ref=?")
            .bind(nonce.to_string())
            .execute(&mut *tx)
            .await?;
        if let Some(fresh) = fresh {
            write_mark(&mut tx, fresh).await?;
        }
        sqlx::query("DELETE FROM meta WHERE key=?")
            .bind(key)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    /// Return the attempts that `reopen_nonce` parked to the lane, in the state they had. How many.
    pub async fn unpark_lanes(&self) -> Result<u64> {
        Ok(sqlx::query("UPDATE txs SET state=substr(state,8) WHERE state IN ('parked_signed','parked_submitted')")
            .execute(&self.pool)
            .await?
            .rows_affected())
    }
    /// How many attempts are parked.
    pub async fn parked_lanes(&self) -> Result<u64> {
        let parked: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM txs WHERE state IN ('parked_signed','parked_submitted')",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(u64::try_from(parked)?)
    }
    /// The recovery is complete: in one transaction the parked attempts return to the lane, the mismatch, its
    /// acknowledgement, a note of a suspicion and everything the recovery kept go, a finalized receipt or finalized
    /// checkpoint the mismatch named goes with them, and `summary` stays as the last recovery. From the next tick the audit runs again and the keeper sends.
    pub async fn clear_finality_incident(&self, summary: &serde_json::Value) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        // A finalized record the incident showed is not the chain's goes, so that the next audit records the receipt or
        // the checkpoint where the chain has it, instead of finding the same conflict again.
        if let Some(recorded) = recorded_mismatch(&mut tx).await? {
            match recorded.kind.as_str() {
                "finalized_receipt" => {
                    sqlx::query("DELETE FROM finalized_receipts WHERE hash=? AND block_number=? AND block_hash=?")
                        .bind(&recorded.reference)
                        .bind(i64::try_from(recorded.number)?)
                        .bind(&recorded.expected)
                        .execute(&mut *tx)
                        .await?;
                }
                "finalized_checkpoint" => {
                    sqlx::query("DELETE FROM meta WHERE key=? AND value=?")
                        .bind(FINALIZED_CHECKPOINT.key)
                        .bind(serde_json::to_string(&(
                            recorded.number,
                            &recorded.expected,
                        ))?)
                        .execute(&mut *tx)
                        .await?;
                }
                _ => {}
            }
        }
        sqlx::query("UPDATE txs SET state=substr(state,8) WHERE state IN ('parked_signed','parked_submitted')")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM meta WHERE key IN (?,?,?,?,?) OR key LIKE ?")
            .bind(MISMATCH_KEY)
            .bind(ACK_KEY)
            .bind(ALERTED_KEY)
            .bind(UNCONFIRMED_ALERTED_KEY)
            .bind(SUSPECTED_KEY)
            .bind(format!("{RECOVERY_PREFIX}%"))
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
            .bind(LAST_RECOVERY_KEY)
            .bind(summary.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    /// The distinct block numbers of the marks at or below `up_to`, lowest first, at most `limit`: the blocks one audit
    /// reads from the chain.
    pub async fn mark_numbers(&self, up_to: u64, limit: usize) -> Result<Vec<u64>> {
        let numbers: Vec<i64> = sqlx::query_scalar(
            "SELECT DISTINCT number FROM soft_marks WHERE number<=? ORDER BY number LIMIT ?",
        )
        .bind(i64::try_from(up_to)?)
        .bind(i64::try_from(limit)?)
        .fetch_all(&self.pool)
        .await?;
        numbers
            .into_iter()
            .map(|number| Ok(u64::try_from(number)?))
            .collect()
    }
    /// When the oldest mark that waits for the audit was written. The `head` mark is not one: it is rewritten by every
    /// tick, and so is never behind.
    pub async fn oldest_mark_created(&self) -> Result<Option<u64>> {
        let oldest: Option<i64> =
            sqlx::query_scalar("SELECT MIN(created) FROM soft_marks WHERE kind!='head'")
                .fetch_one(&self.pool)
                .await?;
        Ok(oldest.map(u64::try_from).transpose()?)
    }
    /// The mismatch the audit stopped at, if one is on record.
    pub async fn finality_mismatch(&self) -> Result<Option<Mismatch>> {
        Ok(self
            .meta(MISMATCH_KEY)
            .await?
            .map(|saved| serde_json::from_str(&saved))
            .transpose()?)
    }
    /// Keep `mismatch` under `finality:mismatch` unless one is kept already: the first one stays the record the keeper
    /// recovers from, and the audit does nothing while it is there. True when this call recorded it.
    ///
    /// The keeper records here only what at least two endpoints agree on (`confirm_finality_mismatch`): one endpoint
    /// alone never makes a record.
    pub async fn record_finality_mismatch(&self, mismatch: &Mismatch) -> Result<bool> {
        let recorded =
            sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO NOTHING")
                .bind(MISMATCH_KEY)
                .bind(serde_json::to_string(mismatch)?)
                .execute(&self.pool)
                .await?;
        Ok(recorded.rows_affected() == 1)
    }
    /// The finality audit of the marks at these blocks: `canonical` is the chain's hash of each, lowest block first.
    /// Everything happens in one transaction.
    ///
    /// - A mismatch is on record: nothing is touched (`Stopped`).
    /// - Every mark of every block has the chain's hash: each `receipt` mark becomes a row of `finalized_receipts` (under
    ///   the conflict rule of `finalized_receipt`), the finalized checkpoint advances to the highest of the blocks, and
    ///   the marks of the blocks are deleted.
    /// - Any mark has another hash, or contradicts a finalized record: the first such mark is returned as the mismatch
    ///   and nothing is touched, so that every mark is still there for the recovery. It is not recorded: `canonical` is
    ///   one endpoint's word, and the keeper asks the other endpoints before it records anything.
    pub async fn audit_marks(&self, canonical: &[(u64, String)], now: u64) -> Result<Audit> {
        let mut tx = self.pool.begin().await?;
        if let Some(recorded) = recorded_mismatch(&mut tx).await? {
            return Ok(Audit::Stopped(recorded));
        }
        let mut marks = Vec::new();
        for (number, actual) in canonical {
            let rows = sqlx::query("SELECT hash,kind,ref,created,status FROM soft_marks WHERE number=? ORDER BY kind,ref")
                .bind(i64::try_from(*number)?)
                .fetch_all(&mut *tx)
                .await?;
            for row in rows {
                let mark = Mark {
                    kind: MarkKind::parse(&row.get::<String, _>("kind"))?,
                    number: *number,
                    hash: row.get("hash"),
                    reference: row.get("ref"),
                    created: u64::try_from(row.get::<i64, _>("created"))?,
                    status: row
                        .get::<Option<i64>, _>("status")
                        .map(u64::try_from)
                        .transpose()?,
                };
                if !mark.hash.eq_ignore_ascii_case(actual) {
                    tx.rollback().await?;
                    return Ok(Audit::Mismatch(Mismatch {
                        kind: mark.kind.name().into(),
                        number: *number,
                        reference: mark.reference,
                        expected: mark.hash,
                        actual: actual.clone(),
                        detected_at: now,
                    }));
                }
                marks.push(mark);
            }
        }
        let Some(last) = marks.iter().max_by_key(|mark| mark.number) else {
            tx.commit().await?;
            return Ok(Audit::Idle);
        };
        let (highest, highest_hash) = (last.number, last.hash.clone());
        let blocks = marks
            .iter()
            .map(|mark| mark.number)
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let mut receipts = 0;
        for mark in marks.iter().filter(|mark| mark.kind == MarkKind::Receipt) {
            let number = i64::try_from(mark.number)?;
            let status = i64::try_from(mark.status.ok_or_else(|| {
                anyhow::anyhow!("Receipt mark of {} has no status", mark.reference)
            })?)?;
            sqlx::query("INSERT INTO finalized_receipts(hash,block_number,block_hash,status) VALUES(?,?,?,?) ON CONFLICT DO NOTHING")
                .bind(&mark.reference)
                .bind(number)
                .bind(&mark.hash)
                .bind(status)
                .execute(&mut *tx)
                .await?;
            let stored: (i64, String, i64) = sqlx::query_as(
                "SELECT block_number,block_hash,status FROM finalized_receipts WHERE hash=?",
            )
            .bind(&mark.reference)
            .fetch_one(&mut *tx)
            .await?;
            if stored != (number, mark.hash.clone(), status) {
                tx.rollback().await?;
                // The mismatch is of the block the finalized record names, which the endpoints are asked about: the
                // record is what may be wrong (made final on an endpoint's word, and then moved by the sequencer). The
                // audit did not read that block, so the chain's hash of it is left for the endpoints to say.
                return Ok(Audit::Mismatch(Mismatch {
                    kind: "finalized_receipt".into(),
                    number: u64::try_from(stored.0)?,
                    reference: mark.reference.clone(),
                    expected: stored.1,
                    actual: String::new(),
                    detected_at: now,
                }));
            }
            receipts += 1;
        }
        // The same block under another hash is a finalized block that was replaced: the checkpoint says so.
        let saved: Option<String> = sqlx::query_scalar("SELECT value FROM meta WHERE key=?")
            .bind(FINALIZED_CHECKPOINT.key)
            .fetch_optional(&mut *tx)
            .await?;
        if let Some(saved) = saved {
            let (number, hash): (u64, String) = serde_json::from_str(&saved)?;
            if number == highest && !hash.eq_ignore_ascii_case(&highest_hash) {
                tx.rollback().await?;
                return Ok(Audit::Mismatch(Mismatch {
                    kind: "finalized_checkpoint".into(),
                    number,
                    reference: String::new(),
                    expected: hash,
                    actual: highest_hash,
                    detected_at: now,
                }));
            }
        }
        checkpoint(&mut tx, FINALIZED_CHECKPOINT, highest, &highest_hash).await?;
        for (number, _) in canonical {
            sqlx::query("DELETE FROM soft_marks WHERE number=?")
                .bind(i64::try_from(*number)?)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(Audit::Audited {
            blocks,
            receipts,
            checkpoint: (highest, highest_hash),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn assigned(
        beacon: u8,
        round: u64,
        fingerprint: &str,
        lag: i64,
        seen_at: u64,
    ) -> RoundAssignment {
        RoundAssignment {
            beacon,
            round,
            fingerprint: fingerprint.into(),
            sealing_lag_ms: lag,
            seen_at,
        }
    }
    #[tokio::test]
    async fn round_rows_keep_the_first_sight_of_a_job_and_round_demand_groups_live_jobs_by_round() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open_for(
            &dir.path().join("rounds.sqlite"),
            "scope",
            CoordinatorKind::Round,
            FinalityMode::Soft,
        )
        .await
        .unwrap();
        assert!(
            j.discovered_round("1", 160, "2", &assigned(0, 7, "0x01", 800, 100))
                .await
                .unwrap()
        );
        assert!(
            j.discovered_round("2", 161, "3", &assigned(0, 7, "0x02", 900, 101))
                .await
                .unwrap()
        );
        assert!(
            j.discovered_round("3", 165, "4", &assigned(0, 8, "0x03", 700, 105))
                .await
                .unwrap()
        );
        assert_eq!(j.meta("cursor").await.unwrap().as_deref(), Some("4"));
        // Discovered again (a recovery took discovery back): the round and fingerprint are the chain's now, the first
        // sight and its lag stay.
        assert!(
            !j.discovered_round("1", 160, "2", &assigned(0, 9, "0x11", 5_000, 130))
                .await
                .unwrap()
        );
        assert_eq!(
            j.round_assignment("1").await.unwrap(),
            Some(assigned(0, 9, "0x11", 800, 100))
        );
        // A job that is new takes its row afresh, whatever an earlier coordinator's request of the id left.
        sqlx::query("DELETE FROM jobs WHERE id='3'")
            .execute(&j.pool)
            .await
            .unwrap();
        assert!(
            j.discovered_round("3", 170, "4", &assigned(1, 2, "0x33", 50, 140))
                .await
                .unwrap()
        );
        assert_eq!(
            j.round_assignment("3").await.unwrap(),
            Some(assigned(1, 2, "0x33", 50, 140))
        );
        assert_eq!(j.round_assignment("4").await.unwrap(), None);
        // Demand: live jobs by round, the earliest deadline first; settled and expiring jobs are not demand.
        assert_eq!(
            j.round_demand(150).await.unwrap(),
            vec![
                DemandedRound {
                    beacon: 0,
                    round: 9,
                    requests: 1,
                    earliest_deadline: 160
                },
                DemandedRound {
                    beacon: 0,
                    round: 7,
                    requests: 1,
                    earliest_deadline: 161
                },
                DemandedRound {
                    beacon: 1,
                    round: 2,
                    requests: 1,
                    earliest_deadline: 170
                },
            ]
        );
        // Two live jobs of one round are one round of demand.
        assert!(
            j.discovered_round("5", 168, "6", &assigned(1, 2, "0x55", 10, 141))
                .await
                .unwrap()
        );
        assert_eq!(
            j.round_demand(165).await.unwrap(),
            vec![DemandedRound {
                beacon: 1,
                round: 2,
                requests: 2,
                earliest_deadline: 168
            }]
        );
        sqlx::query("DELETE FROM jobs WHERE id='5'")
            .execute(&j.pool)
            .await
            .unwrap();
        j.state("2", "served").await.unwrap();
        assert_eq!(
            j.round_demand(150).await.unwrap(),
            vec![
                DemandedRound {
                    beacon: 0,
                    round: 9,
                    requests: 1,
                    earliest_deadline: 160
                },
                DemandedRound {
                    beacon: 1,
                    round: 2,
                    requests: 1,
                    earliest_deadline: 170
                },
            ]
        );
        assert_eq!(
            j.round_demand(165).await.unwrap(),
            vec![DemandedRound {
                beacon: 1,
                round: 2,
                requests: 1,
                earliest_deadline: 170
            }]
        );
    }
    /// Every table, index and trigger of a journal, with its SQL, in a fixed order.
    async fn schema_of(pool: &SqlitePool) -> Vec<(String, String, String)> {
        sqlx::query_as("SELECT type,name,COALESCE(sql,'') FROM sqlite_master ORDER BY type,name")
            .fetch_all(pool)
            .await
            .unwrap()
    }
    /// The names of a journal's tables and indexes, without SQLite's own.
    fn names(schema: &[(String, String, String)]) -> Vec<String> {
        schema
            .iter()
            .filter(|(kind, name, _)| kind != "trigger" && !name.starts_with("sqlite_"))
            .map(|(kind, name, _)| format!("{kind} {name}"))
            .collect()
    }
    /// The digest of a schema: keccak256 of one line per entry, `type|name|sql`.
    fn digest(schema: &[(String, String, String)]) -> String {
        let dump: String = schema
            .iter()
            .map(|(kind, name, sql)| format!("{kind}|{name}|{sql}\n"))
            .collect();
        alloy_primitives::keccak256(dump.as_bytes()).to_string()
    }
    #[tokio::test]
    async fn an_arc_keepers_journal_is_the_journal_of_0_4_1_and_a_round_keepers_has_no_epoch_table()
    {
        let dir = tempfile::tempdir().unwrap();
        // The journal of an epoch coordinator's keeper in finalized mode, as an Arc keeper runs, is exactly the journal
        // of keeper 0.4.1: the same tables, indexes and triggers with the same SQL, byte for byte. The digest was taken
        // in the 0.4.1 tree (f03c688) itself: its `Journal::open`, this query and this dump.
        let epoch = Journal::open_for(
            &dir.path().join("epoch.sqlite"),
            "scope",
            CoordinatorKind::Epoch,
            FinalityMode::Finalized,
        )
        .await
        .unwrap();
        let arc = schema_of(&epoch.pool).await;
        assert_eq!(digest(&arc), SCHEMA_DIGEST_0_4_1, "{arc:#?}");
        assert_eq!(
            names(&arc),
            [
                "index batch_members_request",
                "index epoch_demand_epoch",
                "index epoch_work_identity",
                "index epoch_work_open",
                "index epochs_compact",
                "index jobs_compact",
                "index jobs_open",
                "index txs_active",
                "index txs_compact",
                "index txs_job_state",
                "index txs_nonce_state",
                "table audit_events",
                "table batch_members",
                "table epoch_demand",
                "table epoch_relay_breaker",
                "table epoch_work",
                "table finalized_receipts",
                "table jobs",
                "table meta",
                "table telemetry_outbox",
                "table txs",
            ]
        );
        assert_eq!(epoch.meta(KIND_KEY).await.unwrap(), None);
        // An epoch coordinator's keeper in soft mode has the marks of soft finality beside them, and nothing else.
        let soft = Journal::open(&dir.path().join("soft.sqlite"), "scope")
            .await
            .unwrap();
        let soft_schema = schema_of(&soft.pool).await;
        let added: Vec<String> = soft_schema
            .iter()
            .filter(|entry| !arc.contains(entry))
            .map(|(kind, name, _)| format!("{kind} {name}"))
            .collect();
        assert_eq!(
            added,
            ["index sqlite_autoindex_soft_marks_1", "table soft_marks"]
        );
        assert!(arc.iter().all(|entry| soft_schema.contains(entry)));
        // A round coordinator's journal has none of the epoch lane's tables or indexes, and the round lane's instead:
        // the requests' rounds, the rounds' work and the relays' circuits. Everything both have is the same.
        let round = Journal::open_for(
            &dir.path().join("round.sqlite"),
            "scope",
            CoordinatorKind::Round,
            FinalityMode::Soft,
        )
        .await
        .unwrap();
        let schema = schema_of(&round.pool).await;
        assert_eq!(
            names(&schema),
            [
                "index batch_members_request",
                "index jobs_compact",
                "index jobs_open",
                "index round_demand_round",
                "index txs_active",
                "index txs_compact",
                "index txs_job_state",
                "index txs_nonce_state",
                "table audit_events",
                "table batch_members",
                "table drand_relay_breaker",
                "table finalized_receipts",
                "table jobs",
                "table meta",
                "table round_demand",
                "table round_work",
                "table soft_marks",
                "table telemetry_outbox",
                "table txs",
            ]
        );
        let round_lane = |name: &str| {
            name.starts_with("round_")
                || name.starts_with("drand_relay_breaker")
                || name.starts_with("sqlite_autoindex_round_")
                || name.starts_with("sqlite_autoindex_drand_relay_breaker")
        };
        for entry in &schema {
            if !round_lane(&entry.1) {
                assert!(soft_schema.contains(entry), "{entry:?}");
            }
        }
        assert!(
            !schema
                .iter()
                .any(|(_, name, sql)| name.contains("epoch") || sql.contains("epoch_")),
            "{schema:#?}"
        );
        let work: String =
            sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE name='round_work'")
                .fetch_one(&round.pool)
                .await
                .unwrap();
        assert_eq!(
            work,
            "CREATE TABLE round_work(beacon INTEGER NOT NULL,round INTEGER NOT NULL,state TEXT NOT NULL DEFAULT 'pending',signature TEXT,randomness TEXT,attempts INTEGER NOT NULL DEFAULT 0,retry_at INTEGER NOT NULL DEFAULT 0,last_error TEXT,failing_since INTEGER,failed_at INTEGER,failed_rpc INTEGER,PRIMARY KEY(beacon,round))"
        );
        assert_eq!(
            round.meta(KIND_KEY).await.unwrap().as_deref(),
            Some("round")
        );
        // Compaction runs on each.
        epoch.compact_history(1_000).await.unwrap();
        soft.compact_history_for(1_000, FinalityMode::Soft)
            .await
            .unwrap();
        round
            .compact_history_for(1_000, FinalityMode::Soft)
            .await
            .unwrap();
        epoch.pool.close().await;
        soft.pool.close().await;
        round.pool.close().await;
        // Each kind refuses the other's journal, and opens its own again as it was.
        let error = Journal::open_for(
            &dir.path().join("epoch.sqlite"),
            "scope",
            CoordinatorKind::Round,
            FinalityMode::Finalized,
        )
        .await
        .err()
        .unwrap();
        assert!(
            error
                .to_string()
                .contains("Journal belongs to an epoch coordinator's keeper"),
            "{error}"
        );
        let error = Journal::open(&dir.path().join("round.sqlite"), "scope")
            .await
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("Journal belongs to a round coordinator's keeper"),
            "{error}"
        );
        let again = Journal::open_for(
            &dir.path().join("round.sqlite"),
            "scope",
            CoordinatorKind::Round,
            FinalityMode::Soft,
        )
        .await
        .unwrap();
        assert_eq!(schema_of(&again.pool).await, schema);
        again.pool.close().await;
        let again = Journal::open_for(
            &dir.path().join("epoch.sqlite"),
            "scope",
            CoordinatorKind::Epoch,
            FinalityMode::Finalized,
        )
        .await
        .unwrap();
        assert_eq!(schema_of(&again.pool).await, arc);
        again.pool.close().await;
    }
    /// The digest of the full schema of keeper 0.4.1's journal: see the test above.
    const SCHEMA_DIGEST_0_4_1: &str =
        "0x6ebe7a0d9ad50981cd25c4971d9172993f395c1afe1d16f377490cd0a0545a5a";
    #[tokio::test]
    async fn a_soft_keeper_adds_the_marks_beside_the_tables_of_a_journal_of_0_4_1_and_changes_none_of_them()
     {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arc.sqlite");
        let open = |mode| {
            let path = path.clone();
            async move {
                Journal::open_for(&path, "scope", CoordinatorKind::Epoch, mode)
                    .await
                    .unwrap()
            }
        };
        // A journal as an Arc keeper leaves it, 0.4.1's: a keeper in finalized mode opens it again as it is.
        let arc = open(FinalityMode::Finalized).await;
        let before = schema_of(&arc.pool).await;
        arc.pool.close().await;
        let arc = open(FinalityMode::Finalized).await;
        assert_eq!(schema_of(&arc.pool).await, before);
        arc.pool.close().await;
        // A keeper in soft mode adds the marks and changes no other table or index; a keeper in finalized mode keeps them.
        let soft = open(FinalityMode::Soft).await;
        let after = schema_of(&soft.pool).await;
        soft.pool.close().await;
        assert!(before.iter().all(|entry| after.contains(entry)));
        let added: Vec<String> = after
            .iter()
            .filter(|entry| !before.contains(entry))
            .map(|(_, name, _)| name.clone())
            .collect();
        assert_eq!(added, ["sqlite_autoindex_soft_marks_1", "soft_marks"]);
        let arc = open(FinalityMode::Finalized).await;
        assert_eq!(schema_of(&arc.pool).await, after);
        arc.pool.close().await;
    }
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
    async fn the_soft_checkpoint_survives_restart_never_moves_down_and_is_not_the_finalized_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("soft.sqlite");
        let journal = Journal::open(&path, "scope").await.unwrap();
        journal.soft_checkpoint(10, "block10").await.unwrap();
        journal.pool.close().await;
        let journal = Journal::open(&path, "scope").await.unwrap();
        let saved = journal.meta("soft_checkpoint").await.unwrap();
        assert_eq!(
            saved,
            Some(serde_json::to_string(&(10, "block10")).unwrap())
        );
        // A lower block is no news, the same block again is the same, and another hash for it is a conflict that
        // changes nothing.
        journal.soft_checkpoint(9, "old").await.unwrap();
        journal.soft_checkpoint(10, "block10").await.unwrap();
        assert_eq!(journal.meta("soft_checkpoint").await.unwrap(), saved);
        let conflict = journal.soft_checkpoint(10, "changed").await.unwrap_err();
        assert_eq!(conflict.to_string(), "Soft checkpoint conflict");
        assert_eq!(journal.meta("soft_checkpoint").await.unwrap(), saved);
        journal.soft_checkpoint(11, "block11").await.unwrap();
        assert_eq!(
            journal.meta("soft_checkpoint").await.unwrap(),
            Some(serde_json::to_string(&(11, "block11")).unwrap())
        );
        // The two checkpoints are separate keys: neither writes the other, and a soft block never becomes a finalized
        // receipt.
        assert_eq!(journal.meta("finalized_checkpoint").await.unwrap(), None);
        journal.finalized_checkpoint(5, "final5").await.unwrap();
        assert_eq!(
            journal.meta("soft_checkpoint").await.unwrap(),
            Some(serde_json::to_string(&(11, "block11")).unwrap())
        );
        assert_eq!(
            journal.meta("finalized_checkpoint").await.unwrap(),
            Some(serde_json::to_string(&(5, "final5")).unwrap())
        );
        // The finalized conflict keeps its own words.
        assert_eq!(
            journal
                .finalized_checkpoint(5, "other")
                .await
                .unwrap_err()
                .to_string(),
            "Finalized checkpoint conflict"
        );
        let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM finalized_receipts")
            .fetch_one(&journal.pool)
            .await
            .unwrap();
        assert_eq!(receipts, 0);
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
    async fn a_reverted_batch_returns_its_live_members_for_single_resends_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("batch-revert.sqlite");
        let j = Journal::open(&path, "scope").await.unwrap();
        for id in ["1", "2", "3", "4"] {
            j.discovered(id, 100, "5").await.unwrap();
            j.prepared(id, "proof", "call").await.unwrap();
        }
        let members = ids(&["1", "2", "3"]);
        let key = batch_key(&members).unwrap();
        j.preflight_backoff_many(&members, 50).await.unwrap();
        j.signed_batch(&batch_attempt(&key, 7, "h1", "fulfill_batch"), &members)
            .await
            .unwrap();
        // Member 3 lost its calldata (it can only happen by hand): it is proven again instead.
        sqlx::query("UPDATE jobs SET call=NULL WHERE id='3'")
            .execute(&j.pool)
            .await
            .unwrap();
        let outcome = vec![
            ("1".to_string(), "served"),
            ("2".to_string(), "prepared"),
            ("3".to_string(), "prepared"),
        ];
        // A crash on the last write leaves the batch live and nothing excluded.
        sqlx::raw_sql("CREATE TRIGGER refuse_exclusion BEFORE INSERT ON meta WHEN NEW.key='batch_exclude:3' BEGIN SELECT RAISE(ABORT,'injected crash'); END;")
            .execute(&j.pool).await.unwrap();
        assert!(j.resolve_nonce_batch(7, &key, &outcome).await.is_err());
        j.pool.close().await;
        let j = Journal::open(&path, "scope").await.unwrap();
        assert_eq!(j.unresolved().await.unwrap().len(), 1);
        assert_eq!(j.job("2").await.unwrap().unwrap().state, "signed");
        assert!(j.batch_excluded_prepared().await.unwrap().is_empty());
        assert!(j.meta("batch_exclude:2").await.unwrap().is_none());
        sqlx::query("DROP TRIGGER refuse_exclusion")
            .execute(&j.pool)
            .await
            .unwrap();
        j.resolve_nonce_batch(7, &key, &outcome).await.unwrap();
        j.pool.close().await;
        let j = Journal::open(&path, "scope").await.unwrap();
        assert!(j.unresolved().await.unwrap().is_empty());
        assert_eq!(j.nonce_floor().await.unwrap(), 8);
        assert_eq!(j.job("1").await.unwrap().unwrap().state, "served");
        let resent = j.job("2").await.unwrap().unwrap();
        assert_eq!(
            (resent.state.as_str(), resent.call.as_deref()),
            ("prepared", Some("call"))
        );
        assert_eq!(j.job("3").await.unwrap().unwrap().state, "pending");
        // Both stay out of batches; the resend is due at once, ahead of the batch's own backoff.
        assert!(j.meta("batch_exclude:2").await.unwrap().is_some());
        assert!(j.meta("batch_exclude:3").await.unwrap().is_some());
        assert_eq!(
            j.batch_excluded_prepared().await.unwrap(),
            ["2".to_string()].into()
        );
        let due: Vec<String> = j
            .prepared_due(0)
            .await
            .unwrap()
            .into_iter()
            .map(|job| job.id)
            .collect();
        assert_eq!(due, ["2", "4"]);
        assert_eq!(
            j.meta("preflight_retry:1").await.unwrap().as_deref(),
            Some("50")
        );
        // The resend takes its own nonce; the old batch never counts as live for it.
        j.signed(&batch_attempt("2", 8, "single", "fulfill"))
            .await
            .unwrap();
        assert_eq!(j.job("2").await.unwrap().unwrap().state, "signed");
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

    /// Soft finality: the marks of the blocks a keeper acted on, written with the action, and the audit that checks them.
    mod soft_marks {
        use super::*;
        fn mark(kind: MarkKind, number: u64, hash: &str, reference: &str) -> Mark {
            Mark {
                kind,
                number,
                hash: hash.into(),
                reference: reference.into(),
                created: 1_000,
                status: (kind == MarkKind::Receipt).then_some(1),
            }
        }
        type Row = (String, u64, String, String);
        /// What a test reads of the marks: kind, block, hash and reference, lowest block first.
        async fn marks(j: &Journal) -> Vec<Row> {
            j.soft_marks()
                .await
                .unwrap()
                .into_iter()
                .map(|m| (m.kind.name().to_owned(), m.number, m.hash, m.reference))
                .collect()
        }
        fn row(kind: &str, number: u64, hash: &str, reference: &str) -> Row {
            (kind.into(), number, hash.into(), reference.into())
        }
        async fn put(j: &Journal, mark: &Mark) {
            let mut tx = j.pool.begin().await.unwrap();
            write_mark(&mut tx, mark).await.unwrap();
            tx.commit().await.unwrap();
        }
        async fn count(j: &Journal, table: &str) -> i64 {
            // The statement is built from the constant table names of these tests.
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
                .fetch_one(&j.pool)
                .await
                .unwrap()
        }
        async fn crash_on(j: &Journal, trigger: &str) {
            sqlx::raw_sql(sqlx::AssertSqlSafe(trigger.to_owned()))
                .execute(&j.pool)
                .await
                .unwrap();
        }
        async fn lift(j: &Journal, trigger: &str) {
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP TRIGGER {trigger}")))
                .execute(&j.pool)
                .await
                .unwrap();
        }
        async fn insert_attempt(j: &Journal, job: &str, nonce: i64, hash: &str) {
            sqlx::query("INSERT INTO txs(job,nonce,hash,raw,kind,fee,gas,priority,payload,created) VALUES(?,?,?,'signed-bytes','fulfill','1',1,'1','payload',1)")
                .bind(job).bind(nonce).bind(hash).execute(&j.pool).await.unwrap();
        }

        #[tokio::test]
        async fn a_receipt_mark_commits_with_the_resolution_it_justifies_or_not_at_all() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("receipt-mark.sqlite");
            let j = Journal::open(&path, "scope").await.unwrap();
            j.discovered("1", 100, "2").await.unwrap();
            insert_attempt(&j, "1", 7, "0xtx").await;
            let receipt = mark(MarkKind::Receipt, 50, "0xblock50", "0xtx");

            // The mark cannot be written: the nonce stays unresolved, the floor where it was.
            crash_on(&j, "CREATE TRIGGER refuse_mark BEFORE INSERT ON soft_marks BEGIN SELECT RAISE(ABORT,'injected crash'); END;").await;
            assert!(
                j.resolve_nonce_job_marked(7, "1", "served", Some(&receipt))
                    .await
                    .is_err()
            );
            assert_eq!(j.unresolved().await.unwrap().len(), 1);
            assert_eq!(j.nonce_floor().await.unwrap(), 0);
            assert_eq!(j.pending().await.unwrap()[0].state, "pending");
            lift(&j, "refuse_mark").await;

            // The resolution fails after the mark was written: the mark goes with it, across a restart too.
            crash_on(&j, "CREATE TRIGGER refuse_terminal BEFORE UPDATE OF state ON jobs WHEN NEW.state='served' BEGIN SELECT RAISE(ABORT,'injected crash'); END;").await;
            assert!(
                j.resolve_nonce_job_marked(7, "1", "served", Some(&receipt))
                    .await
                    .is_err()
            );
            j.pool.close().await;
            let j = Journal::open(&path, "scope").await.unwrap();
            assert!(marks(&j).await.is_empty());
            assert_eq!(j.unresolved().await.unwrap().len(), 1);
            assert_eq!(j.nonce_floor().await.unwrap(), 0);
            lift(&j, "refuse_terminal").await;

            // Together: the nonce is resolved, the floor moved and the block is on record, in one commit.
            j.resolve_nonce_job_marked(7, "1", "served", Some(&receipt))
                .await
                .unwrap();
            j.pool.close().await;
            let j = Journal::open(&path, "scope").await.unwrap();
            assert!(j.unresolved().await.unwrap().is_empty());
            assert_eq!(j.nonce_floor().await.unwrap(), 8);
            assert_eq!(marks(&j).await, [row("receipt", 50, "0xblock50", "0xtx")]);
            let stored = j.soft_marks().await.unwrap();
            assert_eq!((stored[0].created, stored[0].status), (1_000, Some(1)));
            // The finalized records are the audit's alone.
            assert_eq!(count(&j, "finalized_receipts").await, 0);
            assert_eq!(j.meta("finalized_checkpoint").await.unwrap(), None);
            j.pool.close().await;
        }

        #[tokio::test]
        async fn the_epoch_and_batch_resolutions_write_their_marks_in_their_own_commit() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("receipt-mark-lanes.sqlite");
            let j = Journal::open(&path, "scope").await.unwrap();

            // An epoch commit: its state changes with the nonce floor and the mark.
            sqlx::raw_sql("INSERT INTO epoch_work(key,registry,catalog,epoch,start,state) VALUES('epoch-1','registry','catalog',1,200,'submitted');")
                .execute(&j.pool).await.unwrap();
            sqlx::query("INSERT INTO txs(job,nonce,hash,raw,kind,fee,gas,priority,payload,created) VALUES('epoch-1',3,'0xepoch','raw','epoch','1',1,'1','p',1)")
                .execute(&j.pool).await.unwrap();
            let committed = mark(MarkKind::Receipt, 60, "0xblock60", "0xepoch");
            crash_on(&j, "CREATE TRIGGER refuse_epoch BEFORE UPDATE OF state ON epoch_work WHEN NEW.state='committed' BEGIN SELECT RAISE(ABORT,'injected crash'); END;").await;
            assert!(
                j.resolve_nonce_epoch_marked(3, "epoch-1", "committed", Some(&committed))
                    .await
                    .is_err()
            );
            assert!(marks(&j).await.is_empty());
            assert_eq!(j.nonce_floor().await.unwrap(), 0);
            lift(&j, "refuse_epoch").await;
            j.resolve_nonce_epoch_marked(3, "epoch-1", "committed", Some(&committed))
                .await
                .unwrap();
            assert_eq!(
                marks(&j).await,
                [row("receipt", 60, "0xblock60", "0xepoch")]
            );
            assert_eq!(j.nonce_floor().await.unwrap(), 4);

            // A batch: every member, the nonce floor and the mark.
            for id in ["1", "2"] {
                j.discovered(id, 100, "3").await.unwrap();
                j.prepared(id, "proof", "call").await.unwrap();
            }
            let members = ids(&["1", "2"]);
            let key = batch_key(&members).unwrap();
            j.signed_batch(
                &batch_attempt(&key, 9, "0xbatch", "fulfill_batch"),
                &members,
            )
            .await
            .unwrap();
            let served = mark(MarkKind::Receipt, 61, "0xblock61", "0xbatch");
            let outcome = vec![("1".to_string(), "served"), ("2".to_string(), "expired")];
            // A member that is missing from the resolution is refused before the mark is written.
            assert!(
                j.resolve_nonce_batch_marked(9, &key, &outcome[..1], Some(&served))
                    .await
                    .is_err()
            );
            crash_on(&j, "CREATE TRIGGER refuse_member BEFORE UPDATE OF state ON jobs WHEN NEW.id='2' AND NEW.state='expired' BEGIN SELECT RAISE(ABORT,'injected crash'); END;").await;
            assert!(
                j.resolve_nonce_batch_marked(9, &key, &outcome, Some(&served))
                    .await
                    .is_err()
            );
            assert_eq!(marks(&j).await.len(), 1, "the failed batch left no mark");
            assert_eq!(j.nonce_floor().await.unwrap(), 4);
            lift(&j, "refuse_member").await;
            j.resolve_nonce_batch_marked(9, &key, &outcome, Some(&served))
                .await
                .unwrap();
            assert_eq!(
                marks(&j).await,
                [
                    row("receipt", 60, "0xblock60", "0xepoch"),
                    row("receipt", 61, "0xblock61", "0xbatch")
                ]
            );
            assert_eq!(j.nonce_floor().await.unwrap(), 10);
            j.pool.close().await;
        }

        #[tokio::test]
        async fn a_nonce_without_a_receipt_is_resolved_with_a_nonce_mark() {
            let dir = tempfile::tempdir().unwrap();
            let j = Journal::open(&dir.path().join("nonce-mark.sqlite"), "scope")
                .await
                .unwrap();
            j.discovered("1", 100, "2").await.unwrap();
            insert_attempt(&j, "1", 4, "0xlost").await;
            // The block the nonce was found consumed at; no receipt, so no status.
            let consumed = mark(MarkKind::Nonce, 80, "0xblock80", "4");
            j.resolve_nonce_job_marked(4, "1", "expired", Some(&consumed))
                .await
                .unwrap();
            assert_eq!(marks(&j).await, [row("nonce", 80, "0xblock80", "4")]);
            assert_eq!(j.nonce_floor().await.unwrap(), 5);
            // The audit checks the block and keeps no finalized receipt, for there was no receipt.
            let audit = j
                .audit_marks(&[(80, "0xblock80".into())], 2_000)
                .await
                .unwrap();
            assert_eq!(
                audit,
                Audit::Audited {
                    blocks: 1,
                    receipts: 0,
                    checkpoint: (80, "0xblock80".into())
                }
            );
            assert!(marks(&j).await.is_empty());
            assert_eq!(count(&j, "finalized_receipts").await, 0);
            j.pool.close().await;
        }

        #[tokio::test]
        async fn a_sign_mark_commits_with_the_signed_bytes_or_not_at_all() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("sign-mark.sqlite");
            let j = Journal::open(&path, "scope").await.unwrap();
            for id in ["1", "2", "3"] {
                j.discovered(id, 100, "4").await.unwrap();
                j.prepared(id, "proof", "call").await.unwrap();
            }
            let single = mark(MarkKind::Sign, 40, "0xhead40", "0xsingle");
            let attempt = batch_attempt("1", 5, "0xsingle", "fulfill");
            // The state change that follows the mark fails: neither the bytes nor the mark are there.
            crash_on(&j, "CREATE TRIGGER refuse_signed BEFORE UPDATE OF state ON jobs WHEN NEW.id='1' AND NEW.state='signed' BEGIN SELECT RAISE(ABORT,'injected crash'); END;").await;
            assert!(j.signed_marked(&attempt, Some(&single)).await.is_err());
            j.pool.close().await;
            let j = Journal::open(&path, "scope").await.unwrap();
            assert!(j.unresolved().await.unwrap().is_empty());
            assert!(marks(&j).await.is_empty());
            lift(&j, "refuse_signed").await;
            j.signed_marked(&attempt, Some(&single)).await.unwrap();
            assert_eq!(j.unresolved().await.unwrap()[0].hash, "0xsingle");
            assert_eq!(marks(&j).await, [row("sign", 40, "0xhead40", "0xsingle")]);
            // A replacement of the same nonce is another transaction, signed on another head, with a mark of its own.
            let replacement = mark(MarkKind::Sign, 41, "0xhead41", "0xreplacement");
            j.signed_marked(
                &batch_attempt("1", 5, "0xreplacement", "fulfill"),
                Some(&replacement),
            )
            .await
            .unwrap();
            assert_eq!(marks(&j).await.len(), 2);

            // A batch: the lane, the members, their states and the mark commit together.
            j.resolve_nonce_job_marked(5, "1", "served", None)
                .await
                .unwrap();
            let members = ids(&["2", "3"]);
            let key = batch_key(&members).unwrap();
            let batch = mark(MarkKind::Sign, 42, "0xhead42", "0xbatch");
            crash_on(&j, "CREATE TRIGGER refuse_last BEFORE UPDATE OF state ON jobs WHEN NEW.id='3' AND NEW.state='signed' BEGIN SELECT RAISE(ABORT,'injected crash'); END;").await;
            assert!(
                j.signed_batch_marked(
                    &batch_attempt(&key, 6, "0xbatch", "fulfill_batch"),
                    &members,
                    Some(&batch)
                )
                .await
                .is_err()
            );
            assert_eq!(marks(&j).await.len(), 2);
            lift(&j, "refuse_last").await;
            j.signed_batch_marked(
                &batch_attempt(&key, 6, "0xbatch", "fulfill_batch"),
                &members,
                Some(&batch),
            )
            .await
            .unwrap();
            assert_eq!(
                marks(&j).await.last(),
                Some(&row("sign", 42, "0xhead42", "0xbatch"))
            );
            j.pool.close().await;
        }

        #[tokio::test]
        async fn without_a_mark_the_journal_is_what_it_was() {
            // Finalized mode passes no mark: the same calls write no row and change nothing else.
            let dir = tempfile::tempdir().unwrap();
            let j = Journal::open(&dir.path().join("none.sqlite"), "scope")
                .await
                .unwrap();
            j.discovered("1", 100, "2").await.unwrap();
            j.prepared("1", "proof", "call").await.unwrap();
            j.signed_marked(&batch_attempt("1", 3, "0xa", "fulfill"), None)
                .await
                .unwrap();
            j.resolve_nonce_job_marked(3, "1", "served", None)
                .await
                .unwrap();
            j.finalized_receipt("0xa", 10, "0xb10", 1).await.unwrap();
            j.finalized_checkpoint(11, "0xb11").await.unwrap();
            assert_eq!(count(&j, "soft_marks").await, 0);
            assert_eq!(j.meta("soft_checkpoint").await.unwrap(), None);
            assert_eq!(j.nonce_floor().await.unwrap(), 4);
            j.pool.close().await;
        }

        #[tokio::test]
        async fn the_head_mark_is_one_row_that_moves_up_with_the_soft_checkpoint_in_one_commit() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("head-mark.sqlite");
            let j = Journal::open(&path, "scope").await.unwrap();
            j.soft_decision(10, "0xh10", 100).await.unwrap();
            assert_eq!(marks(&j).await, [row("head", 10, "0xh10", "")]);
            assert_eq!(
                j.meta("soft_checkpoint").await.unwrap(),
                Some(serde_json::to_string(&(10, "0xh10")).unwrap())
            );
            // Each tick replaces it; the same block again only refreshes when it was written.
            j.soft_decision(11, "0xh11", 101).await.unwrap();
            assert_eq!(marks(&j).await, [row("head", 11, "0xh11", "")]);
            j.soft_decision(11, "0xh11", 150).await.unwrap();
            let stored = j.soft_marks().await.unwrap();
            assert_eq!((stored.len(), stored[0].created), (1, 150));
            // A lower block is no news, as for the checkpoint; another hash for a block it holds is a conflict.
            j.soft_decision(9, "0xh9", 160).await.unwrap();
            assert_eq!(marks(&j).await, [row("head", 11, "0xh11", "")]);
            assert_eq!(
                j.soft_decision(11, "0xother", 170)
                    .await
                    .unwrap_err()
                    .to_string(),
                "Soft checkpoint conflict"
            );
            assert_eq!(marks(&j).await, [row("head", 11, "0xh11", "")]);
            // A mark that cannot be replaced takes the checkpoint's move back with it.
            crash_on(&j, "CREATE TRIGGER refuse_head BEFORE INSERT ON soft_marks WHEN NEW.kind='head' BEGIN SELECT RAISE(ABORT,'injected crash'); END;").await;
            assert!(j.soft_decision(12, "0xh12", 180).await.is_err());
            j.pool.close().await;
            let j = Journal::open(&path, "scope").await.unwrap();
            assert_eq!(marks(&j).await, [row("head", 11, "0xh11", "")]);
            assert_eq!(
                j.meta("soft_checkpoint").await.unwrap(),
                Some(serde_json::to_string(&(11, "0xh11")).unwrap())
            );
            // It sits beside the other marks without replacing them, and is no finalized record.
            lift(&j, "refuse_head").await;
            put(&j, &mark(MarkKind::Sign, 11, "0xh11", "0xtx")).await;
            j.soft_decision(13, "0xh13", 190).await.unwrap();
            assert_eq!(
                marks(&j).await,
                [
                    row("sign", 11, "0xh11", "0xtx"),
                    row("head", 13, "0xh13", "")
                ]
            );
            assert_eq!(j.meta("finalized_checkpoint").await.unwrap(), None);
            // The plain soft checkpoint of the earlier release writes no mark.
            j.soft_checkpoint(14, "0xh14").await.unwrap();
            assert_eq!(marks(&j).await.len(), 2);
            j.pool.close().await;
        }

        #[tokio::test]
        async fn a_mark_is_written_once_and_never_overwritten() {
            let dir = tempfile::tempdir().unwrap();
            let j = Journal::open(&dir.path().join("conflict.sqlite"), "scope")
                .await
                .unwrap();
            let receipt = mark(MarkKind::Receipt, 50, "0xblock50", "0xtx");
            put(&j, &receipt).await;
            put(&j, &receipt).await;
            assert_eq!(marks(&j).await.len(), 1);
            // The same transaction in another block, or with another status, is a conflict, not an update.
            for other in [
                Mark {
                    hash: "0xother".into(),
                    ..receipt.clone()
                },
                Mark {
                    status: Some(0),
                    ..receipt.clone()
                },
            ] {
                let mut tx = j.pool.begin().await.unwrap();
                let error = write_mark(&mut tx, &other).await.unwrap_err();
                assert_eq!(error.to_string(), "Soft mark conflict");
            }
            // The head is written with the soft checkpoint only.
            let mut tx = j.pool.begin().await.unwrap();
            assert!(
                write_mark(&mut tx, &mark(MarkKind::Head, 1, "0xh", ""))
                    .await
                    .is_err()
            );
            drop(tx);
            assert_eq!(j.soft_marks().await.unwrap(), [receipt]);
            j.pool.close().await;
        }

        #[tokio::test]
        async fn an_audit_makes_receipts_final_advances_the_checkpoint_and_clears_what_it_checked()
        {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("audit.sqlite");
            let j = Journal::open(&path, "scope").await.unwrap();
            put(&j, &mark(MarkKind::Receipt, 50, "0xb50", "0xtx1")).await;
            put(&j, &mark(MarkKind::Sign, 50, "0xb50", "0xtx1")).await;
            put(
                &j,
                &Mark {
                    status: Some(0),
                    ..mark(MarkKind::Receipt, 52, "0xb52", "0xtx2")
                },
            )
            .await;
            put(&j, &mark(MarkKind::Nonce, 52, "0xb52", "7")).await;
            put(&j, &mark(MarkKind::Receipt, 70, "0xb70", "0xtx3")).await;
            j.soft_decision(75, "0xb75", 100).await.unwrap();
            assert_eq!(j.mark_numbers(60, 64).await.unwrap(), [50, 52]);
            assert_eq!(j.mark_numbers(100, 64).await.unwrap(), [50, 52, 70, 75]);
            assert_eq!(j.mark_numbers(100, 3).await.unwrap(), [50, 52, 70]);
            assert!(j.mark_numbers(49, 64).await.unwrap().is_empty());

            // Nothing is below the chain's finalized head yet: nothing to do, nothing changes.
            assert_eq!(j.audit_marks(&[], 2_000).await.unwrap(), Audit::Idle);
            assert_eq!(marks(&j).await.len(), 6);

            let audit = j
                .audit_marks(&[(50, "0xb50".into()), (52, "0xb52".into())], 2_000)
                .await
                .unwrap();
            assert_eq!(
                audit,
                Audit::Audited {
                    blocks: 2,
                    receipts: 2,
                    checkpoint: (52, "0xb52".into())
                }
            );
            // Each receipt is a finalized receipt now, with its block and its status; the checkpoint is the highest block.
            let receipts: Vec<(String, i64, String, i64)> = sqlx::query_as(
                "SELECT hash,block_number,block_hash,status FROM finalized_receipts ORDER BY hash",
            )
            .fetch_all(&j.pool)
            .await
            .unwrap();
            assert_eq!(
                receipts,
                [
                    ("0xtx1".into(), 50, "0xb50".into(), 1),
                    ("0xtx2".into(), 52, "0xb52".into(), 0)
                ]
            );
            assert_eq!(
                j.meta("finalized_checkpoint").await.unwrap(),
                Some(serde_json::to_string(&(52, "0xb52")).unwrap())
            );
            // Only the marks of the audited blocks are gone; the soft checkpoint is not the audit's.
            assert_eq!(
                marks(&j).await,
                [
                    row("receipt", 70, "0xb70", "0xtx3"),
                    row("head", 75, "0xb75", "")
                ]
            );
            assert_eq!(
                j.meta("soft_checkpoint").await.unwrap(),
                Some(serde_json::to_string(&(75, "0xb75")).unwrap())
            );
            assert_eq!(j.finality_mismatch().await.unwrap(), None);

            // The rest, after a restart: the head mark is audited like any other.
            j.pool.close().await;
            let j = Journal::open(&path, "scope").await.unwrap();
            let audit = j
                .audit_marks(&[(70, "0xb70".into()), (75, "0xb75".into())], 2_100)
                .await
                .unwrap();
            assert_eq!(
                audit,
                Audit::Audited {
                    blocks: 2,
                    receipts: 1,
                    checkpoint: (75, "0xb75".into())
                }
            );
            assert!(marks(&j).await.is_empty());
            assert_eq!(count(&j, "finalized_receipts").await, 3);
            // A second look at blocks that are gone finds nothing.
            assert_eq!(
                j.audit_marks(&[(70, "0xb70".into())], 2_200).await.unwrap(),
                Audit::Idle
            );
            j.pool.close().await;
        }

        #[tokio::test]
        async fn a_replaced_block_moves_nothing_records_nothing_and_once_recorded_stops_the_audit()
        {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("mismatch.sqlite");
            let j = Journal::open(&path, "scope").await.unwrap();
            put(&j, &mark(MarkKind::Receipt, 50, "0xb50", "0xtx1")).await;
            put(&j, &mark(MarkKind::Sign, 51, "0xb51", "0xtx2")).await;
            put(&j, &mark(MarkKind::Receipt, 52, "0xb52", "0xtx3")).await;
            let canonical = [
                (50, "0xb50".to_string()),
                (51, "0xreplaced".to_string()),
                (52, "0xb52".to_string()),
            ];
            let audit = j.audit_marks(&canonical, 2_000).await.unwrap();
            let found = Mismatch {
                kind: "sign".into(),
                number: 51,
                reference: "0xtx2".into(),
                expected: "0xb51".into(),
                actual: "0xreplaced".into(),
                detected_at: 2_000,
            };
            assert_eq!(audit, Audit::Mismatch(found.clone()));
            // Nothing moved: not the receipt of the matching block before it, and not the one after. Nothing is
            // recorded either: the hashes are one endpoint's, and the keeper asks the others first.
            assert_eq!(marks(&j).await.len(), 3);
            assert_eq!(count(&j, "finalized_receipts").await, 0);
            assert_eq!(j.meta("finalized_checkpoint").await.unwrap(), None);
            assert_eq!(j.finality_mismatch().await.unwrap(), None);
            assert_eq!(
                j.audit_marks(&canonical, 2_050).await.unwrap(),
                Audit::Mismatch(Mismatch {
                    detected_at: 2_050,
                    ..found.clone()
                })
            );
            // Two endpoints agreed: it is recorded with the evidence, and the note of the suspicion goes with it.
            j.note_suspicion(&Suspected {
                mismatch: found.clone(),
                checks: 1,
                endpoints: 2,
                answered: 1,
            })
            .await
            .unwrap();
            assert!(
                j.confirm_finality_mismatch(
                    &found,
                    "finality:recovery:confirmed",
                    &serde_json::json!({"agreeing": 2})
                )
                .await
                .unwrap()
            );
            assert_eq!(j.suspicion_note().await.unwrap(), None);
            assert_eq!(
                j.meta("finality:recovery:confirmed")
                    .await
                    .unwrap()
                    .as_deref(),
                Some(r#"{"agreeing":2}"#)
            );
            assert_eq!(j.finality_mismatch().await.unwrap(), Some(found.clone()));
            // The audit does nothing from there on, even when the chain agrees again; the first record stays, over a restart.
            j.pool.close().await;
            let j = Journal::open(&path, "scope").await.unwrap();
            let agreeing = [(50, "0xb50".to_string()), (51, "0xb51".to_string())];
            assert_eq!(
                j.audit_marks(&agreeing, 2_100).await.unwrap(),
                Audit::Stopped(found.clone())
            );
            assert_eq!(marks(&j).await.len(), 3);
            assert_eq!(count(&j, "finalized_receipts").await, 0);
            let later = Mismatch {
                detected_at: 2_200,
                ..found.clone()
            };
            assert!(!j.record_finality_mismatch(&later).await.unwrap());
            assert!(
                !j.confirm_finality_mismatch(
                    &later,
                    "finality:recovery:confirmed",
                    &serde_json::json!({})
                )
                .await
                .unwrap()
            );
            assert_eq!(
                j.meta("finality:recovery:confirmed")
                    .await
                    .unwrap()
                    .as_deref(),
                Some(r#"{"agreeing":2}"#),
                "the evidence is the first record's"
            );
            assert_eq!(j.finality_mismatch().await.unwrap(), Some(found));
            // The record is a JSON object that a later release reads field by field.
            let saved: serde_json::Value =
                serde_json::from_str(&j.meta(MISMATCH_KEY).await.unwrap().unwrap()).unwrap();
            assert_eq!(
                saved,
                serde_json::json!({"kind":"sign","number":51,"reference":"0xtx2","expected":"0xb51","actual":"0xreplaced","detected_at":2000})
            );
            j.pool.close().await;
        }

        #[tokio::test]
        async fn a_mark_that_contradicts_a_finalized_record_is_a_mismatch_too() {
            let dir = tempfile::tempdir().unwrap();
            let j = Journal::open(&dir.path().join("contradiction.sqlite"), "scope")
                .await
                .unwrap();
            // A receipt that is final already, in another block than the mark says. The mismatch is of the block the
            // record names, which is what the endpoints are asked about.
            j.finalized_receipt("0xtx1", 49, "0xb49", 1).await.unwrap();
            put(&j, &mark(MarkKind::Receipt, 50, "0xb50", "0xtx1")).await;
            let audit = j.audit_marks(&[(50, "0xb50".into())], 2_000).await.unwrap();
            assert!(
                matches!(&audit, Audit::Mismatch(found) if found.kind == "finalized_receipt" && found.number == 49 && found.expected == "0xb49" && found.reference == "0xtx1"),
                "{audit:?}"
            );
            assert_eq!(marks(&j).await.len(), 1);
            assert_eq!(
                j.meta("finalized_checkpoint").await.unwrap(),
                Some(serde_json::to_string(&(49, "0xb49")).unwrap())
            );
            // A finalized checkpoint that has the block under another hash says the same.
            let k = Journal::open(&dir.path().join("checkpoint.sqlite"), "scope")
                .await
                .unwrap();
            k.finalized_checkpoint(50, "0xwas").await.unwrap();
            put(&k, &mark(MarkKind::Sign, 50, "0xb50", "0xtx")).await;
            let audit = k.audit_marks(&[(50, "0xb50".into())], 2_000).await.unwrap();
            assert!(
                matches!(&audit, Audit::Mismatch(found) if found.kind == "finalized_checkpoint" && found.expected == "0xwas" && found.actual == "0xb50"),
                "{audit:?}"
            );
            assert_eq!(marks(&k).await.len(), 1);
            // The recovery from either drops the record it named, and the next audit records the chain's.
            for (journal, audit) in [(&j, (50, "0xb50")), (&k, (50, "0xb50"))] {
                let Audit::Mismatch(found) = journal
                    .audit_marks(&[(audit.0, audit.1.into())], 2_000)
                    .await
                    .unwrap()
                else {
                    panic!("a mismatch");
                };
                assert!(journal.record_finality_mismatch(&found).await.unwrap());
                journal
                    .clear_finality_incident(&serde_json::json!({}))
                    .await
                    .unwrap();
                assert!(matches!(
                    journal
                        .audit_marks(&[(audit.0, audit.1.into())], 2_000)
                        .await
                        .unwrap(),
                    Audit::Audited { .. }
                ));
                assert_eq!(
                    journal.meta("finalized_checkpoint").await.unwrap(),
                    Some(serde_json::to_string(&audit).unwrap())
                );
            }
            let receipts: Vec<(String, i64, String)> =
                sqlx::query_as("SELECT hash,block_number,block_hash FROM finalized_receipts")
                    .fetch_all(&j.pool)
                    .await
                    .unwrap();
            assert_eq!(receipts, vec![("0xtx1".into(), 50, "0xb50".into())]);
            j.pool.close().await;
            k.pool.close().await;
        }

        #[tokio::test]
        async fn a_failed_audit_commit_leaves_every_mark_in_place() {
            let dir = tempfile::tempdir().unwrap();
            let j = Journal::open(&dir.path().join("audit-crash.sqlite"), "scope")
                .await
                .unwrap();
            put(&j, &mark(MarkKind::Receipt, 50, "0xb50", "0xtx1")).await;
            put(&j, &mark(MarkKind::Receipt, 51, "0xb51", "0xtx2")).await;
            // The last write of the audit is the deletion of the marks: the receipts and the checkpoint go back too.
            crash_on(&j, "CREATE TRIGGER refuse_clear BEFORE DELETE ON soft_marks BEGIN SELECT RAISE(ABORT,'injected crash'); END;").await;
            assert!(
                j.audit_marks(&[(50, "0xb50".into()), (51, "0xb51".into())], 2_000)
                    .await
                    .is_err()
            );
            assert_eq!(marks(&j).await.len(), 2);
            assert_eq!(count(&j, "finalized_receipts").await, 0);
            assert_eq!(j.meta("finalized_checkpoint").await.unwrap(), None);
            lift(&j, "refuse_clear").await;
            assert!(matches!(
                j.audit_marks(&[(50, "0xb50".into()), (51, "0xb51".into())], 2_000)
                    .await
                    .unwrap(),
                Audit::Audited { .. }
            ));
            j.pool.close().await;
        }

        #[tokio::test]
        async fn signed_bytes_survive_compaction_in_soft_mode_until_the_audit_has_made_the_receipt_final()
         {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("compact-soft.sqlite");
            let j = Journal::open(&path, "scope").await.unwrap();
            j.discovered("1", 100, "3").await.unwrap();
            j.prepared("1", "proof", "call").await.unwrap();
            j.discovered("2", 100, "3").await.unwrap();
            // Request 1 is settled by a soft receipt. Request 2's nonce was found consumed without a receipt.
            insert_attempt(&j, "1", 7, "0xtx1").await;
            j.resolve_nonce_job_marked(
                7,
                "1",
                "served",
                Some(&mark(MarkKind::Receipt, 50, "0xb50", "0xtx1")),
            )
            .await
            .unwrap();
            insert_attempt(&j, "2", 8, "0xtx2").await;
            j.resolve_nonce_job_marked(
                8,
                "2",
                "expired",
                Some(&mark(MarkKind::Nonce, 51, "0xb51", "8")),
            )
            .await
            .unwrap();
            async fn bodies(j: &Journal) -> Vec<(String, String, String)> {
                sqlx::query_as("SELECT hash,raw,payload FROM txs ORDER BY id")
                    .fetch_all(&j.pool)
                    .await
                    .unwrap()
            }
            let kept = |hash: &str| {
                (
                    hash.to_owned(),
                    "signed-bytes".to_owned(),
                    "payload".to_owned(),
                )
            };
            let blanked = |hash: &str| (hash.to_owned(), String::new(), String::new());

            // Resolved but not audited: the soft rule keeps the bytes of both, and the jobs' proof goes as ever.
            j.compact_history_for(100, FinalityMode::Soft)
                .await
                .unwrap();
            assert_eq!(bodies(&j).await, [kept("0xtx1"), kept("0xtx2")]);
            assert!(j.job("1").await.unwrap().unwrap().proof.is_none());
            // The audit makes request 1's receipt a finalized one; the nonce mark has no receipt to become.
            j.audit_marks(&[(50, "0xb50".into()), (51, "0xb51".into())], 2_000)
                .await
                .unwrap();
            j.pool.close().await;
            let j = Journal::open(&path, "scope").await.unwrap();
            j.compact_history_for(161, FinalityMode::Soft)
                .await
                .unwrap();
            assert_eq!(bodies(&j).await, [blanked("0xtx1"), kept("0xtx2")]);
            // Later passes change nothing: a transaction that never had a receipt keeps its bytes.
            j.compact_history_for(300, FinalityMode::Soft)
                .await
                .unwrap();
            assert_eq!(bodies(&j).await, [blanked("0xtx1"), kept("0xtx2")]);
            // The finalized rule is the one of 0.4.1: it blanks every resolved transaction, audited or not.
            j.compact_history(400).await.unwrap();
            assert_eq!(bodies(&j).await, [blanked("0xtx1"), blanked("0xtx2")]);
            j.pool.close().await;
        }

        #[tokio::test]
        async fn the_audit_lag_is_measured_from_the_oldest_mark_and_the_head_is_never_behind() {
            let dir = tempfile::tempdir().unwrap();
            let j = Journal::open(&dir.path().join("lag.sqlite"), "scope")
                .await
                .unwrap();
            assert_eq!(j.oldest_mark_created().await.unwrap(), None);
            // A head mark of long ago is rewritten by the next tick, so it is no backlog.
            j.soft_decision(10, "0xh10", 5).await.unwrap();
            assert_eq!(j.oldest_mark_created().await.unwrap(), None);
            put(
                &j,
                &Mark {
                    created: 700,
                    ..mark(MarkKind::Sign, 11, "0xh11", "0xa")
                },
            )
            .await;
            put(
                &j,
                &Mark {
                    created: 400,
                    ..mark(MarkKind::Receipt, 12, "0xh12", "0xb")
                },
            )
            .await;
            put(
                &j,
                &Mark {
                    created: 900,
                    ..mark(MarkKind::Nonce, 13, "0xh13", "3")
                },
            )
            .await;
            assert_eq!(j.oldest_mark_created().await.unwrap(), Some(400));
            j.audit_marks(&[(12, "0xh12".into())], 1_000).await.unwrap();
            assert_eq!(j.oldest_mark_created().await.unwrap(), Some(700));
            j.pool.close().await;
        }

        #[tokio::test]
        async fn the_soft_marks_table_is_an_addition_that_older_journals_gain_and_older_releases_ignore()
         {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("additive.sqlite");
            let j = Journal::open(&path, "scope").await.unwrap();
            // The columns, and the key that makes a mark the same mark again.
            let columns: Vec<(String, String, i64)> = sqlx::query_as(
                "SELECT name,type,pk FROM pragma_table_info('soft_marks') ORDER BY cid",
            )
            .fetch_all(&j.pool)
            .await
            .unwrap();
            assert_eq!(
                columns,
                [
                    ("number".into(), "INTEGER".into(), 1),
                    ("hash".into(), "TEXT".into(), 0),
                    ("kind".into(), "TEXT".into(), 2),
                    ("ref".into(), "TEXT".into(), 3),
                    ("created".into(), "INTEGER".into(), 0),
                    ("status".into(), "INTEGER".into(), 0),
                ]
            );
            // A journal of the release before has no such table, and gains it, keeping everything it held.
            j.discovered("1", 100, "2").await.unwrap();
            j.prepared("1", "proof", "call").await.unwrap();
            insert_attempt(&j, "1", 7, "0xtx").await;
            sqlx::raw_sql("DROP TABLE soft_marks")
                .execute(&j.pool)
                .await
                .unwrap();
            j.pool.close().await;
            let j = Journal::open(&path, "scope").await.unwrap();
            assert_eq!(count(&j, "soft_marks").await, 0);
            assert_eq!(j.unresolved().await.unwrap()[0].raw, "signed-bytes");
            // A journal with marks opens again as it is: nothing here changes a column or a key of another table, and
            // the statements of 0.4.1 name none of this one.
            put(&j, &mark(MarkKind::Sign, 5, "0xh5", "0xtx")).await;
            j.pool.close().await;
            let j = Journal::open(&path, "scope").await.unwrap();
            assert_eq!(marks(&j).await, [row("sign", 5, "0xh5", "0xtx")]);
            assert_eq!(
                j.pending().await.unwrap()[0].proof.as_deref(),
                Some("proof")
            );
            j.pool.close().await;
        }
    }

    /// Soft finality: the incident that a mismatch on record is, an operator's acknowledgement of it, and what the
    /// journal does to put the nonce lane back (keeper task C4).
    mod finality_incident {
        use super::*;
        fn mismatch(kind: &str, number: u64) -> Mismatch {
            Mismatch {
                kind: kind.into(),
                number,
                reference: "0xtx".into(),
                expected: "0xold".into(),
                actual: "0xnew".into(),
                detected_at: 1_000,
            }
        }
        fn mark(kind: MarkKind, number: u64, hash: &str, reference: &str) -> Mark {
            Mark {
                kind,
                number,
                hash: hash.into(),
                reference: reference.into(),
                created: 1_000,
                status: (kind == MarkKind::Receipt).then_some(1),
            }
        }
        async fn put(j: &Journal, marks: &[Mark]) {
            let mut tx = j.pool.begin().await.unwrap();
            for mark in marks {
                write_mark(&mut tx, mark).await.unwrap();
            }
            tx.commit().await.unwrap();
        }
        /// A signed attempt (state `state`) with bytes `raw`, of kind `kind` for `job`.
        async fn attempt(
            j: &Journal,
            job: &str,
            kind: &str,
            nonce: i64,
            hash: &str,
            raw: &str,
            state: &str,
        ) {
            sqlx::query("INSERT INTO txs(job,nonce,hash,raw,kind,fee,gas,priority,payload,created,broadcast,state) VALUES(?,?,?,?,?,'1',1,'1','payload',5,7,?)")
                .bind(job).bind(nonce).bind(hash).bind(raw).bind(kind).bind(state)
                .execute(&j.pool).await.unwrap();
        }
        async fn states(j: &Journal, sql: &'static str) -> Vec<String> {
            sqlx::query_scalar(sql).fetch_all(&j.pool).await.unwrap()
        }
        async fn tx_states(j: &Journal) -> Vec<String> {
            states(
                j,
                "SELECT nonce||':'||hash||'='||state FROM txs ORDER BY id",
            )
            .await
        }
        async fn job_states(j: &Journal) -> Vec<String> {
            states(
                j,
                "SELECT id||'='||state FROM jobs ORDER BY CAST(id AS INTEGER)",
            )
            .await
        }

        #[test]
        fn a_mismatch_has_an_id_that_names_all_of_it() {
            let base = mismatch("receipt", 10);
            let id = base.id();
            assert_eq!(id.len(), 12);
            assert!(
                id.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            );
            assert_eq!(id, base.clone().id(), "the same record has the same id");
            for other in [
                Mismatch {
                    kind: "head".into(),
                    ..base.clone()
                },
                Mismatch {
                    number: 11,
                    ..base.clone()
                },
                Mismatch {
                    reference: "0xother".into(),
                    ..base.clone()
                },
                Mismatch {
                    expected: "0xe".into(),
                    ..base.clone()
                },
                Mismatch {
                    actual: "0xa".into(),
                    ..base.clone()
                },
                Mismatch {
                    detected_at: 1_001,
                    ..base.clone()
                },
            ] {
                assert_ne!(other.id(), id, "{other:?}");
            }
        }

        #[tokio::test]
        async fn a_mismatch_on_record_is_recovered_from_and_an_acknowledgement_is_an_operators_word_on_it()
         {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("incident.sqlite");
            let j = Journal::open(&path, "scope").await.unwrap();
            assert_eq!(j.finality_state().await.unwrap(), FinalityState::Clear);
            assert!(
                j.acknowledge_finality("000000000000", 5)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("nothing to acknowledge")
            );

            let first = mismatch("soft_checkpoint", 40);
            assert!(j.record_finality_mismatch(&first).await.unwrap());
            // The first one stays the record, and a second does not replace it. The keeper recovers from it without
            // anyone's word.
            assert!(
                !j.record_finality_mismatch(&mismatch("receipt", 30))
                    .await
                    .unwrap()
            );
            assert_eq!(
                j.finality_state().await.unwrap(),
                FinalityState::Recovering(first.clone(), None)
            );

            // Only the id of the record on record acknowledges it, and a wrong one changes nothing.
            let error = j
                .acknowledge_finality(&mismatch("receipt", 30).id(), 5)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains(&first.id()), "{error}");
            assert_eq!(j.meta(ACK_KEY).await.unwrap(), None);

            // The id as an operator types it: in capitals and with a space.
            let ack = j
                .acknowledge_finality(&format!(" {} ", first.id().to_uppercase()), 77)
                .await
                .unwrap();
            assert_eq!(
                ack,
                Acknowledgement {
                    id: first.id(),
                    acknowledged_at: 77
                }
            );
            // Again is the same acknowledgement, with its first time.
            assert_eq!(j.acknowledge_finality(&first.id(), 99).await.unwrap(), ack);
            // Durable: a restart finds it beside the record.
            j.pool.close().await;
            let j = Journal::open(&path, "scope").await.unwrap();
            assert_eq!(
                j.finality_state().await.unwrap(),
                FinalityState::Recovering(first.clone(), Some(ack.clone()))
            );

            // An acknowledgement of another incident, or one that cannot be read, is none.
            j.set_meta(
                ACK_KEY,
                &serde_json::to_string(&Acknowledgement {
                    id: "ffffffffffff".into(),
                    acknowledged_at: 1,
                })
                .unwrap(),
            )
            .await
            .unwrap();
            assert_eq!(
                j.finality_state().await.unwrap(),
                FinalityState::Recovering(first.clone(), None)
            );
            j.set_meta(ACK_KEY, "not json").await.unwrap();
            assert_eq!(
                j.finality_state().await.unwrap(),
                FinalityState::Recovering(first.clone(), None)
            );
            // Acknowledging replaces both.
            assert_eq!(
                j.acknowledge_finality(&first.id(), 123)
                    .await
                    .unwrap()
                    .acknowledged_at,
                123
            );
            assert!(matches!(
                j.finality_state().await.unwrap(),
                FinalityState::Recovering(_, Some(_))
            ));
            j.pool.close().await;
        }

        #[tokio::test]
        async fn acknowledging_a_suspected_mismatch_records_it_in_place_of_a_second_endpoint() {
            let dir = tempfile::tempdir().unwrap();
            let j = Journal::open(&dir.path().join("suspected.sqlite"), "scope")
                .await
                .unwrap();
            let suspected = Suspected {
                mismatch: mismatch("soft_checkpoint", 40),
                checks: 3,
                endpoints: 1,
                answered: 1,
            };
            j.note_suspicion(&suspected).await.unwrap();
            assert_eq!(j.suspicion_note().await.unwrap(), Some(suspected.clone()));
            // The note is no record, and halts nothing that reads the journal.
            assert_eq!(j.finality_state().await.unwrap(), FinalityState::Clear);
            assert_eq!(j.finality_mismatch().await.unwrap(), None);
            // Another id does nothing, and says which id is the one.
            let error = j
                .acknowledge_finality(&mismatch("receipt", 30).id(), 5)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains(&suspected.mismatch.id()), "{error}");
            assert_eq!(j.finality_mismatch().await.unwrap(), None);
            // Its own id records it and acknowledges it, and the note goes, in one commit.
            let ack = j
                .acknowledge_finality(&suspected.mismatch.id(), 9)
                .await
                .unwrap();
            assert_eq!(ack.id, suspected.mismatch.id());
            assert_eq!(
                j.finality_state().await.unwrap(),
                FinalityState::Recovering(suspected.mismatch.clone(), Some(ack))
            );
            assert_eq!(j.suspicion_note().await.unwrap(), None);
            // A note that is left over beside a record is gone with the incident.
            j.note_suspicion(&suspected).await.unwrap();
            j.clear_finality_incident(&serde_json::json!({}))
                .await
                .unwrap();
            assert_eq!(j.suspicion_note().await.unwrap(), None);
            j.pool.close().await;
        }

        #[tokio::test]
        async fn the_rebase_moves_the_checkpoint_and_the_head_mark_to_the_chain_as_it_is() {
            let dir = tempfile::tempdir().unwrap();
            let j = Journal::open(&dir.path().join("rebase.sqlite"), "scope")
                .await
                .unwrap();
            j.soft_decision(90, "0xh90", 10).await.unwrap();
            put(&j, &[mark(MarkKind::Sign, 80, "0xh80", "0xtx")]).await;
            // The checkpoint never moves down, and holds a block against another hash: the rebase does both.
            assert!(j.soft_decision(70, "0xh70", 11).await.is_ok());
            assert_eq!(
                j.meta("soft_checkpoint").await.unwrap().as_deref(),
                Some(r#"[90,"0xh90"]"#)
            );
            j.rebase_soft_decision(70, "0xnew70", 20).await.unwrap();
            assert_eq!(
                j.meta("soft_checkpoint").await.unwrap().as_deref(),
                Some(r#"[70,"0xnew70"]"#)
            );
            let marks = j.soft_marks().await.unwrap();
            assert_eq!(
                marks
                    .iter()
                    .map(|m| (m.kind.name(), m.number, m.hash.as_str(), m.created))
                    .collect::<Vec<_>>(),
                [("head", 70, "0xnew70", 20), ("sign", 80, "0xh80", 1_000)]
            );
            // The same block under another hash.
            j.rebase_soft_decision(70, "0xagain70", 21).await.unwrap();
            assert_eq!(
                j.meta("soft_checkpoint").await.unwrap().as_deref(),
                Some(r#"[70,"0xagain70"]"#)
            );
            assert_eq!(j.soft_marks().await.unwrap().len(), 2);
            // Ordinary ticks go on from there.
            j.soft_decision(71, "0xh71", 22).await.unwrap();
            assert_eq!(
                j.meta("soft_checkpoint").await.unwrap().as_deref(),
                Some(r#"[71,"0xh71"]"#)
            );
            j.pool.close().await;
        }

        #[tokio::test]
        async fn the_marks_are_read_by_page_and_replaced_by_the_recovery_alone() {
            let dir = tempfile::tempdir().unwrap();
            let j = Journal::open(&dir.path().join("pages.sqlite"), "scope")
                .await
                .unwrap();
            put(
                &j,
                &[
                    mark(MarkKind::Sign, 10, "0xh10", "0xa"),
                    mark(MarkKind::Receipt, 10, "0xh10", "0xa"),
                    mark(MarkKind::Sign, 12, "0xh12", "0xb"),
                    mark(MarkKind::Nonce, 15, "0xh15", "3"),
                    mark(MarkKind::Sign, 20, "0xh20", "0xc"),
                ],
            )
            .await;
            assert_eq!(j.mark_numbers_from(0, 10).await.unwrap(), [10, 12, 15, 20]);
            assert_eq!(j.mark_numbers_from(11, 2).await.unwrap(), [12, 15]);
            assert_eq!(j.mark_numbers_from(21, 2).await.unwrap(), Vec::<u64>::new());
            let page = j.marks_between(10, 12).await.unwrap();
            assert_eq!(
                page.iter()
                    .map(|m| (m.number, m.kind.name(), m.reference.as_str()))
                    .collect::<Vec<_>>(),
                [
                    (10, "receipt", "0xa"),
                    (10, "sign", "0xa"),
                    (12, "sign", "0xb")
                ]
            );
            // Stale marks go and a fresh one takes their place, together or not at all.
            let stale = [page[0].clone(), page[1].clone()];
            let fresh = mark(MarkKind::Receipt, 11, "0xnew11", "0xa");
            j.replace_marks(&stale, Some(&fresh)).await.unwrap();
            assert_eq!(
                j.soft_marks()
                    .await
                    .unwrap()
                    .iter()
                    .map(|m| (m.number, m.kind.name(), m.hash.as_str()))
                    .collect::<Vec<_>>(),
                [
                    (11, "receipt", "0xnew11"),
                    (12, "sign", "0xh12"),
                    (15, "nonce", "0xh15"),
                    (20, "sign", "0xh20")
                ]
            );
            // Without a fresh mark they are only deleted. A mark that is not there is no error.
            j.replace_marks(&[fresh.clone(), page[2].clone()], None)
                .await
                .unwrap();
            assert_eq!(j.soft_marks().await.unwrap().len(), 2);
            // A fresh mark that conflicts with one that stays leaves the stale ones where they were...
            let conflicting = mark(MarkKind::Nonce, 15, "0xother", "3");
            let kept = j.soft_marks().await.unwrap();
            assert!(
                j.replace_marks(&[kept[1].clone()], Some(&conflicting))
                    .await
                    .is_err()
            );
            assert_eq!(j.soft_marks().await.unwrap(), kept);
            // ...and one for a key whose stale mark goes with it takes the key.
            j.replace_marks(&[kept[0].clone()], Some(&conflicting))
                .await
                .unwrap();
            let after = j.soft_marks().await.unwrap();
            assert_eq!(
                after
                    .iter()
                    .map(|m| (m.number, m.hash.as_str()))
                    .collect::<Vec<_>>(),
                [(15, "0xother"), (20, "0xh20")]
            );
            j.pool.close().await;
        }

        #[tokio::test]
        async fn reopening_a_nonce_takes_its_attempts_and_what_they_served_back_into_the_lane() {
            let dir = tempfile::tempdir().unwrap();
            let j = Journal::open(&dir.path().join("reopen.sqlite"), "scope")
                .await
                .unwrap();
            for id in ["1", "2", "3"] {
                j.discovered(id, 1_000, "4").await.unwrap();
            }
            // Nonce 5 served request 1 with a fee replacement; nonce 6 is a batch of requests 2 and 3; nonce 7 is the
            // lane the keeper has open now (request 4 would be next).
            attempt(&j, "1", "fulfill", 5, "0xa1", "0xraw1", "resolved").await;
            attempt(&j, "1", "fulfill", 5, "0xa2", "0xraw2", "resolved").await;
            let batch = batch_key(&ids(&["2", "3"])).unwrap();
            attempt(&j, &batch, "fulfill_batch", 6, "0xb1", "0xrawb", "resolved").await;
            for (position, id) in ["2", "3"].iter().enumerate() {
                sqlx::query("INSERT INTO batch_members(job,request_id,position) VALUES(?,?,?)")
                    .bind(&batch)
                    .bind(id)
                    .bind(position as i64)
                    .execute(&j.pool)
                    .await
                    .unwrap();
            }
            j.discovered("4", 1_000, "5").await.unwrap();
            attempt(&j, "4", "fulfill", 7, "0xc1", "0xrawc", "submitted").await;
            for (id, state) in [
                ("1", "served"),
                ("2", "served"),
                ("3", "served"),
                ("4", "submitted"),
            ] {
                j.state(id, state).await.unwrap();
            }
            put(
                &j,
                &[
                    mark(MarkKind::Sign, 20, "0xh20", "0xa1"),
                    mark(MarkKind::Receipt, 21, "0xh21", "0xa2"),
                    mark(MarkKind::Receipt, 22, "0xh22", "0xb1"),
                    mark(MarkKind::Nonce, 23, "0xh23", "5"),
                    mark(MarkKind::Nonce, 23, "0xh23", "6"),
                ],
            )
            .await;
            sqlx::query("INSERT INTO meta(key,value) VALUES('nonce_started:5','1')")
                .execute(&j.pool)
                .await
                .unwrap();

            // Nonce 5: both attempts are submitted again, stamped 500 and unbroadcast, request 1 is submitted, and the
            // lane that was open is parked, whatever its state was. The marks of the receipt and of the nonce go; the
            // sign mark, which says where the first attempt was signed, stays. Nothing else changed.
            assert_eq!(j.reopen_nonce(5, 500, 5_000).await.unwrap(), Reopened::Lane);
            assert_eq!(
                tx_states(&j).await,
                [
                    "5:0xa1=submitted",
                    "5:0xa2=submitted",
                    "6:0xb1=resolved",
                    "7:0xc1=parked_submitted"
                ]
            );
            let stamped: Vec<(i64, i64)> =
                sqlx::query_as("SELECT created,broadcast FROM txs WHERE nonce=5 ORDER BY id")
                    .fetch_all(&j.pool)
                    .await
                    .unwrap();
            assert_eq!(stamped, [(500, 0), (500, 0)]);
            assert_eq!(
                job_states(&j).await,
                ["1=submitted", "2=served", "3=served", "4=submitted"]
            );
            let left = j.soft_marks().await.unwrap();
            assert_eq!(
                left.iter()
                    .map(|m| (m.kind.name(), m.number, m.reference.as_str()))
                    .collect::<Vec<_>>(),
                [
                    ("sign", 20, "0xa1"),
                    ("receipt", 22, "0xb1"),
                    ("nonce", 23, "6")
                ]
            );
            assert_eq!(
                j.meta("nonce_started:5").await.unwrap().as_deref(),
                Some("5000"),
                "the age of the lane is wall-clock, the stamp of the attempts is the chain's"
            );
            // It is the only lane, and it is the one the keeper reconciles.
            let lane = j.unresolved().await.unwrap();
            assert_eq!(lane.iter().map(|a| a.nonce).collect::<Vec<_>>(), [5, 5]);
            assert_eq!(j.parked_lanes().await.unwrap(), 1);

            // The batch is reopened the same way once the lane is settled again: its members go back to submitted.
            sqlx::query("UPDATE txs SET state='resolved' WHERE nonce=5")
                .execute(&j.pool)
                .await
                .unwrap();
            assert_eq!(j.reopen_nonce(6, 600, 6_000).await.unwrap(), Reopened::Lane);
            assert_eq!(
                job_states(&j).await,
                ["1=submitted", "2=submitted", "3=submitted", "4=submitted"]
            );
            assert_eq!(j.parked_lanes().await.unwrap(), 1);
            assert_eq!(j.soft_marks().await.unwrap().len(), 1, "only the sign mark");

            // The parked lane comes back as it was.
            assert_eq!(j.unpark_lanes().await.unwrap(), 1);
            assert_eq!(j.unpark_lanes().await.unwrap(), 0);
            assert_eq!(
                tx_states(&j).await,
                [
                    "5:0xa1=resolved",
                    "5:0xa2=resolved",
                    "6:0xb1=submitted",
                    "7:0xc1=submitted"
                ]
            );
            j.pool.close().await;
        }

        #[tokio::test]
        async fn reopening_an_epoch_commit_and_a_nonce_without_bytes() {
            let dir = tempfile::tempdir().unwrap();
            let j = Journal::open(&dir.path().join("reopen-epoch.sqlite"), "scope")
                .await
                .unwrap();
            sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start,state) VALUES('epoch:1','r','c',1,100,'committed')")
                .execute(&j.pool)
                .await
                .unwrap();
            attempt(&j, "epoch:1", "epoch", 0, "0xe1", "0xrawe", "resolved").await;
            assert_eq!(j.reopen_nonce(0, 9, 90).await.unwrap(), Reopened::Lane);
            assert_eq!(
                states(&j, "SELECT state FROM epoch_work").await,
                ["submitted"]
            );
            assert_eq!(tx_states(&j).await, ["0:0xe1=submitted"]);

            // A nonce whose last attempt has lost its bytes, and a nonce the journal never signed, cannot be reopened;
            // nothing is changed for either, and no lane is parked for them.
            j.discovered("1", 1_000, "2").await.unwrap();
            attempt(&j, "1", "fulfill", 1, "0xf1", "", "resolved").await;
            attempt(&j, "1", "fulfill", 2, "0xg1", "0xrawg", "resolved").await;
            attempt(&j, "1", "cancel", 2, "0xg2", "", "resolved").await;
            for nonce in [1, 2, 9] {
                assert_eq!(
                    j.reopen_nonce(nonce, 10, 100).await.unwrap(),
                    Reopened::NoBytes
                );
            }
            assert_eq!(
                tx_states(&j).await,
                [
                    "0:0xe1=submitted",
                    "1:0xf1=resolved",
                    "2:0xg1=resolved",
                    "2:0xg2=resolved"
                ]
            );
            assert_eq!(j.parked_lanes().await.unwrap(), 0);
            assert_eq!(j.meta("nonce_started:1").await.unwrap(), None);
            j.pool.close().await;
        }

        #[tokio::test]
        async fn reopening_is_one_commit() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("reopen-atomic.sqlite");
            let j = Journal::open(&path, "scope").await.unwrap();
            j.discovered("1", 1_000, "2").await.unwrap();
            j.state("1", "served").await.unwrap();
            attempt(&j, "1", "fulfill", 3, "0xa1", "0xraw", "resolved").await;
            attempt(&j, "1", "fulfill", 4, "0xb1", "0xrawb", "submitted").await;
            put(&j, &[mark(MarkKind::Receipt, 8, "0xh8", "0xa1")]).await;
            // The last statement of the commit fails: nothing at all changed.
            sqlx::raw_sql("CREATE TRIGGER refuse_stamp BEFORE INSERT ON meta WHEN NEW.key LIKE 'nonce_started:%' BEGIN SELECT RAISE(ABORT,'injected crash'); END;")
                .execute(&j.pool)
                .await
                .unwrap();
            assert!(j.reopen_nonce(3, 50, 500).await.is_err());
            j.pool.close().await;
            let j = Journal::open(&path, "scope").await.unwrap();
            assert_eq!(tx_states(&j).await, ["3:0xa1=resolved", "4:0xb1=submitted"]);
            assert_eq!(job_states(&j).await, ["1=served"]);
            assert_eq!(j.soft_marks().await.unwrap().len(), 1);
            j.pool.close().await;
        }

        #[tokio::test]
        async fn the_incident_adds_keys_and_states_and_changes_no_table_index_or_trigger() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("additive.sqlite");
            let j = Journal::open(&path, "scope").await.unwrap();
            let schema = |j: &Journal| {
                let pool = j.pool.clone();
                async move {
                    sqlx::query_as::<_, (String, String, Option<String>)>(
                        "SELECT type,name,sql FROM sqlite_master ORDER BY type,name",
                    )
                    .fetch_all(&pool)
                    .await
                    .unwrap()
                }
            };
            let before = schema(&j).await;
            // Everything the incident does to a journal, once.
            j.discovered("1", 1_000, "2").await.unwrap();
            attempt(&j, "1", "fulfill", 0, "0xa", "0xraw", "resolved").await;
            attempt(&j, "1", "fulfill", 1, "0xb", "0xrawb", "submitted").await;
            put(&j, &[mark(MarkKind::Receipt, 5, "0xh5", "0xa")]).await;
            let found = mismatch("receipt", 5);
            j.record_finality_mismatch(&found).await.unwrap();
            j.acknowledge_finality(&found.id(), 7).await.unwrap();
            j.rebase_soft_decision(9, "0xh9", 8).await.unwrap();
            assert_eq!(j.reopen_nonce(0, 9, 9).await.unwrap(), Reopened::Lane);
            j.replace_marks(&[], Some(&mark(MarkKind::Nonce, 9, "0xh9", "0")))
                .await
                .unwrap();
            j.unpark_lanes().await.unwrap();
            j.clear_finality_incident(&serde_json::json!({}))
                .await
                .unwrap();
            assert_eq!(schema(&j).await, before);
            // The journal is the schema the release before opens: version 1, and the states of an attempt it does not
            // know are only ever those of a recovery that has not finished.
            assert_eq!(
                j.meta("schema_version").await.unwrap().as_deref(),
                Some("1")
            );
            let states: Vec<String> =
                sqlx::query_scalar("SELECT DISTINCT state FROM txs ORDER BY state")
                    .fetch_all(&j.pool)
                    .await
                    .unwrap();
            assert!(
                states
                    .iter()
                    .all(|state| ["resolved", "signed", "submitted"].contains(&state.as_str())),
                "{states:?}"
            );
            j.pool.close().await;
        }

        #[tokio::test]
        async fn clearing_the_incident_is_one_commit_that_keeps_the_last_recovery() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("clear.sqlite");
            let j = Journal::open(&path, "scope").await.unwrap();
            let found = mismatch("receipt", 40);
            j.record_finality_mismatch(&found).await.unwrap();
            j.acknowledge_finality(&found.id(), 5).await.unwrap();
            j.set_meta(ALERTED_KEY, &found.id()).await.unwrap();
            j.set_meta("finality:recovery:scan", "41").await.unwrap();
            j.set_meta("finality:recovery:jobs", r#"[1,"2"]"#)
                .await
                .unwrap();
            j.set_meta("finality:unrelated", "kept").await.unwrap();
            j.discovered("1", 1_000, "2").await.unwrap();
            attempt(&j, "1", "fulfill", 3, "0xa1", "0xraw", "parked_submitted").await;

            // A failure inside the commit leaves the incident open, parked attempts included.
            sqlx::raw_sql("CREATE TRIGGER refuse_last BEFORE INSERT ON meta WHEN NEW.key='finality:last_recovery' BEGIN SELECT RAISE(ABORT,'injected crash'); END;")
                .execute(&j.pool)
                .await
                .unwrap();
            let summary = serde_json::json!({"mismatch": found.id(), "finished_at": 9});
            assert!(j.clear_finality_incident(&summary).await.is_err());
            assert!(matches!(
                j.finality_state().await.unwrap(),
                FinalityState::Recovering(..)
            ));
            assert_eq!(tx_states(&j).await, ["3:0xa1=parked_submitted"]);
            sqlx::raw_sql("DROP TRIGGER refuse_last")
                .execute(&j.pool)
                .await
                .unwrap();

            j.clear_finality_incident(&summary).await.unwrap();
            j.pool.close().await;
            let j = Journal::open(&path, "scope").await.unwrap();
            assert_eq!(j.finality_state().await.unwrap(), FinalityState::Clear);
            for key in [
                ACK_KEY,
                ALERTED_KEY,
                "finality:recovery:scan",
                "finality:recovery:jobs",
            ] {
                assert_eq!(j.meta(key).await.unwrap(), None, "{key}");
            }
            assert_eq!(
                j.meta("finality:unrelated").await.unwrap().as_deref(),
                Some("kept")
            );
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(
                    &j.meta(LAST_RECOVERY_KEY).await.unwrap().unwrap()
                )
                .unwrap(),
                summary
            );
            assert_eq!(tx_states(&j).await, ["3:0xa1=submitted"]);
            // The next incident is recorded afresh, and replaces the last recovery only when it is cleared.
            assert!(
                j.record_finality_mismatch(&mismatch("head", 90))
                    .await
                    .unwrap()
            );
            assert!(j.meta(LAST_RECOVERY_KEY).await.unwrap().is_some());
            j.pool.close().await;
        }
    }
}
