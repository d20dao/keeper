//! Optional outbound-only heartbeat. No listener, redirects, raw error logging or
//! network await in the randomness tick. One durable payload survives uncertain delivery.
use crate::{config::Config, health, journal::Journal};
use anyhow::{Result, ensure};
use reqwest::{Client, Url, header};
use serde_json::json;
use sqlx::{Row, SqlitePool};
use std::{env, time::Duration};

const BATCH: i64 = 128;
const MAX_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct Settings {
    url: Url,
    authorization: header::HeaderValue,
    interval: Duration,
}
impl Settings {
    pub fn load(chain: u64) -> Result<Option<Self>> {
        Self::parse(
            env::var("HEALTH_API_URL").ok(),
            env::var("HEALTH_API_KEY").ok(),
            env::var("HEALTH_INTERVAL_SECONDS").ok(),
            chain,
        )
    }
    fn parse(
        url: Option<String>,
        key: Option<String>,
        interval: Option<String>,
        chain: u64,
    ) -> Result<Option<Self>> {
        let url = url.filter(|value| !value.trim().is_empty());
        let key = key.filter(|value| !value.trim().is_empty());
        let (url, key) = match (url, key) {
            (None, None) => return Ok(None),
            (Some(url), Some(key)) => (url, key),
            _ => anyhow::bail!("Set both HEALTH_API_URL and HEALTH_API_KEY, or neither"),
        };
        let url = Url::parse(&url).map_err(|_| anyhow::anyhow!("Invalid HEALTH_API_URL"))?;
        let loopback = url.host_str().is_some_and(|host| {
            host.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        });
        ensure!(
            url.scheme() == "https" || (chain == 31337 && url.scheme() == "http" && loopback),
            "Health endpoint requires HTTPS; HTTP is restricted to loopback on chain 31337"
        );
        ensure!(
            url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
                && url.query().is_none(),
            "Health URL cannot contain credentials, query or fragment"
        );
        ensure!(
            !key.is_empty() && key.len() <= 4096 && key.bytes().all(|b| b.is_ascii_graphic()),
            "Invalid HEALTH_API_KEY"
        );
        let mut authorization = header::HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| anyhow::anyhow!("Invalid HEALTH_API_KEY"))?;
        authorization.set_sensitive(true);
        let seconds: u64 = interval
            .as_deref()
            .unwrap_or("30")
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid HEALTH_INTERVAL_SECONDS"))?;
        let minimum = if chain == 31337 { 1 } else { 5 };
        ensure!(
            (minimum..=3600).contains(&seconds),
            "Health interval must be 5..3600 seconds (1..3600 locally)"
        );
        Ok(Some(Self {
            url,
            authorization,
            interval: Duration::from_secs(seconds),
        }))
    }
}

pub struct Task(tokio::task::JoinHandle<()>);
impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Failure to initialize telemetry is reported locally without stopping the keeper.
pub fn spawn(cfg: &Config, journal: &Journal) -> Option<Task> {
    let settings = cfg.telemetry.clone()?;
    let pool = journal.pool.clone();
    let chain = cfg.chain_id;
    let coordinator = cfg.coordinator.to_string();
    Some(Task(tokio::spawn(async move {
        let Ok(client) = client() else {
            tracing::warn!("Health publisher unavailable; randomness continues");
            return;
        };
        let mut timer = tokio::time::interval(settings.interval);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut failed = false;
        loop {
            timer.tick().await;
            // Includes database work and delivery; cancellation preserves committed outbox.
            let success = matches!(
                tokio::time::timeout(
                    Duration::from_secs(4),
                    publish(&pool, &settings, &client, chain, &coordinator)
                )
                .await,
                Ok(Ok(()))
            );
            if success && failed {
                tracing::info!("Health publisher delivery recovered");
            }
            if !success && !failed {
                tracing::warn!(
                    "Health publisher delivery deferred; durable report retained, randomness continues"
                );
            }
            failed = !success;
        }
    })))
}
fn client() -> Result<Client> {
    Ok(Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .connect_timeout(Duration::from_secs(2))
        .pool_max_idle_per_host(0)
        .build()?)
}

