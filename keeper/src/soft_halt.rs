//! A finality mismatch and the recovery from it (keeper task C4) against the scripted chain, on which the script replaces
//! blocks (`replace_blocks`) and makes the sequencer lose the transactions of the blocks it replaces (`drop_blocks`).
//! Each run is a keeper process that starts and ticks once. With two endpoints (`soft_pair`) both show a replacement,
//! and the keeper recovers by itself; with one (`soft`) no second endpoint can confirm it, and the keeper holds its
//! sends until an operator acknowledges it. What the keeper asks the chain in finalized mode is pinned by the golden
//! traces; this module is what only a soft keeper does when a block it acted on is not the chain's.
use crate::{
    finality,
    journal::{Journal, Mismatch, Suspected},
    rig::Rig,
    scripted::Mode,
    soft_finality::{
        LAG, hash_of, mark, marks, meta, ready, sent_hash, soft, soft_pair, suspected,
    },
    worker::tick_failure_ends_the_run,
};

/// The mismatch on record, if there is one.
async fn recorded(rig: &Rig) -> Option<Mismatch> {
    meta(rig, "finality:mismatch")
        .await
        .map(|saved| serde_json::from_str(&saved).unwrap())
}
/// The operator acknowledges the mismatch the keeper holds its sends on, as `d20dao-keeper finality --acknowledge`
/// does with the id that `--status` prints: it is recorded, and the keeper recovers from it.
async fn acknowledge(rig: &Rig) -> serde_json::Value {
    let noted: Suspected = suspected(rig).await.expect("a mismatch is suspected");
    let pool = rig.journal().await;
    let done = finality::acknowledge(&pool, &noted.mismatch.id(), 1_700_000_000)
        .await
        .unwrap();
    pool.close().await;
    done
}
/// What `d20dao-keeper finality --status` prints.
async fn status(rig: &Rig) -> serde_json::Value {
    let pool = rig.journal().await;
    let status = finality::status(&pool).await.unwrap();
    pool.close().await;
    status
}
async fn journal(rig: &Rig) -> Journal {
    Journal {
        pool: rig.journal().await,
    }
}
/// What the recovery reported when it cleared the record.
async fn last_recovery(rig: &Rig) -> serde_json::Value {
    serde_json::from_str(&meta(rig, "finality:last_recovery").await.unwrap()).unwrap()
}
/// A soft keeper that has served request 1: fulfillment signed at nonce 0, included, and settled at the decision head.
/// The block that holds it.
async fn served(rig: &Rig) -> u64 {
    ready(rig);
    rig.run(true, false).await;
    rig.chain(|chain| chain.include());
    let included = rig.chain(|chain| chain.head);
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("jobs [1=served]"), "{}", run.journal);
    included
}
/// The keeper recovers from a mismatch on record, and sends nothing new meanwhile.
const RECOVERING: &str = "healthy=false faults=[\"finality_mismatch\"] send_enabled=false";
/// The keeper holds its sends on a mismatch one endpoint showed and no second confirmed.
const HELD: &str = "healthy=false faults=[\"finality_unconfirmed\"] send_enabled=false";
const CLEAR: &str = "healthy=true faults=[] send_enabled=true";

#[tokio::test]
async fn a_replaced_decision_block_that_one_endpoint_shows_holds_the_sends_and_records_nothing_over_restarts()
 {
    let rig = soft(0).await;
    ready(&rig);
    // A keeper that does not send proves the request and journals the proof; the head it decided on is its checkpoint.
    let run = rig.run(false, false).await;
    assert!(run.journal.contains("jobs [1=prepared]"), "{}", run.journal);
    let decided = rig.chain(|chain| chain.head);
    let hash = hash_of(&rig, decided);
    // The sequencer replaces that block. Every keeper that starts from here, and each is a restart, finds it at its one
    // endpoint, which no second endpoint can confirm: it sends nothing, though a proven request waits and the keeper is
    // configured to send, and it records nothing.
    let replaced = rig.chain(|chain| {
        chain.replace_blocks(decided);
        chain.block_hash(decided).to_string()
    });
    for restart in 0..3 {
        let run = rig
            .run_with(true, false, |_| {})
            .await
            .expect("a hold is not a failed tick");
        assert!(rig.sent().is_empty(), "restart {restart}: {:#?}", run.tick);
        assert!(run.journal.contains("jobs [1=prepared]"), "{}", run.journal);
        assert!(run.journal.contains(HELD), "{}", run.journal);
        assert!(
            !run.tick
                .iter()
                .any(|line| line.contains("eth_sendRawTransaction")
                    || line.contains("eth_estimateGas")),
            "{:#?}",
            run.tick
        );
    }
    assert!(recorded(&rig).await.is_none());
    let noted = suspected(&rig).await.unwrap();
    let found = &noted.mismatch;
    assert_eq!(
        (
            found.kind.as_str(),
            found.number,
            found.reference.as_str(),
            found.expected.as_str(),
            found.actual.as_str()
        ),
        (
            "soft_checkpoint",
            decided,
            "",
            hash.as_str(),
            replaced.as_str()
        )
    );
    assert_eq!((noted.endpoints, noted.answered), (1, 1));
    // The checkpoint is the evidence: it still holds the old hash.
    assert_eq!(
        meta(&rig, "soft_checkpoint").await.as_deref(),
        Some(format!("[{decided},\"{hash}\"]").as_str())
    );
    let status = status(&rig).await;
    assert_eq!(status["state"], "suspected");
    assert_eq!(status["mismatch"]["id"], found.id());
    assert_eq!(status["mismatch"]["block"], decided);
}

