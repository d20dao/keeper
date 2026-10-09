//! The audit of soft finality (`FINALITY_MODE=soft`).
//!
//! A soft keeper acts on the sequencer's latest block, which the sequencer can still replace until the block is
//! posted to L1 and final there. Everything it acts on is therefore written down, in the transaction of the action, as a
//! mark (`journal::Mark`): the block of every settled receipt, the decision head of every tick, and the decision head
//! every transaction was signed at. This audit checks those blocks against L1 finality: it reads the `finalized` header, and
//! the chain's own hash of each marked block at or below it. When every one is the mark's, the receipts become finalized
//! receipts, the finalized checkpoint moves up to the highest block and the marks are deleted. When one differs, the
//! sequencer replaced a block the keeper had acted on, or the endpoint that answered serves another fork.
//!
//! One endpoint's word is a suspicion and nothing more (`worker::suspicion`): the keeper holds its sends and asks every
//! endpoint about the block (`verdict`). Only when at least two of them agree that the block has another hash is the
//! mismatch recorded durably (`finality:mismatch`), and the keeper then recovers from it by itself (`worker::recovery`).
//! An operator reads where it stands with `d20dao-keeper finality --status`; `--acknowledge <id>` is never required,
//! and makes the keeper recover from a mismatch that no second endpoint could confirm (`status` and `acknowledge` below,
//! which the command calls).
use crate::{
    journal::{Audit, FinalityState, Journal, Mismatch},
    rpc::{BlockView, MAX_BLOCK_HASHES, Rpc},
};
use alloy_primitives::B256;
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use std::time::Duration;

/// The most distinct blocks one audit reads, in one batch from one endpoint.
pub const AUDIT_BLOCKS: usize = MAX_BLOCK_HASHES;
/// How long the reads of one audit may take inside a tick.
pub const AUDIT_BUDGET: Duration = Duration::from_secs(2);
/// How long each endpoint has to answer when the endpoints are asked about a disputed block.
pub const VERIFY_BUDGET: Duration = Duration::from_secs(3);
/// The hash an endpoint's view gives a block it does not have, and a mismatch the chain's hash of a block the chain no
/// longer has: the sequencer replaced the tip with a shorter chain, and has not made as many blocks again.
pub const ABSENT: alloy_primitives::B256 = alloy_primitives::B256::ZERO;
/// The most marks `finality --status` lists. It counts all of them.
pub const STATUS_MARKS: usize = 500;

