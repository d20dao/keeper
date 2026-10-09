//! The golden trace of the keeper's chain event subscription: what it sends a WebSocket endpoint, what its pushed heads
//! and logs do to the signals that wake the keeper, and what it asks over HTTP when a dropped connection comes back
//! (a head, and the logs of the blocks it missed, in ranges of 500 blocks, for a gap of up to 5,000). The subscription
//! runs on its own task, so each section waits for its effect and records the endpoint's lines, the HTTP lines and the
//! signals one after another.
use crate::{
    events::{self, Signals},
    golden::{CHAIN_ID, Golden, Trace, market},
    rig::Rig,
    rpc::Rpc,
    scripted::render,
    scripted_ws::Socket,
};
use alloy_primitives::Address;
use std::{sync::Arc, time::Duration};

const GOLDEN: Golden = Golden {
    file: "arc-0.4.1.websocket.trace",
    about: "The chain event subscription of keeper 0.4.1: its handshake, its signals, and its backfill after a reconnect.",
    notes: &[
        "`name session n:` lines are the scripted WebSocket endpoint: `<-` is what the keeper sent it. `signals` lines are",
        "what the subscription has told the keeper: whether it is live, the upgrade and role counters, the block of the",
        "last work event and the last head pushed.",
    ],
};
const REQUESTED: &str =
    "RandomnessRequested(uint256,address,bytes32,bytes32,uint64,uint32,uint256,address,uint64)";
const UPGRADED: &str = "Upgraded(address)";
const ROLE: &str = "BackupCommitterSet(address,bool)";

async fn eventually(what: &str, condition: impl Fn() -> bool) {
    let end = tokio::time::Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(tokio::time::Instant::now() < end, "{what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
/// What the subscription has told the keeper.
fn signals(signals: &Signals, heads: &tokio::sync::watch::Receiver<u64>) -> String {
    format!(
        "signals live={} upgrades={} roles={} activity={} head={}",
        signals.live(),
        signals.upgrades(),
        signals.roles(),
        signals.activity(),
        *heads.borrow()
    )
}

#[tokio::test]
async fn a_subscription_that_drops_reconnects_and_backfills_as_0_4_1_did() {
    let _exclusive = crate::golden::exclusive().await;
    let rig = Rig::new(CHAIN_ID, ()).await;
    rig.chain(market);
    rig.node.hold(Duration::ZERO);
    let (coordinator, registry) = rig.chain(|chain| (chain.coordinator, chain.registry));
    let foreign = Address::repeat_byte(0x99);
    // The first endpoint serves another chain, which the keeper uses once and never again.
    let other = Socket::start("other chain", rig.node.chain.clone(), 1).await;
    let endpoint = Socket::start("endpoint", rig.node.chain.clone(), CHAIN_ID).await;
    let live = Signals::new();
    let heads = live.heads();
    let _subscription = events::spawn(
        vec![other.url.clone(), endpoint.url.clone()],
        Rpc::new(vec![rig.node.url.clone()]).unwrap(),
        CHAIN_ID,
        [coordinator, registry],
        live.clone(),
    )
    .unwrap();
    let mut trace = Trace::new(&GOLDEN);
    let record =
        |trace: &mut Trace, live: &Arc<Signals>, heads: &tokio::sync::watch::Receiver<u64>| {
            trace.lines(other.take());
            trace.lines(endpoint.take());
            trace.lines(
                render(&rig.node.take())
                    .into_iter()
                    .map(|line| format!("http {line}")),
            );
            trace.say(signals(live, heads));
        };

    trace.section("1. the subscription connects");
    eventually("the subscription is live", || live.live()).await;
    record(&mut trace, &live, &heads);

    trace.section("2. heads and logs are pushed");
    endpoint.push_head(1_010);
    endpoint.push_log(coordinator, UPGRADED, 1_010);
    // A log of another contract, pushed by an endpoint that ignores the address filter, is not work.
    endpoint.push_log(foreign, UPGRADED, 1_020);
    endpoint.push_log(coordinator, REQUESTED, 1_011);
    endpoint.push_head(1_011);
    eventually("the first session delivered its events", || {
        live.activity() == 1_011 && *heads.borrow() == 1_011
    })
    .await;
    record(&mut trace, &live, &heads);

    trace.section("3. the connection drops and blocks pass");
    endpoint.drop_connection();
    eventually("the subscription noticed", || !live.live()).await;
    rig.chain(|chain| {
        chain.mine_to(1_040);
        chain.emit(
            registry,
            1_030,
            vec![alloy_primitives::keccak256(ROLE)],
            vec![],
        );
        chain.emit(
            foreign,
            1_035,
            vec![alloy_primitives::keccak256(ROLE)],
            vec![],
        );
    });
    record(&mut trace, &live, &heads);

    trace.section("4. it reconnects, re-checks everything and backfills the blocks it missed");
    eventually("the subscription reconnected and backfilled", || {
        live.live() && live.activity() == 1_030
    })
    .await;
    record(&mut trace, &live, &heads);
    endpoint.push_head(1_041);
    eventually("the head arrived", || *heads.borrow() == 1_041).await;
    trace.say(signals(&live, &heads));

    trace.section("5. a gap of 5,000 blocks, the longest there is, is backfilled in ranges of 500");
    endpoint.drop_connection();
    eventually("the subscription noticed", || !live.live()).await;
    rig.chain(|chain| chain.mine_to(1_041 + 5_000));
    let (upgrades, roles) = (live.upgrades(), live.roles());
    eventually("the subscription reconnected", || {
        live.live() && live.upgrades() > upgrades && live.roles() > roles
    })
    .await;
    // A head, and then the logs of the 5,000 blocks that the last head the subscription saw does not reach: ten
    // ranges, one request after the other.
    eventually("the backfill read all ten ranges", || {
        rig.node.asked() >= 11
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        rig.node.asked(),
        11,
        "a gap of 5,000 blocks is read as a head and ten ranges of 500, and no more"
    );
    record(&mut trace, &live, &heads);

    trace.section("6. a gap of 5,001 blocks is not backfilled");
    endpoint.drop_connection();
    eventually("the subscription noticed", || !live.live()).await;
    rig.chain(|chain| chain.mine_to(1_041 + 5_001));
    let (upgrades, roles) = (live.upgrades(), live.roles());
    eventually("the subscription reconnected", || {
        live.live() && live.upgrades() > upgrades && live.roles() > roles
    })
    .await;
    // The backfill is a head and, for a gap that long, nothing more: the re-check of everything stands in for it.
    eventually("the backfill asked for the head", || rig.node.asked() > 0).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        rig.node.asked(),
        1,
        "a gap of more than 5,000 blocks is read as a head and nothing more"
    );
    record(&mut trace, &live, &heads);
    trace.finish();
}
