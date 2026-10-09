//! Soft finality (`FINALITY_MODE=soft`) against the scripted chain, on which `finalized` trails the latest block by as
//! many blocks as the script sets. The keeper decides on the sequencer's latest block less the soft depth, resolves a
//! nonce from a receipt whose block is the chain's at that head, reads no state at the `finalized` tag, and holds the
//! chain to the block it decided on. It writes down every block it acts on, in the transaction of the action, and audits
//! those marks against the finalized header (the one thing it reads at that tag). What finalized mode asks the chain is
//! pinned by the golden traces; this module is what only soft mode does.
use crate::{
    config::{ChainSettings, FinalityMode},
    finality,
    journal::{Audit, Journal, Mismatch},
    proxy::RuntimePins,
    rig::Rig,
    rpc::Rpc,
    scripted::{Mode, render},
    worker::CheckpointChanged,
};
use std::time::{Duration, Instant};

pub(crate) const ROBINHOOD_TESTNET: u64 = 46_630;
/// How far `finalized` trails the latest block on the chains of these tests: farther than any of them mines, so that a
/// keeper that read the finalized head would see a chain from before its requests.
pub(crate) const LAG: u64 = 200;

pub(crate) fn settings(mode: FinalityMode, depth: u64) -> ChainSettings {
    ChainSettings {
        finality_mode: mode,
        soft_depth_blocks: depth,
        ..ChainSettings::default()
    }
}
/// A soft keeper's rig on a chain whose finalized block is `LAG` blocks behind.
pub(crate) async fn soft(depth: u64) -> Rig {
    let rig = Rig::new(ROBINHOOD_TESTNET, settings(FinalityMode::Soft, depth))
        .await
        .unheld();
    rig.chain(|chain| {
        chain.finalized_lag = LAG;
        chain.safe_lag = LAG - 10;
    });
    rig
}
/// A soft keeper's rig with a second endpoint of the same chain: what it takes for two endpoints to agree that a block
/// was replaced, and so for a mismatch to be recorded and recovered from.
pub(crate) async fn soft_pair(depth: u64) -> Rig {
    let mut rig = soft(depth).await;
    rig.add_endpoint().await;
    rig
}
/// The note of the mismatch a soft keeper suspects, if there is one.
pub(crate) async fn suspected(rig: &Rig) -> Option<crate::journal::Suspected> {
    meta(rig, "finality:suspected")
        .await
        .map(|saved| serde_json::from_str(&saved).unwrap())
}
/// The block tag of a read, as the trace prints it: the word after the method of a call that has a block.
fn tag(line: &str) -> Option<&str> {
    let mut words = line.split_whitespace();
    words.next().filter(|method| method.starts_with("eth_"))?;
    words.next()
}
/// The reads of a trace that were made at the `finalized` tag, except the header of the finalized block: the finality
/// audit reads it (no state) to learn which of the blocks the keeper acted on are final, and nothing else of the keeper
/// does.
fn finalized_reads(lines: &[String]) -> Vec<&str> {
    lines
        .iter()
        .filter(|line| tag(line) == Some("finalized") && !is_finalized_header(line))
        .map(String::as_str)
        .collect()
}
fn is_finalized_header(line: &str) -> bool {
    line.trim_start() == "eth_getBlockByNumber finalized full=false"
}
fn assert_no_finalized_reads(what: &str, lines: &[String]) {
    assert!(
        finalized_reads(lines).is_empty(),
        "{what}: a soft keeper read finalized state: {:#?}",
        finalized_reads(lines)
    );
}
pub(crate) async fn meta(rig: &Rig, key: &str) -> Option<String> {
    let pool = rig.journal().await;
    let value = sqlx::query_scalar("SELECT value FROM meta WHERE key=?")
        .bind(key)
        .fetch_optional(&pool)
        .await
        .unwrap();
    pool.close().await;
    value
}
/// The checkpoint under `key`: the block number and hash the keeper saved.
pub(crate) async fn checkpoint(rig: &Rig, key: &str) -> Option<(u64, String)> {
    meta(rig, key)
        .await
        .map(|saved| serde_json::from_str(&saved).unwrap())
}
pub(crate) async fn finalized_receipts(rig: &Rig) -> i64 {
    let pool = rig.journal().await;
    let count = sqlx::query_scalar("SELECT COUNT(*) FROM finalized_receipts")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    count
}
/// A published epoch and a request, in the block the epoch was published in or after it, with the blocks mined that a
/// keeper that decides on the latest block needs to prove it.
pub(crate) fn ready(rig: &Rig) -> u64 {
    rig.chain(|chain| {
        chain.publish_epoch(1, chain.head);
        chain.mine(1);
    });
    let id = rig.request();
    rig.chain(|chain| chain.mine(3));
    id
}

#[tokio::test]
async fn a_soft_keeper_serves_a_request_while_finalized_is_far_behind() {
    let rig = soft(0).await;
    let id = ready(&rig);
    let run = rig.run(true, false).await;
    assert_eq!(rig.sent().len(), 1, "{:#?}", run.tick);
    assert!(
        run.journal.contains("jobs [1=submitted]"),
        "{}",
        run.journal
    );
    assert_no_finalized_reads("the tick that sends", &run.tick);
    // The sequencer includes the fulfillment in the next block, and the same block is final enough at once.
    rig.chain(|chain| chain.include());
    let run = rig.run(true, false).await;
    assert!(rig.chain(|chain| chain.requests[&id].fulfilled));
    assert!(run.journal.contains("jobs [1=served]"), "{}", run.journal);
    assert!(
        run.journal.contains("txs [1:fulfill@0=resolved]"),
        "{}",
        run.journal
    );
    assert_no_finalized_reads("the tick that settles", &run.tick);
}

#[tokio::test]
async fn a_soft_keeper_reads_nothing_at_the_finalized_tag_from_startup_on() {
    let rig = soft(0).await;
    let startup = rig.start().await;
    assert_no_finalized_reads("startup", &startup);
    // The registry is found through the coordinator at the latest block, and the startup ends on the head it decides on.
    assert!(
        startup
            .iter()
            .any(|line| line.starts_with("eth_call latest coordinator 0x2b12cb69 epochRegistry")),
        "{startup:#?}"
    );
    assert_eq!(
        startup.last().unwrap(),
        "eth_getBlockByNumber latest full=false"
    );
    // The same chain read by a finalized keeper does read it: nothing above is the chain's doing.
    let finalized = Rig::new(ROBINHOOD_TESTNET, settings(FinalityMode::Finalized, 0))
        .await
        .unheld();
    finalized.chain(|chain| chain.finalized_lag = LAG);
    assert!(!finalized_reads(&finalized.start().await).is_empty());
}

#[tokio::test]
async fn the_identity_pins_find_the_registry_at_the_view_tag_of_the_mode() {
    for (mode, tag) in [
        (FinalityMode::Soft, "latest"),
        (FinalityMode::Finalized, "finalized"),
    ] {
        let rig = Rig::new(ROBINHOOD_TESTNET, settings(mode, 0))
            .await
            .unheld();
        let rpc = Rpc::new(vec![rig.node.url.clone()])
            .unwrap()
            .with_finality(mode, 0);
        rig.node.take();
        RuntimePins::observe(&rpc, &rig.config(false))
            .await
            .unwrap();
        let trace = render(&rig.node.take());
        let registry = format!("eth_call {tag} coordinator 0x2b12cb69 epochRegistry");
        assert!(
            trace.iter().any(|line| line.trim_start() == registry),
            "{mode:?}: {trace:#?}"
        );
        assert_eq!(
            finalized_reads(&trace).len(),
            usize::from(tag == "finalized")
        );
    }
}