async fn meta(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, key: &str) -> Result<String> {
    Ok(sqlx::query_scalar("SELECT value FROM meta WHERE key=?")
        .bind(key)
        .fetch_one(&mut **tx)
        .await?)
}

async fn prepare(pool: &SqlitePool, chain: u64, coordinator: &str) -> Result<(String, String)> {
    let mut tx = pool.begin().await?;
    if let Some(row) =
        sqlx::query("SELECT report_id,payload FROM telemetry_outbox WHERE singleton=1")
            .fetch_optional(&mut *tx)
            .await?
    {
        let result = (row.get("report_id"), row.get("payload"));
        tx.commit().await?;
        return Ok(result);
    }
    let cursor: i64 = meta(&mut tx, "telemetry:acked_cursor").await?.parse()?;
    let dropped: i64 = meta(&mut tx, "audit:dropped").await?.parse()?;
    let acked_dropped: i64 = meta(&mut tx, "telemetry:acked_dropped").await?.parse()?;
    let pruned: i64 = meta(&mut tx, "audit:rejections_pruned").await?.parse()?;
    let scope = meta(&mut tx, "scope").await?;
    let node_id = alloy_primitives::keccak256(scope.as_bytes()).to_string();
    let report_id: String = sqlx::query_scalar("SELECT lower(hex(randomblob(32)))")
        .fetch_one(&mut *tx)
        .await?;
    let at = health::now()?;
    let raw_health: Option<String> =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='health:status'")
            .fetch_optional(&mut *tx)
            .await?;
    // Never relay arbitrary fault strings from local diagnostics.
    let public_health = raw_health.and_then(|s| serde_json::from_str::<health::Status>(&s).ok()).map(|mut status| {
        health::check_freshness(&mut status, at, 30);
        let codes: std::collections::BTreeSet<_> = status.faults.iter().map(|s| match s.as_str() {
            "preparation_stalled" => "preparation_stalled", "settlement_stalled" => "settlement_stalled",
            "fee_budget_exceeded" => "fee_budget_exceeded", "epoch_stalled" => "epoch_stalled",
            "tick_failed" => "tick_failed", "rpc_rate_limited" => "rpc_rate_limited", "observation_stale" => "observation_stale",
            "observation_clock_anomaly" => "observation_clock_anomaly",
            s if s.starts_with("node_rejected:") => "node_transaction_rejected",
            s if s.starts_with("nonce_stalled:") => "nonce_stalled", _ => "unknown_fault",
        }).collect();
        json!({"observedAt":status.observed_at,"healthy":status.healthy,"sendEnabled":status.send_enabled,"faults":codes,"role":status.role.as_deref().unwrap_or("primary"),"primaryAlive":status.primary_alive})
    }).unwrap_or_else(||json!({"healthy":false,"faults":["not_observed"]}));
    let rows = sqlx::query("SELECT cursor,request_id,kind,origin,observed_at FROM audit_events WHERE cursor>? ORDER BY cursor LIMIT ?")
        .bind(cursor).bind(BATCH).fetch_all(&mut *tx).await?;
    let mut groups = json!({"completed":[],"rejected":[],"failed":[],"progress":[]});
    let mut next = cursor;
    for row in rows {
        next = row.get("cursor");
        let kind: String = row.get("kind");
        let group = match kind.as_str() {
            "served" | "refunded" => "completed",
            "not_allowlisted" | "ignored" => "rejected",
            "callback_failed_at_acceptance"
            | "expired"
            | "blocked"
            | "work_blocked"
            | "service_degraded"
            | "node_transaction_rejected"
            | "tick_failed" => "failed",
            _ => "progress",
        };
        groups[group].as_array_mut().expect("fixed array").push(json!({"cursor":next.to_string(),"requestId":row.get::<Option<String>,_>("request_id"),"kind":kind,"origin":row.get::<String,_>("origin"),"observedAt":row.get::<i64,_>("observed_at")}));
    }
    let count = |key: &str| groups[key].as_array().expect("fixed array").len();
    let payload = serde_json::to_string(
        &json!({"version":1,"nodeId":node_id,"chainId":chain.to_string(),"coordinator":coordinator,
        "reportId":report_id,"observedAt":at,"health":public_health,
        "summary":{"completed":count("completed"),"rejected":count("rejected"),"failed":count("failed"),"progress":count("progress")},
        "events":groups,"nextCursor":next.to_string(),"droppedCount":dropped-acked_dropped,"droppedTotal":dropped,"rejectionHistoryPrunedTotal":pruned}),
    )?;
    ensure!(
        payload.len() <= MAX_BYTES,
        "Health report exceeds size limit"
    );
    sqlx::query("INSERT INTO telemetry_outbox(singleton,report_id,payload,next_cursor,dropped_count) VALUES(1,?,?,?,?)")
        .bind(&report_id).bind(&payload).bind(next).bind(dropped).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok((report_id, payload))
}

