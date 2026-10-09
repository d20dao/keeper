//! A round keeper against reads that lag and blocks the sequencer replaces (the R2/R3 review of `rh/keeper-round-k3`,
//! findings H1, M1, M2 and L1, ported from its proofs of concept as regression tests). A request is `vanished` only when
//! the chain confirms it is gone, and then never for good; one reverting read of an endpoint behind the others neither
//! kills a live request nor cancels its fulfillment; bytes whose proof a replaced block made stale are never sent again;
//! and a round the coordinator verified before its oldest live request is still fetched. A machine clock behind the
//! chain's (L5) neither stops the rounds being fetched nor goes unreported.
use crate::{
    rig::Rig,
    round_mode::{bound, journal, lane_clock, mine_to_time, round_rig, row},
    scripted::Mode,
};

async fn serving_rig() -> Rig {
    let rig = round_rig().await.unheld();
    rig.chain(|chain| chain.l1_gas = |_| 20_000);
    rig
}
async fn state(rig: &Rig, id: u64) -> String {
    let journal = journal(rig).await;
    let job = journal.job(&id.to_string()).await.unwrap().unwrap();
    journal.pool.close().await;
    job.state
}
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
/// The keeper's mined transactions that reverted.
fn reverted(rig: &Rig) -> usize {
    rig.chain(|chain| {
        chain
            .sends
            .iter()
            .filter(|(hash, ..)| chain.receipt_status(hash) == Some(false))
            .count()
    })
}

/// M1. A fulfillment is in flight when, for one tick, the endpoint answers `getRoundRequest` with a revert (a backend
/// behind the others). The read is not taken as the request gone: the fulfillment is not cancelled, the job is not
/// `vanished`, and the request is served.
#[tokio::test]
async fn one_reverting_read_cancels_no_fulfillment_in_flight_and_the_request_is_served() {
    let rig = serving_rig().await;
    let process = rig.process(true, false).await;
    let id = rig.request();
    let (_, _, due) = bound(&rig, id);
    mine_to_time(&rig, due);
    lane_clock(&process).set(due * 1_000);
    process.tick().await.unwrap();
    process.tick().await.unwrap();
    assert_eq!(sent(&rig).len(), 1, "{:?}", sent(&rig));
    // Ten seconds pass without inclusion, so reconciliation looks at the attempt.
    rig.chain(|chain| chain.mine(12));
    rig.endpoints()[0].set(Mode::RevertingRoundRequests);
    let tick = process.tick().await;
    rig.endpoints()[0].set(Mode::Up);
    // The tick may fail on the read; it cancels nothing and keeps the job.
    if let Ok(run) = &tick {
        assert!(
            !run.tick
                .iter()
                .any(|line| line.contains("eth_sendRawTransaction") && line.contains("cancel")),
            "{:#?}",
            run.tick
        );
    }
    assert!(
        sent(&rig)
            .iter()
            .all(|(label, ..)| !label.contains("cancel")),
        "{:?}",
        sent(&rig)
    );
    assert_ne!(state(&rig, id).await, "vanished");
    for _ in 0..4 {
        rig.chain(|chain| chain.include());
        process.tick().await.unwrap();
    }
    assert!(rig.chain(|chain| chain.requests[&id].fulfilled));
    assert_eq!(state(&rig, id).await, "served");
    process.stop().await;
}

/// M1. A request is pending when, for one tick, the endpoint answers `getRoundRequest` with a revert. The job stays
/// pending, and the request is served once its round is due.
#[tokio::test]
async fn one_reverting_read_leaves_a_live_request_pending_and_it_is_served() {
    let rig = serving_rig().await;
    let process = rig.process(true, false).await;
    let id = rig.request();
    process.tick().await.unwrap();
    assert_eq!(state(&rig, id).await, "pending");
    rig.endpoints()[0].set(Mode::RevertingRoundRequests);
    let _ = process.tick().await;
    rig.endpoints()[0].set(Mode::Up);
    assert_eq!(state(&rig, id).await, "pending");
    let (_, _, due) = bound(&rig, id);
    mine_to_time(&rig, due);
    lane_clock(&process).set(due * 1_000);
    for _ in 0..4 {
        process.tick().await.unwrap();
        rig.chain(|chain| chain.include());
    }
    assert!(rig.chain(|chain| chain.requests[&id].fulfilled));
    process.stop().await;
}

