//! The recovery from a finality mismatch on record (keeper task C4, design 3.2.2 rule 6, and the owner's rule that a
//! keeper runs without anyone's step).
//!
//! A soft keeper acts on the sequencer's latest block, and writes down every block it acts on (`journal::Mark`). When at
//! least two endpoints agree that a block it acted on is not the chain's (`suspicion`), the mismatch is recorded, and from
//! that tick the keeper recovers by itself, one step per tick; nobody's acknowledgement is waited for. Each step reads the
//! journal and the chain afresh, so a restart at any point resumes where the work stood:
//!
//! 1. The soft checkpoint and the `head` mark move to the decision head of this tick: the chain the endpoints serve now
//!    is the one the keeper goes on from.
//! 2. The nonce lane is put right. A nonce the chain has not got although the journal has settled it (`chain nonce <
//!    nonce floor`), and a nonce whose receipt or nonce mark is no longer the chain's (the receipt vanished, or sits in
//!    another block), is taken back, lowest first, and the nonce floor is never lowered:
//!    - When the journal kept the signed bytes of its last transaction, and they are still worth sending, the nonce is
//!      reopened (`Journal::reopen_nonce`): its attempts return to `submitted`, the request, batch or epoch they served
//!      with them, and the marks that justified the old settlement are deleted. The ordinary reconciliation of the tick
//!      then settles it from the chain as it is: it finds the receipt where it lies, or broadcasts the bytes the journal
//!      kept, in nonce order, one nonce at a time (and cancels them when their request has expired meanwhile).
//!    - When the chain has not used the nonce and the journal has no bytes for it (the transfer of an operator sweep is
//!      not kept once it is settled), or the bytes carry a proof whose input the chain has changed (a seed it no longer
//!      has, which would only revert and burn gas), the nonce is filled with a zero-value transfer to the keeper's own
//!      wallet, priced as a cancellation (`fill`). A request whose proof was stale is proved again for the chain as it
//!      is, at the next step.
//!    - When the chain has used the nonce and the journal has no bytes for it, the nonce is marked as found consumed at
//!      this head.
//!
//!    Any other attempt that is live is parked meanwhile, since reconciliation needs one lane, and returns when the lane
//!    is put right.
//! 3. The marks are walked in pages of the audit's size. A stale `head` or `sign` mark has nothing to settle again and
//!    is deleted; a stale mark of a receipt or a nonce leads to its nonce (step 2); a receipt of a transaction the
//!    journal does not keep (an operator sweep's) is looked up and re-marked where it lies, or dropped with a report.
//! 4. The jobs are re-read from chain state at the decision head, a page of them at a time, from the time of the
//!    finalized head on (nothing below it can have been replaced): one the chain has settled gets its state; one that is
//!    open but is not what it was (another deadline, or a proof for a seed the chain does not have) loses its proof and
//!    returns to `pending`; one that is open and was marked served returns to `prepared` or `pending`. Discovery goes
//!    back to the first request of the chain as it is now that was open at that time, so that requests the chain holds
//!    under ids that discovery had passed are found. Epochs the registry no longer has return to `prepared` or `pending`.
//! 5. The record, any acknowledgement and the recovery's own keys are deleted, and the parked attempts return, in one
//!    transaction (`Journal::clear_finality_incident`). The audit runs again from the next tick, the keeper sends, and
//!    the owner is told once, in plain Turkish, that nothing is required of them (`recovered_text`).
//!
//! Until then nothing new is started (`Worker::may_send` is false), and the only signing is that of the one lane the
//! recovery put right: reconciliation's for a reopened nonce, the fill's for a filled one.
//!
//! What a step reads of the coordinator depends on its kind (`CoordinatorKind`). The proof context of a request and the
//! epoch registry exist only on an epoch coordinator, so the steps that read them are the epoch kind's alone. A round
//! coordinator's requests are read with `getRoundRequest`: the fingerprint of a request (design C, 3.7) takes the place
//! of the epoch coordinator's proof context, and a request the coordinator does not have any more is `vanished`.
use super::*;
use crate::journal::Reopened;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::atomic::Ordering};

/// The blocks one page of the walk through the marks reads: the audit's page.
const SCAN_PAGE: usize = crate::finality::AUDIT_BLOCKS;
/// How many pages one tick reads.
const SCAN_PAGES_PER_TICK: usize = 4;
/// How many jobs one tick reads again.
const JOB_PAGE: usize = 64;
/// How many of the most recent committed epochs are read again.
const EPOCH_PAGE: i64 = 32;
/// Seconds after which a fill that is not included is replaced by one that pays more, and how many fills one nonce gets.
const FILL_REPLACE_SECONDS: u64 = 10;
const FILL_ATTEMPTS: usize = 4;
/// How long the reads of one tick's recovery may take before it leaves the rest for the next tick: well within the tick's
/// own budget (TICK_TIMEOUT_SECONDS, 20 by default), so that a slow endpoint slows the recovery and never fails the tick.
pub(super) const STEP_BUDGET: std::time::Duration = std::time::Duration::from_secs(6);

/// The next block number to read from: every mark below it was read and is the chain's, or has been dealt with.
const SCAN_KEY: &str = "finality:recovery:scan";
/// The chain time from which the jobs are read again: that of the finalized head, which no sequencer can replace a block
/// below, and no older than twice the audit's lag limit (a request is open for 60 seconds, so a job older than that is
/// past its deadline whatever the chain says of it).
const SINCE_KEY: &str = "finality:recovery:since";
/// The last job read again: its deadline and id.
const JOBS_KEY: &str = "finality:recovery:jobs";
/// The search for the first request of the chain as it is now that was open at the time of `SINCE_KEY`, while it goes on
/// (`low,high`), and `done` once discovery was taken back to it.
const REWIND_KEY: &str = "finality:recovery:rewind";
/// What the recovery has done so far, for the operator's status and the record it leaves.
const STATS_KEY: &str = "finality:recovery:stats";
/// The fill of a nonce under way (`Fill`).
const FILL_KEY: &str = "finality:recovery:fill";
/// Since when, in wall-clock seconds, the walk through the marks has waited for the decision head to reach marks above
/// it.
const ABOVE_HEAD_KEY: &str = "finality:recovery:above_head_since";
/// How long the walk waits for a decision head below marks the keeper wrote before it takes those marks as stale: far
/// longer than an endpoint lags, far shorter than a request's 60 seconds.
const ABOVE_HEAD_WAIT_SECONDS: u64 = 20;