#[tokio::test]
async fn a_soft_keeper_does_not_depend_on_how_far_finalized_trails() {
    let mut outcomes = Vec::new();
    for lag in [0, 3, LAG] {
        let rig = soft(0).await;
        rig.chain(|chain| chain.finalized_lag = lag);
        ready(&rig);
        let first = rig.run(true, false).await;
        rig.chain(|chain| chain.include());
        let second = rig.run(true, false).await;
        outcomes.push((rig.sent(), first.journal, second.journal));
    }
    assert_eq!(outcomes[0], outcomes[1]);
    assert_eq!(outcomes[0], outcomes[2]);
    assert!(
        outcomes[0].2.contains("jobs [1=served]"),
        "{}",
        outcomes[0].2
    );
}

#[tokio::test]
async fn decisions_are_taken_at_the_latest_block_less_the_soft_depth() {
    let rig = soft(3).await;
    ready(&rig);
    // The request is in block r and the latest block is r + 3: the decision head is r, and a request whose target is r
    // is not confirmed at r. The keeper has found the request, and proves nothing.
    let latest = rig.chain(|chain| chain.head);
    let run = rig.run(true, false).await;
    assert!(rig.sent().is_empty(), "{:#?}", run.tick);
    assert!(run.journal.contains("jobs [1=pending]"), "{}", run.journal);
    // What the tick decided on is the block three below the latest one, and it is what the next tick checks.
    let hash = rig.chain(|chain| chain.block_hash(latest - 3).to_string());
    assert_eq!(
        checkpoint(&rig, "soft_checkpoint").await,
        Some((latest - 3, hash))
    );
    // One block later the target is confirmed at the decision head, and the request is proved and sent.
    rig.chain(|chain| chain.mine(1));
    let run = rig.run(true, false).await;
    assert_eq!(rig.sent().len(), 1, "{:#?}", run.tick);
    assert!(
        run.journal.contains("jobs [1=submitted]"),
        "{}",
        run.journal
    );
    assert_no_finalized_reads("the tick that sends", &run.tick);
    // The same request at depth 0 is proved and sent at once.
    let immediate = soft(0).await;
    ready(&immediate);
    immediate.run(true, false).await;
    assert_eq!(immediate.sent().len(), 1);
    // The blocks a tick reads state at are the decision head's, by number: the latest block is never a tag it reads
    // contract state at, only the head it takes its depth from.
    let head = rig.chain(|chain| chain.head);
    let run = rig.run(false, false).await;
    assert!(
        run.tick
            .iter()
            .any(|line| line.starts_with("eth_call #latest-3/") && line.contains("nextRequestId")),
        "latest is {head}: {:#?}",
        run.tick
    );
}

#[tokio::test]
async fn a_receipt_settles_a_nonce_only_once_its_block_is_at_the_decision_head() {
    let rig = soft(2).await;
    ready(&rig);
    rig.chain(|chain| chain.mine(1));
    let run = rig.run(true, false).await;
    assert_eq!(rig.sent().len(), 1, "{:#?}", run.tick);
    rig.chain(|chain| chain.include());
    // The fulfillment is in the latest block, two above the decision head. The receipt is there and the nonce is used,
    // and the request waits: the lane stays busy.
    for _ in 0..2 {
        let run = rig.run(true, false).await;
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
        assert_no_finalized_reads("an unsettled receipt", &run.tick);
        rig.chain(|chain| chain.mine(1));
    }
    // The decision head has reached the receipt's block.
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("jobs [1=served]"), "{}", run.journal);
    assert!(
        run.journal.contains("txs [1:fulfill@0=resolved]"),
        "{}",
        run.journal
    );
}

#[tokio::test]
async fn a_consumed_nonce_is_read_at_the_decision_head_when_no_endpoint_serves_the_receipt() {
    let rig = soft(0).await;
    ready(&rig);
    rig.run(true, false).await;
    rig.chain(|chain| {
        chain.include();
        chain.hide_receipts = true;
    });
    let run = rig.run(true, false).await;
    // The nonce is used at the block the tick decided on, the request is fulfilled there, and that is enough.
    assert!(run.journal.contains("jobs [1=served]"), "{}", run.journal);
    assert!(
        run.tick.iter().any(|line| {
            line.starts_with("eth_getTransactionCount #latest/") && line.ends_with("keeper")
        }),
        "{:#?}",
        run.tick
    );
    assert_no_finalized_reads("a nonce without a receipt", &run.tick);
}

#[tokio::test]
async fn a_batch_is_sent_and_settled_at_the_decision_head() {
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
    let run = rig.run(true, false).await;
    assert!(
        rig.sent()[0].contains("fulfillRandomnessBatch"),
        "{:?}",
        rig.sent()
    );
    assert_no_finalized_reads("the batch is sent", &run.tick);
    rig.chain(|chain| chain.include());
    let run = rig.run(true, false).await;
    assert!(
        run.journal.contains("jobs [1=served 2=served]"),
        "{}",
        run.journal
    );
    assert_no_finalized_reads("the batch settles", &run.tick);
}

#[tokio::test]
async fn an_epoch_commit_and_a_sweep_settle_at_the_decision_head() {
    let rig = soft(0).await;
    rig.chain(|chain| chain.mine_to(chain.epoch_start(2) + 2));
    rig.run(true, false).await;
    let id = rig.request();
    rig.chain(|chain| chain.mine(1));
    let run = rig.run(true, false).await;
    assert!(
        rig.sent().last().unwrap().contains("commitEpoch"),
        "{:?}",
        rig.sent()
    );
    assert_no_finalized_reads("the commit is sent", &run.tick);
    // The commit is in the latest block: its receipt settles, the epoch is the registry's, and the request is served.
    rig.chain(|chain| {
        chain.include();
        chain.mine(2);
    });
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("2=committed"), "{}", run.journal);
    assert_no_finalized_reads("the commit settles", &run.tick);
    rig.chain(|chain| chain.include());
    let run = rig.run(true, false).await;
    assert!(rig.chain(|chain| chain.requests[&id].fulfilled));
    assert_no_finalized_reads("the request settles", &run.tick);

    rig.queue_sweep("2000000000000000000").await;
    let run = rig.run(true, false).await;
    assert!(
        run.journal.contains("in_flight=true"),
        "the sweep is signed and sent: {}",
        run.journal
    );
    rig.chain(|chain| chain.include());
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("last=sent"), "{}", run.journal);
    assert_no_finalized_reads("the sweep settles", &run.tick);
}

