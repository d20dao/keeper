//! A soft keeper against endpoints that disagree, serve no `finalized` tag, or follow a chain the sequencer shortened,
//! and against a finalized record that is not the chain's (the R2/R3 review of `rh/keeper-round-k3`, findings M3 to M6,
//! ported from its proofs of concept as regression tests).
use super::*;
use crate::{
    rig::Rig,
    scripted::Mode,
    soft_finality::{ready, soft_pair},
};

/// The keeper's transactions: label, gas limit and nonce.
fn sent(rig: &Rig) -> Vec<(String, u64, u64)> {
    rig.chain(|chain| {
        chain
            .sends
            .iter()
            .map(|(_, label, gas, nonce)| (label.clone(), *gas, *nonce))
            .collect()
    })
}

/// M4. Two endpoints; the first, which the keeper reads from, follows another fork from its first tick, so the journal
/// holds that fork's hashes. When the second, honest, endpoint shows another hash, one endpoint stands against one: the
/// journal's hash came from the first and is no witness of its own. Nobody is cooled, nothing is recorded or sent, and the
/// keeper asks again; the fork is never audited as final.
#[tokio::test]
async fn one_endpoint_against_one_is_unresolved_and_cools_nobody() {
    let rig = soft_pair(0).await;
    rig.chain(|chain| chain.finalized_lag = 2);
    rig.endpoints()[0].set(Mode::Fork(0));
    let worker = Worker::new(rig.config(true)).await.unwrap();
    worker.tick().await.unwrap();
    // The first endpoint is down for one tick: the second, honest, endpoint answers, and shows another hash.
    rig.endpoints()[0].set(Mode::Down);
    rig.chain(|chain| chain.mine(1));
    worker.tick().await.unwrap();
    assert!(worker.suspected().unwrap());
    rig.endpoints()[0].set(Mode::Fork(0));
    for _ in 0..3 {
        if let Some(held) = worker.suspicion.lock().unwrap().as_mut() {
            held.next = tokio::time::Instant::now();
        }
        rig.chain(|chain| chain.mine(1));
        worker.tick().await.unwrap();
        assert!(!worker.rpc.cooling(0) && !worker.rpc.cooling(1));
        assert!(worker.suspected().unwrap());
        assert!(worker.journal.finality_mismatch().await.unwrap().is_none());
    }
    let note = worker.journal.suspicion_note().await.unwrap().unwrap();
    assert_eq!((note.endpoints, note.answered), (2, 2));
    // A request comes: nothing is sent while the endpoints do not settle it, and nothing becomes final.
    ready(&rig);
    worker.tick().await.unwrap();
    rig.chain(|chain| chain.mine(5));
    *worker.finality_audit_due.lock().unwrap() = None;
    worker.tick().await.unwrap();
    assert!(sent(&rig).is_empty(), "{:?}", sent(&rig));
    let finalized: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM finalized_receipts")
        .fetch_one(&worker.journal.pool)
        .await
        .unwrap();
    assert_eq!(finalized, 0);
    worker.journal.pool.close().await;
}