/// Which coordinator the keeper serves (COORDINATOR_KIND), as far as the recovery is concerned. The epoch coordinator
/// (Arc's ABI) answers a request's proof context (`getProofContext`) and names an epoch registry (`getEpoch`); the round
/// coordinator has neither. The steps that read them are the epoch kind's alone, and a round coordinator brings its own:
/// its requests are read with `getRoundRequest`, and the comparison of a request's fingerprint takes the place of the
/// seed's.
pub use crate::config::CoordinatorKind;

/// What a tick of the recovery left.
pub(super) enum Recovery {
    /// More to do; the tick goes on and the next one takes the next step.
    Working,
    /// The record is cleared: the keeper is as it was before the mismatch, on the chain as it is now. `resent` when it
    /// broadcast a transaction again or filled a nonce.
    Cleared { resent: bool },
}

/// What the recovery has done, kept under `finality:recovery:stats` while it works and in the record it leaves.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Stats {
    /// The nonces taken back into the lane, in the order the recovery took them.
    reopened_nonces: Vec<u64>,
    /// Of those, the ones whose transaction reconciliation broadcast again (the others had their receipt elsewhere).
    resent_nonces: Vec<u64>,
    /// The nonces filled with a transfer to the keeper's own wallet, in the order the recovery took them.
    filled_nonces: Vec<u64>,
    /// Of those, the ones whose transaction carried a proof for an input the chain had changed.
    stale_nonces: Vec<u64>,
    /// Stale `head` and `sign` marks deleted.
    marks_dropped: u64,
    /// Receipt and nonce marks of nonces the journal cannot reopen, replaced by a mark of the chain as it is.
    marks_replaced: u64,
    /// Jobs the chain had settled, which now say so.
    jobs_settled: u64,
    /// Jobs that were open on the chain and marked served: back to `prepared` or `pending`.
    jobs_reopened: u64,
    /// Jobs whose inputs changed: proof dropped, back to `pending`.
    jobs_reproved: u64,
    /// Committed epochs the registry no longer has.
    epochs_reopened: u64,
    /// Committed epochs whose record the registry refused to give: left as they are.
    epochs_unread: u64,
}

/// One walk through a page or more of the marks.
pub(super) struct Scan {
    /// The nonces that have a stale receipt or nonce mark, with those marks.
    nonces: BTreeMap<u64, Vec<Mark>>,
    /// Every mark has been read.
    pub(super) complete: bool,
}

/// A nonce the recovery fills with a zero-value transfer to the keeper's own wallet, kept under `FILL_KEY` until the
/// chain has used the nonce.
#[derive(Debug, Serialize, Deserialize)]
struct Fill {
    nonce: u64,
    /// Whether the journal's transaction of the nonce carried a stale proof (else it had no bytes).
    stale: bool,
    /// The hashes of the journal's own attempts of the nonce: the receipt of one of them, should the sequencer include
    /// it after all, settles the nonce as well as a fill's.
    known: Vec<String>,
    /// The fills signed so far, the last one in the lane.
    txs: Vec<crate::sweep::SignedTx>,
}

/// What the owner is told once the keeper has recovered from `incident` by itself: in plain Turkish, that nothing is
/// required of them. `resent` when it broadcast a transaction again or filled a nonce.
pub(super) fn recovered_text(chain_id: u64, incident: &Mismatch, resent: bool) -> String {
    let place = if [4663, 46630].contains(&chain_id) {
        "Robinhood'da"
    } else {
        "Zincirde"
    };
    let done = if resent {
        "keeper etkilenen işlemleri kendisi yeniden gönderdi ve çalışmaya devam ediyor."
    } else {
        "keeper kayıtlarını yeni zincire göre kendisi düzeltti ve çalışmaya devam ediyor. Yeniden gönderilmesi gereken bir işlem yoktu."
    };
    format!(
        "{place} sequencer bir bloğu değiştirdi; {done} Bir şey yapmanız gerekmiyor.\nBlok: {}, kayıt: {}",
        incident.number,
        incident.id()
    )
}

/// What the owner is told when the sequencer's replacement of a block took an operator sweep's transfer off the chain:
/// in plain Turkish, the transaction and what to do. The keeper does not queue a transfer again by itself.
pub(super) fn lost_sweep_text(tx_hash: &str) -> String {
    format!(
        "Sequencer bir bloğu değiştirdi ve cüzdandan yapılan bir çekim (sweep) işlemi zincirden düştü; para cüzdanda kaldı.
Yapmanız gereken: çekim hâlâ isteniyorsa `d20dao-keeper sweep` komutuyla yeniden kuyruğa alın. İstenmiyorsa bir şey yapmanız gerekmiyor; keeper çalışmaya devam ediyor.
İşlem: {tx_hash}"
    )
}

/// What the last transaction of `fill` needs up front: its gas limit times its max fee per gas.
fn last_need(fill: &Fill) -> Result<u128> {
    let last = fill
        .txs
        .last()
        .ok_or_else(|| anyhow::anyhow!("Fill has no transaction"))?;
    Ok(last
        .fee
        .parse::<u128>()?
        .saturating_mul(u128::from(last.gas)))
}