#[tokio::test]
async fn a_mismatch_the_audit_finds_is_confirmed_by_two_endpoints_and_recovered_from_before_the_keeper_sends_again()
 {
    let rig = soft_pair(0).await;
    let included = served(&rig).await;
    let receipt = sent_hash(&rig, 0);
    let receipt_hash = hash_of(&rig, included);
    // A second request waits, proven by a keeper that does not send.
    let second = rig.request();
    rig.chain(|chain| chain.mine(3));
    let run = rig.run(false, false).await;
    assert!(
        run.journal.contains(&format!("{second}=prepared")),
        "{}",
        run.journal
    );
    // The sequencer replaces the block that holds the receipt, and L1 finalizes past it: the audit of the next tick is
    // what finds it, and both endpoints agree. The recovery starts in that tick: the receipt is in the replaced block
    // still, so the nonce is settled again from it, and nothing new is sent.
    let replaced = rig.chain(|chain| {
        chain.replace_blocks(included);
        chain.mine(LAG + 100);
        chain.block_hash(included).to_string()
    });
    let run = rig
        .run_with(true, false, |_| {})
        .await
        .expect("a recovery is not a failed tick");
    assert_eq!(rig.sent().len(), 1, "{:#?}", rig.sent());
    assert!(run.journal.contains(RECOVERING), "{}", run.journal);
    let found = recorded(&rig).await.unwrap();
    assert_eq!(
        (
            found.kind.as_str(),
            found.number,
            found.reference.as_str(),
            found.expected.as_str(),
            found.actual.as_str()
        ),
        (
            "receipt",
            included,
            receipt.as_str(),
            receipt_hash.as_str(),
            replaced.as_str()
        )
    );
    assert!(suspected(&rig).await.is_none());
    // The next process clears the record, and sends again: nothing, as the second request expired while L1 caught up.
    let run = rig.run(true, false).await;
    assert!(recorded(&rig).await.is_none());
    assert!(run.journal.contains(CLEAR), "{}", run.journal);
    assert!(
        run.journal.contains(&format!("1=served {second}=expired")),
        "{}",
        run.journal
    );
    assert_eq!(rig.sent().len(), 1, "{:#?}", rig.sent());
    let last = last_recovery(&rig).await;
    assert_eq!(last["id"], found.id());
    assert_eq!(last["done"]["reopened_nonces"], serde_json::json!([0]));
    assert_eq!(last["done"]["resent_nonces"], serde_json::json!([]));
}

#[tokio::test]
async fn the_process_never_exits_while_a_mismatch_is_held() {
    let rig = soft(0).await;
    served(&rig).await;
    let decided = rig.chain(|chain| chain.head);
    rig.chain(|chain| chain.replace_blocks(decided));
    // A long-lived keeper, as the binary runs it: more ticks than the run loop would tolerate failures. None fails.
    let process = rig.process(true, false).await;
    let limit = process_failure_limit(&rig);
    for tick in 0..limit * 4 {
        process
            .tick()
            .await
            .unwrap_or_else(|error| panic!("tick {tick}: {error:#}"));
        rig.chain(|chain| chain.mine(1));
    }
    assert!(process.worker().finality_incident_open().await);
    // If something did fail meanwhile, the loop would report it and go on while the mismatch is held; a failed tick that
    // is not an incident counts as ever, and a one-shot run ends in either case.
    assert!(!tick_failure_ends_the_run(false, true, u64::MAX, limit));
    assert!(tick_failure_ends_the_run(false, false, limit, limit));
    assert!(!tick_failure_ends_the_run(false, false, limit - 1, limit));
    assert!(tick_failure_ends_the_run(true, true, 1, limit));
    assert!(tick_failure_ends_the_run(true, false, 1, limit));
    process.stop().await;
    let journal = journal(&rig).await;
    assert!(journal.finality_mismatch().await.unwrap().is_none());
    assert!(journal.suspicion_note().await.unwrap().is_some());
    journal.pool.close().await;
}
fn process_failure_limit(rig: &Rig) -> u64 {
    rig.config(true).max_tick_failures
}

#[tokio::test]
async fn a_fulfillment_the_chain_lost_is_broadcast_again_from_the_bytes_the_journal_kept() {
    let rig = soft_pair(0).await;
    let included = served(&rig).await;
    let original = sent_hash(&rig, 0);
    assert_eq!(meta(&rig, "nonce_floor").await.as_deref(), Some("1"));
    // The sequencer rebuilds its history without the block that holds the fulfillment: no receipt, the request open
    // again, the wallet's nonce back at 0.
    rig.chain(|chain| chain.drop_blocks(included, false));
    assert!(!rig.chain(|chain| chain.requests[&1].fulfilled));

    // The next tick finds the block it decided on gone at both endpoints, records it, takes nonce 0 back into the lane
    // and broadcasts the very bytes the journal kept for it: the same transaction, at the same nonce. Nobody's step is
    // waited for. The floor stays where it was.
    let run = rig.run(true, false).await;
    assert_eq!(rig.sent().len(), 2, "{:#?}", run.tick);
    assert_eq!(sent_hash(&rig, 1), original);
    assert!(rig.sent()[1].ends_with("nonce=0"), "{:?}", rig.sent());
    assert!(
        run.journal.contains("jobs [1=submitted]"),
        "{}",
        run.journal
    );
    assert!(
        run.journal.contains("txs [1:fulfill@0=submitted]"),
        "{}",
        run.journal
    );
    assert!(run.journal.contains("nonce_floor=1"), "{}", run.journal);
    assert!(run.journal.contains(RECOVERING), "{}", run.journal);

    // The sequencer includes it; the tick settles the nonce again, and the one after clears the record.
    rig.chain(|chain| chain.include());
    let now_included = rig.chain(|chain| chain.head);
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("jobs [1=served]"), "{}", run.journal);
    assert!(
        run.journal.contains("txs [1:fulfill@0=resolved]"),
        "{}",
        run.journal
    );
    assert!(run.journal.contains(RECOVERING), "{}", run.journal);
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(CLEAR), "{}", run.journal);
    assert!(recorded(&rig).await.is_none());
    assert_eq!(rig.sent().len(), 2, "nothing else was signed");
    // What the recovery leaves: the receipt is marked at the block that holds it now, the head mark is this tick's, and
    // the incident is on record as the last recovery.
    let found = marks(&rig).await;
    assert!(
        found.contains(&crate::soft_finality::mark(
            "receipt",
            now_included,
            hash_of(&rig, now_included),
            &original
        )),
        "{found:#?}"
    );
    assert!(
        found
            .iter()
            .all(|m| m.0 != "receipt" || m.1 == now_included),
        "{found:#?}"
    );
    let last = last_recovery(&rig).await;
    assert_eq!(last["done"]["reopened_nonces"], serde_json::json!([0]));
    assert_eq!(last["done"]["resent_nonces"], serde_json::json!([0]));
    assert_eq!(last["mismatch"]["kind"], "soft_checkpoint");
    assert_eq!(last["acknowledged_at"], serde_json::Value::Null);
    assert_eq!(last["confirmed"]["agreeing"], 2);
    assert_eq!(status(&rig).await["state"], "clear");
    assert_eq!(meta(&rig, "nonce_floor").await.as_deref(), Some("1"));
}

/// A soft keeper with two endpoints that serves its requests one at a time (a batch of one), with `count` requests open
/// in the epoch.
async fn singles(count: usize) -> Rig {
    let rig = soft_pair(0).await.setting("FULFILL_BATCH_MAX", "1");
    rig.chain(|chain| {
        chain.publish_epoch(1, chain.head);
        chain.mine(1);
    });
    for _ in 0..count {
        rig.request();
    }
    rig.chain(|chain| chain.mine(3));
    rig
}
/// The nonces of the transactions the keeper has sent, in the order it sent them.
fn nonces(rig: &Rig) -> Vec<u64> {
    rig.chain(|chain| chain.sends.iter().map(|send| send.3).collect())
}
/// The hashes of the transactions the keeper has sent, in the order it sent them.
fn hashes(rig: &Rig) -> Vec<String> {
    rig.chain(|chain| chain.sends.iter().map(|send| send.0.to_string()).collect())
}