/// M1. A read that reverts while the coordinator's `nextRequestId` at the same block says it has the request is the
/// endpoint's failure, not the request's absence; one whose id the coordinator has not reached is absent.
#[tokio::test]
async fn a_reverting_read_is_an_absent_request_only_where_the_coordinator_has_not_reached_its_id() {
    let rig = serving_rig().await;
    let process = rig.process(false, false).await;
    let id = rig.request();
    process.tick().await.unwrap();
    let worker = process.worker();
    let head = rig.chain(|chain| chain.head);
    rig.endpoints()[0].set(Mode::RevertingRoundRequests);
    let read = worker
        .round_request_at(alloy_primitives::U256::from(id), head)
        .await;
    assert!(read.is_err(), "{read:?}");
    rig.endpoints()[0].set(Mode::Up);
    // An id past the coordinator's next one is absent.
    let absent = worker
        .round_request_at(alloy_primitives::U256::from(id + 5), head)
        .await
        .unwrap();
    assert!(absent.is_none());
    process.stop().await;
}

/// H1. The sequencer loses the block of a request, so the job is `vanished`; the chain as it is then reuses the id for a
/// new, paid request. Discovery finds it and the job is pending again, proved for the new request and served.
#[tokio::test]
async fn a_vanished_request_id_the_chain_reuses_is_discovered_again_and_served() {
    let mut rig = serving_rig().await;
    rig.add_endpoint().await;
    rig.chain(|chain| chain.mine(2));
    let process = rig.process(true, false).await;
    process.tick().await.unwrap();
    rig.chain(|chain| chain.mine(1));
    let id = rig.request();
    rig.chain(|chain| chain.mine(1));
    process.tick().await.unwrap();
    assert_eq!(state(&rig, id).await, "pending");
    let request_block = rig.chain(|chain| chain.requests[&id].request_block);
    rig.chain(|chain| {
        chain.drop_blocks(request_block, false);
        chain.mine(1);
    });
    for _ in 0..6 {
        process.tick().await.unwrap();
        rig.chain(|chain| chain.mine(1));
    }
    assert_eq!(state(&rig, id).await, "vanished");
    let again = rig.request();
    assert_eq!(again, id, "the chain as it is reuses the id");
    let (_, _, due) = bound(&rig, again);
    mine_to_time(&rig, due);
    lane_clock(&process).set(due * 1_000);
    for _ in 0..6 {
        process.tick().await.unwrap();
        rig.chain(|chain| chain.include());
    }
    assert!(rig.chain(|chain| chain.requests[&again].fulfilled));
    assert_eq!(state(&rig, again).await, "served");
    process.stop().await;
}

/// M2. A round fulfillment is signed and broadcast but not yet included when a replaced block moves its request (new
/// fields under the id, so its proof's seed is stale) and drops it from the sequencer's queue. After the recovery the
/// stale bytes are never broadcast again: the nonce is cancelled, and the request is proved again and served.
#[tokio::test]
async fn a_fulfillment_whose_request_moved_is_never_rebroadcast_and_the_request_is_proved_again() {
    let mut rig = serving_rig().await;
    rig.add_endpoint().await;
    let process = rig.process(true, false).await;
    let id = rig.request();
    let (_, _, due) = bound(&rig, id);
    mine_to_time(&rig, due);
    lane_clock(&process).set(due * 1_000);
    process.tick().await.unwrap();
    process.tick().await.unwrap();
    let first = sent(&rig);
    assert_eq!(first.len(), 1, "{first:?}");
    let stale_payload = rig.chain(|chain| chain.sends[0].1.clone());
    let logs = crate::rig::Logs::capture(tracing::Level::INFO);
    let request_block = rig.chain(|chain| chain.requests[&id].request_block);
    rig.chain(|chain| {
        chain.replace_blocks(request_block);
        chain.move_request(id);
        chain.drop_queue();
        chain.mine(1);
    });
    for _ in 0..12 {
        let _ = process.tick().await;
        rig.chain(|chain| {
            chain.include();
            chain.mine(2);
        });
    }
    // The stale fulfillment went out once, before the replacement, and never again.
    let fulfillments = sent(&rig)
        .into_iter()
        .filter(|(label, _, nonce)| *label == stale_payload && *nonce == 0)
        .count();
    assert_eq!(fulfillments, 1, "{:?}", sent(&rig));
    assert_eq!(reverted(&rig), 0, "{:?}", sent(&rig));
    assert!(
        rig.chain(|chain| chain.requests[&id].fulfilled),
        "{:?}",
        sent(&rig)
    );
    assert_eq!(state(&rig, id).await, "served");
    // Reconciliation cancelled it: the recovery had no nonce to reopen or fill, the attempt was still in flight.
    assert!(
        logs.text()
            .contains("its nonce is cancelled instead of sending it again"),
        "{}",
        logs.text()
    );
    process.stop().await;
}

