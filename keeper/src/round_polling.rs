//! What a round keeper asks its RPC provider in a minute (keeper task: quota-aware polling). A provider bills every
//! JSON-RPC call, each member of a batch counted, and every message a subscription pushes. Robinhood Chain makes about 10
//! blocks a second on mainnet and 4 on its testnet, so a `newHeads` subscription alone is 240 to 600 messages a minute.
//!
//! The measurement runs the keeper's real ticks against the scripted chain, on a clock the test moves: between two ticks
//! it passes the time the run loop would wait (`events::Cadence`), on the chain (a block a second), on the round lane's
//! clock and on the worker's periodic re-checks (`Worker::pass`), so the pins, the publishing right and the finality
//! audit come due as they would. The HTTP side is counted from what the node was asked; the WebSocket side from the
//! blocks and logs the subscription follows, at Robinhood Chain's block rates. Each scenario runs as the keeper of
//! `rh/keeper-round-k3` paced it (an idle heartbeat of 5 seconds, every block and every log of the coordinator followed),
//! as `e828de5` paced it (a 10-second idle heartbeat, the pins every 15 seconds and the publishing right every 30; the
//! coordinator's requests, role changes and upgrades, and blocks only while work is open), and as it is paced now
//! (IDLE_HEARTBEAT_SECONDS, 30 by default, and while idle with a live subscription the pins and the publishing right every
//! 120 seconds). The ticks themselves are the same code in all three.
use crate::{
    events::{Cadence, IDLE_HEARTBEAT, Kind, Wait, round_topics},
    rig::Rig,
    round_mode::{lane_clock, round_rig},
};
use alloy_primitives::{Address, B256};
use std::collections::BTreeMap;
use std::time::Duration;

/// Blocks a second on Robinhood Chain: testnet, mainnet.
const BLOCK_RATES: [(&str, u64); 2] = [("testnet", 4), ("mainnet", 10)];
/// The measured minute starts after this much time: the first tick's checks and audit are behind it, as for a keeper that
/// has run for a while.
const WARM_UP_MS: u64 = 90_000;
const MINUTE_MS: u64 = 60_000;
/// How far the scripted chain's finalized head trails its latest: short enough for the marks of a fulfillment (its
/// signature and its receipt) to come below it within the minute, so the audit reads them as it would.
const FINALIZED_LAG: u64 = 30;

/// How a scenario is paced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pacing {
    /// The keeper of `rh/keeper-round-k3`: a 5-second idle heartbeat, every block and every log followed.
    Released,
    /// The keeper of `e828de5`, with IDLE_HEARTBEAT_SECONDS of this many seconds: requests, role changes and upgrades
    /// followed, and blocks while work is open; the pins and the publishing right re-checked as when no subscription is
    /// live.
    Demand(u64),
    /// Now, with IDLE_HEARTBEAT_SECONDS of this many seconds: followed as `Demand`, and while idle with the subscription
    /// live the pins and the publishing right re-checked every 120 seconds.
    Now(u64),
}
impl Pacing {
    /// Whether the subscription follows only the coordinator's requests, role changes and upgrades, and blocks while work
    /// is open.
    fn on_demand(self) -> bool {
        !matches!(self, Pacing::Released)
    }
}
#[derive(Clone, Copy, Debug)]
struct Scenario {
    subscription: bool,
    pacing: Pacing,
    /// Seconds into the measured minutes at which a request is made, for a keeper that serves one.
    request_at: Option<u64>,
    /// How many minutes are measured: two cover one round of the 120-second re-checks of a quiet keeper.
    minutes: u64,
}
/// What the measured minutes asked of the provider, in all.
#[derive(Debug, Default)]
struct Minute {
    /// How many minutes were measured.
    minutes: u64,
    ticks: u64,
    /// HTTP requests: a batch is one.
    http: u64,
    /// JSON-RPC calls over HTTP: each member of a batch is one.
    calls: u64,
    /// The calls by what they ask.
    by_kind: BTreeMap<String, u64>,
    /// Milliseconds of the minute the subscription followed new blocks.
    blocks_ms: u64,
    /// Logs of the coordinator the subscription pushed.
    logs: u64,
    /// `eth_subscribe` and `eth_unsubscribe` of new blocks.
    resubscriptions: u64,
    /// Whether the request was served within the minute.
    served: bool,
}
impl Minute {
    /// The WebSocket messages of the minute at `rate` blocks a second.
    fn pushes(&self, rate: u64) -> u64 {
        self.blocks_ms * rate / 1_000 + self.logs + self.resubscriptions
    }
    /// Every call and pushed message of the minute at `rate` blocks a second.
    fn total(&self, rate: u64) -> u64 {
        self.calls + self.pushes(rate)
    }
    /// JSON-RPC calls a minute, on average over the measured minutes.
    fn calls_a_minute(&self) -> f64 {
        self.calls as f64 / self.minutes as f64
    }
}

