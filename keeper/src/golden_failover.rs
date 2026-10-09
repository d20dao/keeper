//! The golden trace of a keeper with two RPC endpoints and two drand relays, one of each failing. The other traces run
//! one endpoint and one relay; here a call that goes to the endpoint that is down is recorded, answered with an HTTP
//! error, and asked again of the other, and a transaction one endpoint refuses is sent to the other.
use crate::{
    golden::{CHAIN_ID, Golden, LAG, Trace, market},
    rig::Rig,
    scripted::{FIRST_EPOCH_START, Mode},
};

const GOLDEN: Golden = Golden {
    file: "arc-0.4.1.failover.trace",
    about: "The calls of keeper 0.4.1 with two RPC endpoints and two drand relays, one of each failing.",
    notes: &[
        "A call to an endpoint that is down is recorded as asked and answered with an HTTP error; `relay[1]` is down in section 2.",
    ],
};

#[tokio::test]
async fn a_keeper_with_two_endpoints_and_two_relays_fails_over_as_0_4_1_did() {
    let _exclusive = crate::golden::exclusive().await;
    let mut rig = Rig::new(CHAIN_ID, ()).await;
    rig.chain(market);
    let first = rig.endpoints()[0].clone();
    let second = rig.add_endpoint().await;
    let first_relay = rig.relays()[0].clone();
    rig.add_relay().await;
    let mut trace = Trace::new(&GOLDEN);

    trace.section("1. both endpoints and both relays answer");
    trace.startup("startup verifies both endpoints", rig.start().await);
    rig.chain(|chain| chain.mine_to(FIRST_EPOCH_START + 5 + LAG));
    let run = rig.run(true, false).await;
    trace.tick("the epoch's snapshot is asked of both relays", run);

    trace.section("2. the first endpoint and the first relay are down");
    first.set(Mode::Down);
    first_relay.set(Mode::Down);
    rig.chain(|chain| chain.mine_to(chain.epoch_start(2) + 2 + LAG));
    trace.startup("startup passes over the first endpoint", rig.start().await);
    let run = rig.run(true, false).await;
    trace.tick(
        "the next epoch's snapshot comes from the second relay, over the second endpoint",
        run,
    );

    trace.section("3. a request is served over the second endpoint");
    rig.chain(|chain| {
        chain.publish_epoch(2, chain.head);
        chain.mine(1 + LAG);
    });
    let id = rig.request();
    rig.chain(|chain| chain.mine(3 + LAG));
    let run = rig.run(true, false).await;
    assert_eq!(
        rig.sent().len(),
        1,
        "the request is sent once: {:#?}",
        run.tick
    );
    trace.tick("the request is proved and fulfilled", run);
    rig.settle();
    let run = rig.run(true, false).await;
    assert!(
        rig.chain(|chain| chain.requests[&id].fulfilled),
        "the request is served"
    );
    trace.tick("the receipt settles it", run);

    trace.section("4. the first endpoint refuses a transaction");
    first.set(Mode::Refusing(
        "eth_sendRawTransaction",
        "transaction underpriced",
    ));
    first_relay.set(Mode::Up);
    let id = rig.request();
    rig.chain(|chain| chain.mine(3 + LAG));
    let process = rig.process(true, false).await;
    trace.startup(
        "startup verifies both endpoints again",
        process.startup.clone(),
    );
    let run = process.tick().await.unwrap();
    // The refused transaction never reached the chain: only the earlier request's is there.
    assert_eq!(
        rig.sent().len(),
        1,
        "the refused transaction is not on the chain: {:?}",
        rig.sent()
    );
    trace.tick(
        "the request is proved and the first endpoint refuses the fulfillment",
        run,
    );
    // The seconds pass after which a transaction that was not acknowledged is sent again: two, counted from the
    // latest block's time, which the finalized head the keeper reads reaches `LAG` blocks later.
    rig.chain(|chain| chain.mine(2 + LAG));
    let run = process.tick().await.unwrap();
    trace.tick("the fulfillment is sent again, to the second endpoint", run);
    assert_eq!(
        rig.sent().len(),
        2,
        "the fulfillment is sent again, now to the second endpoint: {:?}",
        rig.sent()
    );
    process.stop().await;
    rig.settle();
    let run = rig.run(true, false).await;
    assert!(
        rig.chain(|chain| chain.requests[&id].fulfilled),
        "the request is served"
    );
    trace.tick("the receipt settles it", run);
    second.set(Mode::Up);
    trace.finish();
}