/// L1. The coordinator has a round already, and its `RoundVerified` is older than the block of the round's oldest live
/// request (another submitter verified it serving an earlier request). The event is not found from that block on, so
/// the round is fetched from the relays, checked against the randomness the coordinator holds, and the request served.
#[tokio::test]
async fn a_round_the_coordinator_verified_before_its_oldest_live_request_is_fetched_from_the_relays()
 {
    let rig = serving_rig().await;
    let process = rig.process(true, false).await;
    let a = rig.request();
    let (beacon, round, due) = bound(&rig, a);
    rig.chain(|chain| {
        let block = chain.head;
        chain.verify_round(beacon, round, block);
        chain.requests.get_mut(&a).unwrap().fulfilled = true;
        chain.requests.get_mut(&a).unwrap().delivered = true;
        chain.mine(1);
    });
    let b = rig.request();
    assert_eq!(bound(&rig, b).1, round, "both requests bind one round");
    mine_to_time(&rig, due);
    lane_clock(&process).set(due * 1_000);
    let mut lines = Vec::new();
    for _ in 0..6 {
        lines.extend(process.tick().await.unwrap().tick);
        rig.chain(|chain| chain.include());
        lane_clock(&process).set((due + 3) * 1_000);
    }
    assert!(
        rig.chain(|chain| chain.requests[&b].fulfilled),
        "{:?}",
        row(&rig, beacon, round).await
    );
    // The round came from the relays: the trace of the keeper's calls says so whatever the threads' log capture saw (a
    // log line of the fetch's task is not always captured while other tests run).
    assert!(
        crate::round_mode::relay_asks(&lines, round) >= 1,
        "{lines:#?}"
    );
    process.stop().await;
}

/// L5. This machine's clock is a minute behind the chain's: the round is due by the chain's time, not yet by the wall
/// clock. The lane goes by the chain's time, the request is served, and the health fault `clock_behind` stands until the
/// clock is right again.
#[tokio::test]
async fn a_clock_behind_the_chains_still_fetches_due_rounds_and_is_reported() {
    let rig = serving_rig().await;
    let process = rig.process(true, false).await;
    let id = rig.request();
    let (_, _, due) = bound(&rig, id);
    mine_to_time(&rig, due + 30);
    lane_clock(&process).set((due - 30) * 1_000);
    let fault = || async {
        journal(&rig)
            .await
            .meta(crate::health::CLOCK_BEHIND)
            .await
            .unwrap()
    };
    for _ in 0..4 {
        process.tick().await.unwrap();
        rig.chain(|chain| chain.include());
    }
    assert!(rig.chain(|chain| chain.requests[&id].fulfilled));
    assert!(fault().await.is_some());
    let health = journal(&rig)
        .await
        .meta("health:status")
        .await
        .unwrap()
        .unwrap();
    assert!(health.contains("\"clock_behind\""), "{health}");
    // The clock is right again: the fault stands while new blocks show it right for CLOCK_RIGHT_SECONDS, then goes.
    let right = |rig: &crate::rig::Rig| {
        rig.chain(|chain| chain.mine(1));
        rig.chain(|chain| chain.time(chain.head))
    };
    let start = right(&rig);
    lane_clock(&process).set(start * 1_000);
    rig.request();
    process.tick().await.unwrap();
    assert!(fault().await.is_some());
    let later = start + crate::round::CLOCK_RIGHT_SECONDS;
    mine_to_time(&rig, later);
    lane_clock(&process).set(rig.chain(|chain| chain.time(chain.head)) * 1_000);
    process.tick().await.unwrap();
    assert!(fault().await.is_none());
    process.stop().await;
}

/// R4 M3: this machine's clock is a steady 20 seconds behind. Ten seconds without a block make the head ten seconds old,
/// which says nothing of the clock: the fault stands, and the next block does not page the owner again. Nor does a
/// restart, since the journal remembers that the owner was asked.
#[tokio::test]
async fn a_clock_steadily_behind_pages_once_across_an_idle_gap_and_a_restart() {
    let rig = serving_rig().await;
    let pages = |told: &mut crate::telegram::Captured| {
        told.events()
            .into_iter()
            .filter(|event| {
                matches!(event, crate::telegram::Event::Owner(text) if text.contains("saniye geride"))
            })
            .count()
    };
    let head_time = || rig.chain(|chain| chain.time(chain.head));
    let mut process = rig.process(true, false).await;
    let mut told = process.told();
    rig.request();
    let t = head_time();
    lane_clock(&process).set((t - 20) * 1_000);
    process.tick().await.unwrap();
    let first = pages(&mut told);
    // Ten seconds pass without a block.
    lane_clock(&process).set((t - 10) * 1_000);
    process.tick().await.unwrap();
    // A block comes; the clock is still 20 seconds behind.
    mine_to_time(&rig, t + 10);
    let t2 = head_time();
    lane_clock(&process).set((t2 - 20) * 1_000);
    process.tick().await.unwrap();
    let second = pages(&mut told);
    process.stop().await;
    let mut process = rig.process(true, false).await;
    let mut told = process.told();
    lane_clock(&process).set((t2 - 20) * 1_000);
    process.tick().await.unwrap();
    let third = pages(&mut told);
    process.stop().await;
    assert_eq!((first, second, third), (1, 0, 0));
}