/// The HTTP requests and JSON-RPC calls of a tick's lines (`scripted::render`), and the calls by what they ask. A drand
/// relay is not the RPC provider, and is not counted.
fn tally(lines: &[String], minute: &mut Minute) {
    let mut batch = false;
    for line in lines {
        let line = line.trim();
        // The label of another endpoint, `[2] `.
        let line = match line.split_once("] ") {
            Some((label, rest)) if label.starts_with('[') => rest,
            _ => line,
        };
        match line {
            "{" | "}" => continue,
            "]" => {
                batch = false;
                continue;
            }
            "batch [" => {
                batch = true;
                minute.http += 1;
                continue;
            }
            _ => {}
        }
        if line.starts_with("relay") {
            continue;
        }
        minute.calls += 1;
        if !batch {
            minute.http += 1;
        }
        *minute.by_kind.entry(kind(line)).or_default() += 1;
    }
}
/// What a call asks, without its block or arguments: `eth_call nextRequestId`, `eth_getBlockByNumber latest`, the hash of
/// a numbered block, `eth_getCode`.
fn kind(line: &str) -> String {
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.first().copied() {
        Some("eth_call") => {
            let name = words
                .iter()
                .position(|word| word.starts_with("0x") && word.len() == 10)
                .and_then(|at| words.get(at + 1))
                .copied()
                .unwrap_or("?");
            format!("eth_call {name}")
        }
        Some("eth_getBlockByNumber") => match words.get(1).copied() {
            Some(tag @ ("latest" | "finalized" | "safe")) => format!("eth_getBlockByNumber {tag}"),
            _ => "eth_getBlockByNumber <number>".into(),
        },
        Some(method) => method.to_owned(),
        None => String::new(),
    }
}