/// One audit. `None` when its reads did not finish within `budget`: nothing was written, and the next audit starts
/// afresh. An error is a read that failed on every endpoint, or the journal.
///
/// The reads are the `finalized` header (no state, which a public endpoint serves) and, for the lowest `AUDIT_BLOCKS`
/// distinct blocks of the marks that lie at or below it, their hashes. With more than one endpoint every endpoint is
/// asked for both, each within a deadline inside the budget: a mark is made final only at or below `final_bound` of their
/// headers, and only when every endpoint that answered shows the same hashes. The journal's part is one transaction
/// (`Journal::audit_marks`) and is not part of the budget. A mismatch on record stops the audit before it reads anything;
/// a mismatch the audit finds is returned and not recorded (`Audit::Mismatch`).
pub async fn audit(
    rpc: &Rpc,
    journal: &Journal,
    now: u64,
    budget: Duration,
) -> Result<Option<Audit>> {
    if let Some(recorded) = journal.finality_mismatch().await? {
        return Ok(Some(Audit::Stopped(recorded)));
    }
    let started = tokio::time::Instant::now();
    let reads = async {
        let finalized = rpc.finalized_head().await?;
        let mut numbers = journal.mark_numbers(finalized.number, AUDIT_BLOCKS).await?;
        if numbers.is_empty() {
            return Ok::<_, anyhow::Error>((numbers, Vec::new()));
        }
        if rpc.admitted() < 2 {
            let hashes = rpc.block_hashes(&numbers).await?;
            return Ok((numbers, vec![hashes]));
        }
        // With more than one endpoint, every one is asked, each within a deadline well inside the budget, so that one
        // that does not answer is passed over rather than stalling the audit.
        let views = rpc.finalized_views(started + budget / 2).await;
        let up_to = final_bound(&finalized, &views, now);
        numbers.retain(|number| *number <= up_to);
        if numbers.is_empty() {
            return Ok((numbers, Vec::new()));
        }
        let mut sets = rpc
            .block_hash_views(&numbers, started + budget * 3 / 4)
            .await;
        if sets.is_empty() {
            sets.push(rpc.block_hashes(&numbers).await?);
        }
        Ok((numbers, sets))
    };
    let Ok(read) = tokio::time::timeout(budget, reads).await else {
        return Ok(None);
    };
    let (numbers, sets) = read?;
    if numbers.is_empty() {
        return Ok(Some(Audit::Idle));
    }
    let canonical = |hashes: &[B256]| -> Vec<(u64, String)> {
        numbers
            .iter()
            .zip(hashes)
            .map(|(number, hash)| (*number, hash.to_string()))
            .collect()
    };
    // The endpoints that answered agree: their word is the chain's. When they do not, nothing is made final: the audit
    // compares the journal with a word that is not the journal's, which is a mismatch for the endpoints to settle.
    let first = &sets[0];
    if sets.iter().all(|set| set == first) {
        return Ok(Some(journal.audit_marks(&canonical(first), now).await?));
    }
    let (low, high) = (numbers[0], numbers[numbers.len() - 1]);
    let marks = journal.marks_between(low, high).await?;
    let differs = |set: &Vec<B256>| {
        canonical(set).iter().any(|(number, hash)| {
            marks
                .iter()
                .any(|mark| mark.number == *number && !mark.hash.eq_ignore_ascii_case(hash))
        })
    };
    let Some(other) = sets.iter().find(|set| differs(set)) else {
        tracing::warn!(
            "The endpoints disagree on a block the keeper marked; nothing is made final this audit"
        );
        return Ok(None);
    };
    tracing::warn!(
        "The endpoints disagree on a block the keeper marked; nothing is made final, and the endpoints are asked about it"
    );
    Ok(Some(journal.audit_marks(&canonical(other), now).await?))
}

/// How long a `finalized` header must have stood before the audit takes an endpoint's word for it: L1 finality of an
/// Arbitrum chain's batch takes longer, so a younger header is an endpoint that answers the tag with a later block.
pub const FINALIZED_MIN_AGE_SECONDS: u64 = 600;
/// The highest block the audit makes final, of the `finalized` headers the endpoints answered (`first`, the one the
/// audit read first, and `views`) at wall-clock `now`: the highest of those at least `FINALIZED_MIN_AGE_SECONDS` old, so
/// that neither an endpoint that answers the tag with its latest block nor one stuck on an old one decides it. When none
/// is that old (a chain whose clock is not the wall clock's, as a test chain's), the lowest of them.
pub fn final_bound(first: &crate::rpc::Head, views: &[crate::rpc::Head], now: u64) -> u64 {
    let heads = || std::iter::once(first).chain(views);
    heads()
        .filter(|head| head.timestamp.saturating_add(FINALIZED_MIN_AGE_SECONDS) <= now)
        .map(|head| head.number)
        .max()
        .unwrap_or_else(|| {
            heads()
                .map(|head| head.number)
                .min()
                .unwrap_or(first.number)
        })
}