impl Worker {
    /// One step of the recovery from `incident`; `ack` is an operator's acknowledgement of it, if there is one. See the
    /// module for the steps.
    pub(super) async fn recover_finality(
        &self,
        head: &Head,
        incident: &Mismatch,
        ack: Option<&crate::journal::Acknowledgement>,
    ) -> Result<Recovery> {
        let mut stats: Stats = match self.journal.meta(STATS_KEY).await? {
            Some(saved) => serde_json::from_str(&saved)?,
            None => Stats::default(),
        };
        let step = self.recovery_step(head, incident, ack, &mut stats).await;
        // What the step did stays on record whether or not it got to the end; the clear deletes it with the rest.
        if !matches!(step, Ok(Recovery::Cleared { .. })) {
            self.journal
                .set_meta(STATS_KEY, &serde_json::to_string(&stats)?)
                .await?;
        }
        step
    }
    async fn recovery_step(
        &self,
        head: &Head,
        incident: &Mismatch,
        ack: Option<&crate::journal::Acknowledgement>,
        stats: &mut Stats,
    ) -> Result<Recovery> {
        let now = crate::health::now()?;
        let began = tokio::time::Instant::now();
        // 1. The chain the endpoints serve now is the one the keeper goes on from.
        self.journal
            .rebase_soft_decision(head.number, &head.hash.to_string(), now)
            .await?;

        // 2. A nonce being filled, or one the recovery reopened, is the lane. Nothing else is done while it is not
        //    settled: the fill here, and reconciliation, which follows the gate in the tick, for a reopened one.
        if let Some(fill) = self.fill().await? {
            self.recovery_lane.store(true, Ordering::Relaxed);
            self.step_fill(head, fill).await?;
            return Ok(Recovery::Working);
        }
        let floor = self.journal.nonce_floor().await?;
        let reopened = self
            .journal
            .unresolved()
            .await?
            .iter()
            .any(|attempt| u64::try_from(attempt.nonce).is_ok_and(|nonce| nonce < floor));
        if reopened {
            self.recovery_lane.store(true, Ordering::Relaxed);
            return Ok(Recovery::Working);
        }
        self.recovery_lane.store(false, Ordering::Relaxed);

        // 3. Which nonce is next: the lowest the chain has not got although the journal settled it, or the lowest whose
        //    marks are no longer the chain's.
        let chain_nonce = self.rpc.decision_nonce(self.tx_key.address(), head).await?;
        let shorter = incident.actual == crate::finality::ABSENT.to_string();
        let scan = self.scan_marks(head, shorter, stats).await?;
        let vacated = (chain_nonce < floor).then_some(chain_nonce);
        let Some(nonce) = vacated.into_iter().chain(scan.nonces.keys().copied()).min() else {
            // Nothing to reopen: the lane that was parked for a nonce is the lane again.
            self.journal.unpark_lanes().await?;
            return self
                .finish_recovery(head, incident, ack, stats, scan, (now, began))
                .await;
        };
        // Bytes the chain could take, that carry a proof for an input it has changed: the nonce is filled instead.
        if nonce == chain_nonce && self.stale_in_lane(nonce, head).await? {
            self.start_fill(head, nonce, true, stats).await?;
            return Ok(Recovery::Working);
        }
        match self
            .journal
            .reopen_nonce(nonce, head.timestamp, now)
            .await?
        {
            Reopened::Lane => {
                self.recovery_lane.store(true, Ordering::Relaxed);
                stats.reopened_nonces.push(nonce);
                tracing::warn!(nonce,vacated=(vacated==Some(nonce)),mismatch=%incident.id(),
                    "Finality recovery: the nonce is back in the lane, to be settled again from the chain as it is now");
            }
            Reopened::NoBytes if nonce < chain_nonce => {
                // The chain has used the nonce, and the journal has no signed bytes to say by which transaction. That is
                // a nonce found consumed at this head, which is how reconciliation records a nonce it cannot match to
                // a receipt.
                let stale = scan.nonces.get(&nonce).map_or(&[][..], Vec::as_slice);
                let fresh = self.nonce_mark(head, i64::try_from(nonce)?)?;
                self.journal.replace_marks(stale, fresh.as_ref()).await?;
                stats.marks_replaced += u64::try_from(stale.len())?;
            }
            Reopened::NoBytes => self.start_fill(head, nonce, false, stats).await?,
        }
        Ok(Recovery::Working)
    }
    /// The last steps, once there is no nonce left to reopen: read the jobs and the epochs again, and clear the record.
    async fn finish_recovery(
        &self,
        head: &Head,
        incident: &Mismatch,
        ack: Option<&crate::journal::Acknowledgement>,
        stats: &mut Stats,
        scan: Scan,
        (now, began): (u64, tokio::time::Instant),
    ) -> Result<Recovery> {
        if !scan.complete {
            return Ok(Recovery::Working);
        }
        // 4. The lane is right and every mark is the chain's. What is left is to read the jobs and the epochs again.
        if !self.reclassify_jobs(head, stats, began).await? {
            return Ok(Recovery::Working);
        }
        if self.coordinator_kind() == CoordinatorKind::Epoch {
            self.reclassify_epochs(head, stats).await?;
        }

        // 5. Done. A reopened nonce was broadcast again when reconciliation did not find its receipt where it lies.
        for &nonce in &stats.reopened_nonces {
            let broadcast: Option<i64> =
                sqlx::query_scalar("SELECT MAX(broadcast) FROM txs WHERE nonce=?")
                    .bind(i64::try_from(nonce)?)
                    .fetch_one(&self.journal.pool)
                    .await?;
            if broadcast.unwrap_or(0) > 0 && !stats.resent_nonces.contains(&nonce) {
                stats.resent_nonces.push(nonce);
            }
        }
        let confirmed = self
            .journal
            .meta(super::suspicion::CONFIRMED_KEY)
            .await?
            .and_then(|saved| serde_json::from_str::<serde_json::Value>(&saved).ok());
        let summary = serde_json::json!({
            "mismatch": incident,
            "id": incident.id(),
            "confirmed": confirmed,
            "acknowledged_at": ack.map(|ack| ack.acknowledged_at),
            "recovered_at": now,
            "recovered_on": {"block": head.number, "hash": head.hash.to_string()},
            "coordinator": self.coordinator_kind().name(),
            "done": &*stats,
        });
        self.journal.clear_finality_incident(&summary).await?;
        tracing::warn!(mismatch=%incident.id(),reopened_nonces=?stats.reopened_nonces,filled_nonces=?stats.filled_nonces,
            stale_nonces=?stats.stale_nonces,resent_nonces=?stats.resent_nonces,marks_dropped=stats.marks_dropped,marks_replaced=stats.marks_replaced,
            jobs_settled=stats.jobs_settled,jobs_reopened=stats.jobs_reopened,jobs_reproved=stats.jobs_reproved,
            epochs_reopened=stats.epochs_reopened,
            "Finality recovery complete: the keeper is on the chain as it is now and sends again");
        Ok(Recovery::Cleared {
            resent: !stats.filled_nonces.is_empty() || !stats.resent_nonces.is_empty(),
        })
    }