#[tokio::test]
async fn several_nonces_are_broadcast_again_in_nonce_order_one_at_a_time() {
    let rig = singles(3).await;
    // Request 1 is signed at nonce 0; the tick after it is included settles it and signs request 2 at nonce 1; and so on.
    let mut blocks = Vec::new();
    for _ in 0..3 {
        rig.run(true, false).await;
        rig.chain(|chain| chain.include());
        blocks.push(rig.chain(|chain| chain.head));
    }
    let run = rig.run(true, false).await;
    assert!(
        run.journal.contains("jobs [1=served 2=served 3=served]"),
        "{}",
        run.journal
    );
    assert_eq!(nonces(&rig), [0, 1, 2]);
    let originals = hashes(&rig);
    assert!(run.journal.contains("nonce_floor=3"), "{}", run.journal);

    // The sequencer loses all three blocks: the chain's nonce is back at 0 and none of the receipts is there. Nonce 0
    // first, and only nonce 0 until it is back on the chain. The other two are settled, as far as the journal knows,
    // until their turn comes.
    rig.chain(|chain| chain.drop_blocks(blocks[0], false));
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(RECOVERING), "{}", run.journal);
    assert_eq!(nonces(&rig), [0, 1, 2, 0], "{:#?}", run.tick);
    assert!(
        run.journal
            .contains("txs [1:fulfill@0=submitted 2:fulfill@1=resolved 3:fulfill@2=resolved]"),
        "{}",
        run.journal
    );
    rig.chain(|chain| chain.include());
    rig.run(true, false).await;
    // Nonce 1 next: the chain's nonce is 1 now.
    let run = rig.run(true, false).await;
    assert_eq!(nonces(&rig), [0, 1, 2, 0, 1], "{:#?}", run.tick);
    assert!(
        run.journal
            .contains("txs [1:fulfill@0=resolved 2:fulfill@1=submitted 3:fulfill@2=resolved]"),
        "{}",
        run.journal
    );
    rig.chain(|chain| chain.include());
    rig.run(true, false).await;
    // Then nonce 2.
    let run = rig.run(true, false).await;
    assert_eq!(nonces(&rig), [0, 1, 2, 0, 1, 2], "{:#?}", run.tick);
    assert!(run.journal.contains(RECOVERING), "{}", run.journal);
    rig.chain(|chain| chain.include());
    rig.run(true, false).await;
    // The tick after that finds the lane right and every mark the chain's, and clears the record.
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(CLEAR), "{}", run.journal);
    assert!(
        run.journal.contains("jobs [1=served 2=served 3=served]"),
        "{}",
        run.journal
    );
    // The bytes sent again are the bytes signed before, transaction for transaction; nothing new was signed. The floor
    // is where it was.
    assert_eq!(hashes(&rig)[3..], originals[..]);
    assert!(run.journal.contains("nonce_floor=3"), "{}", run.journal);
    assert!(recorded(&rig).await.is_none());
    assert_eq!(
        last_recovery(&rig).await["done"]["reopened_nonces"],
        serde_json::json!([0, 1, 2])
    );
}

#[tokio::test]
async fn a_receipt_that_moved_to_another_block_is_marked_again_and_nothing_is_sent_twice() {
    let rig = soft_pair(0).await;
    let included = served(&rig).await;
    let tx = sent_hash(&rig, 0);
    let before = hash_of(&rig, included);
    // The sequencer replaces the block and sequences the same transaction in it again.
    rig.chain(|chain| chain.replace_blocks(included));
    let after = hash_of(&rig, included);
    assert_ne!(before, after);
    // The nonce is settled again from the receipt the chain has, and the mark of the old block goes: a second row of the
    // same transaction would be a mismatch of the audit again. Nothing is broadcast.
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(RECOVERING), "{}", run.journal);
    assert_eq!(rig.sent().len(), 1, "{:#?}", run.tick);
    let found = marks(&rig).await;
    assert!(
        found.contains(&mark("receipt", included, &after, &tx)),
        "{found:#?}"
    );
    assert!(
        found.iter().all(|m| m.2 != before),
        "no mark of the old block is left: {found:#?}"
    );
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(CLEAR), "{}", run.journal);
    assert_eq!(rig.sent().len(), 1);
    assert!(run.journal.contains("jobs [1=served]"), "{}", run.journal);
    assert_eq!(
        last_recovery(&rig).await["done"]["resent_nonces"],
        serde_json::json!([])
    );
}

#[tokio::test]
async fn the_lane_that_is_open_waits_parked_while_an_older_nonce_is_refilled() {
    let rig = singles(2).await;
    rig.run(true, false).await;
    rig.chain(|chain| chain.include());
    let included = rig.chain(|chain| chain.head);
    // The tick that settles request 1 signs request 2 at nonce 1, which the sequencer has not included yet.
    let run = rig.run(true, false).await;
    assert!(
        run.journal.contains("jobs [1=served 2=submitted]"),
        "{}",
        run.journal
    );
    assert_eq!(nonces(&rig), [0, 1]);
    let originals = hashes(&rig);
    // The sequencer loses the block with request 1's fulfillment; the transaction of request 2 is still in its queue.
    // Nonce 0 is the lane; the transaction of request 2 is parked, whatever it was, so that the lane is one.
    rig.chain(|chain| chain.drop_blocks(included, false));
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(RECOVERING), "{}", run.journal);
    assert_eq!(nonces(&rig), [0, 1, 0], "{:#?}", run.tick);
    assert_eq!(hashes(&rig)[2], originals[0]);
    assert!(
        run.journal
            .contains("txs [1:fulfill@0=submitted 2:fulfill@1=parked_submitted]"),
        "{}",
        run.journal
    );
    assert!(
        run.journal.contains("jobs [1=submitted 2=submitted]"),
        "{}",
        run.journal
    );
    // The sequencer includes what it holds, in nonce order. Nonce 0 is settled; the parked one stays where it is until the
    // lane is right.
    rig.chain(|chain| chain.include());
    let run = rig.run(true, false).await;
    assert!(
        run.journal
            .contains("txs [1:fulfill@0=resolved 2:fulfill@1=parked_submitted]"),
        "{}",
        run.journal
    );
    // The tick after that returns it to the lane, clears the record, and reconciles it: the receipt is there.
    let run = rig.run(true, false).await;
    assert!(
        run.journal
            .contains("txs [1:fulfill@0=resolved 2:fulfill@1=resolved]"),
        "{}",
        run.journal
    );
    assert!(
        run.journal.contains("jobs [1=served 2=served]"),
        "{}",
        run.journal
    );
    assert!(run.journal.contains(CLEAR), "{}", run.journal);
    assert_eq!(
        rig.sent().len(),
        3,
        "the transaction of request 2 was never lost, so it was not sent again"
    );
    assert!(run.journal.contains("nonce_floor=2"), "{}", run.journal);
}

