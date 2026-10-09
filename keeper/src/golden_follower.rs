//! The golden trace of a follower: a keeper that is an allowed backup committer beside a primary, which it watches.
//! It prepares the same work as the primary but sends only when the join rule fires: the oldest request is older
//! than `FOLLOWER_DELAY_SECONDS`, or the queue is long, or the primary has not moved its nonce for
//! `PRIMARY_LIVENESS_SECONDS` while there is work it could do. The follower here is one process that stays up across its
//! ticks, as the binary does, because what it has observed of the primary lives in memory.
use crate::{
    golden::{Golden, LAG, Trace, market},
    golden_production::{ARC_MAINNET_FOLLOWER, ARC_MAINNET_PRIMARY, digest},
    rig::Rig,
    scripted::FIRST_EPOCH_START,
};

const GOLDEN: Golden = Golden {
    file: "arc-0.4.1.follower.trace",
    about: "The configuration and calls of keeper 0.4.1 as a follower beside a primary.",
    notes: &[
        "The primary is another wallet: `primary` in a trace. The follower's process stays up across the ticks of a section.",
    ],
};

#[tokio::test]
async fn a_follower_beside_a_primary_asks_the_chain_what_0_4_1_asked() {
    let _exclusive = crate::golden::exclusive().await;
    let rig = Rig::new(5_042, ())
        .await
        .settings(ARC_MAINNET_PRIMARY)
        .settings(ARC_MAINNET_FOLLOWER)
        .follower();
    rig.chain(market);
    let mut trace = Trace::new(&GOLDEN);

    trace.section("1. the configuration");
    trace.lines(digest(&rig.config(false)));

    trace.section("2. an idle follower");
    let process = rig.process(true, true).await;
    trace.startup(
        "startup checks that its wallet is an allowed backup",
        process.startup.clone(),
    );
    rig.chain(|chain| chain.mine_to(FIRST_EPOCH_START + 5 + LAG));
    let run = process.tick().await.unwrap();
    trace.tick(
        "the follower reads the primary's nonce and prepares the epoch",
        run,
    );

    trace.section("3. the primary serves a request before the follower would");
    rig.chain(|chain| {
        // The primary publishes the epoch.
        chain.publish_epoch(1, chain.head);
        chain.primary_nonce += 1;
        chain.mine(1 + LAG);
    });
    let first = rig.request();
    rig.chain(|chain| chain.mine(3 + LAG));
    let run = process.tick().await.unwrap();
    assert!(
        rig.sent().is_empty(),
        "the follower holds a young request while the primary is alive: {:#?}",
        run.tick
    );
    trace.tick(
        "the request is proved and held: it is young and the primary is alive",
        run,
    );
    rig.chain(|chain| {
        chain.primary_serves(first);
        chain.mine(1 + LAG);
    });
    let run = process.tick().await.unwrap();
    assert!(
        rig.sent().is_empty(),
        "the follower sends nothing for a request the primary served: {:#?}",
        run.tick
    );
    trace.tick("the primary served it: the follower retires it", run);

    trace.section("4. the primary is slow: the follower joins the queue");
    let second = rig.request();
    rig.chain(|chain| chain.mine(3 + LAG));
    let run = process.tick().await.unwrap();
    assert!(
        rig.sent().is_empty(),
        "the follower holds a request younger than the join delay: {:#?}",
        run.tick
    );
    trace.tick("the request is proved and held: the primary is alive", run);
    // The oldest request is over 20 seconds old, and the primary has served nothing.
    rig.chain(|chain| chain.mine(20));
    let run = process.tick().await.unwrap();
    assert_eq!(
        rig.sent().len(),
        1,
        "the follower sends a request older than the join delay: {:#?}",
        run.tick
    );
    trace.tick(
        "the request is older than the join delay: the follower sends it",
        run,
    );
    rig.settle();
    let run = process.tick().await.unwrap();
    assert!(
        rig.chain(|chain| chain.requests[&second].fulfilled),
        "the follower's transaction served the second request"
    );
    trace.tick("the receipt settles it", run);

    trace.section("5. the primary does not publish an epoch: the follower takes over");
    rig.chain(|chain| chain.mine_to(chain.epoch_start(2) + 2 + LAG));
    let run = process.tick().await.unwrap();
    trace.tick("the next epoch's snapshot is fetched", run);
    let third = rig.request();
    rig.chain(|chain| chain.mine(1 + LAG));
    let run = process.tick().await.unwrap();
    assert_eq!(
        rig.sent().len(),
        1,
        "nothing is sent for a request that waits on an unpublished epoch: {:#?}",
        run.tick
    );
    trace.tick(
        "the request is found, and waits on the unpublished epoch",
        run,
    );
    // The tick that watches the primary comes before the one that finds the request: this is the first to see work
    // waiting.
    let run = process.tick().await.unwrap();
    assert_eq!(
        rig.sent().len(),
        1,
        "the follower waits for the primary to publish the epoch: {:#?}",
        run.tick
    );
    trace.tick(
        "the follower sees work waiting; the primary has had no time yet",
        run,
    );
    // The primary's nonce has not moved for longer than the liveness window while there was work to do, and the
    // follower has seen the demand for longer than its delay, which a follower waits for before it publishes.
    rig.chain(|chain| chain.mine(21));
    let run = process.tick().await.unwrap();
    assert_eq!(
        rig.sent().len(),
        2,
        "the follower publishes the epoch once the primary counts as dead: {:#?}",
        run.tick
    );
    trace.tick(
        "the primary counts as dead: the follower publishes the epoch",
        run,
    );
    rig.settle();
    let run = process.tick().await.unwrap();
    trace.tick("the commit settles; its target block is not final yet", run);
    rig.chain(|chain| chain.mine(1 + LAG));
    let run = process.tick().await.unwrap();
    assert_eq!(
        rig.sent().len(),
        3,
        "the follower fulfills the request: {:#?}",
        run.tick
    );
    trace.tick("the request is proved and fulfilled", run);
    rig.settle();
    let run = process.tick().await.unwrap();
    assert!(
        rig.chain(|chain| chain.requests[&third].fulfilled),
        "the follower's transaction served the third request"
    );
    trace.tick("the receipt settles it", run);

    process.stop().await;
    trace.finish();
}