#[tokio::test]
async fn finalized_receipts_and_the_finalized_checkpoint_stay_the_auditors() {
    // The same request on a chain that finalizes three blocks behind, in each mode.
    let finalized = Rig::new(ROBINHOOD_TESTNET, settings(FinalityMode::Finalized, 0))
        .await
        .unheld();
    let soft = soft(0).await;
    for rig in [&finalized, &soft] {
        rig.chain(|chain| {
            chain.finalized_lag = 3;
            chain.safe_lag = 2;
        });
        ready(rig);
        rig.chain(|chain| chain.mine(3));
        rig.run(true, false).await;
        rig.settle();
        let run = rig.run(true, false).await;
        assert!(run.journal.contains("jobs [1=served]"), "{}", run.journal);
    }
    // Finalized mode writes what it always wrote, and no soft checkpoint.
    assert!(
        checkpoint(&finalized, "finalized_checkpoint")
            .await
            .is_some()
    );
    assert_eq!(finalized_receipts(&finalized).await, 1);
    assert!(meta(&finalized, "soft_checkpoint").await.is_none());
    // Soft mode writes the soft checkpoint and leaves the finalized records to the audit: a block that is decided on
    // is not finalized, and only the audit may say so. The audit of the second tick found the first tick's marks below
    // the finalized head and checked them, which moved the finalized checkpoint to that block; the receipt of the
    // second tick is above it, so it is no finalized receipt yet.
    let (number, hash) = checkpoint(&soft, "soft_checkpoint").await.unwrap();
    assert_eq!(number, soft.chain(|chain| chain.head));
    assert_eq!(
        hash,
        soft.chain(|chain| chain.block_hash(number).to_string())
    );
    let (audited, audited_hash) = checkpoint(&soft, "finalized_checkpoint").await.unwrap();
    assert!(audited <= soft.chain(|chain| chain.finalized_head()));
    assert!(audited < number);
    assert_eq!(
        audited_hash,
        soft.chain(|chain| chain.block_hash(audited).to_string())
    );
    assert_eq!(finalized_receipts(&soft).await, 0);
}

#[tokio::test]
async fn the_soft_checkpoint_detects_a_replaced_block_and_ignores_blocks_nothing_was_decided_on() {
    let rig = soft(0).await;
    rig.run(false, false).await;
    let (first, first_hash) = checkpoint(&rig, "soft_checkpoint").await.unwrap();
    assert_eq!(first, rig.chain(|chain| chain.head));
    // The chain moves on and the next tick moves the checkpoint with it.
    rig.chain(|chain| chain.mine(2));
    rig.run(false, false).await;
    let (second, second_hash) = checkpoint(&rig, "soft_checkpoint").await.unwrap();
    assert_eq!(second, first + 2);
    assert_ne!(second_hash, first_hash);

    // The sequencer replaces the blocks above the one the keeper decided on: nothing was acted on in them.
    rig.chain(|chain| {
        chain.mine(3);
        chain.replace_blocks(second + 1);
    });
    rig.run(false, false).await;
    let (third, third_hash) = checkpoint(&rig, "soft_checkpoint").await.unwrap();
    assert_eq!(third, second + 3);

    // It replaces the block the keeper decided on: the keeper suspects a finality mismatch of kind `soft_checkpoint` with
    // the block and both hashes, and holds its sends. Its one endpoint cannot be confirmed by a second, so nothing is
    // recorded: the suspicion is noted for the operator. The tick does not fail (the process would exit after its
    // failed ticks and restart into the same journal), and nothing is signed, sent or moved.
    let replaced = rig.chain(|chain| {
        chain.replace_blocks(third);
        chain.block_hash(third).to_string()
    });
    assert_ne!(replaced, third_hash);
    let run = rig
        .run_with(true, false, |_| {})
        .await
        .expect("a replaced block holds the sends and does not fail the tick");
    assert!(
        run.journal
            .contains("healthy=false faults=[\"finality_unconfirmed\"] send_enabled=false"),
        "{}",
        run.journal
    );
    assert!(meta(&rig, "finality:mismatch").await.is_none());
    let recorded = suspected(&rig).await.unwrap().mismatch;
    assert_eq!(
        (
            recorded.kind.as_str(),
            recorded.number,
            recorded.reference.as_str(),
            recorded.expected.as_str(),
            recorded.actual.as_str()
        ),
        (
            "soft_checkpoint",
            third,
            "",
            third_hash.as_str(),
            replaced.as_str()
        )
    );
    assert_eq!(
        checkpoint(&rig, "soft_checkpoint").await,
        Some((third, third_hash))
    );
    assert!(rig.sent().is_empty());
    assert!(meta(&rig, "finalized_checkpoint").await.is_none());
}

#[test]
fn a_changed_checkpoint_says_which_mode_and_which_block() {
    let changed = |mode| CheckpointChanged {
        mode,
        number: 7,
        saved: "0xaa".into(),
        actual: "0xbb".into(),
    };
    // The finalized message is the one 0.4.1 gave.
    assert_eq!(
        changed(FinalityMode::Finalized).to_string(),
        "Finalized chain checkpoint changed; preserve journal and investigate RPC/finality before resuming"
    );
    assert_eq!(
        changed(FinalityMode::Soft).to_string(),
        "Soft chain checkpoint changed at block 7: the journal has 0xaa and the chain has 0xbb; the sequencer replaced a block this keeper decided on, or this endpoint serves another fork. Nothing is sent while the other endpoints are asked"
    );
}

// The marks a soft keeper writes with its actions, and the audit that checks them against L1 finality (keeper task C3).

/// The marks of the journal: kind, block, hash and reference, lowest block first.
pub(crate) type Marks = Vec<(String, u64, String, String)>;
pub(crate) async fn marks(rig: &Rig) -> Marks {
    let pool = rig.journal().await;
    let rows: Vec<(String, i64, String, String)> =
        sqlx::query_as("SELECT kind,number,hash,ref FROM soft_marks ORDER BY number,kind,ref")
            .fetch_all(&pool)
            .await
            .unwrap();
    pool.close().await;
    rows.into_iter()
        .map(|(kind, number, hash, reference)| {
            (kind, u64::try_from(number).unwrap(), hash, reference)
        })
        .collect()
}
pub(crate) fn mark(
    kind: &str,
    number: u64,
    hash: impl ToString,
    reference: impl ToString,
) -> (String, u64, String, String) {
    (kind.into(), number, hash.to_string(), reference.to_string())
}
/// The marks of `kind` that name `reference`.
pub(crate) fn marked(marks: &Marks, kind: &str, reference: &str) -> usize {
    marks
        .iter()
        .filter(|m| m.0 == kind && m.3 == reference)
        .count()
}
/// The hash the scripted chain has for a block now.
pub(crate) fn hash_of(rig: &Rig, number: u64) -> String {
    rig.chain(|chain| chain.block_hash(number).to_string())
}
/// The hash of the `index`th transaction the keeper sent.
pub(crate) fn sent_hash(rig: &Rig, index: usize) -> String {
    rig.chain(|chain| chain.sends[index].0.to_string())
}
/// Run `statement` on the journal.
pub(crate) async fn exec(rig: &Rig, statement: &'static str) {
    let pool = rig.journal().await;
    sqlx::query(statement).execute(&pool).await.unwrap();
    pool.close().await;
}
/// The numbers of the blocks the audit read at the start of a tick, if it read the finalized header first: the block
/// calls between that header and the read of the decision head, which names the block of each as its distance from
/// `head`, the latest block of the chain then.
fn audit_reads(lines: &[String], head: u64) -> Option<Vec<u64>> {
    if !lines.first().is_some_and(|line| is_finalized_header(line)) {
        return None;
    }
    Some(
        lines[1..]
            .iter()
            .map(|line| line.trim_start())
            .take_while(|line| !line.starts_with("eth_getBlockByNumber latest"))
            .filter_map(|line| line.strip_prefix("eth_getBlockByNumber #latest"))
            .map(|rest| {
                let distance = rest.split('/').next().unwrap();
                match distance.chars().next() {
                    None => head,
                    Some('-') => head - distance[1..].parse::<u64>().unwrap(),
                    _ => panic!("a block above the latest one: {rest}"),
                }
            })
            .collect(),
    )
}
/// Mine the blocks that put everything a soft keeper has done so far below the finalized head.
pub(crate) fn finalize_everything(rig: &Rig) {
    rig.chain(|chain| chain.mine(LAG + 100));
}