/// The proof saved for each job, by request id.
async fn proofs(rig: &Rig) -> std::collections::BTreeMap<u64, Option<String>> {
    let pool = rig.journal().await;
    let rows: Vec<(String, Option<String>)> = sqlx::query_as("SELECT id,proof FROM jobs")
        .fetch_all(&pool)
        .await
        .unwrap();
    pool.close().await;
    rows.into_iter()
        .map(|(id, proof)| (id.parse().unwrap(), proof))
        .collect()
}
/// The seed a saved proof is for.
fn seed_of(proof: &Option<String>) -> alloy_primitives::U256 {
    serde_json::from_str::<crate::abi::VrfProof>(proof.as_ref().expect("a proof"))
        .unwrap()
        .seed
}

#[tokio::test]
async fn a_job_whose_inputs_changed_loses_its_proof_and_the_others_keep_theirs() {
    let rig = soft_pair(0).await;
    ready(&rig);
    rig.request();
    rig.chain(|chain| chain.mine(3));
    let run = rig.run(false, false).await;
    assert!(
        run.journal.contains("jobs [1=prepared 2=prepared]"),
        "{}",
        run.journal
    );
    let before = proofs(&rig).await;
    let decided = rig.chain(|chain| chain.head);
    // The block that request 1's proof input binds is replaced: the chain has another seed for it now, and the same
    // seed for request 2. The block the keeper decided on is replaced too.
    rig.chain(|chain| {
        chain.reseed(1);
        chain.replace_blocks(decided);
    });
    // The recovery reads the jobs again: request 1's proof is for a seed the chain does not have, and goes; request 2's
    // is the chain's, and stays. With nothing else to put right the record is cleared in the same tick, which goes on
    // and proves request 1 again, for the seed the chain has.
    let run = rig.run(false, false).await;
    assert!(
        run.journal.contains("healthy=true faults=[]"),
        "{}",
        run.journal
    );
    let after = proofs(&rig).await;
    assert_eq!(after[&2], before[&2]);
    let seed = rig.chain(|chain| chain.proof_seed(1));
    assert_ne!(seed_of(&before[&1]), seed);
    assert_eq!(seed_of(&after[&1]), seed);
    assert!(
        run.journal.contains("jobs [1=prepared 2=prepared]"),
        "{}",
        run.journal
    );
    let done = last_recovery(&rig).await["done"].clone();
    assert_eq!(
        (done["jobs_reproved"].clone(), done["jobs_settled"].clone()),
        (1.into(), 0.into())
    );
}

#[tokio::test]
async fn jobs_the_chain_settled_or_no_longer_has_follow_the_chain_as_it_is_now() {
    let rig = soft_pair(0).await;
    ready(&rig);
    rig.run(false, false).await;
    // Another keeper serves request 1 in the block this one decides on next, and this keeper, which sends, sees the
    // request settled and sends nothing for it.
    let at = rig.chain(|chain| {
        chain.primary_serves(1);
        chain.head
    });
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("jobs [1=served]"), "{}", run.journal);
    assert!(rig.sent().is_empty());
    // Request 2 is made in the same block, and discovered and proved.
    rig.request();
    rig.chain(|chain| chain.mine(3));
    let run = rig.run(false, false).await;
    assert!(
        run.journal.contains("jobs [1=served 2=prepared]"),
        "{}",
        run.journal
    );
    // The sequencer loses that block and every one after it: request 1 is open again, and request 2 never was.
    rig.chain(|chain| chain.drop_blocks(at, false));
    let run = rig.run(false, false).await;
    assert!(
        run.journal.contains("jobs [1=prepared 2=expired]"),
        "{}",
        run.journal
    );
    let done = last_recovery(&rig).await["done"].clone();
    assert_eq!(
        (done["jobs_reopened"].clone(), done["jobs_settled"].clone()),
        (1.into(), 1.into())
    );
    // Request 1 is the keeper's to serve again, with the proof it has.
    let run = rig.run(true, false).await;
    assert!(
        run.journal.contains("jobs [1=submitted 2=expired]"),
        "{}",
        run.journal
    );
    assert_eq!(rig.sent().len(), 1);
}

#[tokio::test]
async fn a_request_the_chain_holds_under_an_id_discovery_had_passed_is_found() {
    let rig = soft_pair(0).await;
    ready(&rig);
    let at = rig.chain(|chain| chain.head);
    // Request 2 is served by another keeper before this one reads it: it is settled at discovery, which passes its id and
    // keeps no job for it.
    let second = rig.request();
    rig.chain(|chain| {
        chain.primary_serves(second);
        chain.mine(3);
    });
    let run = rig.run(false, false).await;
    assert!(run.journal.contains("jobs [1=prepared]"), "{}", run.journal);
    assert!(run.journal.contains("cursor=3"), "{}", run.journal);
    // The sequencer loses that block; the chain makes another request under the same id. Discovery goes back to it and
    // finds it, and the tick proves it.
    rig.chain(|chain| chain.drop_blocks(at, false));
    assert_eq!(rig.request(), second);
    rig.chain(|chain| chain.mine(3));
    let run = rig.run(false, false).await;
    assert!(
        run.journal.contains("jobs [1=prepared 2=prepared]"),
        "{}",
        run.journal
    );
    assert!(recorded(&rig).await.is_none());
}

/// A soft keeper with two endpoints that has seen another committer publish epoch 2, in which a request waits, and
/// marked its work committed; the block of the publication.
async fn published_by_another() -> (Rig, u64) {
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
    rig.chain(|chain| chain.mine(2));
    let run = rig.run(false, false).await;
    assert!(
        run.journal.contains("epochs [2=committed]"),
        "{}",
        run.journal
    );
    (rig, published)
}

#[tokio::test]
async fn a_committed_epoch_the_registry_no_longer_has_is_published_again_from_the_packet_the_journal_kept()
 {
    let (rig, published) = published_by_another().await;
    // The sequencer loses that block: the registry has no epoch 2. The recovery gives the work back with its packet,
    // and the keeper, which sends again from the moment the record is cleared, publishes the epoch in the same tick: a
    // request waits on it.
    rig.chain(|chain| chain.drop_blocks(published, false));
    let run = rig.run(true, false).await;
    assert_eq!(rig.sent().len(), 1, "{:#?}", run.tick);
    assert!(rig.sent()[0].contains("commitEpoch"), "{:?}", rig.sent());
    assert!(
        run.journal.contains("epochs [2=submitted]"),
        "{}",
        run.journal
    );
    assert_eq!(last_recovery(&rig).await["done"]["epochs_reopened"], 1);
    rig.chain(|chain| chain.include());
    let run = rig.run(true, false).await;
    assert!(
        run.journal.contains("epochs [2=committed]"),
        "{}",
        run.journal
    );
    assert!(run.journal.contains(CLEAR), "{}", run.journal);
}