/// The minutes of a round keeper paced as `scenario` says, after the warm-up.
async fn minute(scenario: Scenario) -> Minute {
    let rig: Rig = round_rig().await.unheld();
    rig.chain(|chain| {
        chain.l1_gas = |_| 20_000;
        chain.finalized_lag = FINALIZED_LAG;
        chain.safe_lag = FINALIZED_LAG - 10;
    });
    let process = rig.process(true, false).await;
    process
        .worker()
        .signals()
        .set_live(scenario.subscription && matches!(scenario.pacing, Pacing::Now(_)));
    let clock = lane_clock(&process);
    let coordinator = rig.chain(|chain| chain.coordinator);
    let start = rig.chain(|chain| chain.head);
    let cadence = Cadence {
        poll: Duration::from_millis(1_000),
        // IDLE_POLL_MS: the larger of POLL_MS and a second.
        idle_poll: Duration::from_millis(1_000),
        heartbeat: match scenario.pacing {
            Pacing::Released => IDLE_HEARTBEAT,
            Pacing::Demand(seconds) | Pacing::Now(seconds) => Duration::from_secs(seconds),
        },
    };
    let request_at = scenario.request_at.map(|at| WARM_UP_MS + at * 1_000);
    let end = WARM_UP_MS + MINUTE_MS * scenario.minutes;
    let mut minute = Minute {
        minutes: scenario.minutes,
        ..Minute::default()
    };
    let mut request = None;
    let mut following = false;
    let mut t = 0u64;
    while t < end {
        // The chain makes a block a second, and the round lane's clock is the chain's.
        rig.chain(|chain| {
            while chain.head < start + t / 1_000 {
                chain.include();
            }
        });
        clock.set(rig.chain(|chain| chain.time(chain.head)) * 1_000 + t % 1_000);
        if request.is_none() && request_at.is_some_and(|at| t >= at) {
            request = Some(rig.request());
        }
        let run = process.tick().await.unwrap();
        let counted = t >= WARM_UP_MS;
        if counted {
            minute.ticks += 1;
            tally(&run.tick, &mut minute);
        }
        let busy = process.worker().open_work().await.unwrap();
        // The blocks the subscription follows until the next tick.
        let blocks = scenario.subscription
            && match scenario.pacing {
                Pacing::Released => true,
                Pacing::Demand(_) | Pacing::Now(_) => busy,
            };
        if counted && scenario.subscription && scenario.pacing.on_demand() && blocks != following {
            minute.resubscriptions += 1;
        }
        following = blocks;
        let mut wait = match cadence.wait(scenario.subscription, busy) {
            Wait::Sleep(interval) => interval,
            // Blocks arrive several times a second: the tick follows the spacing.
            Wait::Blocks { spacing, .. } => spacing,
            Wait::Event(heartbeat) => heartbeat,
        }
        .as_millis() as u64;
        // A pushed request wakes the keeper at once.
        if let Some(at) = request_at
            && request.is_none()
            && scenario.subscription
            && !busy
            && at > t
        {
            wait = wait.min(at - t);
        }
        if blocks {
            let (from, to) = (t.max(WARM_UP_MS), (t + wait).min(end));
            minute.blocks_ms += to.saturating_sub(from);
        }
        process.worker().pass(Duration::from_millis(wait));
        t += wait;
    }
    // The coordinator's logs of the minute that the subscription pushed: every one as released, the requests, role
    // changes and upgrades now.
    let topics = round_topics();
    let (first, last) = (start + WARM_UP_MS / 1_000, start + end / 1_000);
    minute.logs = rig.chain(|chain| {
        chain
            .logs
            .iter()
            .filter(|log| {
                let block = crate::rpc::quantity(&log["blockNumber"]).unwrap_or(0);
                let address: Option<Address> = serde_json::from_value(log["address"].clone()).ok();
                let topic: Option<B256> = serde_json::from_value(log["topics"][0].clone()).ok();
                scenario.subscription
                    && (first..last).contains(&block)
                    && address == Some(coordinator)
                    && (!scenario.pacing.on_demand()
                        || topic.is_some_and(|topic| topics.contains(&topic)))
            })
            .count() as u64
    });
    if let Some(id) = request {
        let pool = rig.journal().await;
        let state: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id=?")
            .bind(id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        pool.close().await;
        minute.served = state == "served";
    }
    process.stop().await;
    minute
}

/// The minutes the report quotes: an idle round keeper without a subscription and with one, and one that serves a
/// request, each as released, as of `e828de5` and now. Without a subscription nothing changed: IDLE_POLL_MS polls as it
/// did. A quiet keeper now is measured over two minutes, a whole round of its 120-second re-checks.
#[tokio::test]
async fn an_idle_round_keeper_with_a_live_subscription_asks_its_provider_about_ten_calls_a_minute()
{
    let scenario = |subscription, pacing, request_at, minutes| Scenario {
        subscription,
        pacing,
        request_at,
        minutes,
    };
    let now = Pacing::Now(crate::config::IDLE_HEARTBEAT_DEFAULT);
    let before = Pacing::Demand(10);
    let polling = minute(scenario(false, now, None, 1)).await;
    let released = minute(scenario(true, Pacing::Released, None, 1)).await;
    let previous = minute(scenario(true, before, None, 1)).await;
    let idle = minute(scenario(true, now, None, 2)).await;
    let released_serving = minute(scenario(true, Pacing::Released, Some(5), 1)).await;
    let previous_serving = minute(scenario(true, before, Some(5), 1)).await;
    let serving = minute(scenario(true, now, Some(5), 1)).await;
    let rows = [
        ("idle, no subscription (released = now)", &polling),
        ("idle, subscription, released", &released),
        ("idle, subscription, e828de5", &previous),
        ("idle, subscription, now", &idle),
        (
            "one request served, subscription, released",
            &released_serving,
        ),
        (
            "one request served, subscription, e828de5",
            &previous_serving,
        ),
        ("one request served, subscription, now", &serving),
    ];
    for (name, minute) in rows {
        let (testnet, mainnet) = (
            minute.total(BLOCK_RATES[0].1),
            minute.total(BLOCK_RATES[1].1),
        );
        println!(
            "{name}: minutes {} | ticks {} | HTTP requests {} | JSON-RPC calls {} ({:.1} a minute) | pushed (testnet/mainnet) {}/{} | total {testnet}/{mainnet} = {:.2}/{:.2} a second",
            minute.minutes,
            minute.ticks,
            minute.http,
            minute.calls,
            minute.calls_a_minute(),
            minute.pushes(BLOCK_RATES[0].1),
            minute.pushes(BLOCK_RATES[1].1),
            testnet as f64 / (60.0 * minute.minutes as f64),
            mainnet as f64 / (60.0 * minute.minutes as f64),
        );
        println!("    calls {:?}", minute.by_kind);
    }
    // Every serving keeper served its request within the minute: a pushed request wakes a quiet keeper at once.
    assert!(released_serving.served && previous_serving.served && serving.served);
    // As of e828de5 an idle minute with a subscription asked 31 calls: a tick every 10 seconds (the decision head with the
    // block the last tick decided on, and nextRequestId), the finalized header every 30 seconds, the pins every 15 and
    // the publishing right every 30.
    assert_eq!((previous.ticks, previous.http, previous.calls), (6, 19, 31));
    // Now, over two minutes: no block followed and nothing pushed; a tick every 30 seconds asks its three calls, the
    // audit reads the finalized header every 30 seconds, and the pins (three calls) and the publishing right (one) are
    // re-checked once. With the scripted chain's finalized head 30 blocks behind, the head mark of each tick comes below
    // it by the next one, and the audit reads its hash too; on Robinhood Chain finality trails by minutes and the next
    // tick rewrites the head mark first, so those four reads are the scripted chain's.
    assert_eq!((idle.blocks_ms, idle.pushes(10), idle.ticks), (0, 0, 4));
    for kind in ["eth_call nextRequestId", "eth_getBlockByNumber latest"] {
        assert_eq!(idle.by_kind[kind], idle.ticks, "{kind}");
    }
    assert_eq!(
        idle.by_kind["eth_getBlockByNumber <number>"],
        2 * idle.ticks
    );
    assert_eq!(idle.by_kind["eth_getBlockByNumber finalized"], 4);
    assert_eq!(idle.by_kind["eth_call keeper"], 1);
    assert_eq!(
        idle.by_kind["eth_getStorageAt"] + idle.by_kind["eth_getCode"],
        3
    );
    assert_eq!((idle.http, idle.calls), (18, 24));
    let head_marks = idle.ticks;
    assert_eq!((idle.calls - head_marks) / idle.minutes, 10);
    // As released the same minute asked nine times as much on testnet and twenty times as much on mainnet, almost all of
    // it pushed blocks; serving a request, three and five times as much.
    for (name, rate, idle_factor, serving_factor) in [("testnet", 4, 9, 3), ("mainnet", 10, 20, 5)]
    {
        assert!(
            released.total(rate) * idle.minutes >= idle_factor * idle.total(rate),
            "{name}"
        );
        assert!(
            released_serving.total(rate) >= serving_factor * serving.total(rate),
            "{name}"
        );
    }
    assert!(
        serving.calls < previous_serving.calls && previous_serving.calls < released_serving.calls
    );
    // Without a subscription the keeper polls every second, as it always has.
    assert_eq!(polling.ticks, 60);
    // Serving a request follows blocks only while the request is open: subscribed once, unsubscribed once.
    assert_eq!(serving.resubscriptions, 2);
    assert!(
        serving.blocks_ms > 0 && serving.blocks_ms < MINUTE_MS / 4,
        "{serving:?}"
    );
    // Of the coordinator's logs, only the request was pushed.
    assert_eq!(serving.logs, 1);
    assert!(released_serving.logs > 1);
}

/// A round keeper with nothing open and a live subscription re-checks its runtime pins and its publishing right every 120
/// seconds instead of 15 and 30; an upgrade or role event has it check at once, and so does open work or a subscription
/// that is not live.
#[tokio::test]
async fn a_quiet_round_keeper_rechecks_its_pins_and_its_right_every_two_minutes_and_at_once_on_an_event()
 {
    let rig: Rig = round_rig().await.unheld();
    let process = rig.process(true, false).await;
    let signals = process.worker().signals();
    signals.set_live(true);
    // The pins (their storage slot) and the publishing right (`keeper`) a tick asks.
    let rechecks = |lines: &[String]| {
        let mut minute = Minute::default();
        tally(lines, &mut minute);
        let asked = |kind: &str| minute.by_kind.get(kind).copied().unwrap_or(0);
        (asked("eth_getStorageAt"), asked("eth_call keeper"))
    };
    let tick = || async { rechecks(&process.tick().await.unwrap().tick) };
    let pass = |seconds| process.worker().pass(Duration::from_secs(seconds));
    tick().await;
    pass(100);
    assert_eq!(tick().await, (0, 0), "quiet for 100 seconds");
    pass(20);
    assert_eq!(tick().await, (1, 1), "quiet for 120 seconds");
    signals.push(Kind::Upgrade, 0);
    assert_eq!(tick().await, (1, 0), "an upgrade");
    signals.push(Kind::Role, 0);
    assert_eq!(tick().await, (0, 1), "a role change");
    // Without a live subscription the pins wait 15 seconds and the publishing right 30, as they always have.
    signals.set_live(false);
    pass(20);
    assert_eq!(
        tick().await,
        (1, 0),
        "20 seconds without a live subscription"
    );
    pass(10);
    assert_eq!(
        tick().await,
        (0, 1),
        "30 seconds without a live subscription"
    );
    // Open work checks the pins after 15 seconds and the publishing right after 2, subscription or not.
    signals.set_live(true);
    pass(20);
    let head = rig.chain(|chain| chain.head);
    signals.push(Kind::Work, head + 1);
    assert_eq!(tick().await, (1, 1), "open work");
    process.stop().await;
}