/// What the endpoints say of a block one of them showed with another hash than the journal's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// At least two endpoints agree on another hash, and more of them than show the journal's: the chain changed.
    /// The endpoints that show anything else (`odd`) are put on cooldown.
    Changed {
        hash: String,
        agreeing: usize,
        odd: Vec<usize>,
    },
    /// The journal's block stands: more endpoints show the journal's hash than show any other hash. The journal's hash
    /// was itself read from an endpoint, which may be the one that shows it now, so it is no witness of its own: one
    /// endpoint against one is a tie, not the journal's. The endpoints that show another hash (`odd`) are put on
    /// cooldown, and the keeper goes on with the others.
    Unchanged { agreeing: usize, odd: Vec<usize> },
    /// Neither: one endpoint alone that shows another block (which is all a keeper with one RPC endpoint has),
    /// endpoints that do not answer, endpoints that show two other hashes, or as many endpoints on the journal's side
    /// as on another's, one against one included. Nobody is put on cooldown; the keeper holds its sends and asks again
    /// later.
    Unresolved,
}
/// The verdict of `views` on a block for which the journal holds `expected`. See `Verdict`.
pub fn verdict(expected: &str, views: &[BlockView]) -> Verdict {
    let same = |view: &BlockView| view.hash.to_string().eq_ignore_ascii_case(expected);
    let agreeing = views.iter().filter(|view| same(view)).count();
    let mut others: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for view in views.iter().filter(|view| !same(view)) {
        *others.entry(view.hash.to_string()).or_default() += 1;
    }
    let best = others
        .iter()
        .map(|(hash, count)| (*count, hash.clone()))
        .max();
    let tied = best
        .as_ref()
        .is_some_and(|(count, _)| others.values().filter(|c| *c == count).count() > 1);
    let odd = |kept: &dyn Fn(&BlockView) -> bool| -> Vec<usize> {
        views
            .iter()
            .filter(|view| !kept(view))
            .map(|view| view.endpoint)
            .collect()
    };
    match best {
        Some((count, hash)) if count >= 2 && count > agreeing && !tied => Verdict::Changed {
            odd: odd(&|view| view.hash.to_string() == hash),
            hash,
            agreeing: count,
        },
        best if agreeing >= 1 && best.as_ref().is_none_or(|(count, _)| *count < agreeing) => {
            Verdict::Unchanged {
                agreeing,
                odd: odd(&same),
            }
        }
        _ => Verdict::Unresolved,
    }
}
/// Ask every endpoint about block `number` (`Rpc::block_views`, within `VERIFY_BUDGET`): the views, and their verdict
/// on the journal's `expected` hash. An endpoint that does not have the block gives a view (`ABSENT`) only when
/// `absent` says the suspicion is that the chain has no such block any more; otherwise it has only not reached it, and
/// gives none.
pub async fn verify(
    rpc: &Rpc,
    number: u64,
    expected: &str,
    absent: bool,
) -> (Vec<BlockView>, Verdict) {
    let mut views = rpc.block_views(number, VERIFY_BUDGET).await;
    if !absent {
        views.retain(|view| view.hash != ABSENT);
    }
    let verdict = verdict(expected, &views);
    (views, verdict)
}

/// Record whether the audit is behind: the oldest mark that waits for it is older than `max_lag_seconds`. That means
/// the chain's batches are not reaching L1, or L1 is not finalizing them; it is a health observation, degraded and not
/// a halt. True when it is behind.
pub async fn observe_lag(journal: &Journal, max_lag_seconds: u64, now: u64) -> Result<bool> {
    let behind = match journal.oldest_mark_created().await? {
        Some(oldest) if now.saturating_sub(oldest) > max_lag_seconds => Some(oldest),
        _ => None,
    };
    match behind {
        Some(oldest) => crate::health::finality_audit_stalled(journal, oldest).await?,
        None => crate::health::finality_audit_current(journal).await?,
    }
    Ok(behind.is_some())
}