#[tokio::test]
async fn a_committed_epoch_whose_packet_was_compacted_away_is_prepared_again() {
    let (rig, published) = published_by_another().await;
    // Compaction had blanked the packet of the committed work.
    crate::soft_finality::exec(
        &rig,
        "UPDATE epoch_work SET api=NULL,selection=NULL WHERE state='committed'",
    )
    .await;
    rig.chain(|chain| chain.drop_blocks(published, false));
    // The work is pending again; the tick prepares it (the drand round is fetched again), and the next publishes it.
    let run = rig.run(true, false).await;
    assert!(
        run.journal.contains("epochs [2=prepared]"),
        "{}",
        run.journal
    );
    assert!(rig.sent().is_empty(), "{:?}", rig.sent());
    assert_eq!(last_recovery(&rig).await["done"]["epochs_reopened"], 1);
    rig.run(true, false).await;
    assert!(
        rig.sent()
            .last()
            .is_some_and(|sent| sent.contains("commitEpoch")),
        "{:?}",
        rig.sent()
    );
}

#[tokio::test]
async fn an_epoch_commit_the_chain_lost_is_broadcast_again_beside_a_parked_fulfillment() {
    let rig = soft_pair(0).await;
    rig.chain(|chain| chain.mine_to(chain.epoch_start(2) + 2));
    rig.run(true, false).await;
    rig.request();
    rig.chain(|chain| chain.mine(1));
    rig.run(true, false).await;
    assert!(rig.sent()[0].contains("commitEpoch"), "{:?}", rig.sent());
    rig.chain(|chain| {
        chain.include();
        chain.mine(2);
    });
    let committed = rig.chain(|chain| chain.head - 2);
    // The tick settles the commit and signs the fulfillment of the request at nonce 1.
    let run = rig.run(true, false).await;
    assert!(
        run.journal.contains("epochs [2=committed]"),
        "{}",
        run.journal
    );
    assert_eq!(nonces(&rig), [0, 1]);
    let originals = hashes(&rig);
    // The sequencer loses the block with the commit; the fulfillment is still queued with it. The commit is the lane
    // again, with the epoch it served; the fulfillment waits behind it, parked.
    rig.chain(|chain| chain.drop_blocks(committed, false));
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(RECOVERING), "{}", run.journal);
    assert_eq!(nonces(&rig), [0, 1, 0], "{:#?}", run.tick);
    assert_eq!(hashes(&rig)[2], originals[0]);
    assert!(
        run.journal.contains("epochs [2=submitted]"),
        "{}",
        run.journal
    );
    assert!(
        run.journal.contains("@0=submitted") && run.journal.contains("@1=parked_submitted"),
        "{}",
        run.journal
    );
    // The sequencer includes both, in nonce order. The commit is settled, then the lane is returned and cleared, and the
    // fulfillment is settled as the lane that was open.
    rig.chain(|chain| chain.include());
    let run = rig.run(true, false).await;
    assert!(
        run.journal.contains("epochs [2=committed]"),
        "{}",
        run.journal
    );
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("jobs [1=served]"), "{}", run.journal);
    assert!(run.journal.contains(CLEAR), "{}", run.journal);
    assert_eq!(rig.sent().len(), 3);
}

#[tokio::test]
async fn a_batch_the_chain_lost_is_broadcast_again_and_its_members_follow_the_chain() {
    let rig = soft_pair(0).await;
    rig.chain(|chain| {
        chain.publish_epoch(1, chain.head);
        chain.mine(1);
    });
    for _ in 0..2 {
        rig.request();
        rig.chain(|chain| chain.mine(3));
        rig.run(false, false).await;
    }
    rig.run(true, false).await;
    assert!(
        rig.sent()[0].contains("fulfillRandomnessBatch"),
        "{:?}",
        rig.sent()
    );
    rig.chain(|chain| chain.include());
    let included = rig.chain(|chain| chain.head);
    let run = rig.run(true, false).await;
    assert!(
        run.journal.contains("jobs [1=served 2=served]"),
        "{}",
        run.journal
    );
    let batch = sent_hash(&rig, 0);
    // Both members go back to submitted with their batch, and the batch bytes are broadcast again.
    rig.chain(|chain| chain.drop_blocks(included, false));
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(RECOVERING), "{}", run.journal);
    assert_eq!(rig.sent().len(), 2, "{:#?}", run.tick);
    assert_eq!(sent_hash(&rig, 1), batch);
    assert!(
        run.journal.contains("jobs [1=submitted 2=submitted]"),
        "{}",
        run.journal
    );
    rig.chain(|chain| chain.include());
    rig.run(true, false).await;
    let run = rig.run(true, false).await;
    assert!(
        run.journal.contains("jobs [1=served 2=served]"),
        "{}",
        run.journal
    );
    assert!(run.journal.contains(CLEAR), "{}", run.journal);
    assert_eq!(rig.sent().len(), 2);
}

#[tokio::test]
async fn a_fulfillment_of_a_request_that_expired_meanwhile_is_cancelled_and_not_sent_again() {
    let rig = soft_pair(0).await;
    let included = served(&rig).await;
    // The sequencer loses the block while no keeper runs, and the request is past its deadline when one starts.
    rig.chain(|chain| {
        chain.drop_blocks(included, false);
        chain.mine(70);
    });
    // The nonce is reopened, but its transaction would only revert on a request that can no longer be served: the lane
    // is filled with a cancellation, which the recovery's own reconciliation signs and sends.
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(RECOVERING), "{}", run.journal);
    assert_eq!(rig.sent().len(), 2, "{:#?}", run.tick);
    assert!(rig.sent()[1].starts_with("cancel"), "{:?}", rig.sent());
    assert!(rig.sent()[1].ends_with("nonce=0"), "{:?}", rig.sent());
    assert_ne!(sent_hash(&rig, 1), sent_hash(&rig, 0));
    rig.chain(|chain| chain.include());
    rig.run(true, false).await;
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("jobs [1=expired]"), "{}", run.journal);
    assert!(run.journal.contains(CLEAR), "{}", run.journal);
    assert_eq!(rig.sent().len(), 2);
}