/// M5. Two endpoints, one of which refuses the `finalized` tag with a JSON-RPC error (its other reads are fine). The
/// sequencer replaces a block the keeper decided on, and both endpoints show the new hash. Each endpoint is asked for
/// the block apart from its `finalized` header, so the one without the tag still gives its view: two agree, and the
/// keeper records the mismatch and recovers from it by itself instead of holding its sends.
#[tokio::test]
async fn an_endpoint_without_the_finalized_tag_still_gives_its_view_of_the_block() {
    let rig = soft_pair(0).await;
    rig.endpoints()[1].set(Mode::NoFinalized);
    ready(&rig);
    let worker = Worker::new(rig.config(true)).await.unwrap();
    worker.tick().await.unwrap();
    rig.chain(|chain| {
        let decided = chain.head;
        chain.replace_blocks(decided);
        chain.mine(1);
    });
    worker.tick().await.unwrap();
    let recovered = worker
        .journal
        .meta(crate::journal::LAST_RECOVERY_KEY)
        .await
        .unwrap();
    assert!(
        worker.journal.finality_mismatch().await.unwrap().is_some() || recovered.is_some(),
        "the replacement two endpoints show is confirmed"
    );
    assert!(!worker.suspected().unwrap());
    let confirmed: serde_json::Value = match recovered {
        Some(last) => {
            serde_json::from_str::<serde_json::Value>(&last).unwrap()["confirmed"].clone()
        }
        None => serde_json::from_str(
            &worker
                .journal
                .meta(super::suspicion::CONFIRMED_KEY)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap(),
    };
    assert_eq!(confirmed["agreeing"], 2, "{confirmed}");
    for _ in 0..4 {
        worker.tick().await.unwrap();
        rig.chain(|chain| chain.include());
    }
    assert!(worker.journal.finality_mismatch().await.unwrap().is_none());
    worker.journal.pool.close().await;
}

/// M6. The sequencer replaces the tip with a shorter chain: it lost the block of the keeper's fulfillment, which it had
/// not posted, and has made no block since. The block the last tick decided on is above every endpoint's head. That is
/// a suspicion like any other, not a failed tick: once no endpoint has had the block over three asks and
/// `suspicion::ABSENT_WINDOW`, the mismatch is recorded and the keeper recovers by itself, broadcasting the lost fulfillment again, without waiting for
/// the chain to grow past the old block. No tick fails, so none counts toward MAX_TICK_FAILURES.
#[tokio::test]
async fn a_shorter_replacement_chain_is_suspected_and_recovered_from_without_a_failed_tick() {
    let rig = soft_pair(0).await;
    let id = ready(&rig);
    let mut worker = Worker::new(rig.config(true)).await.unwrap();
    worker.suspicion_retry = std::time::Duration::ZERO;
    worker.absent_window = std::time::Duration::ZERO;
    worker.tick().await.unwrap();
    rig.chain(|chain| chain.include());
    let included = rig.chain(|chain| chain.head);
    worker.tick().await.unwrap();
    assert!(rig.chain(|chain| chain.requests[&id].fulfilled));
    rig.chain(|chain| {
        chain.drop_blocks(included, false);
        chain.head = included - 1;
    });
    assert!(!rig.chain(|chain| chain.requests[&id].fulfilled));
    // Asked twice, the endpoints still have not confirmed it: an absence is confirmed only at the third ask.
    for _ in 0..2 {
        worker.tick().await.unwrap();
        assert!(
            worker.suspected().unwrap(),
            "a suspicion until the third ask"
        );
        assert!(worker.journal.finality_mismatch().await.unwrap().is_none());
    }
    for _ in 0..6 {
        worker.tick().await.unwrap();
        rig.chain(|chain| chain.include());
    }
    let last: serde_json::Value = serde_json::from_str(
        &worker
            .journal
            .meta(crate::journal::LAST_RECOVERY_KEY)
            .await
            .unwrap()
            .expect("recovered"),
    )
    .unwrap();
    assert_eq!(
        last["mismatch"]["actual"],
        crate::finality::ABSENT.to_string()
    );
    assert_eq!(last["mismatch"]["number"], included);
    assert!(worker.journal.finality_mismatch().await.unwrap().is_none());
    assert!(!worker.suspected().unwrap());
    assert!(rig.chain(|chain| chain.requests[&id].fulfilled));
    worker.journal.pool.close().await;
}

/// M3. Both endpoints answer the `finalized` tag with the latest block, as a provider that aliases the tag does, so the
/// audit makes a receipt final that the sequencer then moves to another block. The audit finds the receipt's new mark
/// against the finalized record; the endpoints are asked about the record's own block, confirm that it changed, and the
/// recovery drops the record, so that the next audit makes the receipt final where the chain has it. One incident for
/// the record, not one per audit.
#[tokio::test]
async fn a_finalized_receipt_the_sequencer_moved_is_one_incident_and_is_put_right() {
    let rig = soft_pair(0).await;
    rig.chain(|chain| chain.finalized_lag = 0);
    ready(&rig);
    let (notifier, mut captured) = crate::telegram::TelegramNotifier::capture();
    let mut worker = Worker::new(rig.config(true)).await.unwrap();
    worker.telegram = Some(notifier);
    worker.suspicion_retry = std::time::Duration::ZERO;
    worker.tick().await.unwrap();
    rig.chain(|chain| chain.include());
    let first_block = rig.chain(|chain| chain.head);
    worker.tick().await.unwrap();
    *worker.finality_audit_due.lock().unwrap() = None;
    rig.chain(|chain| chain.mine(1));
    worker.tick().await.unwrap();
    let finalized = || async {
        sqlx::query_as::<_, (String, i64, String)>(
            "SELECT hash,block_number,block_hash FROM finalized_receipts",
        )
        .fetch_all(&worker.journal.pool)
        .await
        .unwrap()
    };
    assert_eq!(finalized().await.len(), 1);
    assert_eq!(finalized().await[0].1, i64::try_from(first_block).unwrap());
    // The sequencer replaces the block, and the keeper's fulfillment is sequenced again in a later one.
    rig.chain(|chain| {
        chain.drop_blocks(first_block, false);
        chain.mine(1);
    });
    let mut incidents = std::collections::BTreeSet::new();
    let mut messages = 0;
    for _ in 0..24 {
        *worker.finality_audit_due.lock().unwrap() = None;
        worker.tick().await.unwrap();
        if let Some(last) = worker
            .journal
            .meta(crate::journal::LAST_RECOVERY_KEY)
            .await
            .unwrap()
        {
            let last: serde_json::Value = serde_json::from_str(&last).unwrap();
            incidents.insert((
                last["id"].as_str().unwrap().to_owned(),
                last["mismatch"]["kind"].as_str().unwrap().to_owned(),
                last["mismatch"]["number"].as_u64().unwrap(),
            ));
        }
        messages += captured
            .events()
            .into_iter()
            .filter(|event| matches!(event, crate::telegram::Event::Finality(_)))
            .count();
        rig.chain(|chain| chain.include());
        rig.chain(|chain| chain.mine(1));
    }
    assert!(incidents.len() <= 2, "{incidents:?}");
    assert!(messages <= 2, "{messages} messages");
    assert!(worker.journal.finality_mismatch().await.unwrap().is_none());
    assert!(!worker.suspected().unwrap());
    // The receipt is final once, in the block the chain has it in.
    let rows = finalized().await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let (_, number, hash) = &rows[0];
    assert_ne!(*number, i64::try_from(first_block).unwrap());
    assert_eq!(
        *hash,
        rig.chain(|chain| chain
            .block_hash(u64::try_from(*number).unwrap())
            .to_string())
    );
    worker.journal.pool.close().await;
}

/// M3, its cause. Of two endpoints, one answers the `finalized` tag with the latest block. The audit makes a receipt
/// final only at or below the lowest `finalized` header of the endpoints that answer, so the receipt waits for the
/// honest endpoint's finality.
#[tokio::test]
async fn the_audit_makes_final_only_what_every_answering_endpoint_has_finalized() {
    let rig = soft_pair(0).await;
    rig.chain(|chain| chain.finalized_lag = 20);
    rig.endpoints()[0].set(Mode::FinalizedLatest);
    ready(&rig);
    let worker = Worker::new(rig.config(true)).await.unwrap();
    worker.tick().await.unwrap();
    rig.chain(|chain| chain.include());
    let included = rig.chain(|chain| chain.head);
    worker.tick().await.unwrap();
    let finalized = || async {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM finalized_receipts")
            .fetch_one(&worker.journal.pool)
            .await
            .unwrap()
    };
    rig.chain(|chain| chain.mine(5));
    *worker.finality_audit_due.lock().unwrap() = None;
    worker.tick().await.unwrap();
    assert_eq!(finalized().await, 0, "not final at the honest endpoint");
    rig.chain(|chain| chain.mine(included + 20 - chain.head));
    *worker.finality_audit_due.lock().unwrap() = None;
    worker.tick().await.unwrap();
    assert_eq!(finalized().await, 1);
    worker.journal.pool.close().await;
}

/// A round keeper's endpoint that does not answer at startup is held out of reads, not dropped: it is probed again with
/// a growing wait, and read from again once it answers. Until then it is not asked, and nothing fails.
#[tokio::test]
async fn an_endpoint_down_at_startup_is_probed_again_and_admitted_once_it_answers() {
    let mut rig = crate::round_mode::round_rig().await.unheld();
    rig.add_endpoint().await;
    rig.endpoints()[1].set(Mode::Down);
    let worker = Worker::new(rig.config(true)).await.unwrap();
    assert_eq!((worker.rpc.urls.len(), worker.rpc.admitted()), (2, 1));
    assert_eq!(worker.rpc.held(), [1]);
    let due = |worker: &Worker| {
        for probe in worker.readmission.lock().unwrap().values_mut() {
            probe.next = tokio::time::Instant::now();
        }
    };
    // Still down when probed: held, and probed later. The probe runs beside the tick.
    worker.tick().await.unwrap();
    due(&worker);
    worker.tick().await.unwrap();
    worker.probes_settled().await;
    assert_eq!(worker.rpc.held(), [1]);
    let wait = worker.readmission.lock().unwrap()[&1]
        .next
        .saturating_duration_since(tokio::time::Instant::now());
    assert!(wait > std::time::Duration::from_secs(15), "{wait:?}");
    // It answers: admitted at its next probe, and read from again.
    rig.endpoints()[1].set(Mode::Up);
    due(&worker);
    worker.tick().await.unwrap();
    worker.probes_settled().await;
    assert!(worker.rpc.held().is_empty());
    assert_eq!(worker.rpc.admitted(), 2);
    assert!(worker.readmission.lock().unwrap().is_empty());
    worker.journal.pool.close().await;
}

/// R4 L1: the owner was asked to add a provider while one stayed down. A restart while it is still down, held for less
/// than PAGE_AFTER so far, does not ask again; once it is read from again, the notice ends.
#[tokio::test]
async fn a_restart_while_a_provider_is_down_does_not_ask_again_for_a_provider() {
    let mut rig = crate::round_mode::round_rig().await.unheld();
    rig.add_endpoint().await;
    rig.endpoints()[1].set(Mode::Down);
    let owner = |captured: &mut crate::telegram::Captured| {
        captured
            .events()
            .into_iter()
            .filter(|event| {
                matches!(event, crate::telegram::Event::Owner(text) if text.contains("RPC sağlayıcısıyla"))
            })
            .count()
    };
    let (notifier, mut captured) = crate::telegram::TelegramNotifier::capture();
    let mut worker = Worker::new(rig.config(true)).await.unwrap();
    worker.set_telegram(notifier);
    worker.finality_page_after = std::time::Duration::ZERO;
    worker.tick().await.unwrap();
    assert_eq!(owner(&mut captured), 1);
    worker.journal.pool.close().await;
    drop(worker);
    let (notifier, mut captured) = crate::telegram::TelegramNotifier::capture();
    let mut worker = Worker::new(rig.config(true)).await.unwrap();
    worker.set_telegram(notifier);
    worker.tick().await.unwrap();
    assert_eq!(owner(&mut captured), 0);
    assert!(worker.journal.meta(SINGLE).await.unwrap().is_some());
    rig.endpoints()[1].set(Mode::Up);
    for probe in worker.readmission.lock().unwrap().values_mut() {
        probe.next = tokio::time::Instant::now();
    }
    worker.tick().await.unwrap();
    worker.probes_settled().await;
    worker.tick().await.unwrap();
    assert!(worker.journal.meta(SINGLE).await.unwrap().is_none());
    worker.journal.pool.close().await;
}
const SINGLE: &str = "rpc:single_endpoint_alerted";