async fn publish(
    pool: &SqlitePool,
    settings: &Settings,
    client: &Client,
    chain: u64,
    coordinator: &str,
) -> Result<()> {
    let (report_id, payload) = prepare(pool, chain, coordinator).await?;
    let response = client
        .post(settings.url.clone())
        .header(header::AUTHORIZATION, settings.authorization.clone())
        .header(header::CONTENT_TYPE, "application/json")
        .header("Idempotency-Key", &report_id)
        .body(payload)
        .send()
        .await?;
    // The receiver's committed 2xx status is the entire acknowledgement. Never
    // read/log a response body; malicious or endless bodies cannot consume memory.
    ensure!(
        response.status().is_success(),
        "Health receiver did not acknowledge"
    );
    drop(response);
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT next_cursor,dropped_count FROM telemetry_outbox WHERE singleton=1 AND report_id=?",
    )
    .bind(&report_id)
    .fetch_one(&mut *tx)
    .await?;
    let next: i64 = row.get("next_cursor");
    let dropped: i64 = row.get("dropped_count");
    sqlx::query("DELETE FROM audit_events WHERE cursor<=?")
        .bind(next)
        .execute(&mut *tx)
        .await?;
    for (key, value) in [
        ("telemetry:acked_cursor", next),
        ("telemetry:acked_dropped", dropped),
    ] {
        sqlx::query("UPDATE meta SET value=? WHERE key=?")
            .bind(value.to_string())
            .bind(key)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM telemetry_outbox WHERE singleton=1 AND report_id=?")
        .bind(report_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn settings(url: &str) -> Settings {
        Settings::parse(
            Some(url.into()),
            Some("local-test-token".into()),
            Some("1".into()),
            31337,
        )
        .unwrap()
        .unwrap()
    }
    // A test-only loopback receiver. The keeper production binary has no listener.
    fn receiver(
        statuses: Vec<Option<&'static str>>,
    ) -> (String, std::thread::JoinHandle<Vec<(String, String)>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/health", listener.local_addr().unwrap());
        let task = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for status in statuses {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let end = loop {
                    let mut buf = [0; 2048];
                    let n = socket.read(&mut buf).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
                let len: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(str::to_owned)
                    })
                    .unwrap()
                    .parse()
                    .unwrap();
                while bytes.len() < end + len {
                    let mut buf = [0; 2048];
                    let n = socket.read(&mut buf).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buf[..n]);
                }
                requests.push((
                    headers,
                    String::from_utf8(bytes[end..end + len].to_vec()).unwrap(),
                ));
                if let Some(status) = status {
                    socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
                }
            }
            requests
        });
        (url, task)
    }

    #[test]
    fn config_is_opt_in_and_does_not_leak_rejected_values() {
        assert!(Settings::parse(None, None, None, 1).unwrap().is_none());
        assert!(
            Settings::parse(Some(" \t".into()), Some("\r\n".into()), None, 1)
                .unwrap()
                .is_none()
        );
        assert!(
            Settings::parse(
                Some("https://example.com/".into()),
                Some(" ".into()),
                None,
                1
            )
            .is_err()
        );
        for (url, key, chain, seconds) in [
            ("http://example.com/secret", "secret", 31337, "1"),
            ("http://127.0.0.1/secret", "secret", 1, "5"),
            ("https://user:secret@example.com/", "secret", 1, "5"),
            ("https://example.com/?secret", "secret", 1, "5"),
            ("https://example.com/", "secret\n", 1, "5"),
            ("https://example.com/", "secret", 1, "1"),
        ] {
            let err = Settings::parse(
                Some(url.into()),
                Some(key.into()),
                Some(seconds.into()),
                chain,
            )
            .err()
            .unwrap();
            assert!(!err.to_string().contains("secret"));
        }
        assert!(Settings::parse(Some("https://example.com/health".into()), None, None, 1).is_err());
    }

    #[tokio::test]
    async fn receiver_auth_grouping_uncertain_retry_restart_and_ack() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.sqlite");
        let j = Journal::open(&path, "31337:coordinator:sender")
            .await
            .unwrap();
        j.discovered("1", 100, "2").await.unwrap();
        j.prepared("1", "PRIVATE_PROOF", "PRIVATE_CALLDATA")
            .await
            .unwrap();
        j.state("1", "served").await.unwrap();
        crate::audit::callback_failed(&j.pool, "1").await.unwrap();
        crate::audit::callback_failed(&j.pool, "1").await.unwrap();
        health::rejection(&j, "PRIVATE_NODE_ERROR").await.unwrap();
        health::rejection(&j, "PRIVATE_NODE_ERROR").await.unwrap();
        health::assess(&j, true, health::now().unwrap(), 20, Some((0, 0)), 120)
            .await
            .unwrap();
        let (url, receiver) = receiver(vec![Some("500 Failed"), None, Some("204 No Content")]);
        let cfg = settings(&url);
        let client = client().unwrap();
        assert!(
            publish(&j.pool, &cfg, &client, 31337, "coordinator")
                .await
                .is_err()
        );
        let (_, first) = prepare(&j.pool, 31337, "coordinator").await.unwrap();
        // Receiver downtime does not hold the journal or prevent randomness state changes.
        j.discovered("3", 110, "4").await.unwrap();
        j.prepared("3", "p", "c").await.unwrap();
        j.pool.close().await;
        let j = Journal::open(&path, "31337:coordinator:sender")
            .await
            .unwrap();
        assert!(
            publish(&j.pool, &cfg, &client, 31337, "coordinator")
                .await
                .is_err()
        );
        publish(&j.pool, &cfg, &client, 31337, "coordinator")
            .await
            .unwrap();
        let requests = receiver.join().unwrap();
        for (headers, body) in requests {
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("authorization: bearer local-test-token")
            );
            assert_eq!(body, first);
            assert!(!body.contains("PRIVATE_"));
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert!(headers.contains(v["reportId"].as_str().unwrap()));
            assert_eq!(v["summary"]["completed"], 1);
            assert_eq!(v["chainId"], "31337");
            // Kept for the receiver's version-1 envelope; nothing produces rejections any more.
            assert_eq!(v["summary"]["rejected"], 0);
            assert_eq!(v["events"]["rejected"], serde_json::json!([]));
            assert_eq!(v["summary"]["failed"], 3);
            assert_eq!(
                v["events"]["failed"][0]["kind"],
                "callback_failed_at_acceptance"
            );
        }
        assert!(j.job("2").await.unwrap().is_none());
        let next: serde_json::Value =
            serde_json::from_str(&prepare(&j.pool, 31337, "coordinator").await.unwrap().1).unwrap();
        assert_eq!(next["summary"]["progress"], 2);
        assert_eq!(next["summary"]["completed"], 0);
        j.pool.close().await;
    }

    #[tokio::test]
    async fn bounded_retention_preserves_pending_payload_and_counts_loss() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(&dir.path().join("j.sqlite"), "scope")
            .await
            .unwrap();
        j.discovered("first", 100, "2").await.unwrap();
        let first = prepare(&j.pool, 31337, "coordinator").await.unwrap();
        sqlx::raw_sql("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<4200) INSERT INTO audit_events(request_id,kind,origin) SELECT CAST(x AS TEXT),'discovered','scope' FROM n;")
            .execute(&j.pool).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events")
            .fetch_one(&j.pool)
            .await
            .unwrap();
        assert_eq!(count, crate::audit::CAPACITY);
        assert_eq!(
            j.meta("audit:dropped").await.unwrap().as_deref(),
            Some("105")
        );
        assert_eq!(first, prepare(&j.pool, 31337, "coordinator").await.unwrap());
        j.pool.close().await;
    }

    #[tokio::test]
    async fn immutable_origin_survives_scope_change_and_health_errors_are_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(&dir.path().join("j.sqlite"), "old_scope")
            .await
            .unwrap();
        j.discovered("1", 100, "2").await.unwrap();
        health::preparation_waiting(&j, "1", 1).await.unwrap();
        health::preparation_waiting(&j, "1", 2).await.unwrap();
        health::assess(&j, true, 30, 20, None, 120).await.unwrap();
        health::assess(&j, true, 31, 20, None, 120).await.unwrap();
        health::preparation_progress(&j, "1", 31).await.unwrap();
        health::assess(&j, true, 32, 20, None, 120).await.unwrap();
        sqlx::query("UPDATE meta SET value='new_scope' WHERE key='scope'")
            .execute(&j.pool)
            .await
            .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&prepare(&j.pool, 31337, "new_coordinator").await.unwrap().1)
                .unwrap();
        assert_eq!(v["events"]["progress"][0]["origin"], "old_scope");
        assert_eq!(v["summary"]["failed"], 1);
        assert_eq!(v["summary"]["progress"], 2);
        j.pool.close().await;
    }

    #[tokio::test]
    async fn routine_successful_preparation_never_reports_failure_or_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(&dir.path().join("j.sqlite"), "scope")
            .await
            .unwrap();
        health::assess(&j, true, 100, 20, None, 120).await.unwrap();
        j.discovered("1", 160, "2").await.unwrap();
        health::preparation_waiting(&j, "1", 100).await.unwrap();
        health::assess(&j, true, 101, 20, None, 120).await.unwrap();
        j.prepared("1", "proof", "call").await.unwrap();
        health::preparation_progress(&j, "1", 101).await.unwrap();
        health::blocked(&j, "settlement", 101).await.unwrap();
        j.state("1", "served").await.unwrap();
        health::recovered(&j, "settlement").await.unwrap();
        health::assess(&j, true, 102, 20, None, 120).await.unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&prepare(&j.pool, 31337, "coordinator").await.unwrap().1).unwrap();
        assert_eq!(v["summary"]["failed"], 0);
        assert_eq!(v["summary"]["completed"], 1);
        assert_eq!(v["summary"]["progress"], 2);
        assert_eq!(v["events"]["progress"][0]["kind"], "discovered");
        assert_eq!(v["events"]["progress"][1]["kind"], "prepared");
        j.pool.close().await;
    }

    #[tokio::test]
    async fn migration_suspension_suppresses_status_and_rejection_events() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(&dir.path().join("j.sqlite"), "scope")
            .await
            .unwrap();
        health::tick_failed(&j, true).await.unwrap();
        health::tick_failed(&j, true).await.unwrap();
        sqlx::query("INSERT INTO meta(key,value) VALUES('audit:suspended','1')")
            .execute(&j.pool)
            .await
            .unwrap();
        health::assess(&j, true, 100, 20, None, 120).await.unwrap();
        health::rejection(&j, "private_reason").await.unwrap();
        health::rejection(&j, "other_private_reason").await.unwrap();
        health::tick_failed(&j, true).await.unwrap();
        sqlx::query("DELETE FROM meta WHERE key='health:status'")
            .execute(&j.pool)
            .await
            .unwrap();
        health::tick_failed(&j, true).await.unwrap();
        health::clear_rejection(&j).await.unwrap();
        sqlx::query("DELETE FROM meta WHERE key='audit:suspended'")
            .execute(&j.pool)
            .await
            .unwrap();
        health::assess(&j, true, 101, 20, None, 120).await.unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&prepare(&j.pool, 31337, "coordinator").await.unwrap().1).unwrap();
        assert_eq!(v["summary"]["failed"], 1);
        assert_eq!(v["summary"]["progress"], 1);
        assert_eq!(v["events"]["failed"][0]["kind"], "service_degraded");
        assert_eq!(v["events"]["progress"][0]["kind"], "service_recovered");
        j.pool.close().await;
    }

    #[tokio::test]
    async fn slow_receiver_does_not_hold_database_and_redirects_are_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(&dir.path().join("j.sqlite"), "scope")
            .await
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = settings(&format!("http://{}/", listener.local_addr().unwrap()));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let receiver = std::thread::spawn(move || {
            let (_socket, _) = listener.accept().unwrap();
            let _ = started_tx.send(());
            std::thread::sleep(Duration::from_millis(750));
        });
        let pool = j.pool.clone();
        let task = tokio::spawn(async move {
            let client = Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_millis(300))
                .build()
                .unwrap();
            publish(&pool, &cfg, &client, 31337, "coordinator").await
        });
        started_rx.await.unwrap();
        tokio::time::timeout(Duration::from_millis(200), j.discovered("1", 100, "2"))
            .await
            .unwrap()
            .unwrap();
        assert!(task.await.unwrap().is_err());
        receiver.join().unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM telemetry_outbox")
                .fetch_one(&j.pool)
                .await
                .unwrap(),
            1
        );

        let trap = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        trap.set_nonblocking(true).unwrap();
        let redirect = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = settings(&format!("http://{}/", redirect.local_addr().unwrap()));
        let location = format!("http://{}/secret", trap.local_addr().unwrap());
        let receiver = std::thread::spawn(move || {
            let (mut socket, _) = redirect.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut buf = [0; 4096];
            assert!(socket.read(&mut buf).unwrap() > 0);
            socket.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
        });
        assert!(
            publish(&j.pool, &cfg, &client().unwrap(), 31337, "coordinator")
                .await
                .is_err()
        );
        receiver.join().unwrap();
        assert_eq!(
            trap.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        j.pool.close().await;
    }

    #[tokio::test]
    async fn event_loss_and_retired_rejection_history_are_reported_after_pending_ack() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(&dir.path().join("j.sqlite"), "scope")
            .await
            .unwrap();
        prepare(&j.pool, 31337, "coordinator").await.unwrap();
        // A journal from the allowlist pilot: queued rejection events and a frozen pruning counter are still
        // reported in the envelope's rejected group and rejectionHistoryPrunedTotal.
        sqlx::raw_sql("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<4100) INSERT INTO audit_events(request_id,kind,origin) SELECT CAST(x AS TEXT),'not_allowlisted','scope' FROM n; UPDATE meta SET value='4' WHERE key='audit:rejections_pruned';")
            .execute(&j.pool).await.unwrap();
        let (url, receiver) = receiver(vec![Some("204 No Content")]);
        publish(
            &j.pool,
            &settings(&url),
            &client().unwrap(),
            31337,
            "coordinator",
        )
        .await
        .unwrap();
        receiver.join().unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&prepare(&j.pool, 31337, "coordinator").await.unwrap().1).unwrap();
        assert_eq!(v["droppedCount"], 4);
        assert_eq!(v["droppedTotal"], 4);
        assert_eq!(v["rejectionHistoryPrunedTotal"], 4);
        assert_eq!(v["summary"]["rejected"], 128);
        j.pool.close().await;
    }
}