#[tokio::test]
async fn a_fulfillment_whose_proof_input_changed_is_not_sent_again_its_nonce_is_filled_and_the_request_proved_again()
 {
    let rig = soft_pair(0).await;
    let included = served(&rig).await;
    let original = sent_hash(&rig, 0);
    // The sequencer loses the block with the fulfillment, and the block its proof input binds: the request is open
    // again with another seed. The bytes the journal kept carry a proof that would only revert.
    rig.chain(|chain| {
        chain.drop_blocks(included, false);
        chain.reseed(1);
    });
    // The recovery fills nonce 0 with a zero-value transfer to the keeper's own wallet instead of broadcasting them.
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(RECOVERING), "{}", run.journal);
    assert_eq!(rig.sent().len(), 2, "{:#?}", run.tick);
    assert!(
        rig.sent()[1].starts_with("cancel") && rig.sent()[1].ends_with("nonce=0"),
        "{:?}",
        rig.sent()
    );
    assert_ne!(sent_hash(&rig, 1), original);
    assert_eq!(
        status(&rig).await["recovery"]["stale_nonces"],
        serde_json::json!([0])
    );
    // The fill is included and settles the nonce; the record is cleared at the tick after, which proves the request
    // again for the seed the chain has and sends it at the next nonce.
    rig.chain(|chain| chain.include());
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(RECOVERING), "{}", run.journal);
    let run = rig.run(true, false).await;
    assert!(
        run.journal.contains("jobs [1=submitted]"),
        "{}",
        run.journal
    );
    assert_eq!(rig.sent().len(), 3, "{:?}", rig.sent());
    assert!(
        rig.sent()[2].contains("fulfillRandomness") && rig.sent()[2].ends_with("nonce=1"),
        "{:?}",
        rig.sent()
    );
    assert!(
        !hashes(&rig)[1..].contains(&original),
        "the stale bytes were never broadcast again"
    );
    let seed = rig.chain(|chain| chain.proof_seed(1));
    assert_eq!(seed_of(&proofs(&rig).await[&1]), seed);
    let done = last_recovery(&rig).await["done"].clone();
    assert_eq!(
        (
            done["filled_nonces"].clone(),
            done["stale_nonces"].clone(),
            done["jobs_reproved"].clone()
        ),
        (serde_json::json!([0]), serde_json::json!([0]), 1.into())
    );
    rig.chain(|chain| chain.include());
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("jobs [1=served]"), "{}", run.journal);
    assert!(run.journal.contains("nonce_floor=2"), "{}", run.journal);
}

// Every path that signs or sends, while the keeper holds its sends. The sequencer replaces the block the last tick decided
// on, and the next tick would have signed or sent: it does not. The keeper has one endpoint, so no second one confirms
// the replacement; an operator's acknowledgement lets it recover.

/// The sequencer replaces the block the last tick decided on, and the blocks after it.
fn replace_decided(rig: &Rig, then_mine: u64) {
    rig.chain(|chain| {
        let decided = chain.head;
        chain.replace_blocks(decided);
        chain.mine(then_mine);
    });
}
/// Whether a tick asked the node to send a transaction.
fn broadcast(tick: &[String]) -> bool {
    tick.iter()
        .any(|line| line.contains("eth_sendRawTransaction"))
}

#[tokio::test]
async fn a_held_keeper_sends_no_fulfillment_and_no_batch_and_sends_again_once_acknowledged() {
    // One request.
    let rig = soft(0).await;
    ready(&rig);
    let run = rig.run(false, false).await;
    assert!(run.journal.contains("jobs [1=prepared]"), "{}", run.journal);
    replace_decided(&rig, 0);
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(HELD), "{}", run.journal);
    assert!(
        rig.sent().is_empty() && !broadcast(&run.tick),
        "{:#?}",
        run.tick
    );
    // The recovery clears the record, and the same tick sends: the proof it had is still the chain's.
    acknowledge(&rig).await;
    let run = rig.run(true, false).await;
    assert_eq!(rig.sent().len(), 1, "{:#?}", run.tick);
    assert!(
        rig.sent()[0].contains("fulfillRandomness"),
        "{:?}",
        rig.sent()
    );

    // Two requests, which the keeper would batch.
    let rig = soft(0).await;
    rig.chain(|chain| {
        chain.publish_epoch(1, chain.head);
        chain.mine(1);
    });
    for _ in 0..2 {
        rig.request();
        rig.chain(|chain| chain.mine(3));
        rig.run(false, false).await;
    }
    replace_decided(&rig, 0);
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(HELD), "{}", run.journal);
    assert!(
        rig.sent().is_empty() && !broadcast(&run.tick),
        "{:#?}",
        run.tick
    );
    acknowledge(&rig).await;
    rig.run(true, false).await;
    assert_eq!(rig.sent().len(), 1);
    assert!(
        rig.sent()[0].contains("fulfillRandomnessBatch"),
        "{:?}",
        rig.sent()
    );
}

#[tokio::test]
async fn a_held_keeper_publishes_no_epoch() {
    let rig = soft(0).await;
    rig.chain(|chain| chain.mine_to(chain.epoch_start(2) + 2));
    rig.run(true, false).await;
    rig.request();
    replace_decided(&rig, 1);
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(HELD), "{}", run.journal);
    assert!(
        rig.sent().is_empty() && !broadcast(&run.tick),
        "{:#?}",
        run.tick
    );
    // It goes on preparing the epoch, which is reading and a local packet; it is the publication that waits.
    let again = rig.run(true, false).await;
    assert!(rig.sent().is_empty() && !broadcast(&again.tick));
    assert!(
        again.journal.contains("epochs [2=prepared]"),
        "{}",
        again.journal
    );
    acknowledge(&rig).await;
    rig.run(true, false).await;
    assert!(rig.sent()[0].contains("commitEpoch"), "{:?}", rig.sent());
}

#[tokio::test]
async fn a_held_keeper_does_not_replace_rebroadcast_or_cancel_the_transaction_in_its_lane() {
    for (name, then_mine, drop_queue, send_marker) in [
        // Two seconds pass: the transaction would be broadcast again.
        ("rebroadcast", 3, false, "eth_sendRawTransaction"),
        // Ten seconds pass without it being included: it would be replaced by one that pays more.
        ("replacement", 12, true, "eth_sendRawTransaction"),
        // The request expires: the nonce would be cancelled.
        ("cancellation", 70, true, "eth_sendRawTransaction"),
    ] {
        let rig = soft(0).await;
        ready(&rig);
        rig.run(true, false).await;
        assert_eq!(rig.sent().len(), 1, "{name}");
        replace_decided(&rig, 0);
        rig.chain(|chain| {
            if drop_queue {
                chain.drop_queue();
            }
            chain.mine(then_mine);
        });
        let control = rig.run(true, false).await;
        assert!(
            control.journal.contains(HELD),
            "{name}: {}",
            control.journal
        );
        assert_eq!(rig.sent().len(), 1, "{name}: {:#?}", control.tick);
        assert!(
            !control.tick.iter().any(|line| line.contains(send_marker)),
            "{name}: {:#?}",
            control.tick
        );
        assert!(
            control.journal.contains("txs [1:fulfill@0=submitted]"),
            "{name}: {}",
            control.journal
        );
        // The lane is the same one after the recovery, and the reconciliation that was waiting goes on.
        acknowledge(&rig).await;
        let run = rig.run(true, false).await;
        assert!(rig.sent().len() >= 2, "{name}: {:?}", rig.sent());
        assert!(
            !run.journal.contains("faults=[\"finality_mismatch\"]")
                || run.journal.contains("txs [1:fulfill@0=submitted]"),
            "{name}: {}",
            run.journal
        );
    }
}