#[tokio::test]
async fn a_soft_keeper_writes_down_each_block_it_acts_on_with_the_action_that_needs_it() {
    let rig = soft(0).await;
    ready(&rig);
    // The tick that signs: the decision head it signed at is the transaction's sign mark, and that block is the tick's
    // head mark. Nothing is settled, so nothing else.
    let signed_at = rig.chain(|chain| chain.head);
    rig.run(true, false).await;
    let (tx, signed_hash) = (sent_hash(&rig, 0), hash_of(&rig, signed_at));
    assert_eq!(
        marks(&rig).await,
        [
            mark("head", signed_at, &signed_hash, ""),
            mark("sign", signed_at, &signed_hash, &tx)
        ]
    );
    assert_eq!(meta(&rig, "nonce_floor").await.as_deref(), Some("0"));

    // The tick that settles: the nonce floor moves with the receipt mark, at the block the receipt names, and the head
    // mark moves up to the block this tick decided on.
    rig.chain(|chain| chain.include());
    let included = rig.chain(|chain| chain.head);
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("jobs [1=served]"), "{}", run.journal);
    let included_hash = hash_of(&rig, included);
    assert_eq!(
        marks(&rig).await,
        [
            mark("sign", signed_at, &signed_hash, &tx),
            mark("head", included, &included_hash, ""),
            mark("receipt", included, &included_hash, &tx)
        ]
    );
    assert_eq!(meta(&rig, "nonce_floor").await.as_deref(), Some("1"));
    // The mark keeps the status that the finalized receipt will have.
    let pool = rig.journal().await;
    let status: i64 = sqlx::query_scalar("SELECT status FROM soft_marks WHERE kind='receipt'")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert_eq!(status, 1);
    // Nothing is finalized that the audit has not checked.
    assert_eq!(finalized_receipts(&rig).await, 0);
    assert!(meta(&rig, "finalized_checkpoint").await.is_none());
    assert!(meta(&rig, "finality:mismatch").await.is_none());
}

#[tokio::test]
async fn a_finalized_keeper_writes_no_mark_and_observes_no_audit() {
    let rig = Rig::new(ROBINHOOD_TESTNET, settings(FinalityMode::Finalized, 0))
        .await
        .unheld();
    ready(&rig);
    rig.run(true, false).await;
    rig.settle();
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("jobs [1=served]"), "{}", run.journal);
    // A sweep too: signed, sent and settled with the same functions, and with no mark.
    rig.queue_sweep("2000000000000000000").await;
    rig.run(true, false).await;
    rig.settle();
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("last=sent"), "{}", run.journal);
    // What it wrote is what it always wrote: the finalized receipt and checkpoint. Its journal has no table of marks at
    // all, as 0.4.1's has none.
    let pool = rig.journal().await;
    let tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='soft_marks'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    assert_eq!(tables, 0);
    assert!(meta(&rig, "health:finality_audit_stalled").await.is_none());
    assert!(meta(&rig, "soft_checkpoint").await.is_none());
    assert_eq!(finalized_receipts(&rig).await, 1);
}

#[tokio::test]
async fn the_audit_makes_what_a_soft_keeper_acted_on_final_once_the_chain_finalizes_it() {
    let rig = soft(0).await;
    ready(&rig);
    let signed_at = rig.chain(|chain| chain.head);
    rig.run(true, false).await;
    rig.chain(|chain| chain.include());
    rig.run(true, false).await;
    let (tx, included) = (sent_hash(&rig, 0), rig.chain(|chain| chain.head));
    let included_hash = hash_of(&rig, included);

    // Finalized trails the keeper by LAG blocks: a tick reads the finalized header, finds no marked block at or below
    // it and reads no block. The signed bytes stay in the journal through compaction, whose pass is due every tick here.
    exec(
        &rig,
        "UPDATE meta SET value='0' WHERE key='history:compact_after'",
    )
    .await;
    let early = rig.run(false, false).await;
    let head = rig.chain(|chain| chain.head);
    assert_eq!(audit_reads(&early.tick, head), Some(vec![]));
    assert_eq!(finalized_receipts(&rig).await, 0);
    let pool = rig.journal().await;
    let raw: String = sqlx::query_scalar("SELECT raw FROM txs")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert!(raw.starts_with("0x"), "{raw}");

    // L1 catches up. The next tick starts with the audit: the header, and in one batch the two blocks that were marked.
    finalize_everything(&rig);
    let head = rig.chain(|chain| chain.head);
    exec(
        &rig,
        "UPDATE meta SET value='0' WHERE key='history:compact_after'",
    )
    .await;
    let run = rig.run(false, false).await;
    assert_eq!(
        audit_reads(&run.tick, head),
        Some(vec![signed_at, included]),
        "{:#?}",
        run.tick
    );
    assert_eq!(run.tick[1], "batch [");
    // The receipt is a finalized one, the checkpoint is the highest block checked, and the signed bytes that had to wait
    // are blanked by the compaction that follows in the same tick.
    let pool = rig.journal().await;
    let receipts: Vec<(String, i64, String, i64)> =
        sqlx::query_as("SELECT hash,block_number,block_hash,status FROM finalized_receipts")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        receipts,
        [(
            tx,
            i64::try_from(included).unwrap(),
            included_hash.clone(),
            1
        )]
    );
    let raw: String = sqlx::query_scalar("SELECT raw FROM txs")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert_eq!(raw, "");
    assert_eq!(
        checkpoint(&rig, "finalized_checkpoint").await,
        Some((included, included_hash))
    );
    // What was checked is gone; the tick's own head mark is the one that is left, and the soft checkpoint is not the
    // audit's.
    assert_eq!(
        marks(&rig).await,
        [mark("head", head, hash_of(&rig, head), "")]
    );
    assert_eq!(
        checkpoint(&rig, "soft_checkpoint").await,
        Some((head, hash_of(&rig, head)))
    );
    assert!(meta(&rig, "finality:mismatch").await.is_none());
}

#[tokio::test]
async fn the_audit_reads_the_lowest_sixty_four_marked_blocks_in_one_batch() {
    let rig = soft(0).await;
    rig.chain(|chain| {
        chain.finalized_lag = 0;
        chain.safe_lag = 0;
    });
    let head = rig.chain(|chain| chain.head);
    // A first tick makes the journal; its only mark is the head.
    rig.run(false, false).await;
    // Seventy blocks that the keeper acted on, two marks each: the page is 64 distinct blocks, however many marks.
    let pool = rig.journal().await;
    for number in 100..170u64 {
        for (kind, reference) in [
            ("sign", format!("0xtx{number}")),
            ("nonce", number.to_string()),
        ] {
            sqlx::query("INSERT INTO soft_marks(number,hash,kind,ref,created) VALUES(?,?,?,?,?)")
                .bind(i64::try_from(number).unwrap())
                .bind(hash_of(&rig, number))
                .bind(kind)
                .bind(reference)
                .bind(i64::try_from(crate::health::now().unwrap()).unwrap())
                .execute(&pool)
                .await
                .unwrap();
        }
    }
    pool.close().await;
    let run = rig.run(false, false).await;
    let blocks = audit_reads(&run.tick, head).unwrap();
    assert_eq!(blocks, (100..164).collect::<Vec<u64>>(), "{:#?}", run.tick);
    // One batch, after the header: the block reads are consecutive lines inside one pair of brackets.
    assert_eq!(run.tick[1], "batch [");
    assert_eq!(run.tick[66], "]");
    // The blocks of the page are checked and gone, with both their marks; the rest wait for the next audit.
    let left = marks(&rig).await;
    let numbers: std::collections::BTreeSet<u64> = left.iter().map(|m| m.1).collect();
    assert_eq!(left.len(), 12 + 1, "six blocks of two marks and the head");
    assert_eq!(
        numbers.into_iter().collect::<Vec<_>>(),
        [164, 165, 166, 167, 168, 169, head]
    );
    assert_eq!(
        checkpoint(&rig, "finalized_checkpoint").await,
        Some((163, hash_of(&rig, 163)))
    );
    // The next audit takes the six that are left, and the head mark of the tick before, which is final too.
    let run = rig.run(false, false).await;
    assert_eq!(
        audit_reads(&run.tick, head).unwrap(),
        [164, 165, 166, 167, 168, 169, head]
    );
    assert_eq!(marks(&rig).await.len(), 1);
    assert_eq!(
        checkpoint(&rig, "finalized_checkpoint").await,
        Some((head, hash_of(&rig, head)))
    );
}