/// The journal of the operator's command, which opens an existing journal and creates or migrates nothing (see
/// `sweep::open_existing`): it holds the keeper's tables, or it is not a keeper journal.
async fn initialized(pool: &SqlitePool) -> Result<Journal> {
    let journal = Journal { pool: pool.clone() };
    ensure!(
        journal.meta("scope").await?.is_some(),
        "Not an initialized keeper journal"
    );
    Ok(journal)
}
/// A mismatch as the operator's command prints it.
fn printed(found: &Mismatch) -> Value {
    json!({
        "id": found.id(),
        "kind": found.kind,
        "block": found.number,
        "reference": found.reference,
        "recorded_hash": found.expected,
        "chain_hash": found.actual,
        "detected_at": found.detected_at,
    })
}
/// What `d20dao-keeper finality --status` prints: whether the keeper is clear, holding its sends on a mismatch no two
/// endpoints have confirmed (`suspected`), or recovering from one on record; the mismatch with its id; and the marks the
/// audit has not checked yet. A journal from before soft finality has none of these and says so.
pub async fn status(pool: &SqlitePool) -> Result<Value> {
    let journal = initialized(pool).await?;
    let has_marks: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='soft_marks'",
    )
    .fetch_one(pool)
    .await?;
    let marks = if has_marks == 0 {
        Vec::new()
    } else {
        journal.soft_marks().await?
    };
    let mut by_kind = serde_json::Map::new();
    for mark in &marks {
        let counted = by_kind
            .get(mark.kind.name())
            .and_then(Value::as_u64)
            .unwrap_or(0);
        by_kind.insert(mark.kind.name().to_owned(), json!(counted + 1));
    }
    // The lowest blocks first, which the audit takes first: a journal can hold thousands of them after a long incident.
    let listed: Vec<Value> = marks
        .iter()
        .take(STATUS_MARKS)
        .map(|mark| {
            json!({
                "kind": mark.kind.name(),
                "block": mark.number,
                "hash": mark.hash,
                "reference": mark.reference,
                "status": mark.status,
                "created": mark.created,
            })
        })
        .collect();
    let (state, mismatch, suspected, acknowledged, next) = match journal.finality_state().await? {
        FinalityState::Clear => match journal.suspicion_note().await? {
            None => (
                "clear",
                None,
                None,
                None,
                "No mismatch is on record or suspected; the keeper is not holding its sends.",
            ),
            Some(noted) => (
                "suspected",
                Some(noted.mismatch.clone()),
                Some(json!({
                    "checks": noted.checks,
                    "endpoints": noted.endpoints,
                    "answered": noted.answered,
                })),
                None,
                "One endpoint shows another block than the journal's and no second endpoint has confirmed it: the keeper sends nothing and asks the endpoints again by itself. Nothing is required. With a single RPC endpoint, add a second provider to RPC_URLS. If you are sure the chain changed, `finality --acknowledge <id>` with the id above makes the keeper recover from it at its next tick.",
            ),
        },
        FinalityState::Recovering(found, ack) => (
            "recovering",
            Some(found),
            None,
            ack,
            "The endpoints agree that the chain changed. The keeper is recovering by itself: it takes the chain as it is now, settles the nonces and jobs again, and clears the record. It sends nothing new meanwhile. Nothing is required.",
        ),
    };
    let saved = |key: &'static str| {
        let journal = &journal;
        async move {
            Ok::<_, anyhow::Error>(
                journal
                    .meta(key)
                    .await?
                    .and_then(|saved| serde_json::from_str::<Value>(&saved).ok()),
            )
        }
    };
    Ok(json!({
        "state": state,
        "mismatch": mismatch.as_ref().map(printed),
        "suspected": suspected,
        "acknowledged": acknowledged,
        "next": next,
        "confirmed": saved("finality:recovery:confirmed").await?,
        "recovery": saved("finality:recovery:stats").await?,
        "last_recovery": saved(crate::journal::LAST_RECOVERY_KEY).await?,
        "nonce_floor": journal.nonce_floor().await?,
        "unaudited_marks": {
            "count": marks.len(),
            "by_kind": by_kind,
            "listed": listed.len(),
            "marks": listed,
        },
    }))
}
/// `d20dao-keeper finality --acknowledge <id>`: an operator's word on a mismatch. It is never required: the keeper
/// recovers by itself from a mismatch the endpoints agree on. The id of the mismatch on record only records that someone
/// has looked. The id of a suspected one, which no second endpoint has confirmed, records it, and the running keeper
/// recovers from it at its next tick (one that is stopped, at its next start). Any other id is refused.
pub async fn acknowledge(pool: &SqlitePool, id: &str, now: u64) -> Result<Value> {
    let journal = initialized(pool).await?;
    let ack = journal.acknowledge_finality(id, now).await?;
    Ok(json!({
        "acknowledged": ack.id,
        "acknowledged_at": ack.acknowledged_at,
        "note": "The keeper recovers from the mismatch by itself: it takes the chain as it is now, broadcasts the signed bytes of a transaction the chain no longer has (or fills its nonce with a transfer to itself), reads the jobs again, and clears the record. Follow it with `finality --status` and `health`.",
    }))
}