#[tokio::test]
async fn a_held_keeper_starts_no_sweep_and_replaces_none_in_flight() {
    // A sweep that is queued is not started.
    let rig = soft(0).await;
    rig.run(false, false).await;
    rig.queue_sweep("2000000000000000000").await;
    replace_decided(&rig, 1);
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(HELD), "{}", run.journal);
    assert!(
        run.journal.contains("sweep queued=true in_flight=false"),
        "{}",
        run.journal
    );
    assert!(
        rig.sent().is_empty() && !broadcast(&run.tick),
        "{:#?}",
        run.tick
    );
    acknowledge(&rig).await;
    let run = rig.run(true, false).await;
    assert_eq!(rig.sent().len(), 1, "{:#?}", run.tick);
    assert!(run.journal.contains("in_flight=true"), "{}", run.journal);

    // A sweep in flight that the sequencer never includes would be cancelled after its wait; it is not, and neither is
    // it broadcast again.
    rig.chain(|chain| chain.drop_queue());
    rig.age_sweep().await;
    replace_decided(&rig, 1);
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(HELD), "{}", run.journal);
    assert_eq!(rig.sent().len(), 1, "{:#?}", run.tick);
    assert!(!broadcast(&run.tick), "{:#?}", run.tick);
    assert!(run.journal.contains("in_flight=true"), "{}", run.journal);
}

#[tokio::test]
async fn a_held_keeper_goes_on_reading_and_settles_the_receipt_of_what_it_sent_before() {
    let rig = soft(0).await;
    ready(&rig);
    rig.run(true, false).await;
    assert_eq!(rig.sent().len(), 1);
    replace_decided(&rig, 0);
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(HELD), "{}", run.journal);
    assert!(
        run.journal.contains("txs [1:fulfill@0=submitted]"),
        "{}",
        run.journal
    );
    // The sequencer includes the transaction. A held keeper reads the receipt and settles the nonce from it, as it reads
    // everything else; it sends nothing.
    rig.chain(|chain| chain.include());
    let included = rig.chain(|chain| chain.head);
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(HELD), "{}", run.journal);
    assert!(run.journal.contains("jobs [1=served]"), "{}", run.journal);
    assert!(
        run.journal.contains("txs [1:fulfill@0=resolved]"),
        "{}",
        run.journal
    );
    assert!(run.journal.contains("nonce_floor=1"), "{}", run.journal);
    assert_eq!(rig.sent().len(), 1);
    assert!(marks(&rig).await.contains(&mark(
        "receipt",
        included,
        hash_of(&rig, included),
        sent_hash(&rig, 0)
    )));
    // After the acknowledgement there is little to put right: the signature mark is stale and goes, the receipt is the
    // chain's, and the record is cleared without a transaction.
    acknowledge(&rig).await;
    rig.run(true, false).await;
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(CLEAR), "{}", run.journal);
    assert_eq!(rig.sent().len(), 1);
}

#[tokio::test]
async fn an_endpoint_that_has_not_seen_a_replacement_yet_delays_the_recovery_and_does_not_stop_it()
{
    let rig = soft_pair(0).await;
    ready(&rig);
    rig.run(true, false).await;
    rig.chain(|chain| chain.include());
    let included = rig.chain(|chain| chain.head);
    rig.run(true, false).await;
    // The second endpoint does not see the sequencer's replacements for a while: it shows the journal's blocks.
    rig.endpoints()[1].set(Mode::Unreplaced(0));
    rig.chain(|chain| chain.drop_blocks(included, false));
    // The first endpoint shows the replacement, the second the journal's block: one against one is a tie, since the
    // journal's hash is no witness of its own. Nothing is recorded or sent, nobody is cooled, and the keeper asks again.
    let sent = rig.sent().len();
    for _ in 0..2 {
        rig.run(true, false).await;
        assert!(recorded(&rig).await.is_none());
        assert!(suspected(&rig).await.is_some());
        assert_eq!(rig.sent().len(), sent);
    }
    // It catches up: both show the replacement, and the keeper recovers from it, broadcasting again what the chain lost.
    rig.endpoints()[1].set(Mode::Up);
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(RECOVERING), "{}", run.journal);
    let original = sent_hash(&rig, 0);
    assert!(hashes(&rig)[1..].contains(&original), "{:?}", rig.sent());
    rig.chain(|chain| chain.include());
    rig.run(true, false).await;
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(CLEAR), "{}", run.journal);
    assert!(run.journal.contains("jobs [1=served]"), "{}", run.journal);
}

#[tokio::test]
async fn a_nonce_the_chain_has_used_and_the_journal_has_no_bytes_for_is_marked_as_consumed() {
    let rig = soft_pair(0).await;
    let included = served(&rig).await;
    // Compaction had blanked the signed bytes of the transaction. (It does so only for a receipt the audit made final;
    // this is the journal of a keeper that was restored from an older backup, or edited.)
    crate::soft_finality::exec(&rig, "UPDATE txs SET raw='',payload=''").await;
    rig.chain(|chain| chain.replace_blocks(included));
    // The chain has the nonce (the transaction is in the replaced block, as before), and there is nothing to broadcast
    // from: the receipt mark of the old block is replaced by a mark that the chain consumed the nonce at this head.
    let head = rig.chain(|chain| chain.head);
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(RECOVERING), "{}", run.journal);
    assert_eq!(rig.sent().len(), 1, "{:#?}", run.tick);
    let found = marks(&rig).await;
    assert!(
        found.contains(&mark("nonce", head, hash_of(&rig, head), "0")),
        "{found:#?}"
    );
    assert!(found.iter().all(|m| m.0 != "receipt"), "{found:#?}");
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(CLEAR), "{}", run.journal);
    assert_eq!(
        last_recovery(&rig).await["done"]["marks_replaced"],
        1,
        "{}",
        last_recovery(&rig).await
    );
}