#[tokio::test]
async fn an_audit_runs_at_the_first_tick_and_then_when_its_interval_has_passed() {
    let rig = soft(0).await;
    rig.chain(|chain| chain.finalized_lag = 0);
    let process = rig.process(false, false).await;
    let head = rig.chain(|chain| chain.head);
    let first = process.tick().await.unwrap();
    assert_eq!(audit_reads(&first.tick, head), Some(vec![]));
    // The process stays up: its next ticks are inside the 30 seconds and read the finalized header no more.
    for _ in 0..3 {
        rig.chain(|chain| chain.mine(1));
        let tick = process.tick().await.unwrap();
        assert!(
            tick.tick.iter().all(|line| !is_finalized_header(line)),
            "{:#?}",
            tick.tick
        );
    }
    process.stop().await;
    // A process that starts afterwards audits at its first tick, as every restart does: it finds the head mark of the
    // ticks before.
    let head = rig.chain(|chain| chain.head);
    let run = rig.run(false, false).await;
    assert_eq!(audit_reads(&run.tick, head).map(|b| b.len()), Some(1));

    // With the shortest interval a process audits at every tick.
    let eager = Rig::new(
        ROBINHOOD_TESTNET,
        ChainSettings {
            finality_mode: FinalityMode::Soft,
            finality_audit_interval_seconds: 0,
            ..ChainSettings::default()
        },
    )
    .await
    .unheld();
    eager.chain(|chain| chain.finalized_lag = LAG);
    let process = eager.process(false, false).await;
    for _ in 0..3 {
        let head = eager.chain(|chain| chain.head);
        let tick = process.tick().await.unwrap();
        assert!(audit_reads(&tick.tick, head).is_some(), "{:#?}", tick.tick);
        eager.chain(|chain| chain.mine(1));
    }
    process.stop().await;
}

#[tokio::test]
async fn the_audit_gives_up_at_its_budget_and_writes_nothing() {
    // The budget is the design's two seconds.
    assert_eq!(finality::AUDIT_BUDGET, Duration::from_secs(2));
    let rig = soft(0).await;
    rig.chain(|chain| chain.finalized_lag = 0);
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(&dir.path().join("audit.sqlite"), "scope")
        .await
        .unwrap();
    let marked = hash_of(&rig, 500);
    let mark_500 = || async {
        sqlx::query(
            "INSERT INTO soft_marks(number,hash,kind,ref,created) VALUES(500,?,'sign','0xtx',1)",
        )
        .bind(&marked)
        .execute(&journal.pool)
        .await
        .unwrap();
    };
    mark_500().await;
    let rpc = Rpc::new(vec![rig.node.url.clone()]).unwrap();

    // A node that answers in time: the mark is the chain's and goes.
    rig.node.hold(Duration::from_millis(50));
    let audited = finality::audit(&rpc, &journal, 100, finality::AUDIT_BUDGET)
        .await
        .unwrap();
    assert!(
        matches!(audited, Some(Audit::Audited { blocks: 1, .. })),
        "{audited:?}"
    );
    assert!(journal.soft_marks().await.unwrap().is_empty());
    mark_500().await;
    sqlx::query("DELETE FROM meta WHERE key='finalized_checkpoint'")
        .execute(&journal.pool)
        .await
        .unwrap();

    // A node that answers after three seconds: the audit returns at its budget, with nothing read and nothing written,
    // and the same marks are there for the next one.
    rig.node.hold(Duration::from_secs(3));
    let began = Instant::now();
    let late = finality::audit(&rpc, &journal, 100, finality::AUDIT_BUDGET)
        .await
        .unwrap();
    let took = began.elapsed();
    assert_eq!(late, None);
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_millis(2_900),
        "{took:?}"
    );
    assert_eq!(journal.soft_marks().await.unwrap().len(), 1);
    assert_eq!(journal.finality_mismatch().await.unwrap(), None);
    assert_eq!(journal.meta("finalized_checkpoint").await.unwrap(), None);
    rig.node.hold(Duration::ZERO);
    journal.pool.close().await;
}

#[tokio::test]
async fn a_tick_is_held_no_longer_than_the_budget_by_a_finalized_header_that_comes_late() {
    let rig = soft(0).await;
    rig.chain(|chain| chain.finalized_lag = 0);
    ready(&rig);
    rig.run(true, false).await;
    rig.chain(|chain| chain.include());
    let signed = marks(&rig).await;
    assert_eq!(signed.len(), 2, "the sign mark and the head mark");

    // The node answers a read of the finalized header six seconds after it arrives, and everything else as ever. The
    // audit gives up at its two seconds, with nothing audited, and the tick goes on to settle the receipt.
    rig.node.late_finalized(Duration::from_secs(6));
    let began = Instant::now();
    let run = rig.run(true, false).await;
    let took = began.elapsed();
    rig.node.late_finalized(Duration::ZERO);
    assert!(run.journal.contains("jobs [1=served]"), "{}", run.journal);
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_secs(4),
        "{took:?}"
    );
    let found = marks(&rig).await;
    assert!(found.contains(signed.iter().find(|m| m.0 == "sign").unwrap()));
    assert_eq!(finalized_receipts(&rig).await, 0);
    assert!(meta(&rig, "finalized_checkpoint").await.is_none());
    assert!(meta(&rig, "finality:mismatch").await.is_none());

    // The next tick, with the header on time, audits what the late one could not.
    rig.run(false, false).await;
    assert_eq!(finalized_receipts(&rig).await, 1);
    assert_eq!(marks(&rig).await.len(), 1, "only the head mark of the tick");
}

