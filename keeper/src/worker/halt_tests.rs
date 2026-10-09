//! The incident of soft finality (keeper task C4) asked of the worker directly: where it signs and broadcasts while a
//! mismatch is suspected, how the endpoints settle a suspicion, how the recovery runs and fails without anyone's step,
//! and what the owner is told. The scenarios of `soft_halt` run the whole tick through one process per run; these keep
//! one worker, as the binary does, and look into it.
use super::*;
use crate::{
    config::{ChainSettings, FinalityMode},
    rig::Rig,
    scripted::Mode,
    soft_finality::{ROBINHOOD_TESTNET, ready, settings, soft, soft_pair},
    telegram::{Captured, Event, TelegramNotifier},
};
use std::time::Duration;

/// A keeper process on the rig's chain, as `Rig::run` starts one.
async fn start(rig: &Rig, send: bool) -> Worker {
    rig.node.hold(Duration::ZERO);
    Worker::new(rig.config(send)).await.unwrap()
}
/// A keeper process whose Telegram messages a test reads.
async fn told(rig: &Rig, send: bool) -> (Worker, Captured) {
    let (notifier, captured) = TelegramNotifier::capture();
    let mut worker = start(rig, send).await;
    worker.telegram = Some(notifier);
    (worker, captured)
}
/// The process ends: its journal closes and its locks go.
async fn stop(worker: Worker) {
    worker.journal.pool.close().await;
    drop(worker);
}
fn found() -> Mismatch {
    Mismatch {
        kind: "receipt".into(),
        number: 7,
        reference: "0xtx".into(),
        expected: "0xrecorded".into(),
        actual: "0xchain".into(),
        detected_at: 99,
    }
}
fn plan(head: &Head, kind: &str, payload: &str) -> TxPlan {
    TxPlan {
        nonce: 0,
        gas: 800_000,
        fee: head.base_fee * 2 + 1,
        priority: 1,
        payload: payload.into(),
        kind: kind.into(),
    }
}
async fn count(worker: &Worker, table: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
        .fetch_one(&worker.journal.pool)
        .await
        .unwrap()
}
fn halted(error: &anyhow::Error) -> bool {
    error.downcast_ref::<FinalityHalted>().is_some()
}
/// The Finality messages among the events a notifier was given since the last look.
fn finality_texts(captured: &mut Captured) -> Vec<String> {
    captured
        .events()
        .into_iter()
        .filter_map(|event| match event {
            Event::Finality(text) => Some(text),
            _ => None,
        })
        .collect()
}
/// The sequencer replaces the block the last tick decided on, and mines one after it.
fn replace_decided(rig: &Rig) {
    rig.chain(|chain| {
        let decided = chain.head;
        chain.replace_blocks(decided);
        chain.mine(1);
    });
}
/// One tick of a process, and the end of the epoch fetch it started, as the binary's next tick would find it.
async fn tick(worker: &Worker) {
    worker.tick().await.unwrap();
    worker.stop_epoch_fetch().await.unwrap();
}
/// The suspicion the worker holds, if any.
fn held(worker: &Worker) -> Option<suspicion::Suspicion> {
    worker.suspicion.lock().unwrap().clone()
}
/// Make the next ask of the endpoints about the suspicion due now, as the wait after the last one having passed.
fn due_now(worker: &Worker) {
    if let Some(held) = worker.suspicion.lock().unwrap().as_mut() {
        held.next = tokio::time::Instant::now();
    }
}
async fn last_recovery(worker: &Worker) -> serde_json::Value {
    serde_json::from_str(
        &worker
            .journal
            .meta(crate::journal::LAST_RECOVERY_KEY)
            .await
            .unwrap()
            .expect("a recovery was recorded"),
    )
    .unwrap()
}

