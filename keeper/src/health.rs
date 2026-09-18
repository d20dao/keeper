use crate::journal::Journal;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub fn now() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs())
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Status {
    pub observed_at: u64,
    pub send_enabled: bool,
    pub healthy: bool,
    pub faults: Vec<String>,
    /// KEEPER_ROLE of the keeper that wrote the observation; absent in observations from earlier releases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// A follower's current view of the primary: whether committer()'s confirmed nonce advanced within its
    /// liveness window. Absent for a primary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_alive: Option<bool>,
}
async fn primary_alive(journal: &Journal) -> Result<Option<bool>> {
    Ok(journal
        .meta("keeper:primary_alive")
        .await?
        .map(|value| value == "true"))
}
/// Per-request preparation evidence: `health:preparing:<request id>` holds when this keeper first tried to prepare
/// that request while it was responsible for it (a primary always is; a follower once it has joined the queue for the
/// request's lane, or once the request reaches the safety age). The keys sort between these two bounds.
const PREPARING: (&str, &str) = ("health:preparing:", "health:preparing;");
/// Preparation stall marker written by earlier releases: one timestamp for all work, set for every request the keeper
/// looked at and cleared only by its own next proof. It is read only to retire it (see `assess`).
const LEGACY_PREPARATION: &str = "health:blocked:preparation";
/// When this keeper last journaled a proof.
const PREPARED_AT: &str = "health:prepared_at";
/// A request this keeper was responsible for that expired unprepared, after waiting at least the progress limit,
/// stays evidence of a preparation stall until a later proof is journaled or for this long after its deadline.
pub const EXPIRED_PREPARATION_EVIDENCE_SECONDS: u64 = 15 * 60;
/// This keeper is responsible for preparing the request and is about to try. Only the first call per request counts,
/// so the wait of one stuck request is never reset by other requests' progress or by the responsibility changing.
pub async fn preparation_waiting(journal: &Journal, request_id: &str, at: u64) -> Result<()> {
    sqlx::query("INSERT OR IGNORE INTO meta(key,value) VALUES(?,?)")
        .bind(format!("{}{request_id}", PREPARING.0))
        .bind(at.to_string())
        .execute(&journal.pool)
        .await?;
    Ok(())
}
/// A proof for this request was journaled. It ends that request's wait, marks progress for requests that already
/// expired unprepared, and retires the legacy marker the way earlier releases cleared it.
pub async fn preparation_progress(journal: &Journal, request_id: &str, at: u64) -> Result<()> {
    let mut tx = journal.pool.begin().await?;
    sqlx::query("DELETE FROM meta WHERE key=? OR key=?")
        .bind(format!("{}{request_id}", PREPARING.0))
        .bind(LEGACY_PREPARATION)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
        .bind(PREPARED_AT)
        .bind(at.to_string())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
/// Whether preparation is stalled at `at`: a request this keeper was responsible for has waited unprepared for
/// `limit` seconds and is still open, or expired unprepared after waiting that long (reported until a later proof
/// or for EXPIRED_PREPARATION_EVIDENCE_SECONDS). Evidence for requests that were prepared, served or refunded,
/// or that expired after a shorter wait, is dropped here. The legacy marker counts only while an open request this
/// keeper is responsible for is waiting, and is removed as soon as none is.
async fn preparation_stalled(journal: &Journal, at: u64, limit: u64) -> Result<bool> {
    let (at, limit) = (i64::try_from(at)?, i64::try_from(limit)?);
    let retention = i64::try_from(EXPIRED_PREPARATION_EVIDENCE_SECONDS)?;
    let mut tx = journal.pool.begin().await?;
    sqlx::query("DELETE FROM meta WHERE key>=? AND key<? AND NOT EXISTS(SELECT 1 FROM jobs WHERE jobs.id=substr(meta.key,18) AND jobs.call IS NULL AND ((jobs.state='pending' AND jobs.deadline>?) OR (jobs.state IN ('pending','expired') AND jobs.deadline<=? AND ?-jobs.deadline<=? AND jobs.deadline-CAST(meta.value AS INTEGER)>=? AND jobs.deadline>=CAST(COALESCE((SELECT value FROM meta AS progress WHERE progress.key=?),'0') AS INTEGER))))")
        .bind(PREPARING.0).bind(PREPARING.1).bind(at).bind(at).bind(at).bind(retention).bind(limit).bind(PREPARED_AT)
        .execute(&mut *tx).await?;
    let (stalled, waiting): (i64, i64) = sqlx::query_as("SELECT COALESCE(MAX(jobs.deadline<=? OR ?-CAST(meta.value AS INTEGER)>=?),0),COALESCE(MAX(jobs.deadline>?),0) FROM meta JOIN jobs ON jobs.id=substr(meta.key,18) WHERE meta.key>=? AND meta.key<?")
        .bind(at).bind(at).bind(limit).bind(at).bind(PREPARING.0).bind(PREPARING.1)
        .fetch_one(&mut *tx).await?;
    let legacy: Option<String> = sqlx::query_scalar("SELECT value FROM meta WHERE key=?")
        .bind(LEGACY_PREPARATION)
        .fetch_optional(&mut *tx)
        .await?;
    let legacy_stalled = match legacy {
        Some(_) if waiting == 0 => {
            sqlx::query("DELETE FROM meta WHERE key=?")
                .bind(LEGACY_PREPARATION)
                .execute(&mut *tx)
                .await?;
            false
        }
        Some(started) => at.saturating_sub(started.parse()?) >= limit,
        None => false,
    };
    tx.commit().await?;
    Ok(stalled != 0 || legacy_stalled)
}
// These observations are independent of jobs: expiry, backlog arrival and restart
// cannot erase evidence that the service could not perform allowed work.
pub async fn blocked(journal: &Journal, stage: &str, at: u64) -> Result<()> {
    sqlx::query("INSERT OR IGNORE INTO meta(key,value) VALUES(?,?)")
        .bind(format!("health:blocked:{stage}"))
        .bind(at.to_string())
        .execute(&journal.pool)
        .await?;
    Ok(())
}
pub async fn recovered(journal: &Journal, stage: &str) -> Result<()> {
    sqlx::query("DELETE FROM meta WHERE key=?")
        .bind(format!("health:blocked:{stage}"))
        .execute(&journal.pool)
        .await?;
    Ok(())
}
/// The transaction wallet may no longer publish in its role. Sending stops; reconciliation, receipts and the
/// journal continue, so nothing this keeper already signed is left unresolved.
pub async fn unauthorized(journal: &Journal, reason: &str) -> Result<()> {
    sqlx::query("INSERT INTO meta(key,value) VALUES('health:unauthorized',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
        .bind(reason).execute(&journal.pool).await?;
    Ok(())
}
pub async fn authorized(journal: &Journal) -> Result<()> {
    sqlx::query("DELETE FROM meta WHERE key='health:unauthorized'")
        .execute(&journal.pool)
        .await?;
    Ok(())
}
pub async fn rejection(journal: &Journal, reason: &str) -> Result<()> {
    sqlx::query("INSERT INTO meta(key,value) VALUES('health:rejection',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
        .bind(reason).execute(&journal.pool).await?;
    Ok(())
}
pub async fn clear_rejection(journal: &Journal) -> Result<()> {
    sqlx::query("DELETE FROM meta WHERE key='health:rejection'")
        .execute(&journal.pool)
        .await?;
    Ok(())
}
pub async fn assess(
    journal: &Journal,
    send: bool,
    at: u64,
    progress_limit: u64,
    lane: Option<(i64, u64)>,
    lane_limit: u64,
) -> Result<Status> {
    let mut faults = Vec::new();
    if let Some(reason) = journal.meta("health:unauthorized").await? {
        faults.push(format!("wallet_unauthorized:{reason}"));
    }
    // Evidence is pruned whether or not this keeper may send; it only becomes a fault when it may.
    let preparation = preparation_stalled(journal, at, progress_limit).await?;
    if send {
        if preparation {
            faults.push("preparation_stalled".into());
        }
        for (stage, fault) in [
            ("settlement", "settlement_stalled"),
            ("fee_budget", "fee_budget_exceeded"),
            ("epoch", "epoch_stalled"),
        ] {
            if let Some(started) = journal.meta(&format!("health:blocked:{stage}")).await? {
                let age = at.saturating_sub(started.parse()?);
                if age >= progress_limit {
                    faults.push(fault.into());
                }
            }
        }
    }
    if let Some((nonce, started)) = lane {
        if at.saturating_sub(started) >= lane_limit {
            faults.push(format!("nonce_stalled:{nonce}"));
        }
        if let Some(reason) = journal.meta("health:rejection").await? {
            faults.push(format!("node_rejected:{reason}"));
        }
    } else {
        clear_rejection(journal).await?;
    }
    let status = Status {
        observed_at: at,
        send_enabled: send,
        healthy: faults.is_empty(),
        faults,
        role: journal.meta("keeper:role").await?,
        primary_alive: primary_alive(journal).await?,
    };
    let previous = journal
        .meta("health:status")
        .await?
        .map(|s| serde_json::from_str::<Status>(&s))
        .transpose()?;
    if previous
        .as_ref()
        .is_none_or(|p| p.faults != status.faults || p.send_enabled != send)
    {
        if status.healthy {
            tracing::info!(
                health = "healthy",
                send_enabled = send,
                "Keeper health transition"
            );
        } else {
            tracing::error!(health="degraded",faults=?status.faults,"Keeper health transition; reconciliation continues, inspect local health status");
        }
    }
    sqlx::query("INSERT INTO meta(key,value) VALUES('health:status',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
        .bind(serde_json::to_string(&status)?).execute(&journal.pool).await?;
    Ok(status)
}
pub async fn read(path: &Path) -> Result<Status> {
    crate::migration::ensure_startable(path)?;
    tokio::time::timeout(std::time::Duration::from_secs(2), read_bounded(path))
        .await
        .context("Health status read exceeded two seconds")?
}
async fn read_bounded(path: &Path) -> Result<Status> {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(1))
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(path)
                .read_only(true)
                .busy_timeout(std::time::Duration::from_millis(500)),
        )
        .await?;
    let value: Option<String> =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='health:status'")
            .fetch_optional(&pool)
            .await?;
    pool.close().await;
    serde_json::from_str(&value.context("No health observation recorded yet")?).map_err(Into::into)
}
pub fn check_freshness(status: &mut Status, at: u64, max_age: u64) {
    let fault = if status.observed_at > at {
        Some("observation_clock_anomaly")
    } else if at - status.observed_at > max_age {
        Some("observation_stale")
    } else {
        None
    };
    if let Some(fault) = fault {
        status.healthy = false;
        status.faults.push(fault.into());
    }
}
pub fn require_healthy(status: &Status) -> Result<()> {
    ensure!(
        status.healthy,
        "Keeper unhealthy: {}",
        status.faults.join(", ")
    );
    Ok(())
}
pub async fn tick_failed(journal: &Journal, send: bool) -> Result<()> {
    mark(journal, send, "tick_failed").await
}
/// Every RPC endpoint was rate limiting, so a tick could not run. The keeper is degraded, not failing: it keeps
/// trying, the tick does not count toward MAX_TICK_FAILURES, and the next tick that runs writes a fresh observation
/// without this fault. A watchdog sees a prolonged limit as an unhealthy, fresh observation.
pub async fn rate_limited(journal: &Journal, send: bool) -> Result<()> {
    mark(journal, send, "rpc_rate_limited").await
}
/// Keep the last observation's faults, add this one, and make the observation current and unhealthy.
async fn mark(journal: &Journal, send: bool, fault: &str) -> Result<()> {
    let mut status = journal
        .meta("health:status")
        .await?
        .map(|s| serde_json::from_str::<Status>(&s))
        .transpose()?
        .unwrap_or(Status {
            observed_at: now()?,
            send_enabled: send,
            healthy: false,
            faults: vec![],
            role: journal.meta("keeper:role").await?,
            primary_alive: primary_alive(journal).await?,
        });
    status.observed_at = now()?;
    status.healthy = false;
    if !status.faults.iter().any(|f| f == fault) {
        status.faults.push(fault.into());
    }
    sqlx::query("INSERT INTO meta(key,value) VALUES('health:status',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value").bind(serde_json::to_string(&status)?).execute(&journal.pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn freshness_rejects_stale_and_future_observations() {
        for (at, healthy, fault) in [
            (100, true, ""),
            (131, false, "observation_stale"),
            (99, false, "observation_clock_anomaly"),
        ] {
            let mut status = Status {
                observed_at: 100,
                send_enabled: true,
                healthy: true,
                faults: vec![],
                role: None,
                primary_alive: None,
            };
            check_freshness(&mut status, at, 30);
            assert_eq!(status.healthy, healthy);
            if !healthy {
                assert_eq!(status.faults, vec![fault]);
            }
        }
    }
    #[tokio::test]
    async fn observations_survive_restart_expiry_and_arrivals_until_progress() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.sqlite");
        let j = Journal::open(&path, "scope").await.unwrap();
        assert!(assess(&j, true, 100, 20, None, 120).await.unwrap().healthy);
        j.discovered("1", 110, "2").await.unwrap();
        // Request 1 waited from 90 (the second observation does not restart its wait) and expired unprepared.
        preparation_waiting(&j, "1", 90).await.unwrap();
        preparation_waiting(&j, "1", 105).await.unwrap();
        j.expire_unstarted(120).await.unwrap();
        j.pool.close().await;
        let j = Journal::open(&path, "scope").await.unwrap();
        j.discovered("2", 180, "3").await.unwrap();
        assert!(!assess(&j, true, 120, 20, None, 120).await.unwrap().healthy);
        assert!(assess(&j, false, 120, 20, None, 120).await.unwrap().healthy);
        assert!(!assess(&j, true, 121, 20, None, 120).await.unwrap().healthy);
        preparation_progress(&j, "2", 121).await.unwrap();
        assert!(assess(&j, true, 122, 20, None, 120).await.unwrap().healthy);
        // A budget deferral is a configuration fault distinct from ordinary stalls.
        blocked(&j, "fee_budget", 100).await.unwrap();
        assert_eq!(
            assess(&j, true, 122, 20, None, 120).await.unwrap().faults,
            vec!["fee_budget_exceeded"]
        );
        assert!(assess(&j, false, 122, 20, None, 120).await.unwrap().healthy);
        recovered(&j, "fee_budget").await.unwrap();
        assert!(assess(&j, true, 122, 20, None, 120).await.unwrap().healthy);
        // Live demand on blocked or stale epoch work is its own fault until publication or expiry.
        blocked(&j, "epoch", 100).await.unwrap();
        assert!(assess(&j, true, 119, 20, None, 120).await.unwrap().healthy);
        assert_eq!(
            assess(&j, true, 120, 20, None, 120).await.unwrap().faults,
            vec!["epoch_stalled"]
        );
        recovered(&j, "epoch").await.unwrap();
        assert!(assess(&j, true, 122, 20, None, 120).await.unwrap().healthy);
        rejection(&j, "insufficient_funds").await.unwrap();
        assert!(
            !assess(&j, true, 122, 20, Some((7, 122)), 120)
                .await
                .unwrap()
                .healthy
        );
        // Even no-send mode must report a pre-existing unsafe lane.
        assert!(
            !assess(&j, false, 300, 20, Some((7, 122)), 120)
                .await
                .unwrap()
                .healthy
        );
        assert!(assess(&j, true, 301, 20, None, 120).await.unwrap().healthy);
        assert!(read(&path).await.unwrap().healthy);
        tick_failed(&j, true).await.unwrap();
        let failed = read(&path).await.unwrap();
        assert!(!failed.healthy);
        assert!(failed.faults.iter().any(|f| f == "tick_failed"));
        assert!(assess(&j, true, 302, 20, None, 120).await.unwrap().healthy);
        // A rate-limited tick is degraded and current, never healthy, and clears with the next tick that runs.
        rate_limited(&j, true).await.unwrap();
        rate_limited(&j, true).await.unwrap();
        let limited = read(&path).await.unwrap();
        assert!(!limited.healthy);
        assert_eq!(limited.faults, vec!["rpc_rate_limited"]);
        let mut current = limited;
        check_freshness(&mut current, now().unwrap(), 30);
        assert_eq!(current.faults, vec!["rpc_rate_limited"]);
        assert!(assess(&j, true, 303, 20, None, 120).await.unwrap().healthy);
        j.pool.close().await;
    }
}
