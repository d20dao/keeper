//! The golden trace of a keeper configured the way Arc's keepers are. The other traces run on the settings that
//! `Config::load` defaults to; this one gives the loader the tuning of a production environment file and checks what
//! it builds, field by field, and what a keeper with those settings asks the chain: a startup, an idle tick, and a
//! batch of the largest callbacks that only fits the gas cap that production sets.
use crate::{
    config::Config,
    golden::{self, Golden, Trace, market},
    rig::Rig,
};

/// Arc Mainnet's primary keeper: the settings the environment file generator writes for it (its snapshot under
/// `scripts/tests/fixtures/keeper-env/`) with the two the operations guide adds, `FULFILL_BATCH_MAX=16` and
/// `MAX_GAS=13000000`, which the generator's snapshot has not caught up with. What belongs to one deployment (the
/// coordinator and its pins, the endpoints, the database and key files) is the rig's, and so is whether it sends.
pub(crate) const ARC_MAINNET_PRIMARY: &[(&str, &str)] = &[
    ("NATIVE_CURRENCY_SYMBOL", "USDC"),
    ("EXPLORER_URL", "https://explorer.arc.io"),
    ("WS_URLS", "wss://rpc.mainnet.arc.io"),
    ("POLL_MS", "250"),
    ("MAX_TICK_FAILURES", "5"),
    ("TICK_TIMEOUT_SECONDS", "20"),
    ("SEND_MARGIN_SECONDS", "5"),
    ("MAX_GAS", "13000000"),
    ("MAX_FEE_PER_GAS_WEI", "2000000000000"),
    ("CANCEL_MAX_FEE_PER_GAS_WEI", "2500000000000"),
    ("MAX_TX_COST_WEI", "4000000000000000000"),
    ("MIN_PRIORITY_FEE_WEI", "1000000000"),
    ("MAX_PRIORITY_FEE_WEI", "50000000000"),
    ("FEE_COVERAGE_BPS", "10000"),
    ("FULFILL_BATCH_MAX", "16"),
    ("NONCE_STUCK_SECONDS", "120"),
    ("PROGRESS_STUCK_SECONDS", "20"),
    ("RUST_LOG", "d20dao_keeper=info"),
    (
        "HEALTH_API_URL",
        "https://watchdog.invalid/v1/health/arc-mainnet",
    ),
    ("HEALTH_API_KEY", "local-test-token"),
];
/// Arc Mainnet's follower keeper: the primary's settings and its own role, with the join rule of the snapshot.
pub(crate) const ARC_MAINNET_FOLLOWER: &[(&str, &str)] = &[
    ("KEEPER_ROLE", "follower"),
    ("FOLLOWER_DELAY_SECONDS", "20"),
    ("FOLLOWER_QUEUE_JOIN", "150"),
    ("PRIMARY_LIVENESS_SECONDS", "10"),
];

const GOLDEN: Golden = Golden {
    file: "arc-0.4.1.production.trace",
    about: "The configuration and calls of keeper 0.4.1 with the settings of Arc Mainnet's primary keeper.",
    notes: &[
        "`config` lines are the fields of the configuration that Config::load builds from the environment file's settings.",
    ],
};

/// The fields of a configuration that every release has, one line a group.
pub(crate) fn digest(config: &Config) -> Vec<String> {
    vec![
        format!(
            "config role={:?} chain_id={} send={} once={}",
            config.role, config.chain_id, config.send, config.once
        ),
        format!(
            "config timing poll_ms={} idle_poll_ms={} max_tick_failures={} tick_timeout_seconds={} margin={}",
            config.poll_ms,
            config.idle_poll_ms,
            config.max_tick_failures,
            config.tick_timeout_seconds,
            config.margin
        ),
        format!(
            "config lanes max_gas={} fulfill_batch_max={} nonce_stuck_seconds={} progress_stuck_seconds={}",
            config.max_gas,
            config.fulfill_batch_max,
            config.nonce_stuck_seconds,
            config.progress_stuck_seconds
        ),
        format!(
            "config fees max_fee={} cancel_max_fee={} max_cost={} min_priority_fee={} max_priority_fee={} fee_coverage_bps={}",
            config.max_fee,
            config.cancel_max_fee,
            config.max_cost,
            config.min_priority_fee,
            config.max_priority_fee,
            config.fee_coverage_bps
        ),
        format!(
            "config reports ws_urls={:?} health_reports={}",
            config.ws_urls,
            config.telemetry.is_some()
        ),
    ]
}

#[tokio::test]
async fn a_keeper_with_the_settings_of_arc_mainnets_primary_asks_the_chain_what_0_4_1_asked() {
    let _exclusive = crate::golden::exclusive().await;
    // Arc Mainnet, chain 5042, with the loader's own checks of a production environment: HTTPS endpoints, all four
    // pins, a cancellation cap 12.5% above the fulfillment cap, and the budget for a recovery.
    let rig = Rig::new(5_042, ()).await.settings(ARC_MAINNET_PRIMARY);
    rig.chain(market);
    let mut trace = Trace::new(&GOLDEN);

    trace.section("1. the configuration");
    trace.lines(digest(&rig.config(false)));
    trace.section("2. startup, and an idle tick");
    let startup = rig.start().await;
    trace.startup("startup", startup);
    rig.chain(|chain| chain.mine_to(crate::scripted::FIRST_EPOCH_START + 5 + golden::LAG));
    let run = rig.run(true, false).await;
    trace.tick("the epoch's snapshot is fetched", run);

    // Six requests whose callbacks have the greatest gas limit the coordinator allows. Their floors together are
    // over the 3,000,000 gas that a keeper on the defaults is capped at, and fit the 13,000,000 of production.
    trace.section("3. six large requests in one batch");
    rig.chain(|chain| {
        chain.publish_epoch(1, chain.head);
        chain.mine(1 + golden::LAG);
    });
    let ids: Vec<u64> = (0..6).map(|_| rig.request_with(1_000_000)).collect();
    rig.chain(|chain| chain.mine(3 + golden::LAG));
    let run = rig.run(true, false).await;
    trace.tick("the requests are proved and sent", run);
    let batches = rig
        .sent()
        .iter()
        .filter(|sent| sent.contains("fulfillRandomnessBatch"))
        .count();
    assert_eq!(
        batches,
        1,
        "the six requests are one batch: {:?}",
        rig.sent()
    );
    rig.settle();
    let run = rig.run(true, false).await;
    assert!(
        rig.chain(|chain| ids.iter().all(|id| chain.requests[id].fulfilled)),
        "the batch serves all six requests"
    );
    trace.tick("the receipt settles them", run);
    trace.finish();
}
