//! Small, bounded, public event journal. Triggers keep events atomic with job changes.
use anyhow::Result;
use sqlx::SqlitePool;

pub const CAPACITY: i64 = 4096;

pub async fn install(pool: &SqlitePool) -> Result<()> {
    let exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('jobs') WHERE name='callback_failed_observed'",
    )
    .fetch_one(pool)
    .await?;
    if exists == 0 {
        sqlx::query(
            "ALTER TABLE jobs ADD COLUMN callback_failed_observed INTEGER NOT NULL DEFAULT 0",
        )
        .execute(pool)
        .await?;
    }
    // Archives made before this additive field must retain SELECT * compatibility
    // when a later explicit migration archives the current jobs again.
    let archive: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='migration_archive_jobs'",
    )
    .fetch_one(pool)
    .await?;
    if archive != 0 {
        let column: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info('migration_archive_jobs') WHERE name='callback_failed_observed'")
            .fetch_one(pool).await?;
        if column == 0 {
            sqlx::query("ALTER TABLE migration_archive_jobs ADD COLUMN callback_failed_observed INTEGER NOT NULL DEFAULT 0")
                .execute(pool).await?;
        }
    }
    sqlx::raw_sql(
        r#"
CREATE TABLE IF NOT EXISTS audit_events(
 cursor INTEGER PRIMARY KEY AUTOINCREMENT,
 request_id TEXT,
 kind TEXT NOT NULL,
 origin TEXT NOT NULL,
 observed_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE IF NOT EXISTS telemetry_outbox(
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 report_id TEXT NOT NULL,
 payload TEXT NOT NULL,
 next_cursor INTEGER NOT NULL,
 dropped_count INTEGER NOT NULL
);
INSERT OR IGNORE INTO meta(key,value) VALUES('audit:dropped','0');
-- Frozen since the pilot allowlist was retired; still reported as rejectionHistoryPrunedTotal (see telemetry).
INSERT OR IGNORE INTO meta(key,value) VALUES('audit:rejections_pruned','0');
INSERT OR IGNORE INTO meta(key,value) VALUES('telemetry:acked_cursor','0');
INSERT OR IGNORE INTO meta(key,value) VALUES('telemetry:acked_dropped','0');
CREATE TRIGGER IF NOT EXISTS audit_job_insert AFTER INSERT ON jobs BEGIN
 INSERT INTO audit_events(request_id,kind,origin) VALUES(NEW.id,'discovered',(SELECT value FROM meta WHERE key='scope'));
END;
CREATE TRIGGER IF NOT EXISTS audit_job_state AFTER UPDATE OF state ON jobs
 WHEN NEW.state != OLD.state AND NEW.state IN
 ('prepared','served','callback_failed','refunded','expired','blocked','ignored') BEGIN
 INSERT INTO audit_events(request_id,kind,origin) VALUES(NEW.id,NEW.state,(SELECT value FROM meta WHERE key='scope'));
END;
CREATE TRIGGER IF NOT EXISTS audit_callback AFTER UPDATE OF callback_failed_observed ON jobs
 WHEN OLD.callback_failed_observed=0 AND NEW.callback_failed_observed=1 BEGIN
 INSERT INTO audit_events(request_id,kind,origin) VALUES(NEW.id,'callback_failed_at_acceptance',(SELECT value FROM meta WHERE key='scope'));
END;
-- The pilot allowlist's ignored-request history had no writer left once consumer access became public.
DROP TRIGGER IF EXISTS audit_rejected;
DROP TABLE IF EXISTS audit_rejections;
-- Progress timers start before normal work, so their insertion/deletion is not
-- evidence of failure/recovery. Only assessed status transitions are reportable.
DROP TRIGGER IF EXISTS audit_health_insert;
DROP TRIGGER IF EXISTS audit_health_rejection;
DROP TRIGGER IF EXISTS audit_health_recovered;
DROP TRIGGER IF EXISTS audit_tick_failed;
DROP TRIGGER IF EXISTS audit_first_tick_failed;
DROP TRIGGER IF EXISTS audit_health_status_insert;
DROP TRIGGER IF EXISTS audit_health_status_update;
CREATE TRIGGER audit_health_insert AFTER INSERT ON meta
 WHEN NEW.key='health:rejection'
 AND NOT EXISTS(SELECT 1 FROM meta WHERE key='audit:suspended' AND value='1') BEGIN
 INSERT INTO audit_events(kind,origin) VALUES('node_transaction_rejected',(SELECT value FROM meta WHERE key='scope'));
END;
CREATE TRIGGER audit_health_rejection AFTER UPDATE OF value ON meta
 WHEN NEW.key='health:rejection' AND NEW.value != OLD.value
 AND NOT EXISTS(SELECT 1 FROM meta WHERE key='audit:suspended' AND value='1') BEGIN
 INSERT INTO audit_events(kind,origin) VALUES('node_transaction_rejected',(SELECT value FROM meta WHERE key='scope'));
END;
CREATE TRIGGER audit_health_status_insert AFTER INSERT ON meta
 WHEN CASE WHEN NEW.key='health:status' THEN json_extract(NEW.value,'$.healthy')=0 ELSE 0 END
 AND NOT EXISTS(SELECT 1 FROM meta WHERE key='audit:suspended' AND value='1') BEGIN
 INSERT INTO audit_events(kind,origin) VALUES('service_degraded',(SELECT value FROM meta WHERE key='scope'));
END;
CREATE TRIGGER audit_health_status_update AFTER UPDATE OF value ON meta
 WHEN CASE WHEN NEW.key='health:status' THEN
 (json_extract(NEW.value,'$.healthy') != json_extract(OLD.value,'$.healthy')
 OR (json_extract(NEW.value,'$.healthy')=0
 AND json_extract(NEW.value,'$.faults') != json_extract(OLD.value,'$.faults'))) ELSE 0 END
 AND NOT EXISTS(SELECT 1 FROM meta WHERE key='audit:suspended' AND value='1') BEGIN
 INSERT INTO audit_events(kind,origin) VALUES(
 CASE WHEN json_extract(NEW.value,'$.healthy')=1 THEN 'service_recovered' ELSE 'service_degraded' END,
 (SELECT value FROM meta WHERE key='scope'));
END;
"#,
    )
    .execute(pool)
    .await?;
    // Preserve existing unacknowledged events, including the in-flight batch. On
    // overflow, reject new audit events only and expose the cumulative loss count.
    sqlx::raw_sql(
        "CREATE TRIGGER IF NOT EXISTS audit_capacity BEFORE INSERT ON audit_events
         WHEN (SELECT COUNT(*) FROM audit_events)>=4096 BEGIN
         UPDATE meta SET value=CAST(CAST(value AS INTEGER)+1 AS TEXT) WHERE key='audit:dropped';
         SELECT RAISE(IGNORE); END;",
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn callback_failed(pool: &SqlitePool, request_id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE jobs SET callback_failed_observed=1 WHERE id=? AND callback_failed_observed=0",
    )
    .bind(request_id)
    .execute(pool)
    .await?;
    Ok(())
}