#[tokio::test]
async fn a_nonce_the_chain_lost_and_the_journal_has_no_bytes_for_is_filled_with_a_transfer_to_the_wallet_itself()
 {
    let rig = soft_pair(0).await;
    let included = served(&rig).await;
    crate::soft_finality::exec(&rig, "UPDATE txs SET raw='',payload=''").await;
    rig.chain(|chain| chain.drop_blocks(included, false));
    // Nothing the journal kept can refill nonce 0: the recovery fills it with a zero-value transfer to the keeper's own
    // wallet, at once and by itself.
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(RECOVERING), "{}", run.journal);
    assert_eq!(rig.sent().len(), 2, "{:#?}", run.tick);
    assert!(
        rig.sent()[1].starts_with("cancel") && rig.sent()[1].ends_with("nonce=0"),
        "{:?}",
        rig.sent()
    );
    let status = status(&rig).await;
    assert_eq!(status["state"], "recovering");
    assert_eq!(status["recovery"]["filled_nonces"], serde_json::json!([0]));
    // The fill is included and settles the nonce, with the mark of its receipt.
    rig.chain(|chain| chain.include());
    let filled_in = rig.chain(|chain| chain.head);
    let run = rig.run(true, false).await;
    assert!(run.journal.contains(RECOVERING), "{}", run.journal);
    let fill = sent_hash(&rig, 1);
    assert!(
        marks(&rig)
            .await
            .contains(&mark("receipt", filled_in, hash_of(&rig, filled_in), &fill)),
        "{:#?}",
        marks(&rig).await
    );
    // The record is cleared, and request 1, which the chain lost with its block, is served again at the next nonce.
    let run = rig.run(true, false).await;
    assert!(recorded(&rig).await.is_none());
    assert!(
        run.journal.contains("jobs [1=submitted]"),
        "{}",
        run.journal
    );
    assert!(
        rig.sent()[2].contains("fulfillRandomness") && rig.sent()[2].ends_with("nonce=1"),
        "{:?}",
        rig.sent()
    );
    let done = last_recovery(&rig).await["done"].clone();
    assert_eq!(
        (done["filled_nonces"].clone(), done["stale_nonces"].clone()),
        (serde_json::json!([0]), serde_json::json!([]))
    );
    assert_eq!(meta(&rig, "nonce_floor").await.as_deref(), Some("1"));
}

#[tokio::test]
async fn a_long_list_of_jobs_is_read_again_a_page_at_a_time_and_a_restart_resumes_it() {
    let rig = soft_pair(0).await;
    ready(&rig);
    rig.run(false, false).await;
    // 150 jobs the keeper had settled, of requests the chain does not have: all in the window, and each of them
    // expired as the chain tells it now.
    let pool = rig.journal().await;
    let deadline = rig.chain(|chain| chain.time(chain.head)) + 30;
    for id in 1_000..1_150 {
        sqlx::query("INSERT INTO jobs(id,deadline,state) VALUES(?,?,'served')")
            .bind(id.to_string())
            .bind(i64::try_from(deadline).unwrap())
            .execute(&pool)
            .await
            .unwrap();
    }
    pool.close().await;
    let decided = rig.chain(|chain| chain.head);
    rig.chain(|chain| chain.replace_blocks(decided));

    // A page of 64 at each tick, the first in the tick that confirms the mismatch, each tick a new process; the record
    // stays until the last page is read.
    let settled = |journal: &str| journal.matches("=expired").count();
    let run = rig.run(false, false).await;
    assert_eq!(settled(&run.journal), 64, "{}", run.journal);
    assert!(run.journal.contains(RECOVERING), "{}", run.journal);
    let run = rig.run(false, false).await;
    assert_eq!(settled(&run.journal), 128, "{}", run.journal);
    assert!(run.journal.contains(RECOVERING), "{}", run.journal);
    let run = rig.run(false, false).await;
    assert_eq!(settled(&run.journal), 150, "{}", run.journal);
    assert!(
        run.journal.contains("healthy=true faults=[]"),
        "{}",
        run.journal
    );
    assert_eq!(
        last_recovery(&rig).await["done"]["jobs_settled"],
        150,
        "{}",
        last_recovery(&rig).await
    );
}

#[tokio::test]
async fn the_marks_are_walked_a_page_at_a_time_and_the_stale_ones_of_signatures_go() {
    let rig = soft_pair(0).await;
    // L1 is far behind: the audit has nothing at or below the finalized head to check, so the recovery is what reads
    // these marks.
    rig.chain(|chain| chain.finalized_lag = 600);
    rig.run(false, false).await;
    // 300 signature marks of blocks the keeper signed on, at 600 to 899.
    let pool = rig.journal().await;
    for block in 600..900u64 {
        sqlx::query("INSERT INTO soft_marks(number,hash,kind,ref,created) VALUES(?,?,'sign',?,?)")
            .bind(i64::try_from(block).unwrap())
            .bind(hash_of(&rig, block))
            .bind(format!("0xtx{block}"))
            .bind(i64::try_from(crate::health::now().unwrap()).unwrap())
            .execute(&pool)
            .await
            .unwrap();
    }
    pool.close().await;
    let decided = rig.chain(|chain| chain.head);
    // The sequencer replaces the chain from block 750 on, and the head block with it. The tick that confirms it reads
    // four pages of 64 blocks: 600 to 855, of which 750 to 855 are stale and deleted.
    rig.chain(|chain| chain.replace_blocks(750));
    rig.run(false, false).await;
    assert!(recorded(&rig).await.is_some());
    let found = marks(&rig).await;
    let signs: Vec<u64> = found
        .iter()
        .filter(|m| m.0 == "sign")
        .map(|m| m.1)
        .collect();
    assert_eq!(
        signs.len(),
        300 - 106,
        "{} marks, from {:?} to {:?}",
        found.len(),
        signs.first(),
        signs.last()
    );
    // The second tick reads the rest and the jobs, and clears the record.
    let run = rig.run(false, false).await;
    assert!(
        run.journal.contains("healthy=true faults=[]"),
        "{}",
        run.journal
    );
    let left: Vec<u64> = marks(&rig)
        .await
        .into_iter()
        .filter(|m| m.0 == "sign")
        .map(|m| m.1)
        .collect();
    assert_eq!(left, (600..750).collect::<Vec<u64>>());
    assert!(decided > 900);
    let done = last_recovery(&rig).await["done"].clone();
    assert_eq!(done["marks_dropped"], 150, "{done}");
    // The marks below the fork point are the chain's still, and the audit will check them.
    assert_eq!(hash_of(&rig, 749), {
        let found = marks(&rig).await;
        found.iter().find(|m| m.1 == 749).unwrap().2.clone()
    });
}