#[tokio::test]
async fn every_place_that_signs_or_broadcasts_refuses_by_itself_while_a_mismatch_is_suspected() {
    let rig = soft(0).await;
    ready(&rig);
    // A fulfillment signed and sent, and not yet included: a lane with a transaction in it.
    rig.run(true, false).await;
    assert_eq!(rig.sent().len(), 1);
    let worker = start(&rig, true).await;
    let head = worker.rpc.decision_head().await.unwrap();
    let broadcast_before: i64 = sqlx::query_scalar("SELECT broadcast FROM txs")
        .fetch_one(&worker.journal.pool)
        .await
        .unwrap();
    worker.suspect(found(), "A test's suspicion").await.unwrap();

    // Sending what is in the lane: nothing is broadcast, and not even the bookkeeping of a broadcast is written.
    let error = worker
        .broadcast_latest(head.timestamp + 100)
        .await
        .unwrap_err();
    assert!(halted(&error), "{error:#}");
    let broadcast_after: i64 = sqlx::query_scalar("SELECT broadcast FROM txs")
        .fetch_one(&worker.journal.pool)
        .await
        .unwrap();
    assert_eq!(broadcast_after, broadcast_before);
    assert_eq!(rig.sent().len(), 1);

    // Signing: a cancellation, a replacement, a single fulfillment, a batch and an epoch commit are all this function.
    for (job, kind, members) in [
        ("1", "cancel", vec![]),
        ("1", "fulfill", vec![]),
        ("1", "epoch", vec![]),
        (
            "batch:1:2:00",
            "fulfill_batch",
            vec!["1".to_owned(), "2".to_owned()],
        ),
    ] {
        let error = worker
            .sign_and_journal_members(
                job,
                plan(&head, kind, "0x"),
                head.timestamp,
                &members,
                &head,
            )
            .await
            .unwrap_err();
        assert!(halted(&error), "{kind}: {error:#}");
    }
    let error = worker
        .sign_and_journal("1", plan(&head, "cancel", "0x"), head.timestamp, &head)
        .await
        .unwrap_err();
    assert!(halted(&error), "{error:#}");
    // The replacement a reconciliation makes goes through it too.
    let error = worker
        .replace_within_budget("1", plan(&head, "fulfill", "0x"), &head)
        .await
        .unwrap_err();
    assert!(halted(&error), "{error:#}");
    assert_eq!(count(&worker, "txs").await, 1, "no attempt was journaled");

    // An operator sweep, and a recovery's fill, which is a transfer too: not signed, and a transfer that was signed is not
    // broadcast.
    let to = Address::repeat_byte(0xfe);
    let error = worker
        .sign_transfer(
            &plan(&head, "sweep", "0x"),
            to,
            U256::from(1),
            head.timestamp,
        )
        .await
        .unwrap_err();
    assert!(halted(&error), "{error:#}");
    let mut attempt = crate::sweep::Attempt {
        request: crate::sweep::Request {
            mode: crate::sweep::Mode::Amount,
            wei: "1".into(),
            requested_at: 1,
        },
        nonce: 5,
        to: to.to_string(),
        value: "1".into(),
        txs: vec![crate::sweep::SignedTx {
            kind: "sweep".into(),
            hash: B256::repeat_byte(0x11).to_string(),
            raw: "0x00".into(),
            gas: 21_000,
            fee: "1".into(),
            priority: "1".into(),
            created: 1,
            broadcast: 0,
        }],
    };
    let error = worker.broadcast_sweep(&mut attempt).await.unwrap_err();
    assert!(halted(&error), "{error:#}");
    assert_eq!(attempt.txs[0].broadcast, 0);
    assert!(
        worker
            .journal
            .meta(crate::sweep::ATTEMPT_KEY)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(rig.sent().len(), 1);

    // Once the mismatch is on record the suspicion is the recovery's, and the recovery signs and broadcasts the lane it
    // puts right.
    worker
        .journal
        .record_finality_mismatch(&found())
        .await
        .unwrap();
    worker.settle_suspicion().await.unwrap();
    assert!(held(&worker).is_none());
    worker.ensure_not_halted().await.unwrap();
    worker
        .sign_transfer(
            &plan(&head, "sweep", "0x"),
            to,
            U256::from(1),
            head.timestamp,
        )
        .await
        .unwrap();
    stop(worker).await;
}

#[tokio::test]
async fn the_tick_gate_follows_the_suspicion_and_the_record_and_the_recovery_waits_for_nobody() {
    let rig = soft(0).await;
    let worker = start(&rig, true).await;
    assert!(worker.may_send());
    assert!(!worker.finality_incident_open().await);
    worker.tick().await.unwrap();
    assert!(worker.may_send());
    // A suspicion that arises behind the tick's back (the audit's, between the start of a tick and its gate) is seen at
    // the gate of the same tick, before anything is started. The one endpoint shows another hash than the journal's,
    // and there is no second one: it stays a suspicion, the sends are held, and nothing is recorded.
    worker.suspect(found(), "A test's suspicion").await.unwrap();
    assert!(worker.finality_incident_open().await);
    worker.tick().await.unwrap();
    assert!(!worker.may_send());
    assert!(worker.sending_halted().await.unwrap());
    assert!(worker.journal.finality_mismatch().await.unwrap().is_none());
    let noted = worker.journal.suspicion_note().await.unwrap().unwrap();
    assert_eq!(
        (
            noted.mismatch,
            noted.checks,
            noted.endpoints,
            noted.answered
        ),
        (found(), 1, 1, 1)
    );
    let health: crate::health::Status =
        serde_json::from_str(&worker.journal.meta("health:status").await.unwrap().unwrap())
            .unwrap();
    assert_eq!(health.faults, ["finality_unconfirmed"]);
    // On record, by an operator's word here: the recovery runs at the next tick by itself, and as there is nothing to
    // put right it clears the record in that tick; the keeper sends again in it.
    worker
        .journal
        .acknowledge_finality(&found().id(), 100)
        .await
        .unwrap();
    worker.tick().await.unwrap();
    assert!(held(&worker).is_none());
    assert!(!worker.finality_incident_open().await);
    assert!(worker.may_send());
    assert!(worker.journal.suspicion_note().await.unwrap().is_none());
    assert_eq!(last_recovery(&worker).await["acknowledged_at"], 100);
    stop(worker).await;
}

#[tokio::test]
async fn two_endpoints_that_agree_on_a_replaced_block_start_a_recovery_that_resends_and_resumes_with_one_message()
 {
    let rig = soft_pair(0).await;
    ready(&rig);
    let (worker, mut captured) = told(&rig, true).await;
    // Request 1 is served at nonce 0, and the receipt settled at the decision head.
    worker.tick().await.unwrap();
    rig.chain(|chain| chain.include());
    let included = rig.chain(|chain| chain.head);
    worker.tick().await.unwrap();
    let original = crate::soft_finality::sent_hash(&rig, 0);
    assert_eq!(worker.journal.nonce_floor().await.unwrap(), 1);
    assert!(finality_texts(&mut captured).is_empty());

    // The sequencer loses the block with the fulfillment. Both endpoints show it: the tick confirms it, records it,
    // and recovers at once, broadcasting the bytes it kept, with no one's step and nothing told yet.
    rig.chain(|chain| chain.drop_blocks(included, false));
    worker.tick().await.unwrap();
    let incident = worker.journal.finality_mismatch().await.unwrap().unwrap();
    assert_eq!(incident.kind, "soft_checkpoint");
    assert_eq!(rig.sent().len(), 2);
    assert_eq!(crate::soft_finality::sent_hash(&rig, 1), original);
    assert!(!worker.may_send());
    assert!(finality_texts(&mut captured).is_empty());
    let confirmed: serde_json::Value = serde_json::from_str(
        &worker
            .journal
            .meta(suspicion::CONFIRMED_KEY)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        (
            confirmed["agreeing"].clone(),
            confirmed["answered"].clone(),
            confirmed["cooled"].clone()
        ),
        (2.into(), 2.into(), serde_json::json!([]))
    );

    // The sequencer includes it; the next tick settles it, the one after clears the record, and the owner is told once
    // that the keeper sent it again by itself.
    rig.chain(|chain| chain.include());
    worker.tick().await.unwrap();
    worker.tick().await.unwrap();
    assert!(worker.journal.finality_mismatch().await.unwrap().is_none());
    assert!(worker.may_send());
    let texts = finality_texts(&mut captured);
    assert_eq!(
        texts,
        [recovery::recovered_text(ROBINHOOD_TESTNET, &incident, true)]
    );
    assert!(texts[0].contains("yeniden gönderdi"), "{}", texts[0]);
    let last = last_recovery(&worker).await;
    assert_eq!(last["acknowledged_at"], serde_json::Value::Null);
    assert_eq!(last["confirmed"]["agreeing"], 2);
    assert_eq!(last["done"]["reopened_nonces"], serde_json::json!([0]));
    for _ in 0..3 {
        worker.tick().await.unwrap();
    }
    assert!(finality_texts(&mut captured).is_empty());
    stop(worker).await;

    // A restart tells nothing again, and the next replaced block is told once more, with its own record: nothing was
    // to be sent again this time.
    let (worker, mut captured) = told(&rig, true).await;
    worker.tick().await.unwrap();
    assert!(finality_texts(&mut captured).is_empty());
    // The receipt is in the block replaced this time, and the sequencer sequences it again in the block that takes its
    // place: the first tick settles the nonce from it there, the second clears the record.
    replace_decided(&rig);
    worker.tick().await.unwrap();
    assert!(worker.journal.finality_mismatch().await.unwrap().is_some());
    worker.tick().await.unwrap();
    assert!(worker.journal.finality_mismatch().await.unwrap().is_none());
    let second = last_recovery(&worker).await;
    assert_ne!(second["id"], incident.id());
    let texts = finality_texts(&mut captured);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(
        texts[0].contains("Yeniden gönderilmesi gereken bir işlem yoktu"),
        "{}",
        texts[0]
    );
    assert!(texts[0].contains(second["id"].as_str().unwrap()));
    assert_eq!(
        (
            second["done"]["reopened_nonces"].clone(),
            second["done"]["resent_nonces"].clone()
        ),
        (serde_json::json!([0]), serde_json::json!([]))
    );
    stop(worker).await;
}

#[tokio::test]
async fn an_endpoint_that_shows_another_fork_is_put_on_cooldown_and_the_keeper_goes_on_with_the_others()
 {
    let mut rig = soft_pair(0).await;
    rig.add_endpoint().await;
    let (worker, mut captured) = told(&rig, true).await;
    worker.tick().await.unwrap();
    // The first endpoint, which the keeper reads from, follows another fork from here; the others are the sequencer's.
    rig.endpoints()[0].set(Mode::Fork(0));
    ready(&rig);
    // The tick finds the block it decided on under another hash at the first endpoint, asks all three, and the other two
    // show the journal's: the first is put on cooldown, the head is read again from the others, and the same tick proves
    // and sends the request. Nothing is recorded, held or told. (With two endpoints it would be one against one, which
    // settles nothing: `finality_review::one_endpoint_against_one_is_unresolved_and_cools_nobody`.)
    worker.tick().await.unwrap();
    assert!(worker.rpc.cooling(0) && !worker.rpc.cooling(1) && !worker.rpc.cooling(2));
    assert!(held(&worker).is_none());
    assert!(worker.journal.finality_mismatch().await.unwrap().is_none());
    assert!(worker.journal.suspicion_note().await.unwrap().is_none());
    assert_eq!(rig.sent().len(), 1, "{:?}", rig.sent());
    assert!(worker.may_send());
    assert!(finality_texts(&mut captured).is_empty());
    // The checkpoint is the sequencer's block, and the next tick, read from the second endpoint, finds nothing amiss.
    let (number, hash): (u64, String) = serde_json::from_str(
        &worker
            .journal
            .meta("soft_checkpoint")
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(hash, crate::soft_finality::hash_of(&rig, number));
    rig.chain(|chain| chain.include());
    worker.tick().await.unwrap();
    assert!(held(&worker).is_none());
    assert!(worker.journal.finality_mismatch().await.unwrap().is_none());
    let health: crate::health::Status =
        serde_json::from_str(&worker.journal.meta("health:status").await.unwrap().unwrap())
            .unwrap();
    assert!(health.healthy, "{health:?}");
    stop(worker).await;
}

#[tokio::test]
async fn without_two_endpoints_that_agree_the_sends_are_held_and_asked_again_and_the_owner_is_asked_only_later()
 {
    // A keeper with one endpoint, which shows the block the last tick decided on replaced.
    let rig = soft(0).await;
    ready(&rig);
    let (mut worker, mut captured) = told(&rig, true).await;
    worker.suspicion_retry = Duration::from_secs(10);
    worker.tick().await.unwrap();
    assert_eq!(rig.sent().len(), 1);
    rig.chain(|chain| chain.drop_queue());
    replace_decided(&rig);
    // Each tick holds the sends, records nothing and does not fail; the endpoints are asked again only when the wait has
    // passed, and the wait doubles, up to its bound.
    let asked = |worker: &Worker| held(worker).map_or(0, |held| held.checks);
    let waits = |worker: &Worker| {
        held(worker)
            .unwrap()
            .next
            .saturating_duration_since(tokio::time::Instant::now())
    };
    worker.tick().await.unwrap();
    assert_eq!(asked(&worker), 1);
    for _ in 0..3 {
        worker.tick().await.unwrap();
        assert_eq!(asked(&worker), 1, "not asked again before the wait");
    }
    let wait = waits(&worker);
    assert!(
        wait > Duration::from_secs(9) && wait <= Duration::from_secs(10),
        "{wait:?}"
    );
    for (checks, bound) in [(2, 20), (3, 30), (4, 30)] {
        due_now(&worker);
        worker.tick().await.unwrap();
        assert_eq!(asked(&worker), checks);
        let wait = waits(&worker);
        assert!(
            wait > Duration::from_secs(bound - 1) && wait <= Duration::from_secs(bound),
            "{checks}: {wait:?}"
        );
    }
    assert!(!worker.may_send());
    assert!(worker.journal.finality_mismatch().await.unwrap().is_none());
    assert_eq!(
        rig.sent().len(),
        1,
        "nothing was sent, replaced or broadcast again"
    );
    assert!(worker.finality_incident_open().await);
    assert!(!tick_failure_ends_the_run(false, true, u64::MAX, 5));
    // Nothing is told while the wait for the owner has not passed.
    assert!(finality_texts(&mut captured).is_empty());

    // Once it has, the owner is asked once, with what to do: a keeper with one endpoint needs a second.
    worker.finality_page_after = Duration::ZERO;
    due_now(&worker);
    worker.tick().await.unwrap();
    let suspected = held(&worker).unwrap().found;
    let texts = finality_texts(&mut captured);
    assert_eq!(texts, [suspicion::unconfirmed_text(&suspected, 1)]);
    assert!(
        texts[0].contains("RPC_URLS") && texts[0].contains("ikinci"),
        "{}",
        texts[0]
    );
    assert!(texts[0].contains(&format!("--acknowledge {}", suspected.id())));
    for _ in 0..2 {
        due_now(&worker);
        worker.tick().await.unwrap();
    }
    assert!(finality_texts(&mut captured).is_empty(), "asked once");

    // An operator who is sure the chain changed acknowledges the id the message names: the keeper recovers from it at
    // its next tick, and tells that it did.
    worker
        .journal
        .acknowledge_finality(&suspected.id(), 500)
        .await
        .unwrap();
    for _ in 0..4 {
        worker.tick().await.unwrap();
    }
    assert!(worker.journal.finality_mismatch().await.unwrap().is_none());
    assert!(held(&worker).is_none());
    let texts = finality_texts(&mut captured);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(
        texts[0].contains("Bir şey yapmanız gerekmiyor"),
        "{}",
        texts[0]
    );
    assert_eq!(last_recovery(&worker).await["acknowledged_at"], 500);
    stop(worker).await;
}

#[tokio::test]
async fn a_recovery_step_that_fails_is_tried_again_and_the_owner_is_asked_only_when_it_keeps_failing()
 {
    let rig = soft_pair(0).await;
    let (mut worker, mut captured) = told(&rig, true).await;
    worker.tick().await.unwrap();
    // Both endpoints refuse to tell the wallet's nonce, which the recovery reads; the blocks they serve.
    for endpoint in rig.endpoints() {
        endpoint.set(Mode::Refusing("eth_getTransactionCount", "scripted outage"));
    }
    replace_decided(&rig);
    // The mismatch is confirmed and recorded; the recovery fails at each tick, and no tick fails with it.
    for _ in 0..3 {
        worker.tick().await.unwrap();
        assert!(worker.journal.finality_mismatch().await.unwrap().is_some());
        assert!(!worker.may_send());
    }
    assert!(finality_texts(&mut captured).is_empty());
    // It has failed long enough: the owner is asked once, with the reason and what to do; a restart does not ask again.
    worker.finality_page_after = Duration::ZERO;
    worker.tick().await.unwrap();
    let incident = worker.journal.finality_mismatch().await.unwrap().unwrap();
    let texts = finality_texts(&mut captured);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(
        texts[0].contains("RPC sağlayıcılarına ulaşılamıyor") && texts[0].contains(&incident.id()),
        "{}",
        texts[0]
    );
    worker.tick().await.unwrap();
    stop(worker).await;
    let (mut worker, mut captured) = told(&rig, true).await;
    worker.finality_page_after = Duration::ZERO;
    worker.tick().await.unwrap();
    assert!(
        finality_texts(&mut captured).is_empty(),
        "asked once for the incident"
    );
    // The endpoints answer again: the next tick's step succeeds, the record is cleared, and the owner is told it is over.
    for endpoint in rig.endpoints() {
        endpoint.set(Mode::Up);
    }
    worker.tick().await.unwrap();
    assert!(worker.journal.finality_mismatch().await.unwrap().is_none());
    assert!(worker.may_send());
    let texts = finality_texts(&mut captured);
    assert_eq!(
        texts,
        [recovery::recovered_text(
            ROBINHOOD_TESTNET,
            &incident,
            false
        )]
    );
    assert!(
        worker
            .journal
            .meta(crate::journal::ALERTED_KEY)
            .await
            .unwrap()
            .is_none()
    );
    stop(worker).await;
}

/// The recovery of an epoch coordinator's keeper from the loss of the block in which another committer published epoch
/// 2, in which request 1 waits proven, and which changed request 1's proof input: what it reported, what it asked the
/// chain in its ticks, and the state of epoch 2's work afterwards.
async fn epoch_lost() -> (serde_json::Value, Vec<String>, String) {
    let rig = soft_pair(0).await;
    rig.chain(|chain| chain.mine_to(chain.epoch_start(2) + 2));
    rig.run(false, false).await;
    rig.request();
    rig.chain(|chain| chain.mine(1));
    let published = rig.chain(|chain| {
        let at = chain.head;
        chain.publish_epoch(2, at);
        at
    });
    rig.chain(|chain| chain.mine(3));
    let run = rig.run(false, false).await;
    assert!(
        run.journal.contains("jobs [1=prepared]") && run.journal.contains("epochs [2=committed]"),
        "{}",
        run.journal
    );
    let worker = start(&rig, false).await;
    // The sequencer loses the block of the publication: the registry has no epoch 2 now, and the seed is another.
    rig.chain(|chain| {
        chain.drop_blocks(published, false);
        chain.reseed(1);
    });
    rig.node.take();
    let mut ticks = Vec::new();
    for _ in 0..4 {
        tick(&worker).await;
        ticks.extend(crate::scripted::render(&rig.node.take()));
        if worker.journal.finality_mismatch().await.unwrap().is_none()
            && worker
                .journal
                .meta(crate::journal::LAST_RECOVERY_KEY)
                .await
                .unwrap()
                .is_some()
        {
            break;
        }
    }
    assert!(worker.journal.finality_mismatch().await.unwrap().is_none());
    let last = last_recovery(&worker).await;
    let epoch: String = sqlx::query_scalar("SELECT state FROM epoch_work WHERE epoch=2")
        .fetch_one(&worker.journal.pool)
        .await
        .unwrap();
    stop(worker).await;
    (last, ticks, epoch)
}

#[tokio::test]
async fn the_epoch_steps_of_the_recovery_read_the_proof_context_and_the_registry() {
    // An epoch coordinator: the recovery reads the proof context, and finds request 1's seed changed, and the
    // registry's epochs, and finds epoch 2 gone. A round coordinator's keeper has neither step, and reads its requests
    // with getRoundRequest (round_mode::a_round_keeper_recovers_with_the_round_coordinators_reads_alone).
    let (last, ticks, epoch) = epoch_lost().await;
    assert_eq!(last["coordinator"], "epoch");
    assert_eq!(
        (
            last["done"]["epochs_reopened"].clone(),
            last["done"]["jobs_reproved"].clone()
        ),
        (1.into(), 1.into())
    );
    assert!(
        ticks.iter().any(|line| line.contains("getProofContext")),
        "{ticks:#?}"
    );
    assert_ne!(epoch, "committed");
}

#[test]
fn the_messages_say_in_plain_turkish_what_happened_and_what_to_do() {
    let incident = found();
    let id = incident.id();
    assert_eq!(
        recovery::recovered_text(ROBINHOOD_TESTNET, &incident, true),
        format!(
            "Robinhood'da sequencer bir bloğu değiştirdi; keeper etkilenen işlemleri kendisi yeniden gönderdi ve çalışmaya devam ediyor. Bir şey yapmanız gerekmiyor.\nBlok: 7, kayıt: {id}"
        )
    );
    assert_eq!(
        recovery::recovered_text(31_337, &incident, false),
        format!(
            "Zincirde sequencer bir bloğu değiştirdi; keeper kayıtlarını yeni zincire göre kendisi düzeltti ve çalışmaya devam ediyor. Yeniden gönderilmesi gereken bir işlem yoktu. Bir şey yapmanız gerekmiyor.\nBlok: 7, kayıt: {id}"
        )
    );
    assert_eq!(
        suspicion::unconfirmed_text(&incident, 1),
        format!(
            "Keeper işlem göndermeyi bekletiyor.\nTek RPC adresi, keeper'ın daha önce kullandığı bir bloğun değiştiğini gösteriyor; ikinci bir sağlayıcı olmadan bu doğrulanamıyor.\nYapmanız gereken: RPC_URLS ayarına başka bir sağlayıcıdan ikinci bir RPC adresi ekleyip keeper'ı yeniden başlatın.\nBlok eski hâline dönerse keeper kendiliğinden devam eder. Zincirin gerçekten değiştiğinden eminseniz keeper'ı şu komutla devam ettirebilirsiniz:\nd20dao-keeper finality --db <journal> --acknowledge {id}\nBlok: 7"
        )
    );
    assert_eq!(
        suspicion::unconfirmed_text(&incident, 3),
        format!(
            "Keeper işlem göndermeyi bekletiyor.\nRPC sağlayıcıları, keeper'ın daha önce kullandığı bir blok hakkında anlaşamıyor ya da yanıt vermiyor; bloğun değişip değişmediği doğrulanamıyor.\nYapmanız gereken: RPC_URLS'teki adresleri kontrol edin; yanıt vermeyen ya da farklı bir zincir gösteren sağlayıcıyı değiştirip keeper'ı yeniden başlatın.\nSağlayıcılar anlaşınca keeper kendiliğinden devam eder. Zincirin gerçekten değiştiğinden eminseniz keeper'ı şu komutla devam ettirebilirsiniz:\nd20dao-keeper finality --db <journal> --acknowledge {id}\nBlok: 7"
        )
    );
    let unreachable = crate::rpc::rate_limited_error("every endpoint is down");
    assert_eq!(
        suspicion::failing_text(&incident, &unreachable, Duration::from_secs(660)),
        format!(
            "Keeper bir blok değişikliğinden sonra kayıtlarını düzeltiyor, ama 11 dakikadır bitiremiyor; bitene kadar yeni işlem göndermiyor.\nSebep: RPC sağlayıcılarına ulaşılamıyor.\nYapmanız gereken: RPC_URLS'teki sağlayıcıların çalıştığını kontrol edin; gerekirse çalışan bir adres ekleyip keeper'ı yeniden başlatın. Keeper bu arada denemeye devam ediyor.\nBlok: 7, kayıt: {id}"
        )
    );
    let other = anyhow::anyhow!("a journal row that does not read");
    let text = suspicion::failing_text(&incident, &other, Duration::ZERO);
    assert!(
        text.contains("1 dakikadır")
            && text.contains("Düzeltme adımlarından biri hata veriyor")
            && text.contains("\"Finality recovery deferred\""),
        "{text}"
    );
    // No host, path, key or error text is in any of them.
    for text in [
        recovery::recovered_text(ROBINHOOD_TESTNET, &incident, true),
        suspicion::unconfirmed_text(&incident, 2),
        text,
    ] {
        assert!(
            !text.contains("http") && !text.contains(".sqlite") && !text.contains("journal row"),
            "{text}"
        );
    }
    assert!(
        FinalityHalted
            .to_string()
            .contains("nothing is signed or broadcast")
    );
}

#[tokio::test]
async fn a_finalized_keeper_has_no_incident_to_consult() {
    // A record in the journal of a keeper that decides on the finalized head is not read: Arc has no such record, and the
    // places that sign and broadcast do not look for one.
    let rig = Rig::new(ROBINHOOD_TESTNET, settings(FinalityMode::Finalized, 0))
        .await
        .unheld();
    ready(&rig);
    let worker = start(&rig, true).await;
    worker
        .journal
        .record_finality_mismatch(&found())
        .await
        .unwrap();
    worker.ensure_not_halted().await.unwrap();
    assert!(!worker.sending_halted().await.unwrap());
    assert!(!worker.finality_incident_open().await);
    worker.tick().await.unwrap();
    assert!(worker.may_send());
    assert!(
        !worker
            .finality_open
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    assert!(
        !worker
            .recovery_lane
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    assert!(held(&worker).is_none());
    stop(worker).await;
}

#[tokio::test]
async fn a_finalized_keeper_whose_checkpoint_is_replaced_fails_the_tick_as_it_always_has() {
    let rig = Rig::new(ROBINHOOD_TESTNET, ChainSettings::default())
        .await
        .unheld();
    rig.chain(|chain| chain.finalized_lag = 3);
    ready(&rig);
    rig.run(false, false).await;
    // The finalized block the last tick decided on is replaced.
    rig.chain(|chain| {
        let finalized = chain.finalized_head();
        chain.replace_blocks(finalized);
    });
    let error = rig
        .run_with(true, false, |_| {})
        .await
        .err()
        .expect("a replaced finalized checkpoint stops the tick");
    let changed = error.downcast_ref::<CheckpointChanged>().unwrap();
    assert_eq!(changed.mode, FinalityMode::Finalized);
    assert_eq!(
        error.to_string(),
        "Finalized chain checkpoint changed; preserve journal and investigate RPC/finality before resuming"
    );
    // Nothing was recorded or suspected as a finality mismatch, and nothing halted.
    let pool = rig.journal().await;
    let recorded: Option<String> = sqlx::query_scalar(
        "SELECT value FROM meta WHERE key IN ('finality:mismatch','finality:suspected')",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    pool.close().await;
    assert_eq!(recorded, None);
    assert!(rig.sent().is_empty());
}

#[tokio::test]
async fn a_recovery_without_budget_still_moves_one_step_on_at_every_tick() {
    // Twelve requests the epoch of which is not published: discovered, never proved.
    let rig = soft_pair(0).await;
    for _ in 0..12 {
        rig.request();
    }
    let mut worker = start(&rig, false).await;
    worker.recovery_budget = Duration::ZERO;
    worker.tick().await.unwrap();
    // Five jobs the keeper had settled, of requests the chain does not have.
    let deadline = rig.chain(|chain| chain.time(chain.head)) + 30;
    for id in 1_000..1_005 {
        sqlx::query("INSERT INTO jobs(id,deadline,state) VALUES(?,?,'served')")
            .bind(id.to_string())
            .bind(i64::try_from(deadline).unwrap())
            .execute(&worker.journal.pool)
            .await
            .unwrap();
    }
    replace_decided(&rig);

    // With no time to spend each tick does one step: one bisection of the request ids, or one job. The first is the
    // tick that confirms the mismatch.
    let mut searched = Vec::new();
    let mut expired = Vec::new();
    let mut ticks = 0;
    loop {
        ticks += 1;
        assert!(ticks < 60, "the recovery does not finish");
        worker.tick().await.unwrap();
        searched.push(
            worker
                .journal
                .meta("finality:recovery:rewind")
                .await
                .unwrap(),
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE state='expired'")
            .fetch_one(&worker.journal.pool)
            .await
            .unwrap();
        expired.push(count);
        if worker.journal.finality_mismatch().await.unwrap().is_none() {
            break;
        }
    }
    // The search was seen between steps, and finished.
    assert!(
        searched
            .iter()
            .any(|saved| saved.as_deref().is_some_and(|saved| saved.contains(','))),
        "{searched:?}"
    );
    // The jobs were read one at a time: the five that expired did so at five ticks, never two at once.
    assert!(
        expired.windows(2).all(|pair| pair[1] - pair[0] <= 1),
        "{expired:?}"
    );
    assert_eq!(*expired.last().unwrap(), 5, "{expired:?}");
    assert!(ticks > 12, "{ticks} ticks");
    stop(worker).await;
}

/// R2/R3 L8: a keeper restarted more often than `PAGE_AFTER` still asks the owner, once. The suspicion's note keeps when
/// the block was first suspected, and the failing recovery when it first failed; the journal keeps that the owner was
/// asked.
#[tokio::test]
async fn restarts_more_often_than_the_page_wait_still_ask_the_owner_once() {
    let back = |worker: &Worker, sql: &'static str| {
        let pool = worker.journal.pool.clone();
        async move {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
    };
    // An unsettled suspicion, with one endpoint: first suspected 700 seconds ago by a process before a restart.
    let rig = soft(0).await;
    ready(&rig);
    let (worker, mut captured) = told(&rig, true).await;
    worker.tick().await.unwrap();
    replace_decided(&rig);
    worker.tick().await.unwrap();
    assert!(held(&worker).is_some());
    assert!(finality_texts(&mut captured).is_empty());
    back(&worker, "UPDATE meta SET value=json_set(value,'$.mismatch.detected_at',json_extract(value,'$.mismatch.detected_at')-700) WHERE key='finality:suspected'").await;
    stop(worker).await;
    let (worker, mut captured) = told(&rig, true).await;
    worker.tick().await.unwrap();
    assert_eq!(finality_texts(&mut captured).len(), 1, "asked at once");
    stop(worker).await;
    let (worker, mut captured) = told(&rig, true).await;
    worker.tick().await.unwrap();
    due_now(&worker);
    worker.tick().await.unwrap();
    assert!(finality_texts(&mut captured).is_empty(), "asked once");
    stop(worker).await;

    // A recovery that has failed since 700 seconds ago, by a process before a restart.
    let rig = soft_pair(0).await;
    let (worker, mut captured) = told(&rig, true).await;
    worker.tick().await.unwrap();
    for endpoint in rig.endpoints() {
        endpoint.set(Mode::Refusing("eth_getTransactionCount", "scripted outage"));
    }
    replace_decided(&rig);
    worker.tick().await.unwrap();
    worker.tick().await.unwrap();
    assert!(worker.journal.finality_mismatch().await.unwrap().is_some());
    assert!(finality_texts(&mut captured).is_empty());
    back(&worker, "UPDATE meta SET value=CAST(value AS INTEGER)-700 WHERE key='finality:recovery:failing_since'").await;
    stop(worker).await;
    let (worker, mut captured) = told(&rig, true).await;
    worker.tick().await.unwrap();
    let texts = finality_texts(&mut captured);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(
        texts[0].contains("RPC sağlayıcılarına ulaşılamıyor"),
        "{}",
        texts[0]
    );
    stop(worker).await;
}

/// R2/R3 L8: a mark above the decision head may only be ahead of the endpoint that gave the head. The recovery waits for
/// the head to reach it, and takes it as stale only after `ABOVE_HEAD_WAIT_SECONDS` (or at once when the endpoints
/// confirmed a shorter chain: `finality_review`).
#[tokio::test]
async fn the_recovery_waits_for_a_head_below_a_mark_before_it_takes_the_mark_as_stale() {
    let rig = soft_pair(0).await;
    let (worker, _captured) = told(&rig, true).await;
    worker.tick().await.unwrap();
    let above = rig.chain(|chain| chain.head) + 50;
    sqlx::query(
        "INSERT INTO soft_marks(number,hash,kind,ref,created) VALUES(?,'0xahead','sign','0xtx',1)",
    )
    .bind(i64::try_from(above).unwrap())
    .execute(&worker.journal.pool)
    .await
    .unwrap();
    replace_decided(&rig);
    for _ in 0..4 {
        worker.tick().await.unwrap();
    }
    let marked = || async {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM soft_marks WHERE number=?")
            .bind(i64::try_from(above).unwrap())
            .fetch_one(&worker.journal.pool)
            .await
            .unwrap()
    };
    assert!(worker.journal.finality_mismatch().await.unwrap().is_some());
    assert_eq!(marked().await, 1, "waited for the head");
    sqlx::query("UPDATE meta SET value=CAST(value AS INTEGER)-30 WHERE key='finality:recovery:above_head_since'")
        .execute(&worker.journal.pool)
        .await
        .unwrap();
    for _ in 0..3 {
        worker.tick().await.unwrap();
    }
    assert_eq!(marked().await, 0);
    assert!(worker.journal.finality_mismatch().await.unwrap().is_none());
    stop(worker).await;
}

/// R2/R3 L8: an operator sweep's transfer that a replaced block took off the chain is not queued again by the keeper;
/// the owner, who alone knows whether it is still wanted, is asked once in Turkish.
#[tokio::test]
async fn a_sweep_a_replaced_block_took_off_the_chain_is_told_to_the_owner() {
    let rig = soft_pair(0).await;
    let (worker, mut captured) = told(&rig, true).await;
    worker.tick().await.unwrap();
    let head = rig.chain(|chain| chain.head);
    let transfer = format!("0x{}", "5e".repeat(32));
    sqlx::query(
        "INSERT INTO soft_marks(number,hash,kind,ref,created,status) VALUES(?,?,'receipt',?,1,1)",
    )
    .bind(i64::try_from(head).unwrap())
    .bind(crate::soft_finality::hash_of(&rig, head))
    .bind(&transfer)
    .execute(&worker.journal.pool)
    .await
    .unwrap();
    replace_decided(&rig);
    for _ in 0..4 {
        worker.tick().await.unwrap();
    }
    assert!(worker.journal.finality_mismatch().await.unwrap().is_none());
    let asked: Vec<String> = captured
        .events()
        .into_iter()
        .filter_map(|event| match event {
            Event::Owner(text) => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(asked, [recovery::lost_sweep_text(&transfer)]);
    assert!(asked[0].contains("d20dao-keeper sweep"), "{}", asked[0]);
    stop(worker).await;
}

/// R2/R3 L8: a soft keeper with one RPC endpoint asks the owner once, in Turkish, to add a provider; a restart does not
/// ask again, and a keeper with two never asks.
#[tokio::test]
async fn a_soft_keeper_with_one_endpoint_asks_the_owner_once_to_add_a_provider() {
    let owner = |captured: &mut Captured| -> Vec<String> {
        captured
            .events()
            .into_iter()
            .filter_map(|event| match event {
                Event::Owner(text) => Some(text),
                _ => None,
            })
            .collect()
    };
    let rig = soft(0).await;
    let (worker, mut captured) = told(&rig, true).await;
    worker.tick().await.unwrap();
    worker.tick().await.unwrap();
    assert_eq!(owner(&mut captured), [suspicion::single_endpoint_text(1)]);
    stop(worker).await;
    let (worker, mut captured) = told(&rig, true).await;
    worker.tick().await.unwrap();
    assert!(owner(&mut captured).is_empty());
    stop(worker).await;
    let rig = soft_pair(0).await;
    let (worker, mut captured) = told(&rig, true).await;
    worker.tick().await.unwrap();
    assert!(owner(&mut captured).is_empty());
    stop(worker).await;
}

/// R4 M1: the owner is asked about an unsettled suspicion, acknowledges it as the page says (which records it under the
/// same id), and its recovery then fails for longer than PAGE_AFTER: the owner is asked again, now about the recovery. The
/// two pages keep their own keys.
#[tokio::test]
async fn a_failing_recovery_of_an_acknowledged_suspicion_is_paged_on_its_own() {
    let rig = soft(0).await;
    let (worker, _captured) = told(&rig, true).await;
    worker.tick().await.unwrap();
    replace_decided(&rig);
    worker.tick().await.unwrap();
    assert!(held(&worker).is_some());
    sqlx::query("UPDATE meta SET value=json_set(value,'$.mismatch.detected_at',json_extract(value,'$.mismatch.detected_at')-700) WHERE key='finality:suspected'")
        .execute(&worker.journal.pool)
        .await
        .unwrap();
    stop(worker).await;
    let (worker, mut captured) = told(&rig, true).await;
    worker.tick().await.unwrap();
    assert_eq!(
        finality_texts(&mut captured).len(),
        1,
        "the unconfirmed page"
    );
    let id = worker
        .journal
        .suspicion_note()
        .await
        .unwrap()
        .unwrap()
        .mismatch
        .id();
    worker
        .journal
        .acknowledge_finality(&id, crate::health::now().unwrap())
        .await
        .unwrap();
    rig.node
        .endpoint()
        .set(Mode::Refusing("eth_getTransactionCount", "scripted outage"));
    worker.tick().await.unwrap();
    worker.tick().await.unwrap();
    assert!(worker.journal.finality_mismatch().await.unwrap().is_some());
    assert!(
        worker
            .journal
            .meta("finality:recovery:failing_since")
            .await
            .unwrap()
            .is_some()
    );
    sqlx::query("UPDATE meta SET value=CAST(value AS INTEGER)-700 WHERE key='finality:recovery:failing_since'")
        .execute(&worker.journal.pool)
        .await
        .unwrap();
    for _ in 0..3 {
        worker.tick().await.unwrap();
    }
    let texts = finality_texts(&mut captured);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(
        texts[0].contains("RPC sağlayıcılarına ulaşılamıyor"),
        "{}",
        texts[0]
    );
    worker.tick().await.unwrap();
    assert!(finality_texts(&mut captured).is_empty(), "asked once");
    stop(worker).await;
}

/// R4 L4: once the head has reached the marks the walk waited for, the wait is forgotten, so that a later mark above a
/// lagging head in the same incident waits afresh instead of being taken as stale at once.
#[tokio::test]
async fn the_wait_for_a_head_below_a_mark_ends_when_the_head_reaches_it() {
    let rig = soft_pair(0).await;
    let (worker, _captured) = told(&rig, true).await;
    worker.tick().await.unwrap();
    let head = worker.rpc.decision_head().await.unwrap();
    let mark = |number: u64, hash: String| {
        let pool = worker.journal.pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO soft_marks(number,hash,kind,ref,created) VALUES(?,?,'sign','0xtx',1)",
            )
            .bind(i64::try_from(number).unwrap())
            .bind(hash)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    let waiting = || async {
        worker
            .journal
            .meta("finality:recovery:above_head_since")
            .await
            .unwrap()
    };
    let mut stats = recovery::Stats::default();
    // A wait that began long ago, for marks the head has reached by now: it ends.
    mark(head.number, head.hash.to_string()).await;
    let long_ago = (crate::health::now().unwrap() - 60).to_string();
    worker
        .journal
        .set_meta("finality:recovery:above_head_since", &long_ago)
        .await
        .unwrap();
    assert!(
        worker
            .scan_marks(&head, false, &mut stats)
            .await
            .unwrap()
            .complete
    );
    assert!(waiting().await.is_none());
    // A later mark above the head waits afresh.
    sqlx::query("DELETE FROM meta WHERE key='finality:recovery:scan'")
        .execute(&worker.journal.pool)
        .await
        .unwrap();
    mark(head.number + 50, "0xahead".into()).await;
    let scan = worker.scan_marks(&head, false, &mut stats).await.unwrap();
    assert!(!scan.complete);
    assert!(waiting().await.is_some());
    stop(worker).await;
}