#[tokio::test]
async fn an_audit_that_cannot_read_the_chain_fails_without_writing_and_moves_to_another_endpoint() {
    let mut rig = soft(0).await;
    rig.chain(|chain| chain.finalized_lag = 0);
    let second = rig.add_endpoint().await;
    rig.node.hold(Duration::ZERO);
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(&dir.path().join("audit.sqlite"), "scope")
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO soft_marks(number,hash,kind,ref,created) VALUES(500,?,'sign','0xtx',1)",
    )
    .bind(hash_of(&rig, 500))
    .execute(&journal.pool)
    .await
    .unwrap();
    // Every endpoint down: the audit is an error of reads, which the keeper logs and tries again at the next interval.
    // Nothing is written, and nothing is a mismatch.
    rig.node.endpoint().set(Mode::Down);
    second.set(Mode::Down);
    let rpc = Rpc::new(vec![rig.node.url.clone(), second.url.clone()]).unwrap();
    let error = finality::audit(&rpc, &journal, 100, finality::AUDIT_BUDGET)
        .await
        .unwrap_err();
    assert!(crate::rpc::is_delivery_failure(&error), "{error:#}");
    assert_eq!(journal.soft_marks().await.unwrap().len(), 1);
    assert_eq!(journal.finality_mismatch().await.unwrap(), None);
    // One of them answers again; the header and the blocks are that endpoint's.
    second.set(Mode::Up);
    let audited = finality::audit(&rpc, &journal, 100, finality::AUDIT_BUDGET)
        .await
        .unwrap();
    assert!(
        matches!(audited, Some(Audit::Audited { blocks: 1, .. })),
        "{audited:?}"
    );
    assert!(journal.soft_marks().await.unwrap().is_empty());
    journal.pool.close().await;
}

#[tokio::test]
async fn an_audit_that_falls_behind_is_a_degraded_observation_and_not_a_halt() {
    let rig = soft(0).await;
    ready(&rig);
    rig.run(true, false).await;
    rig.chain(|chain| chain.include());
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("healthy=true"), "{}", run.journal);

    // The oldest mark is younger than the limit (2,700 seconds): nothing is observed.
    exec(
        &rig,
        "UPDATE soft_marks SET created=created-100 WHERE kind='sign'",
    )
    .await;
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("healthy=true"), "{}", run.journal);
    assert!(meta(&rig, "health:finality_audit_stalled").await.is_none());

    // Older than the limit, and the audit has not caught up (finalized trails the blocks): the keeper is degraded, and
    // goes on serving; it does not stop, and no mismatch is recorded.
    exec(
        &rig,
        "UPDATE soft_marks SET created=created-3000 WHERE kind='sign'",
    )
    .await;
    rig.request();
    rig.chain(|chain| chain.mine(3));
    let run = rig.run(true, false).await;
    assert!(
        run.journal
            .contains("healthy=false faults=[\"finality_audit_stalled\"]"),
        "{}",
        run.journal
    );
    assert!(meta(&rig, "health:finality_audit_stalled").await.is_some());
    assert_eq!(
        rig.sent().len(),
        2,
        "a stalled audit does not stop the keeper from sending: {:?}",
        rig.sent()
    );
    assert!(meta(&rig, "finality:mismatch").await.is_none());

    // L1 catches up and the audit checks the old marks: the observation goes.
    rig.chain(|chain| chain.include());
    finalize_everything(&rig);
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("faults=[]"), "{}", run.journal);
    assert!(meta(&rig, "health:finality_audit_stalled").await.is_none());
    assert_eq!(marks(&rig).await.len(), 2, "this tick's head and receipt");
}

#[tokio::test]
async fn a_replaced_block_is_found_by_the_audit_before_anything_is_decided_on_it() {
    let rig = soft(0).await;
    ready(&rig);
    rig.run(true, false).await;
    rig.chain(|chain| chain.include());
    let included = rig.chain(|chain| chain.head);
    rig.run(true, false).await;
    let included_hash = hash_of(&rig, included);
    let before = marks(&rig).await;

    // The sequencer replaces the block that holds the receipt, and the chain goes on until L1 has finalized past it.
    let replaced = rig.chain(|chain| {
        chain.replace_blocks(included);
        chain.mine(LAG + 100);
        chain.block_hash(included).to_string()
    });
    assert_ne!(replaced, included_hash);
    // The block the last tick decided on is gone too: the checkpoint would find it, and the audit has already.
    rig.run_with(true, false, |_| {})
        .await
        .expect("a held keeper is not a failed tick");
    // The audit had run first. Of the marks it read, in the order of their blocks, the first that is not the chain's is
    // the head mark of the block that was replaced; the sign mark of the block before it is still the chain's. One
    // endpoint showed it, and nothing is recorded: it is the suspicion the keeper holds its sends on.
    assert!(meta(&rig, "finality:mismatch").await.is_none());
    let recorded = suspected(&rig).await.unwrap().mismatch;
    assert_eq!(
        (
            recorded.kind.as_str(),
            recorded.number,
            recorded.expected.as_str(),
            recorded.actual.as_str()
        ),
        ("head", included, included_hash.as_str(), replaced.as_str())
    );
    // Nothing was moved or deleted: every mark is there for the recovery, and nothing became final.
    assert_eq!(marks(&rig).await, before);
    assert_eq!(finalized_receipts(&rig).await, 0);
    assert!(meta(&rig, "finalized_checkpoint").await.is_none());

    // A tick that gets as far as the audit again finds the same block, records nothing and moves nothing.
    rig.node.take();
    rig.run_with(false, false, |_| {}).await.unwrap();
    let still = suspected(&rig).await.unwrap().mismatch;
    assert_eq!(
        Mismatch {
            detected_at: recorded.detected_at,
            ..still
        },
        recorded
    );
    assert!(meta(&rig, "finality:mismatch").await.is_none());
    assert_eq!(marks(&rig).await, before);
}

#[tokio::test]
async fn a_replaced_block_the_audit_finds_and_two_endpoints_agree_on_is_recovered_from_by_itself() {
    let rig = soft_pair(0).await;
    ready(&rig);
    rig.run(true, false).await;
    rig.chain(|chain| chain.include());
    let included = rig.chain(|chain| chain.head);
    rig.run(true, false).await;
    let included_hash = hash_of(&rig, included);
    let replaced = rig.chain(|chain| {
        chain.replace_blocks(included);
        chain.mine(LAG + 100);
        chain.block_hash(included).to_string()
    });
    // The audit finds the head mark, both endpoints agree, and the recovery starts in the same tick; the receipt is in
    // the replaced block still, so nothing is sent again, and a tick after clears the record.
    for _ in 0..3 {
        rig.run(true, false).await;
    }
    assert!(meta(&rig, "finality:mismatch").await.is_none());
    assert!(suspected(&rig).await.is_none());
    let last: serde_json::Value =
        serde_json::from_str(&meta(&rig, "finality:last_recovery").await.unwrap()).unwrap();
    assert_eq!(
        (
            last["mismatch"]["kind"].clone(),
            last["mismatch"]["number"].clone(),
            last["mismatch"]["expected"].clone(),
            last["mismatch"]["actual"].clone()
        ),
        (
            "head".into(),
            included.into(),
            included_hash.into(),
            replaced.into()
        )
    );
    assert_eq!(last["confirmed"]["agreeing"], 2);
    assert_eq!(last["confirmed"]["final"], true, "{last}");
    assert_eq!(rig.sent().len(), 1);
}