/// What the keeper says of an audit in its log.
pub fn report(audit: &Audit) {
    match audit {
        Audit::Idle => {
            tracing::debug!("Finality audit: no marked block at or below the finalized head")
        }
        Audit::Stopped(recorded) => {
            tracing::debug!(kind=%recorded.kind,block=recorded.number,"Finality audit stopped by a mismatch on record")
        }
        Audit::Audited {
            blocks,
            receipts,
            checkpoint,
        } => tracing::info!(
            blocks,
            receipts,
            finalized_checkpoint = checkpoint.0,
            "Finality audit: the blocks the keeper acted on are final"
        ),
        Audit::Mismatch(found) => tracing::warn!(
            kind=%found.kind,
            block=found.number,
            reference=%found.reference,
            recorded=%found.expected,
            chain=%found.actual,
            "Finality audit: the endpoint that answered has another block than the one this keeper acted on. Nothing is sent while the other endpoints are asked about it"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        health,
        journal::{Mark, MarkKind, write_mark},
    };

    async fn put(journal: &Journal, kind: MarkKind, number: u64, created: u64) {
        let mut tx = journal.pool.begin().await.unwrap();
        write_mark(
            &mut tx,
            &Mark {
                kind,
                number,
                hash: format!("0xb{number}"),
                reference: "0xtx".into(),
                created,
                status: None,
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    /// R4 M4: an endpoint that answers `finalized` with its latest block (too young to be L1-final) and one stuck on an
    /// old block decide nothing; the most advanced header old enough does. Without one old enough, the lowest.
    #[test]
    fn the_final_bound_ignores_young_and_stuck_finalized_headers() {
        let head = |number: u64, timestamp: u64| crate::rpc::Head {
            number,
            hash: alloy_primitives::B256::ZERO,
            timestamp,
            base_fee: 0,
        };
        let now = 10_000;
        let honest = head(900, now - 900);
        let aliased = head(1_000, now - 1);
        let stuck = head(100, now - 9_000);
        assert_eq!(
            final_bound(&aliased, &[honest.clone(), stuck.clone()], now),
            900
        );
        assert_eq!(
            final_bound(&stuck, &[aliased.clone(), honest.clone()], now),
            900
        );
        assert_eq!(final_bound(&aliased, &[], now), 1_000);
        assert_eq!(final_bound(&aliased, &[head(990, now - 5)], now), 990);
    }
    #[test]
    fn the_page_and_the_budget_are_the_designs() {
        assert_eq!(AUDIT_BLOCKS, 64);
        assert_eq!(AUDIT_BUDGET, Duration::from_secs(2));
        assert_eq!(VERIFY_BUDGET, Duration::from_secs(3));
    }

    /// A view of endpoint `endpoint` that has the hash `byte` repeated for the block.
    fn view(endpoint: usize, byte: u8) -> BlockView {
        BlockView {
            endpoint,
            hash: alloy_primitives::B256::repeat_byte(byte),
            finalized: None,
        }
    }
    fn hash(byte: u8) -> String {
        alloy_primitives::B256::repeat_byte(byte).to_string()
    }

    #[test]
    fn two_endpoints_that_agree_on_another_hash_say_the_chain_changed() {
        let journal = hash(0xaa);
        for (views, odd) in [
            (vec![view(0, 0xbb), view(1, 0xbb)], vec![]),
            (vec![view(0, 0xbb), view(1, 0xbb), view(2, 0xaa)], vec![2]),
            (vec![view(0, 0xbb), view(1, 0xcc), view(2, 0xbb)], vec![1]),
        ] {
            assert_eq!(
                verdict(&journal, &views),
                Verdict::Changed {
                    hash: hash(0xbb),
                    agreeing: 2,
                    odd
                },
                "{views:?}"
            );
        }
        // The journal's hash is compared as the chain writes it, in any case.
        assert_eq!(
            verdict(
                &journal.to_uppercase().replace("0X", "0x"),
                &[view(0, 0xaa)]
            ),
            Verdict::Unchanged {
                agreeing: 1,
                odd: vec![]
            }
        );
    }

    #[test]
    fn an_endpoint_that_shows_another_block_is_outvoted_by_one_that_shows_the_journals() {
        let journal = hash(0xaa);
        for (views, agreeing, odd) in [
            (
                vec![view(0, 0xaa), view(1, 0xbb), view(2, 0xaa)],
                2,
                vec![1],
            ),
            // One endpoint alone that shows the journal's hash again: the suspicion was the endpoint's.
            (vec![view(0, 0xaa)], 1, vec![]),
            // Two that agree on another hash, but more that show the journal's.
            (
                vec![
                    view(0, 0xbb),
                    view(1, 0xbb),
                    view(2, 0xaa),
                    view(3, 0xaa),
                    view(4, 0xaa),
                ],
                3,
                vec![0, 1],
            ),
        ] {
            assert_eq!(
                verdict(&journal, &views),
                Verdict::Unchanged { agreeing, odd },
                "{views:?}"
            );
        }
    }

    #[test]
    fn without_two_endpoints_that_agree_and_without_the_journals_block_nothing_is_decided() {
        let journal = hash(0xaa);
        for views in [
            // No endpoint answered.
            vec![],
            // A keeper with one endpoint, which shows another block.
            vec![view(0, 0xbb)],
            // Two endpoints that show two other blocks.
            vec![view(0, 0xbb), view(1, 0xcc)],
            // One endpoint against one: the journal's hash was read from an endpoint, perhaps the one that shows it now,
            // so it is no second witness.
            vec![view(0, 0xbb), view(1, 0xaa)],
            vec![view(0, 0xaa), view(1, 0xbb)],
            // Three endpoints, three hashes.
            vec![view(0, 0xbb), view(1, 0xcc), view(2, 0xaa)],
            // As many on each side.
            vec![view(0, 0xbb), view(1, 0xbb), view(2, 0xaa), view(3, 0xaa)],
            // Two other blocks, each with two endpoints.
            vec![view(0, 0xbb), view(1, 0xbb), view(2, 0xcc), view(3, 0xcc)],
        ] {
            assert_eq!(verdict(&journal, &views), Verdict::Unresolved, "{views:?}");
        }
    }

    #[tokio::test]
    async fn the_audit_is_behind_only_when_the_oldest_mark_is_older_than_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(&dir.path().join("lag.sqlite"), "scope")
            .await
            .unwrap();
        // A head mark of long ago is rewritten by the next tick and is no backlog; nothing to audit is nothing behind.
        j.soft_decision(10, "0xh10", 5).await.unwrap();
        assert!(!observe_lag(&j, 600, 100_000).await.unwrap());
        assert!(
            j.meta("health:finality_audit_stalled")
                .await
                .unwrap()
                .is_none()
        );

        // A mark written at 1,000: at the limit it is not behind, a second past it is.
        put(&j, MarkKind::Sign, 7, 1_000).await;
        assert!(!observe_lag(&j, 600, 1_600).await.unwrap());
        assert!(
            j.meta("health:finality_audit_stalled")
                .await
                .unwrap()
                .is_none()
        );
        assert!(observe_lag(&j, 600, 1_601).await.unwrap());
        assert_eq!(
            j.meta("health:finality_audit_stalled").await.unwrap(),
            Some("1000".into())
        );
        // It is a fault of the health report, degraded and no halt, whether or not the keeper sends, and it stands over
        // a restart.
        for send in [true, false] {
            let status = health::assess(&j, send, 1_601, 20, None, 120)
                .await
                .unwrap();
            assert_eq!(status.faults, ["finality_audit_stalled"]);
            assert!(!status.healthy);
        }
        assert!(j.finality_mismatch().await.unwrap().is_none());
        let path = dir.path().join("lag.sqlite");
        j.pool.close().await;
        let j = Journal::open(&path, "scope").await.unwrap();
        assert!(observe_lag(&j, 600, 1_700).await.unwrap());

        // The audit checks the mark, and the observation goes with the backlog.
        j.audit_marks(&[(7, "0xb7".into())], 1_800).await.unwrap();
        assert!(!observe_lag(&j, 600, 1_800).await.unwrap());
        assert!(
            j.meta("health:finality_audit_stalled")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            health::assess(&j, true, 1_800, 20, None, 120)
                .await
                .unwrap()
                .healthy
        );
        j.pool.close().await;
    }

    /// The pool of the operator's command on a fresh journal, and the journal beside it.
    async fn command(dir: &std::path::Path) -> (SqlitePool, Journal) {
        let path = dir.join("command.sqlite");
        let journal = Journal::open(&path, "scope").await.unwrap();
        let pool = crate::sweep::open_existing(&path).await.unwrap();
        (pool, journal)
    }
    fn found() -> crate::journal::Mismatch {
        crate::journal::Mismatch {
            kind: "receipt".into(),
            number: 42,
            reference: "0xtx".into(),
            expected: "0xrecorded".into(),
            actual: "0xchain".into(),
            detected_at: 1_000,
        }
    }

    #[tokio::test]
    async fn the_status_tells_clear_suspected_and_recovering_apart_and_lists_the_marks() {
        let dir = tempfile::tempdir().unwrap();
        let (pool, j) = command(dir.path()).await;
        let clear = status(&pool).await.unwrap();
        assert_eq!(clear["state"], "clear");
        assert_eq!(clear["mismatch"], Value::Null);
        assert_eq!(clear["suspected"], Value::Null);
        assert_eq!(clear["acknowledged"], Value::Null);
        assert_eq!(clear["unaudited_marks"]["count"], 0);

        put(&j, MarkKind::Receipt, 40, 500).await;
        put(&j, MarkKind::Sign, 41, 501).await;
        // A mismatch one endpoint showed and none confirmed: the note the keeper leaves for the operator.
        j.note_suspicion(&crate::journal::Suspected {
            mismatch: found(),
            checks: 4,
            endpoints: 1,
            answered: 1,
        })
        .await
        .unwrap();
        let suspected = status(&pool).await.unwrap();
        assert_eq!(suspected["state"], "suspected");
        assert_eq!(
            suspected["mismatch"],
            json!({
                "id": found().id(),
                "kind": "receipt",
                "block": 42,
                "reference": "0xtx",
                "recorded_hash": "0xrecorded",
                "chain_hash": "0xchain",
                "detected_at": 1_000,
            })
        );
        assert_eq!(
            suspected["suspected"],
            json!({"checks": 4, "endpoints": 1, "answered": 1})
        );
        let next = suspected["next"].as_str().unwrap();
        assert!(
            next.contains("Nothing is required") && next.contains("RPC_URLS"),
            "{next}"
        );
        assert!(next.contains("finality --acknowledge <id>"), "{next}");
        // The marks the audit has not checked, lowest block first, with what the audit will compare and the recovery
        // read again.
        assert_eq!(suspected["unaudited_marks"]["count"], 2);
        assert_eq!(
            suspected["unaudited_marks"]["marks"][0],
            json!({"kind": "receipt", "block": 40, "hash": "0xb40", "reference": "0xtx", "status": null, "created": 500})
        );
        assert_eq!(suspected["unaudited_marks"]["marks"][1]["kind"], "sign");

        // On record: the keeper recovers by itself, and no acknowledgement is waited for.
        j.clear_suspicion_note().await.unwrap();
        j.confirm_finality_mismatch(
            &found(),
            "finality:recovery:confirmed",
            &json!({"agreeing": 2, "answered": 2}),
        )
        .await
        .unwrap();
        let recovering = status(&pool).await.unwrap();
        assert_eq!(recovering["state"], "recovering");
        assert_eq!(recovering["mismatch"]["id"], found().id());
        assert_eq!(recovering["suspected"], Value::Null);
        assert_eq!(recovering["acknowledged"], Value::Null);
        assert_eq!(
            recovering["confirmed"],
            json!({"agreeing": 2, "answered": 2})
        );
        assert!(
            recovering["next"]
                .as_str()
                .unwrap()
                .contains("Nothing is required")
        );
        // An operator's acknowledgement is shown when there is one.
        let ack = acknowledge(&pool, &found().id(), 2_000).await.unwrap();
        assert_eq!(ack["acknowledged"], found().id());
        assert_eq!(
            status(&pool).await.unwrap()["acknowledged"],
            json!({"id": found().id(), "acknowledged_at": 2_000})
        );
        assert_eq!(status(&pool).await.unwrap()["last_recovery"], Value::Null);

        // The recovery's progress and its last result are shown.
        j.set_meta("finality:recovery:stats", r#"{"reopened_nonces":[3]}"#)
            .await
            .unwrap();
        assert_eq!(
            status(&pool).await.unwrap()["recovery"]["reopened_nonces"],
            json!([3])
        );
        j.clear_finality_incident(&json!({"id": "x"}))
            .await
            .unwrap();
        let after = status(&pool).await.unwrap();
        assert_eq!(after["state"], "clear");
        assert_eq!(after["recovery"], Value::Null);
        assert_eq!(after["confirmed"], Value::Null);
        assert_eq!(after["last_recovery"], json!({"id": "x"}));
        pool.close().await;
        j.pool.close().await;
    }

    #[tokio::test]
    async fn the_status_counts_every_mark_and_lists_the_lowest_ones() {
        let dir = tempfile::tempdir().unwrap();
        let (pool, j) = command(dir.path()).await;
        for number in 1..=(STATUS_MARKS as u64 + 100) {
            put(&j, MarkKind::Sign, 1_000 + number, 500).await;
        }
        put(&j, MarkKind::Receipt, 5, 500).await;
        let status = status(&pool).await.unwrap();
        let total = STATUS_MARKS + 100 + 1;
        assert_eq!(status["unaudited_marks"]["count"], total);
        assert_eq!(status["unaudited_marks"]["listed"], STATUS_MARKS);
        assert_eq!(
            status["unaudited_marks"]["by_kind"],
            json!({"sign": total - 1, "receipt": 1})
        );
        let listed = status["unaudited_marks"]["marks"].as_array().unwrap();
        assert_eq!(listed.len(), STATUS_MARKS);
        assert_eq!(listed[0]["block"], 5, "the lowest block first");
        let blocks: Vec<u64> = listed
            .iter()
            .map(|m| m["block"].as_u64().unwrap())
            .collect();
        assert!(blocks.windows(2).all(|pair| pair[0] <= pair[1]));
        pool.close().await;
        j.pool.close().await;
    }

    #[tokio::test]
    async fn the_command_acknowledges_only_the_mismatch_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let (pool, j) = command(dir.path()).await;
        // Nothing on record or suspected.
        let error = acknowledge(&pool, "abcdefabcdef", 5).await.unwrap_err();
        assert!(
            error.to_string().contains("nothing to acknowledge"),
            "{error}"
        );
        j.record_finality_mismatch(&found()).await.unwrap();
        // Another id, or none that reads as one, acknowledges nothing and says which id is the one.
        for wrong in ["abcdefabcdef", "", "0x1234"] {
            let error = acknowledge(&pool, wrong, 5).await.unwrap_err().to_string();
            assert!(error.contains(&found().id()), "{wrong}: {error}");
        }
        assert_eq!(
            j.finality_state().await.unwrap(),
            crate::journal::FinalityState::Recovering(found(), None)
        );
        // The id is the operator's; once acknowledged the same command is the same answer.
        let first = acknowledge(&pool, &found().id(), 7).await.unwrap();
        assert_eq!(first["acknowledged_at"], 7);
        assert_eq!(
            acknowledge(&pool, &found().id(), 9).await.unwrap()["acknowledged_at"],
            7
        );
        pool.close().await;
        j.pool.close().await;
    }

    #[tokio::test]
    async fn a_journal_from_before_soft_finality_reads_as_clear_and_nothing_is_created_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let (pool, j) = command(dir.path()).await;
        sqlx::raw_sql("DROP TABLE soft_marks")
            .execute(&pool)
            .await
            .unwrap();
        let old = status(&pool).await.unwrap();
        assert_eq!(old["state"], "clear");
        assert_eq!(old["unaudited_marks"]["count"], 0);
        let tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='soft_marks'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(tables, 0, "the command creates and migrates nothing");
        // A database that is not the keeper's journal is refused by both commands.
        let other = dir.path().join("other.sqlite");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&other)
            .create_if_missing(true);
        let alien = sqlx::sqlite::SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .unwrap();
        sqlx::raw_sql("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL)")
            .execute(&alien)
            .await
            .unwrap();
        for result in [
            status(&alien).await.map(|_| ()),
            acknowledge(&alien, "abcdefabcdef", 1).await.map(|_| ()),
        ] {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("Not an initialized keeper journal")
            );
        }
        alien.close().await;
        pool.close().await;
        j.pool.close().await;
    }

    #[tokio::test]
    async fn a_mismatch_on_record_stops_the_audit_before_it_reads_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(&dir.path().join("stopped.sqlite"), "scope")
            .await
            .unwrap();
        let found = crate::journal::Mismatch {
            kind: "receipt".into(),
            number: 3,
            reference: "0xtx".into(),
            expected: "0xa".into(),
            actual: "0xb".into(),
            detected_at: 9,
        };
        assert!(j.record_finality_mismatch(&found).await.unwrap());
        // An endpoint that nothing listens on: the audit never asks it.
        let rpc = Rpc::new(vec!["http://127.0.0.1:9".into()]).unwrap();
        let audit = audit(&rpc, &j, 10, AUDIT_BUDGET).await.unwrap();
        assert_eq!(audit, Some(Audit::Stopped(found)));
        j.pool.close().await;
    }
}