    /// Walk the marks from where the last tick stopped, a page of the audit's size at a time, at most
    /// `SCAN_PAGES_PER_TICK` pages. A mark is stale when the chain has another hash for its block (or has no such block).
    /// Stale `head` and `sign` marks are deleted at once. The stale marks of receipts and nonces are returned, by nonce,
    /// and the walk stops at their page: the nonce is reopened, and the page is read again when it is settled.
    ///
    /// A mark above the decision head is stale only when the endpoints confirmed that the chain is shorter (`shorter`), or
    /// after `ABOVE_HEAD_WAIT_SECONDS`: until then the endpoint that gave the head may only be behind the one that gave
    /// the mark, and the walk waits for it.
    pub(super) async fn scan_marks(
        &self,
        head: &Head,
        shorter: bool,
        stats: &mut Stats,
    ) -> Result<Scan> {
        let mut from: u64 = self
            .journal
            .meta(SCAN_KEY)
            .await?
            .and_then(|saved| saved.parse().ok())
            .unwrap_or(0);
        for _ in 0..SCAN_PAGES_PER_TICK {
            let numbers = self.journal.mark_numbers_from(from, SCAN_PAGE).await?;
            let (Some(&first), Some(&last)) = (numbers.first(), numbers.last()) else {
                return Ok(Scan {
                    nonces: BTreeMap::new(),
                    complete: true,
                });
            };
            if !shorter && last > head.number && !self.waited_above_head(head).await? {
                return Ok(Scan {
                    nonces: BTreeMap::new(),
                    complete: false,
                });
            }
            if last <= head.number {
                // The head has reached the marks: a later mark above a lagging head waits afresh.
                self.reset_above_head_wait().await?;
            }
            // A block above the decision head is not the chain's, whatever any endpoint says of it.
            let readable: Vec<u64> = numbers
                .iter()
                .copied()
                .filter(|number| *number <= head.number)
                .collect();
            let canonical: BTreeMap<u64, B256> = if readable.is_empty() {
                BTreeMap::new()
            } else {
                let hashes = self.rpc.block_hashes(&readable).await?;
                readable.iter().copied().zip(hashes).collect()
            };
            let stale: Vec<Mark> = self
                .journal
                .marks_between(first, last)
                .await?
                .into_iter()
                .filter(|mark| {
                    canonical
                        .get(&mark.number)
                        .is_none_or(|hash| !hash.to_string().eq_ignore_ascii_case(&mark.hash))
                })
                .collect();
            let mut gone = Vec::new();
            let mut nonces: BTreeMap<u64, Vec<Mark>> = BTreeMap::new();
            let mut waiting = false;
            for mark in stale {
                match mark.kind {
                    MarkKind::Head | MarkKind::Sign => gone.push(mark),
                    MarkKind::Nonce => match mark.reference.parse::<u64>() {
                        Ok(nonce) => nonces.entry(nonce).or_default().push(mark),
                        Err(_) => gone.push(mark),
                    },
                    MarkKind::Receipt => match self.nonce_of(&mark.reference).await? {
                        Some(nonce) => nonces.entry(nonce).or_default().push(mark),
                        None => waiting |= !self.remark_foreign_receipt(&mark).await?,
                    },
                }
            }
            if !gone.is_empty() {
                self.journal.replace_marks(&gone, None).await?;
                stats.marks_dropped += u64::try_from(gone.len())?;
            }
            if nonces.is_empty() && !waiting {
                from = last.saturating_add(1);
                self.journal.set_meta(SCAN_KEY, &from.to_string()).await?;
                continue;
            }
            return Ok(Scan {
                nonces,
                complete: false,
            });
        }
        Ok(Scan {
            nonces: BTreeMap::new(),
            complete: false,
        })
    }
    /// Whether the walk has waited `ABOVE_HEAD_WAIT_SECONDS` for the decision head to reach marks above it (`scan_marks`).
    async fn waited_above_head(&self, head: &Head) -> Result<bool> {
        let now = crate::health::now()?;
        let since = match self.journal.meta(ABOVE_HEAD_KEY).await? {
            Some(since) => since.parse().unwrap_or(now),
            None => {
                self.journal
                    .set_meta(ABOVE_HEAD_KEY, &now.to_string())
                    .await?;
                now
            }
        };
        let waited = now.saturating_sub(since) >= ABOVE_HEAD_WAIT_SECONDS;
        if !waited {
            tracing::debug!(
                head = head.number,
                "Finality recovery: marks lie above the decision head; waiting for the endpoint to reach them"
            );
        }
        Ok(waited)
    }
    /// Forget how long the walk has waited for the head (`waited_above_head`).
    async fn reset_above_head_wait(&self) -> Result<()> {
        if self.journal.meta(ABOVE_HEAD_KEY).await?.is_some() {
            sqlx::query("DELETE FROM meta WHERE key=?")
                .bind(ABOVE_HEAD_KEY)
                .execute(&self.journal.pool)
                .await?;
        }
        Ok(())
    }
    /// The nonce of a transaction the journal signed.
    async fn nonce_of(&self, hash: &str) -> Result<Option<u64>> {
        let nonce: Option<i64> = sqlx::query_scalar("SELECT nonce FROM txs WHERE hash=?")
            .bind(hash)
            .fetch_optional(&self.journal.pool)
            .await?;
        Ok(nonce.map(u64::try_from).transpose()?)
    }
    /// A stale receipt mark of a transaction that is not in the journal's `txs`: the transfer of an operator sweep, whose
    /// signed bytes are not kept once it is settled. Where the chain has the receipt now, the mark becomes the mark of
    /// that block. Where it has none, the mark goes and the report says the transfer is not on the chain. False while the
    /// receipt is there but not settled at the decision head yet.
    async fn remark_foreign_receipt(&self, stale: &Mark) -> Result<bool> {
        match self.rpc.receipt(&stale.reference).await? {
            Some(receipt) => {
                if !self
                    .rpc
                    .receipt_is_settled(&stale.reference, &receipt)
                    .await?
                {
                    return Ok(false);
                }
                let fresh = self.receipt_mark(&stale.reference, &receipt)?;
                self.journal
                    .replace_marks(std::slice::from_ref(stale), fresh.as_ref())
                    .await?;
                Ok(true)
            }
            None => {
                tracing::error!(tx_hash=%stale.reference,block=stale.number,
                    "A transaction the journal does not keep (an operator sweep's transfer) has no receipt on the chain any more; if the transfer is still wanted, queue it again with `sweep`");
                // Only the owner can say whether the transfer is still wanted: asked once, as the mark goes with it.
                if let Some(notifier) = &self.telegram {
                    notifier.notify(crate::telegram::Event::Owner(lost_sweep_text(
                        &stale.reference,
                    )));
                }
                self.journal
                    .replace_marks(std::slice::from_ref(stale), None)
                    .await?;
                Ok(true)
            }
        }
    }