#[tokio::test]
async fn the_audit_alone_finds_a_replaced_block_the_checkpoint_cannot_see() {
    // A marked block that the chain replaced, below a head that is the chain's: on the scripted chain only a journal
    // that holds marks no tick wrote can have that, so the marks are written as a keeper's earlier ticks would have.
    let rig = soft(0).await;
    rig.chain(|chain| chain.finalized_lag = 0);
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(&dir.path().join("audit.sqlite"), "scope")
        .await
        .unwrap();
    let (old_450, old_500) = (hash_of(&rig, 450), hash_of(&rig, 500));
    for (number, hash, kind, reference) in [
        (450, &old_450, "sign", "0xtx1"),
        (500, &old_500, "receipt", "0xtx2"),
    ] {
        sqlx::query(
            "INSERT INTO soft_marks(number,hash,kind,ref,created,status) VALUES(?,?,?,?,1,1)",
        )
        .bind(number)
        .bind(hash)
        .bind(kind)
        .bind(reference)
        .execute(&journal.pool)
        .await
        .unwrap();
    }
    rig.chain(|chain| chain.replace_blocks(480));
    let rpc = Rpc::new(vec![rig.node.url.clone()]).unwrap();
    let audit = finality::audit(&rpc, &journal, 7_000, finality::AUDIT_BUDGET)
        .await
        .unwrap()
        .unwrap();
    let found = Mismatch {
        kind: "receipt".into(),
        number: 500,
        reference: "0xtx2".into(),
        expected: old_500,
        actual: hash_of(&rig, 500),
        detected_at: 7_000,
    };
    assert_eq!(audit, Audit::Mismatch(found.clone()));
    // The block below the fork point is the chain's still, and stays a mark: the audit moves nothing from a page that
    // holds a mismatch. It records nothing either: that is for the endpoints to agree on.
    assert_eq!(journal.soft_marks().await.unwrap().len(), 2);
    assert_eq!(hash_of(&rig, 450), old_450);
    assert_eq!(journal.finality_mismatch().await.unwrap(), None);
    // Once it is on record, the audit reads nothing from there on.
    journal.record_finality_mismatch(&found).await.unwrap();
    rig.node.take();
    let stopped = finality::audit(&rpc, &journal, 7_100, finality::AUDIT_BUDGET)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(stopped, Audit::Stopped(_)));
    assert_eq!(rig.node.asked(), 0);
    journal.pool.close().await;
}

#[tokio::test]
async fn an_epoch_commit_a_fulfillment_and_a_sweep_leave_their_marks_too() {
    let rig = soft(0).await;
    // An epoch commit for the first request of a new epoch.
    rig.chain(|chain| chain.mine_to(chain.epoch_start(2) + 2));
    rig.run(true, false).await;
    rig.request();
    rig.chain(|chain| chain.mine(1));
    rig.run(true, false).await;
    assert!(rig.sent()[0].contains("commitEpoch"), "{:?}", rig.sent());
    let commit = sent_hash(&rig, 0);
    rig.chain(|chain| {
        chain.include();
        chain.mine(2);
    });
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("2=committed"), "{}", run.journal);
    let found = marks(&rig).await;
    assert_eq!(
        (
            marked(&found, "sign", &commit),
            marked(&found, "receipt", &commit)
        ),
        (1, 1),
        "{found:#?}"
    );

    // The request is served by a single fulfillment, which that tick signed.
    let fulfillment = sent_hash(&rig, 1);
    assert!(
        rig.sent()[1].contains("fulfillRandomness"),
        "{:?}",
        rig.sent()
    );
    rig.chain(|chain| chain.include());
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("jobs [1=served]"), "{}", run.journal);
    let found = marks(&rig).await;
    assert_eq!(
        (
            marked(&found, "sign", &fulfillment),
            marked(&found, "receipt", &fulfillment)
        ),
        (1, 1),
        "{found:#?}"
    );

    // A sweep: the transfer is signed on a decision head and settled by a receipt, both on record.
    rig.queue_sweep("2000000000000000000").await;
    rig.run(true, false).await;
    let sweep = sent_hash(&rig, rig.sent().len() - 1);
    assert_eq!(marked(&marks(&rig).await, "sign", &sweep), 1);
    rig.chain(|chain| chain.include());
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("last=sent"), "{}", run.journal);
    let found = marks(&rig).await;
    let receipt = found
        .iter()
        .find(|m| m.0 == "receipt" && m.3 == sweep)
        .unwrap_or_else(|| panic!("the sweep's receipt has no mark: {found:#?}"));
    assert_eq!(receipt.2, hash_of(&rig, receipt.1));
    // The nonce floor followed every one of them: epoch, fulfillment, sweep.
    assert_eq!(meta(&rig, "nonce_floor").await.as_deref(), Some("3"));
    // Once L1 has finalized past them all, the audit makes the three receipts final.
    finalize_everything(&rig);
    rig.run(false, false).await;
    assert_eq!(finalized_receipts(&rig).await, 3);
    assert_eq!(
        marks(&rig).await.len(),
        1,
        "only the head mark of the tick is left"
    );
}

#[tokio::test]
async fn a_batch_leaves_one_sign_mark_and_one_receipt_mark_for_its_nonce() {
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
    rig.run(true, false).await;
    assert!(
        rig.sent()[0].contains("fulfillRandomnessBatch"),
        "{:?}",
        rig.sent()
    );
    let batch = sent_hash(&rig, 0);
    let found = marks(&rig).await;
    assert_eq!(
        (
            marked(&found, "sign", &batch),
            marked(&found, "receipt", &batch)
        ),
        (1, 0)
    );
    rig.chain(|chain| chain.include());
    let run = rig.run(true, false).await;
    assert!(
        run.journal.contains("jobs [1=served 2=served]"),
        "{}",
        run.journal
    );
    let found = marks(&rig).await;
    assert_eq!(
        (
            marked(&found, "sign", &batch),
            marked(&found, "receipt", &batch)
        ),
        (1, 1),
        "{found:#?}"
    );
    assert_eq!(meta(&rig, "nonce_floor").await.as_deref(), Some("1"));
}

#[tokio::test]
async fn a_replacement_and_a_cancellation_are_signed_on_the_head_of_the_tick_that_signs_them() {
    let rig = soft(0).await;
    ready(&rig);
    rig.run(true, false).await;
    // The sequencer includes nothing, and ten seconds pass: the next tick replaces the transaction with a dearer one,
    // signed on the head that tick decided on.
    rig.chain(|chain| {
        chain.drop_queue();
        chain.mine(12);
    });
    let head = rig.chain(|chain| chain.head);
    rig.run(true, false).await;
    assert_eq!(rig.sent().len(), 2, "{:?}", rig.sent());
    let replacement = sent_hash(&rig, 1);
    assert!(
        marks(&rig)
            .await
            .contains(&mark("sign", head, hash_of(&rig, head), &replacement))
    );
    // The request expires unserved: the next tick cancels the nonce, on its own head again.
    rig.chain(|chain| {
        chain.drop_queue();
        chain.mine(70);
    });
    let head = rig.chain(|chain| chain.head);
    rig.run(true, false).await;
    let cancel = sent_hash(&rig, rig.sent().len() - 1);
    assert!(
        rig.sent().last().unwrap().starts_with("cancel"),
        "{:?}",
        rig.sent()
    );
    assert!(
        marks(&rig)
            .await
            .contains(&mark("sign", head, hash_of(&rig, head), &cancel))
    );
}

#[tokio::test]
async fn a_nonce_resolved_without_a_receipt_is_resolved_with_a_mark_of_the_head_it_was_found_at() {
    let rig = soft(0).await;
    ready(&rig);
    rig.run(true, false).await;
    rig.chain(|chain| {
        chain.include();
        chain.hide_receipts = true;
    });
    let head = rig.chain(|chain| chain.head);
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("jobs [1=served]"), "{}", run.journal);
    assert_eq!(meta(&rig, "nonce_floor").await.as_deref(), Some("1"));
    let found = marks(&rig).await;
    // The nonce was found consumed at the head this tick decided on, and that block is on record beside it. There is
    // no receipt to mark.
    assert!(
        found.contains(&mark("nonce", head, hash_of(&rig, head), "0")),
        "{found:#?}"
    );
    assert!(found.iter().all(|m| m.0 != "receipt"), "{found:#?}");
    // The audit checks the block, and the signed bytes of a nonce no receipt was served for are kept.
    finalize_everything(&rig);
    exec(
        &rig,
        "UPDATE meta SET value='0' WHERE key='history:compact_after'",
    )
    .await;
    rig.run(false, false).await;
    assert_eq!(finalized_receipts(&rig).await, 0);
    assert_eq!(marks(&rig).await.len(), 1);
    let pool = rig.journal().await;
    let raw: String = sqlx::query_scalar("SELECT raw FROM txs")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert!(raw.starts_with("0x"), "{raw}");
}

