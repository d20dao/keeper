//! Operator-requested transfer of keeper wallet balance to the coordinator's fee recipient.
//!
//! `d20dao-keeper sweep` only records a request in the journal. The running keeper executes it
//! on its own nonce lane while no other attempt is unresolved, commits the signed bytes before
//! any broadcast, and treats an unresolved sweep as a busy lane. Game and epoch transactions
//! therefore never race the sweep for a nonce, and nothing outside the keeper signs with its key.
use alloy_primitives::U256;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

pub const REQUEST_KEY: &str = "sweep:request";
pub const ATTEMPT_KEY: &str = "sweep:attempt";
pub const LAST_KEY: &str = "sweep:last";
/// Every sweep leaves at least this much (1 USDC, or MAX_TX_COST_WEI when larger) for gas.
pub const MIN_RESERVE_WEI: u128 = 1_000_000_000_000_000_000;
/// A transfer to a Safe proxy needs a little more than 21,000 gas; anything far above is refused.
pub const MAX_TRANSFER_GAS: u64 = 100_000;
/// A sweep with no receipt after this long is replaced by a zero-value nonce cancellation.
pub const REPLACE_AFTER_SECONDS: u64 = 30;
/// The transfer plus at most three cancellations.
pub const MAX_ATTEMPTS: usize = 4;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Send exactly this many wei.
    Amount,
    /// Send everything above this many wei, after gas.
    Keep,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub mode: Mode,
    /// Decimal wei.
    pub wei: String,
    pub requested_at: u64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SignedTx {
    /// "sweep" for the transfer, "cancel" for a zero-value self transaction on the same nonce.
    pub kind: String,
    pub hash: String,
    pub raw: String,
    pub gas: u64,
    pub fee: String,
    pub priority: String,
    pub created: u64,
    pub broadcast: u64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attempt {
    pub request: Request,
    pub nonce: u64,
    pub to: String,
    /// Decimal wei.
    pub value: String,
    pub txs: Vec<SignedTx>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    /// "sent", "reverted", "cancelled" or "refused".
    pub state: String,
    pub detail: String,
    pub to: Option<String>,
    pub value: Option<String>,
    pub tx_hash: Option<String>,
    pub requested_at: u64,
    pub finished_at: u64,
}

/// Parses a non-negative decimal USDC amount (18 decimals) into wei.
pub fn parse_usdc(text: &str) -> Result<U256> {
    let text = text.trim();
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    ensure!(
        !(whole.is_empty() && fraction.is_empty())
            && whole.chars().all(|c| c.is_ascii_digit())
            && fraction.chars().all(|c| c.is_ascii_digit())
            && fraction.len() <= 18,
        "Amount must be a decimal USDC value with at most 18 decimals, for example 5 or 2.5"
    );
    let whole: U256 = if whole.is_empty() {
        U256::ZERO
    } else {
        whole.parse().context("Amount too large")?
    };
    let fraction: U256 = format!("{fraction:0<18}").parse()?;
    whole
        .checked_mul(U256::from(MIN_RESERVE_WEI))
        .and_then(|wei| wei.checked_add(fraction))
        .context("Amount too large")
}
pub fn format_usdc(wei: U256) -> String {
    let unit = U256::from(MIN_RESERVE_WEI);
    let fraction = format!("{:018}", wei % unit);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        format!("{}", wei / unit)
    } else {
        format!("{}.{fraction}", wei / unit)
    }
}

/// The value to transfer, or why the request is refused. `gas_cost` is the transfer's maximum
/// gas cost; `reserve` is what must remain in the wallet afterwards.
pub fn plan(
    request: &Request,
    balance: U256,
    gas_cost: U256,
    reserve: U256,
) -> std::result::Result<U256, String> {
    let wei: U256 = request
        .wei
        .parse()
        .map_err(|_| "Invalid amount in sweep request".to_string())?;
    let spendable = balance.saturating_sub(gas_cost);
    match request.mode {
        Mode::Amount => {
            if wei.is_zero() {
                return Err("Amount must be greater than zero".into());
            }
            if spendable < wei.saturating_add(reserve) {
                return Err(format!(
                    "Balance {} USDC cannot send {} USDC and keep the {} USDC reserve plus gas",
                    format_usdc(balance),
                    format_usdc(wei),
                    format_usdc(reserve)
                ));
            }
            Ok(wei)
        }
        Mode::Keep => {
            if wei < reserve {
                return Err(format!(
                    "Keep at least the {} USDC reserve",
                    format_usdc(reserve)
                ));
            }
            let value = spendable.saturating_sub(wei);
            if value.is_zero() {
                return Err(format!(
                    "Balance {} USDC is not above {} USDC plus gas; nothing to send",
                    format_usdc(balance),
                    format_usdc(wei)
                ));
            }
            Ok(value)
        }
    }
}