    /// Read a page of the jobs again from chain state at the decision head. True when there is none left.
    ///
    /// The jobs are those with a deadline from the oldest stale block's time on (a request that was served, or open,
    /// since then), in the states that chain state decides: served, callback failed, pending, prepared, signed and
    /// submitted, and without a live attempt of their own (the lane has those). The first page also takes discovery
    /// back to the first request of the chain as it is now that was still open at that time: a block the sequencer
    /// replaced can have held requests under ids that discovery had passed, and the chain now holds others under them.
    async fn reclassify_jobs(
        &self,
        head: &Head,
        stats: &mut Stats,
        began: tokio::time::Instant,
    ) -> Result<bool> {
        let known: Option<u64> = self
            .journal
            .meta(SINCE_KEY)
            .await?
            .and_then(|saved| saved.parse().ok());
        let since = match known {
            Some(since) => since,
            None => {
                // An endpoint that does not serve the finalized header leaves the age limit alone as the window.
                let finalized = self
                    .rpc
                    .finalized_head()
                    .await
                    .map_or(0, |head| head.timestamp);
                let oldest = head
                    .timestamp
                    .saturating_sub(2 * self.cfg.chain.finality_audit_max_lag_seconds);
                let since = finalized.max(oldest).min(head.timestamp);
                self.journal.set_meta(SINCE_KEY, &since.to_string()).await?;
                since
            }
        };
        if !self.rewind_discovery(head, since, began).await? {
            return Ok(false);
        }
        let since = i64::try_from(since)?;
        let (after_deadline, after_id): (i64, String) = match self.journal.meta(JOBS_KEY).await? {
            Some(saved) => serde_json::from_str(&saved)?,
            None => (i64::MIN, String::new()),
        };
        // A vanished job whose deadline has not passed is read too: the chain as it is can hold its request again, or
        // another under its id (review H1).
        let rows = sqlx::query("SELECT * FROM jobs WHERE (state IN ('served','callback_failed','pending','prepared','signed','submitted') OR (state='vanished' AND deadline>?)) AND deadline>=? AND (deadline>? OR (deadline=? AND id>?)) AND NOT EXISTS(SELECT 1 FROM txs WHERE txs.job=jobs.id AND txs.state!='resolved') AND NOT EXISTS(SELECT 1 FROM batch_members JOIN txs ON txs.job=batch_members.job WHERE batch_members.request_id=jobs.id AND txs.state!='resolved') ORDER BY deadline,id LIMIT ?")
            .bind(i64::try_from(head.timestamp)?)
            .bind(since)
            .bind(after_deadline)
            .bind(after_deadline)
            .bind(&after_id)
            .bind(i64::try_from(JOB_PAGE)?)
            .fetch_all(&self.journal.pool)
            .await?;
        let jobs: Vec<Job> = rows.into_iter().map(crate::journal::job_from_row).collect();
        if jobs.is_empty() {
            return Ok(true);
        }
        let ids = jobs
            .iter()
            .map(|job| job.id.parse())
            .collect::<std::result::Result<Vec<U256>, _>>()?;
        // The coordinator's own read: `getRequest` of an epoch coordinator, `getRoundRequest` of a round coordinator,
        // whose requests are kept to compare their fingerprints.
        let tag = self.rpc.decision_tag(head);
        let (requests, rounds): (Vec<Status>, Vec<Option<RoundRequest>>) = match self
            .coordinator_kind()
        {
            CoordinatorKind::Epoch => (self.statuses_in(&ids, &tag).await?, vec![None; ids.len()]),
            CoordinatorKind::Round => {
                let read = self.round_requests_in(&ids, &tag).await?;
                (
                    read.iter()
                        .map(|request| Status::of_round(request.as_ref()))
                        .collect(),
                    read,
                )
            }
        };
        // The page is read job by job, and what the budget leaves unread is the next tick's: the cursor is the last job
        // that was read, and at least one is, so that every tick moves on.
        let mut read = 0;
        for ((job, request), round) in jobs.iter().zip(&requests).zip(&rounds) {
            if read > 0 && began.elapsed() >= self.recovery_budget {
                break;
            }
            self.reclassify_job(job, request, round.as_ref(), head, stats)
                .await?;
            read += 1;
        }
        let last = &jobs[read - 1];
        self.journal
            .set_meta(
                JOBS_KEY,
                &serde_json::to_string(&(last.deadline, &last.id))?,
            )
            .await?;
        Ok(read == jobs.len() && jobs.len() < JOB_PAGE)
    }
    /// Take discovery back to the first request of the chain as it is now that was still open at `since`: a block the
    /// sequencer replaced can have held requests under ids that discovery had passed, and the chain now holds others under
    /// them. The first request with a deadline from `since` on is found by bisection, since deadlines rise with ids. The
    /// search keeps its interval in the journal and leaves what the tick's budget does not allow to the next tick. True
    /// when discovery has been taken back.
    async fn rewind_discovery(
        &self,
        head: &Head,
        since: u64,
        began: tokio::time::Instant,
    ) -> Result<bool> {
        let saved = self.journal.meta(REWIND_KEY).await?;
        if saved.as_deref() == Some("done") {
            return Ok(true);
        }
        let interval = saved.as_deref().and_then(|saved| {
            let (low, high) = saved.split_once(',')?;
            Some((low.parse::<u64>().ok()?, high.parse::<u64>().ok()?))
        });
        let (mut low, mut high) = match interval {
            Some(interval) => interval,
            None => (1, self.next_request_id_at(head.number).await?),
        };
        let mut steps = 0;
        while low < high {
            // At least one step each tick, so that a tick without budget still moves the search on.
            if steps > 0 && began.elapsed() >= self.recovery_budget {
                self.journal
                    .set_meta(REWIND_KEY, &format!("{low},{high}"))
                    .await?;
                return Ok(false);
            }
            steps += 1;
            let middle = low + (high - low) / 2;
            let deadline = self.deadline_at(U256::from(middle), head.number).await?;
            if deadline < since {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        sqlx::query(
            "UPDATE meta SET value=CAST(MIN(CAST(value AS INTEGER),?) AS TEXT) WHERE key='cursor'",
        )
        .bind(i64::try_from(low)?)
        .execute(&self.journal.pool)
        .await?;
        self.journal.set_meta(REWIND_KEY, "done").await?;
        Ok(true)
    }
    /// One job against the request as the chain has it at the decision head; `round` is the request itself when the
    /// coordinator is a round coordinator and has it.
    async fn reclassify_job(
        &self,
        job: &Job,
        request: &Status,
        round: Option<&RoundRequest>,
        head: &Head,
        stats: &mut Stats,
    ) -> Result<()> {
        // The chain settled it: served, refunded, or past its deadline (a request the chain no longer has is that too).
        if let Some(state) = terminal(request, head.timestamp) {
            if request.fulfilled && !request.delivered {
                crate::audit::callback_failed(&self.journal.pool, &job.id).await?;
            }
            if job.state != state {
                self.journal.state(&job.id, state).await?;
                stats.jobs_settled += 1;
            }
            return Ok(());
        }
        // Open on the chain. It is not the request the journal holds when its deadline is another, or, on an epoch
        // coordinator, when the proof it holds is for a seed the chain does not have (the seed binds the target block,
        // among the rest). On a round coordinator the fingerprint takes the seed's place: the request moved when the
        // fingerprint of its fields is not the one its round row holds, or the one its proof was made for.
        let deadline = i64::try_from(request.deadline)?;
        let mut changed = deadline != job.deadline;
        if let Some(round) = round {
            let fingerprint = crate::round::fingerprint(round).to_string();
            let assigned = self.journal.round_assignment(&job.id).await?;
            let proved = job.proof.as_deref().map(|saved| {
                serde_json::from_str::<crate::round::Prepared>(saved)
                    .map(|prepared| prepared.fingerprint.to_string())
                    .unwrap_or_default()
            });
            if assigned.is_none_or(|assigned| assigned.fingerprint != fingerprint)
                || proved.is_some_and(|proved| proved != fingerprint)
            {
                let assignment = crate::journal::RoundAssignment {
                    beacon: round.beaconId,
                    round: round.round,
                    fingerprint: fingerprint.clone(),
                    sealing_lag_ms: 0,
                    seen_at: 0,
                };
                self.journal
                    .request_moved(&job.id, &assignment, crate::health::now()?)
                    .await?;
                tracing::warn!(request_id=%job.id,beacon=round.beaconId,round=round.round,"Finality recovery: the request moved; its proof is dropped and it is proved again");
                changed = true;
            }
        }
        if !changed
            && self.coordinator_kind() == CoordinatorKind::Epoch
            && let Some(saved) = &job.proof
        {
            let seed = self.proof_seed_at(job.id.parse()?, head).await?;
            changed = serde_json::from_str::<VrfProof>(saved)
                .map(|proof| Some(proof.seed) != seed)
                .unwrap_or(true);
        }
        if changed {
            sqlx::query(
                "UPDATE jobs SET proof=NULL,call=NULL,state='pending',deadline=? WHERE id=?",
            )
            .bind(deadline)
            .bind(&job.id)
            .execute(&self.journal.pool)
            .await?;
            sqlx::query("DELETE FROM meta WHERE key IN (?,?,?)")
                .bind(format!("prepare_retry_ms:{}", job.id))
                .bind(format!("preflight_retry:{}", job.id))
                .bind(format!("batch_exclude:{}", job.id))
                .execute(&self.journal.pool)
                .await?;
            stats.jobs_reproved += 1;
        } else if matches!(
            job.state.as_str(),
            "served" | "callback_failed" | "signed" | "submitted" | "vanished"
        ) {
            sqlx::query("UPDATE jobs SET state=CASE WHEN call IS NULL THEN 'pending' ELSE 'prepared' END WHERE id=?")
                .bind(&job.id)
                .execute(&self.journal.pool)
                .await?;
            stats.jobs_reopened += 1;
        }
        Ok(())
    }
    /// The epochs this registry's recent committed work names, read again at the decision head: one the registry no
    /// longer has goes back to `prepared` (its packet is kept) or `pending`, and is published again when demand waits.
    /// An epoch coordinator's step alone. A record the registry refuses to give (a node's error, which asking again
    /// would not change) leaves that epoch as it is, so that it can never keep the recovery from finishing; a read that
    /// was not delivered is tried again at the next tick.
    async fn reclassify_epochs(&self, head: &Head, stats: &mut Stats) -> Result<()> {
        let epoch_lane = self.epoch()?;
        let committed: Vec<(String, i64, Option<String>)> = sqlx::query_as("SELECT key,epoch,api FROM epoch_work WHERE registry=? AND catalog=? AND state='committed' ORDER BY epoch DESC LIMIT ?")
            .bind(epoch_lane.registry.to_string())
            .bind(epoch_lane.catalog.to_string())
            .bind(EPOCH_PAGE)
            .fetch_all(&self.journal.pool)
            .await?;
        for (key, epoch, api) in committed {
            let record = match self
                .rpc
                .call_at(
                    epoch_lane.registry,
                    ER::getEpochCall {
                        epochId: u64::try_from(epoch)?,
                    },
                    head.number,
                )
                .await
            {
                Ok(record) => record,
                Err(error) if crate::rpc::is_node_error_response(&error) => {
                    tracing::warn!(epoch,error=%error,"Finality recovery: the registry refused the epoch's record; the epoch is left as it is");
                    stats.epochs_unread += 1;
                    continue;
                }
                Err(error) => return Err(error),
            };
            if record.epochHash != B256::ZERO {
                continue;
            }
            let moved = sqlx::query("UPDATE epoch_work SET state=? WHERE key=? AND state='committed' AND NOT EXISTS(SELECT 1 FROM txs WHERE txs.job=epoch_work.key AND txs.state!='resolved')")
                .bind(if api.is_some() { "prepared" } else { "pending" })
                .bind(&key)
                .execute(&self.journal.pool)
                .await?;
            stats.epochs_reopened += moved.rows_affected();
        }
        Ok(())
    }

    /// The seed of request `id`'s proof input at the decision head: an epoch coordinator's proof context. `None` when the
    /// input does not exist there (the request, or the confirmation of its target, is not on the chain as it is now).
    async fn proof_seed_at(&self, id: U256, head: &Head) -> Result<Option<U256>> {
        match self
            .rpc
            .call_at(
                self.cfg.coordinator,
                C::getProofContextCall { id },
                head.number,
            )
            .await
        {
            Ok(context) => Ok(Some(context.seed)),
            Err(error) if crate::rpc::is_node_error_response(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }
    /// Whether the transaction of `nonce` that reconciliation would broadcast again carries a proof whose input the chain
    /// has changed: a fulfillment, or a batch, any of whose proofs is for another seed than the chain's at the decision
    /// head, or for an input the chain does not have. Such bytes would only revert and burn gas, so the nonce is filled
    /// instead. A round coordinator's are checked by `stale_round_in_lane`.
    async fn stale_in_lane(&self, nonce: u64, head: &Head) -> Result<bool> {
        if self.coordinator_kind() == CoordinatorKind::Round {
            return self.stale_round_in_lane(nonce, head).await;
        }
        let last: Option<(String, String)> =
            sqlx::query_as("SELECT payload,raw FROM txs WHERE nonce=? ORDER BY id DESC LIMIT 1")
                .bind(i64::try_from(nonce)?)
                .fetch_optional(&self.journal.pool)
                .await?;
        let Some((payload, _)) = last.filter(|(_, raw)| !raw.is_empty()) else {
            return Ok(false);
        };
        let Ok(data) = payload.parse::<Bytes>() else {
            return Ok(false);
        };
        let proofs: Vec<(U256, U256)> =
            if let Ok(call) = C::fulfillRandomnessCall::abi_decode(&data) {
                vec![(call.id, call.proof.seed)]
            } else if let Ok(call) = C::fulfillRandomnessBatchCall::abi_decode(&data) {
                call.ids
                    .into_iter()
                    .zip(call.proofs.into_iter().map(|proof| proof.seed))
                    .collect()
            } else {
                return Ok(false);
            };
        for (id, seed) in proofs {
            if self.proof_seed_at(id, head).await? != Some(seed) {
                tracing::warn!(nonce,request_id=%id,
                    "Finality recovery: the transaction of the nonce carries a proof for an input the chain has changed; the nonce is filled instead of broadcasting it again");
                return Ok(true);
            }
        }
        Ok(false)
    }
    /// `stale_in_lane` of a round coordinator (design C, 3.7): the transaction of `nonce` is stale when any request it
    /// serves is not the one its proof was made for at the decision head. A request the coordinator does not have any
    /// more makes it stale, and so does one whose fingerprint changed: its seed, computed here over the fields the chain
    /// has now and the randomness of its round (the coordinator's, or that of the signature the transaction carries for
    /// the round), is not the seed of the proof. A round that is neither verified nor listed makes it stale too. Such a
    /// fulfillment would revert `WrongSeed`, possibly after paying for a BLS check, so its nonce is filled by a
    /// cancellation and its requests are proved again.
    async fn stale_round_in_lane(&self, nonce: u64, head: &Head) -> Result<bool> {
        let stale = self.stale_round_requests(nonce, head).await?;
        if let Some((id, _)) = stale.first() {
            tracing::warn!(nonce,request_id=%id,
                "Finality recovery: the transaction of the nonce serves a request that moved or vanished; the nonce is filled instead of broadcasting it again");
        }
        Ok(!stale.is_empty())
    }
    /// The requests the transaction of `nonce` serves that are not the ones its proofs were made for at the decision head
    /// (see `stale_round_in_lane`): each with the request the coordinator has now under its id, `None` for one it does
    /// not have. Empty when the bytes are still the requests' or are not a round fulfillment.
    pub(super) async fn stale_round_requests(
        &self,
        nonce: u64,
        head: &Head,
    ) -> Result<Vec<(U256, Option<RoundRequest>)>> {
        let Lane::Round(lane) = &self.lane else {
            return Ok(Vec::new());
        };
        let last: Option<(String, String)> =
            sqlx::query_as("SELECT payload,raw FROM txs WHERE nonce=? ORDER BY id DESC LIMIT 1")
                .bind(i64::try_from(nonce)?)
                .fetch_optional(&self.journal.pool)
                .await?;
        let Some((payload, _)) = last.filter(|(_, raw)| !raw.is_empty()) else {
            return Ok(Vec::new());
        };
        let Ok(data) = payload.parse::<Bytes>() else {
            return Ok(Vec::new());
        };
        // Each request with its proof's seed, and the signatures the transaction carries: a single's is its request's
        // round's, a batch lists each round's.
        type Carried = (Vec<(U256, U256)>, Option<Bytes>, Vec<(u8, u64, Bytes)>);
        let (proofs, single, listed): Carried =
            if let Ok(call) = RC::fulfillRandomnessCall::abi_decode(&data) {
                (
                    vec![(call.requestId, call.proof.seed)],
                    Some(call.roundSignature),
                    Vec::new(),
                )
            } else if let Ok(call) = RC::fulfillRandomnessBatchCall::abi_decode(&data) {
                (
                    call.ids
                        .into_iter()
                        .zip(call.proofs.into_iter().map(|proof| proof.seed))
                        .collect(),
                    None,
                    call.rounds
                        .into_iter()
                        .map(|round| (round.beaconId, round.round, round.signature))
                        .collect(),
                )
            } else {
                return Ok(Vec::new());
            };
        let tag = self.rpc.decision_tag(head);
        let mut found = Vec::new();
        for (id, seed) in proofs {
            let request = self.round_request_tagged(id, &tag).await?;
            let stale = match &request {
                None => true,
                Some(request) => {
                    let carried = single.clone().or_else(|| {
                        listed
                            .iter()
                            .find(|(beacon, round, _)| {
                                *beacon == request.beaconId && *round == request.round
                            })
                            .map(|(_, _, signature)| signature.clone())
                    });
                    let randomness = if request.roundRandomness != B256::ZERO {
                        Some(request.roundRandomness)
                    } else {
                        carried.map(|signature| crate::round::randomness(&signature))
                    };
                    randomness.is_none_or(|randomness| {
                        crate::round::seed(
                            self.cfg.chain_id,
                            self.cfg.coordinator,
                            lane.facts.key_hash,
                            id,
                            request,
                            randomness,
                        ) != seed
                    })
                }
            };
            if stale {
                found.push((id, request));
            }
        }
        Ok(found)
    }
    /// The fill under way, if any.
    async fn fill(&self) -> Result<Option<Fill>> {
        self.journal
            .meta(FILL_KEY)
            .await?
            .map(|saved| serde_json::from_str(&saved))
            .transpose()
            .map_err(Into::into)
    }
    /// Start to fill `nonce`, which the chain has not used: the journal's transaction of it carried a stale proof
    /// (`stale`), or the journal has no bytes for it. Every live attempt is parked so that the nonce is the lane, and the
    /// first fill is signed and broadcast at once.
    async fn start_fill(
        &self,
        head: &Head,
        nonce: u64,
        stale: bool,
        stats: &mut Stats,
    ) -> Result<()> {
        let known: Vec<String> =
            sqlx::query_scalar("SELECT hash FROM txs WHERE nonce=? ORDER BY id")
                .bind(i64::try_from(nonce)?)
                .fetch_all(&self.journal.pool)
                .await?;
        let fill = Fill {
            nonce,
            stale,
            known,
            txs: Vec::new(),
        };
        self.journal
            .begin_fill(FILL_KEY, &serde_json::to_string(&fill)?)
            .await?;
        self.recovery_lane.store(true, Ordering::Relaxed);
        stats.filled_nonces.push(nonce);
        if stale {
            stats.stale_nonces.push(nonce);
        }
        tracing::warn!(
            nonce,
            stale,
            "Finality recovery: the nonce is filled with a zero-value transfer to the keeper's own wallet"
        );
        self.step_fill(head, fill).await
    }
    /// One step of a fill: settle it once a receipt of the nonce is settled at the decision head, or the chain has used
    /// the nonce without one any endpoint serves; otherwise sign the fill (priced as a cancellation, `cancel_gas`, above
    /// what the nonce's earlier transaction offered), replace it with one that pays more when it is not included, and
    /// broadcast it again every two seconds.
    async fn step_fill(&self, head: &Head, mut fill: Fill) -> Result<()> {
        let wallet = self.tx_key.address();
        let hashes: Vec<String> = fill
            .txs
            .iter()
            .map(|tx| tx.hash.clone())
            .chain(fill.known.iter().cloned())
            .collect();
        for hash in &hashes {
            if let Some(receipt) = self.rpc.receipt(hash).await? {
                if !self.rpc.receipt_is_settled(hash, &receipt).await? {
                    return Ok(());
                }
                let fresh = self.receipt_mark(hash, &receipt)?;
                return self.fill_settled(&fill, &hashes, fresh).await;
            }
        }
        let now = crate::health::now()?;
        if self.rpc.decision_nonce(wallet, head).await? > fill.nonce {
            if let Some((found, receipt)) = self.rpc.receipt_from_any(&hashes).await?
                && self
                    .rpc
                    .receipt_is_settled(&hashes[found], &receipt)
                    .await?
            {
                let fresh = self.receipt_mark(&hashes[found], &receipt)?;
                return self.fill_settled(&fill, &hashes, fresh).await;
            }
            let seen = fill.txs.last().map_or(0, |tx| tx.broadcast.max(tx.created));
            if now.saturating_sub(seen) < RECEIPT_VISIBILITY_GRACE_SECONDS {
                return Ok(());
            }
            let fresh = self.nonce_mark(head, i64::try_from(fill.nonce)?)?;
            return self.fill_settled(&fill, &hashes, fresh).await;
        }
        if !self.cfg.send {
            tracing::debug!(
                nonce = fill.nonce,
                "Finality recovery: a keeper that does not send leaves the nonce unfilled"
            );
            return Ok(());
        }
        let due = fill.txs.last().is_none_or(|tx| {
            now.saturating_sub(tx.created) >= FILL_REPLACE_SECONDS && fill.txs.len() < FILL_ATTEMPTS
        });
        if due {
            // A fill must outbid what the nonce's earlier transaction offered, which a node may still hold.
            let previous: Option<(String, String)> = match fill.txs.last() {
                Some(tx) => Some((tx.priority.clone(), tx.fee.clone())),
                None => {
                    sqlx::query_as(
                        "SELECT priority,fee FROM txs WHERE nonce=? ORDER BY id DESC LIMIT 1",
                    )
                    .bind(i64::try_from(fill.nonce)?)
                    .fetch_optional(&self.journal.pool)
                    .await?
                }
            };
            let tip = self.priority_fee().await;
            let (fee, priority) = match previous {
                Some((priority, fee)) => {
                    replacement_fees(priority.parse()?, fee.parse()?, head.base_fee, tip)?
                }
                None => (required_fee(head.base_fee, tip)?, tip),
            };
            let plan = TxPlan {
                nonce: fill.nonce,
                gas: self.cancel_gas().await?,
                fee,
                priority,
                payload: "0x".into(),
                kind: "cancel".into(),
            };
            if let Some(exceeded) = self.over_budget(&plan) {
                self.defer_for_budget("finality_fill", "cancel", &exceeded)
                    .await?;
                return Ok(());
            }
            self.verify_runtime().await?;
            let signed = self.sign_transfer(&plan, wallet, U256::ZERO, now).await?;
            let mark = self.sign_mark(head, &signed.hash)?;
            fill.txs.push(signed);
            // The fill is journaled before it is broadcast.
            self.journal
                .set_meta_marked(FILL_KEY, &serde_json::to_string(&fill)?, mark.as_ref())
                .await?;
            return self.broadcast_fill(&mut fill).await;
        }
        if fill
            .txs
            .last()
            .is_some_and(|tx| now.saturating_sub(tx.broadcast) >= 2)
        {
            self.broadcast_fill(&mut fill).await?;
        }
        Ok(())
    }
    /// Broadcast the last fill, as an operator sweep's transfer is broadcast: the journal says when before it is sent.
    async fn broadcast_fill(&self, fill: &mut Fill) -> Result<()> {
        self.ensure_not_halted().await?;
        self.verify_runtime().await?;
        let last = fill
            .txs
            .last_mut()
            .ok_or_else(|| anyhow::anyhow!("Fill has no transaction"))?;
        last.broadcast = crate::health::now()?;
        let (raw, hash) = (last.raw.clone(), last.hash.clone());
        self.journal
            .set_meta(FILL_KEY, &serde_json::to_string(fill)?)
            .await?;
        match self.rpc.broadcast(&raw, hash.parse()?).await? {
            crate::rpc::BroadcastOutcome::Rejected(reason) => {
                self.notify_error(crate::telegram::ErrorClass::TransactionSubmission);
                if self.coordinator_kind() == CoordinatorKind::Round
                    && reason == "insufficient_funds"
                {
                    let need = last_need(fill)?;
                    self.funds_short(need).await?;
                }
                tracing::error!(tx_hash=%hash,nonce=fill.nonce,reason,"Node rejected the recovery's fill; it is broadcast again or replaced")
            }
            crate::rpc::BroadcastOutcome::Ambiguous => {
                tracing::warn!(tx_hash=%hash,nonce=fill.nonce,"Fill broadcast ambiguous")
            }
            _ => tracing::info!(tx_hash=%hash,nonce=fill.nonce,"Recovery's fill broadcast"),
        }
        Ok(())
    }
    /// The chain has used the nonce of `fill`, by the transaction `fresh` marks: the marks of the nonce's earlier
    /// settlement go, `fresh` is written, and the fill is done.
    async fn fill_settled(
        &self,
        fill: &Fill,
        hashes: &[String],
        fresh: Option<Mark>,
    ) -> Result<()> {
        self.journal
            .settle_fill(FILL_KEY, fill.nonce, hashes, fresh.as_ref())
            .await?;
        tracing::warn!(
            nonce = fill.nonce,
            stale = fill.stale,
            "Finality recovery: the nonce is used on the chain as it is now"
        );
        Ok(())
    }
}