#[tokio::test]
async fn a_sweep_that_is_never_included_is_cancelled_on_the_head_of_the_tick_and_settled_with_a_mark()
 {
    let rig = soft(0).await;
    // A first tick makes the journal that the operator's command queues the sweep in.
    rig.run(false, false).await;
    rig.queue_sweep("2000000000000000000").await;
    rig.run(true, false).await;
    let sweep = sent_hash(&rig, 0);
    assert_eq!(marked(&marks(&rig).await, "sign", &sweep), 1);
    // The sequencer never includes it, and the seconds it waits for a receipt pass: the next tick cancels its nonce, on
    // the head that tick decided on.
    rig.chain(|chain| chain.drop_queue());
    rig.age_sweep().await;
    let head = rig.chain(|chain| chain.head);
    rig.run(true, false).await;
    assert!(
        rig.sent().last().unwrap().starts_with("cancel"),
        "{:?}",
        rig.sent()
    );
    let cancel = sent_hash(&rig, 1);
    assert!(
        marks(&rig)
            .await
            .contains(&mark("sign", head, hash_of(&rig, head), &cancel))
    );
    // The cancellation is included and its receipt resolves the sweep as cancelled, with the mark of that receipt: the
    // transaction that used the nonce, and not the transfer that was dropped.
    rig.chain(|chain| chain.include());
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("last=cancelled"), "{}", run.journal);
    let found = marks(&rig).await;
    assert_eq!(
        (
            marked(&found, "receipt", &cancel),
            marked(&found, "receipt", &sweep)
        ),
        (1, 0),
        "{found:#?}"
    );
    assert_eq!(meta(&rig, "nonce_floor").await.as_deref(), Some("1"));
}

#[tokio::test]
async fn a_batch_and_an_epoch_commit_that_no_endpoint_serves_a_receipt_for_are_resolved_with_nonce_marks()
 {
    // An epoch commit whose block is the chain's but whose receipt is not served.
    let rig = soft(0).await;
    rig.chain(|chain| chain.mine_to(chain.epoch_start(2) + 2));
    rig.run(true, false).await;
    rig.request();
    rig.chain(|chain| chain.mine(1));
    rig.run(true, false).await;
    assert!(rig.sent()[0].contains("commitEpoch"), "{:?}", rig.sent());
    rig.chain(|chain| {
        chain.include();
        chain.mine(2);
        chain.hide_receipts = true;
    });
    let head = rig.chain(|chain| chain.head);
    let run = rig.run(true, false).await;
    assert!(run.journal.contains("2=committed"), "{}", run.journal);
    let found = marks(&rig).await;
    assert!(
        found.contains(&mark("nonce", head, hash_of(&rig, head), "0")),
        "{found:#?}"
    );
    assert!(found.iter().all(|m| m.0 != "receipt"), "{found:#?}");
    assert_eq!(meta(&rig, "nonce_floor").await.as_deref(), Some("1"));

    // A batch the same way.
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
    rig.run(true, false).await;
    assert!(
        rig.sent()[0].contains("fulfillRandomnessBatch"),
        "{:?}",
        rig.sent()
    );
    rig.chain(|chain| {
        chain.include();
        chain.hide_receipts = true;
    });
    let head = rig.chain(|chain| chain.head);
    let run = rig.run(true, false).await;
    assert!(
        run.journal.contains("jobs [1=served 2=served]"),
        "{}",
        run.journal
    );
    let found = marks(&rig).await;
    assert!(
        found.contains(&mark("nonce", head, hash_of(&rig, head), "0")),
        "{found:#?}"
    );
    assert!(found.iter().all(|m| m.0 != "receipt"), "{found:#?}");
    assert_eq!(meta(&rig, "nonce_floor").await.as_deref(), Some("1"));
}

/// R4 M4: one admitted endpoint that accepts connections and never answers does not stall the audit: every endpoint is
/// asked within a deadline inside the audit's budget, and the one that answers audits the mark at once.
#[tokio::test]
async fn one_silent_endpoint_does_not_stall_the_audit() {
    let rig = soft(0).await;
    rig.chain(|chain| chain.finalized_lag = 0);
    rig.node.hold(Duration::ZERO);
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(&dir.path().join("audit.sqlite"), "scope")
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO soft_marks(number,hash,kind,ref,created) VALUES(500,?,'sign','0xtx',1)",
    )
    .bind(hash_of(&rig, 500))
    .execute(&journal.pool)
    .await
    .unwrap();
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let silent_url = format!("http://{}", silent.local_addr().unwrap());
    tokio::spawn(async move {
        let mut open = Vec::new();
        loop {
            if let Ok((socket, _)) = silent.accept().await {
                open.push(socket);
            }
        }
    });
    let rpc = Rpc::new(vec![rig.node.url.clone(), silent_url]).unwrap();
    let audited = finality::audit(&rpc, &journal, 100, finality::AUDIT_BUDGET)
        .await
        .unwrap();
    assert!(
        matches!(audited, Some(Audit::Audited { blocks: 1, .. })),
        "{audited:?}"
    );
    assert!(journal.soft_marks().await.unwrap().is_empty());
    journal.pool.close().await;
}

/// R4 L3: the audit asks every endpoint for the marked blocks' hashes. The one that answers first shows the journal's
/// block, another shows another fork's: nothing is made final, and the audit returns the other word as a mismatch for the
/// endpoints to settle.
#[tokio::test]
async fn the_audit_makes_nothing_final_while_the_endpoints_disagree_on_a_marked_block() {
    let mut rig = soft(0).await;
    rig.chain(|chain| chain.finalized_lag = 0);
    rig.node.hold(Duration::ZERO);
    let forked = rig.add_endpoint().await;
    forked.set(Mode::Fork(0));
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(&dir.path().join("audit.sqlite"), "scope")
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO soft_marks(number,hash,kind,ref,created) VALUES(500,?,'sign','0xtx',1)",
    )
    .bind(hash_of(&rig, 500))
    .execute(&journal.pool)
    .await
    .unwrap();
    let rpc = Rpc::new(vec![rig.node.url.clone(), forked.url.clone()]).unwrap();
    let audited = finality::audit(&rpc, &journal, 100, finality::AUDIT_BUDGET)
        .await
        .unwrap();
    assert!(
        matches!(&audited, Some(Audit::Mismatch(found)) if found.number == 500),
        "{audited:?}"
    );
    assert_eq!(journal.soft_marks().await.unwrap().len(), 1);
    // The honest endpoint alone makes it final.
    let alone = Rpc::new(vec![rig.node.url.clone()]).unwrap();
    let audited = finality::audit(&alone, &journal, 100, finality::AUDIT_BUDGET)
        .await
        .unwrap();
    assert!(
        matches!(audited, Some(Audit::Audited { blocks: 1, .. })),
        "{audited:?}"
    );
    journal.pool.close().await;
}