async fn read<T: for<'de> Deserialize<'de>>(pool: &SqlitePool, key: &str) -> Result<Option<T>> {
    let value: Option<String> = sqlx::query_scalar("SELECT value FROM meta WHERE key=?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    value
        .map(|v| serde_json::from_str(&v).with_context(|| format!("Corrupt journal entry {key}")))
        .transpose()
}
pub async fn request(pool: &SqlitePool) -> Result<Option<Request>> {
    read(pool, REQUEST_KEY).await
}
pub async fn attempt(pool: &SqlitePool) -> Result<Option<Attempt>> {
    read(pool, ATTEMPT_KEY).await
}
pub async fn last(pool: &SqlitePool) -> Result<Option<Outcome>> {
    read(pool, LAST_KEY).await
}
pub async fn in_flight(pool: &SqlitePool) -> Result<bool> {
    Ok(attempt(pool).await?.is_some())
}

/// Queues a request. Only one request or attempt may exist at a time.
pub async fn submit(pool: &SqlitePool, request: &Request) -> Result<()> {
    let mut tx = pool.begin().await?;
    let scope: Option<String> = sqlx::query_scalar("SELECT value FROM meta WHERE key='scope'")
        .fetch_optional(&mut *tx)
        .await?;
    ensure!(scope.is_some(), "Not an initialized keeper journal");
    let busy: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM meta WHERE key IN (?,?)")
        .bind(REQUEST_KEY)
        .bind(ATTEMPT_KEY)
        .fetch_one(&mut *tx)
        .await?;
    ensure!(
        busy == 0,
        "A sweep is already queued or in flight; check `sweep --status`"
    );
    sqlx::query("INSERT INTO meta(key,value) VALUES(?,?)")
        .bind(REQUEST_KEY)
        .bind(serde_json::to_string(request)?)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
/// Removes a queued request that has not been signed yet.
pub async fn cancel_request(pool: &SqlitePool) -> Result<bool> {
    let removed = sqlx::query("DELETE FROM meta WHERE key=?")
        .bind(REQUEST_KEY)
        .execute(pool)
        .await?;
    Ok(removed.rows_affected() == 1)
}
/// The request becomes an attempt, atomically and before any broadcast. The lane must be free.
pub async fn start(pool: &SqlitePool, attempt: &Attempt) -> Result<()> {
    let mut tx = pool.begin().await?;
    // Same predicate as the journal's active nonce lane (signed or submitted attempts).
    let active: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM txs WHERE state IN ('signed','submitted')")
            .fetch_one(&mut *tx)
            .await?;
    ensure!(
        active == 0,
        "A game or epoch attempt owns the nonce lane; sweep not signed"
    );
    let floor: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE((SELECT value FROM meta WHERE key='nonce_floor'),'0') AS INTEGER)",
    )
    .fetch_one(&mut *tx)
    .await?;
    ensure!(
        i64::try_from(attempt.nonce)? >= floor,
        "Sweep nonce below the durable nonce floor"
    );
    let removed = sqlx::query("DELETE FROM meta WHERE key=? AND value=?")
        .bind(REQUEST_KEY)
        .bind(serde_json::to_string(&attempt.request)?)
        .execute(&mut *tx)
        .await?;
    ensure!(
        removed.rows_affected() == 1,
        "Sweep request changed before signing"
    );
    let inserted = sqlx::query("INSERT OR IGNORE INTO meta(key,value) VALUES(?,?)")
        .bind(ATTEMPT_KEY)
        .bind(serde_json::to_string(attempt)?)
        .execute(&mut *tx)
        .await?;
    ensure!(
        inserted.rows_affected() == 1,
        "A sweep is already in flight"
    );
    tx.commit().await?;
    Ok(())
}
/// Persists a replacement or broadcast time. Replacements are committed before broadcast.
pub async fn save(pool: &SqlitePool, attempt: &Attempt) -> Result<()> {
    let updated = sqlx::query("UPDATE meta SET value=? WHERE key=?")
        .bind(serde_json::to_string(attempt)?)
        .bind(ATTEMPT_KEY)
        .execute(pool)
        .await?;
    ensure!(updated.rows_affected() == 1, "Sweep attempt missing");
    Ok(())
}
/// A finalized receipt resolves the attempt: the outcome, the cleared attempt and the nonce floor commit together.
pub async fn finish(pool: &SqlitePool, nonce: u64, outcome: &Outcome) -> Result<()> {
    let next = nonce.checked_add(1).context("Nonce overflow")?;
    let mut tx = pool.begin().await?;
    let removed = sqlx::query("DELETE FROM meta WHERE key=?")
        .bind(ATTEMPT_KEY)
        .execute(&mut *tx)
        .await?;
    ensure!(removed.rows_affected() == 1, "Sweep attempt missing");
    record(&mut tx, outcome).await?;
    sqlx::query("INSERT INTO meta(key,value) VALUES('nonce_floor',?) ON CONFLICT(key) DO UPDATE SET value=CAST(MAX(CAST(meta.value AS INTEGER),CAST(excluded.value AS INTEGER)) AS TEXT)")
        .bind(next.to_string())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
/// A request that cannot be signed is dropped with its reason; nothing was signed.
pub async fn refuse(pool: &SqlitePool, request: &Request, detail: String, now: u64) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM meta WHERE key=? AND value=?")
        .bind(REQUEST_KEY)
        .bind(serde_json::to_string(request)?)
        .execute(&mut *tx)
        .await?;
    record(
        &mut tx,
        &Outcome {
            state: "refused".into(),
            detail,
            to: None,
            value: None,
            tx_hash: None,
            requested_at: request.requested_at,
            finished_at: now,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
async fn record(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, outcome: &Outcome) -> Result<()> {
    sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
        .bind(LAST_KEY)
        .bind(serde_json::to_string(outcome)?)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Operator view without signed bytes.
pub async fn status(pool: &SqlitePool) -> Result<serde_json::Value> {
    let attempt = attempt(pool).await?.map(|a| {
        serde_json::json!({
            "nonce": a.nonce,
            "to": a.to,
            "value_usdc": a.value.parse::<U256>().map(format_usdc).unwrap_or(a.value),
            "requested_at": a.request.requested_at,
            "transactions": a.txs.iter().map(|t| serde_json::json!({"kind": t.kind, "hash": t.hash, "created": t.created})).collect::<Vec<_>>(),
        })
    });
    let request = request(pool).await?.map(|r| {
        serde_json::json!({
            "mode": r.mode,
            "usdc": r.wei.parse::<U256>().map(format_usdc).unwrap_or(r.wei),
            "requested_at": r.requested_at,
        })
    });
    Ok(serde_json::json!({
        "queued": request,
        "in_flight": attempt,
        "last": last(pool).await?,
    }))
}

/// Opens an existing keeper journal for the operator command, without creating or migrating it.
pub async fn open_existing(path: &std::path::Path) -> Result<SqlitePool> {
    if !path.exists() {
        bail!("Keeper journal not found at {}", path.display());
    }
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .busy_timeout(std::time::Duration::from_secs(5));
    Ok(sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    const USDC: u128 = MIN_RESERVE_WEI;
    fn req(mode: Mode, usdc: u128) -> Request {
        Request {
            mode,
            wei: (U256::from(usdc) * U256::from(USDC)).to_string(),
            requested_at: 7,
        }
    }
    #[test]
    fn usdc_amounts_parse_exactly() {
        assert_eq!(parse_usdc("5").unwrap(), U256::from(5 * USDC));
        assert_eq!(parse_usdc("2.5").unwrap(), U256::from(5 * USDC / 2));
        assert_eq!(parse_usdc(".000000000000000001").unwrap(), U256::from(1));
        assert_eq!(parse_usdc("0.10").unwrap(), U256::from(USDC / 10));
        for bad in ["", ".", "-1", "1e3", "1.0000000000000000001", "0x10", "1,5"] {
            assert!(parse_usdc(bad).is_err(), "{bad}");
        }
        assert_eq!(format_usdc(U256::from(5 * USDC / 2)), "2.5");
        assert_eq!(format_usdc(U256::from(3 * USDC)), "3");
    }
    #[test]
    fn plans_keep_the_reserve_and_gas() {
        let gas = U256::from(USDC / 1000);
        let reserve = U256::from(USDC);
        let balance = U256::from(10 * USDC);
        assert_eq!(
            plan(&req(Mode::Amount, 8), balance, gas, reserve),
            Ok(U256::from(8 * USDC))
        );
        assert!(plan(&req(Mode::Amount, 9), balance, gas, reserve).is_err());
        assert!(plan(&req(Mode::Amount, 0), balance, gas, reserve).is_err());
        assert_eq!(
            plan(&req(Mode::Keep, 2), balance, gas, reserve),
            Ok(balance - gas - U256::from(2 * USDC))
        );
        assert!(plan(&req(Mode::Keep, 0), balance, gas, reserve).is_err());
        assert!(plan(&req(Mode::Keep, 10), balance, gas, reserve).is_err());
    }
    fn signed(kind: &str, hash: &str) -> SignedTx {
        SignedTx {
            kind: kind.into(),
            hash: hash.into(),
            raw: "0xsigned".into(),
            gas: 21000,
            fee: "1".into(),
            priority: "1".into(),
            created: 10,
            broadcast: 0,
        }
    }
    #[tokio::test]
    async fn one_sweep_at_a_time_and_resolution_raises_the_nonce_floor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sweep.sqlite");
        let journal = crate::journal::Journal::open(&path, "scope").await.unwrap();
        let pool = open_existing(&path).await.unwrap();
        let request = req(Mode::Amount, 2);
        submit(&pool, &request).await.unwrap();
        assert!(submit(&pool, &request).await.is_err());
        let attempt = Attempt {
            request: request.clone(),
            nonce: 5,
            to: "0x00000000000000000000000000000000000000aa".into(),
            value: request.wei.clone(),
            txs: vec![signed("sweep", "0x01")],
        };
        // A live game attempt owns the lane: the sweep is not started and stays queued.
        sqlx::query("INSERT INTO txs(job,nonce,hash,raw,kind,fee,gas,priority,payload,created) VALUES('1',5,'0xgame','raw','fulfill','1',1,'1','0x',1)")
            .execute(&journal.pool).await.unwrap();
        assert!(start(&journal.pool, &attempt).await.is_err());
        assert_eq!(super::request(&pool).await.unwrap(), Some(request.clone()));
        sqlx::query("UPDATE txs SET state='resolved'")
            .execute(&journal.pool)
            .await
            .unwrap();
        start(&journal.pool, &attempt).await.unwrap();
        assert_eq!(super::request(&pool).await.unwrap(), None);
        assert!(in_flight(&pool).await.unwrap());
        assert!(submit(&pool, &request).await.is_err());
        let status = status(&pool).await.unwrap().to_string();
        assert!(!status.contains("0xsigned"), "{status}");
        // Defense in depth: the journal refuses any game attempt while the sweep owns the lane.
        journal.discovered("9", 100, "0").await.unwrap();
        let game = crate::journal::Attempt {
            id: 0,
            job: "9".into(),
            nonce: 6,
            hash: "0xgame9".into(),
            raw: "raw".into(),
            kind: "fulfill".into(),
            fee: "1".into(),
            state: "signed".into(),
            gas: 1,
            priority: "1".into(),
            payload: "0x".into(),
            created: 1,
            broadcast: 0,
        };
        let refused = journal.signed(&game).await.unwrap_err().to_string();
        assert!(refused.contains("sweep"), "{refused}");
        finish(
            &journal.pool,
            5,
            &Outcome {
                state: "sent".into(),
                detail: String::new(),
                to: Some(attempt.to.clone()),
                value: Some(attempt.value.clone()),
                tx_hash: Some("0x01".into()),
                requested_at: 7,
                finished_at: 20,
            },
        )
        .await
        .unwrap();
        assert!(!in_flight(&pool).await.unwrap());
        assert_eq!(journal.nonce_floor().await.unwrap(), 6);
        assert_eq!(last(&pool).await.unwrap().unwrap().state, "sent");
        // A sweep below the floor is refused before any state changes.
        submit(&pool, &request).await.unwrap();
        assert!(start(&journal.pool, &attempt).await.is_err());
        refuse(&journal.pool, &request, "stale".into(), 30)
            .await
            .unwrap();
        assert_eq!(super::request(&pool).await.unwrap(), None);
        assert_eq!(last(&pool).await.unwrap().unwrap().state, "refused");
    }
}
