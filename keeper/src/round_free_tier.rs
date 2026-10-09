//! A round keeper on free RPC plans (Robinhood testnet, 2026-10-07). A burst of requests had every endpoint fail the same
//! `eth_call` batch at once: one free plan rate limited it (HTTP 429), another refused a batch of more than three calls
//! (HTTP 500), and the public endpoint answered -32000. The primary reached MAX_TICK_FAILURES within two seconds and
//! exited twice. Now a batch refused for its size is asked again in smaller chunks on the same endpoint, and a tick that
//! failed only on such provider errors is deferred and not counted.
use crate::{
    rig::{Process, Rig},
    round_mode::{lane_clock, round_rig},
    scripted::Mode,
    worker::{FailedTick, ProviderOutage, tick_failure_ends_the_run},
};
use std::time::Duration;

/// MAX_TICK_FAILURES by default.
const MAX_TICK_FAILURES: u64 = 5;

/// A round keeper's rig on which every payload has an L1 component.
async fn free_tier_rig() -> Rig {
    let rig = round_rig().await.unheld();
    rig.chain(|chain| chain.l1_gas = |_| 20_000);
    rig
}
/// Request `id`'s state in the journal a run left.
fn state(journal: &str, id: u64) -> Option<String> {
    let jobs = journal
        .lines()
        .next()?
        .strip_prefix("jobs [")?
        .strip_suffix(']')?;
    jobs.split(' ')
        .find_map(|job| job.strip_prefix(&format!("{id}=")))
        .map(str::to_owned)
}
/// Tick, a block a second with the keeper's clock on the chain's, until every request of `ids` is served: at most `ticks`
/// ticks. The lines of every tick, and whether they were served.
async fn serve(rig: &Rig, process: &Process<'_>, ids: &[u64], ticks: usize) -> (Vec<String>, bool) {
    let clock = lane_clock(process);
    let mut lines = Vec::new();
    for _ in 0..ticks {
        rig.chain(|chain| chain.include());
        clock.set(rig.chain(|chain| chain.time(chain.head)) * 1_000);
        let run = process.tick().await.unwrap();
        lines.extend(run.tick);
        if ids
            .iter()
            .all(|id| state(&run.journal, *id).as_deref() == Some("served"))
        {
            return (lines, true);
        }
    }
    (lines, false)
}
/// The JSON-RPC batches of a tick's lines, by their number of calls.
fn batch_sizes(lines: &[String]) -> Vec<usize> {
    let mut sizes = Vec::new();
    let mut open = None;
    for line in lines {
        let line = line.trim();
        if line.ends_with("batch [") {
            open = Some(0);
        } else if line == "]" {
            sizes.extend(open.take());
        } else if let Some(size) = open.as_mut() {
            *size += 1;
        }
    }
    sizes
}

#[tokio::test]
async fn a_plan_that_refuses_batches_of_more_than_three_serves_a_burst_in_chunks_without_a_failed_tick()
 {
    let rig = free_tier_rig().await;
    rig.endpoints()[0].set(Mode::BatchLimit(3));
    let process = rig.process(true, false).await;
    // The burst of the live case: twelve requests, which discovery reads in one batch.
    let ids: Vec<u64> = (0..12).map(|_| rig.request()).collect();
    // Every tick runs: the batches the endpoint refused were asked again in chunks it takes.
    let (lines, served) = serve(&rig, &process, &ids, 90).await;
    assert!(served, "{lines:#?}");
    let sizes = batch_sizes(&lines);
    // Discovery's batch of twelve was refused, and its half; the endpoint answered chunks of three, and no batch of more
    // than three was asked again.
    let refused: Vec<usize> = sizes.iter().copied().filter(|size| *size > 3).collect();
    assert_eq!(refused, [12, 6], "{sizes:?}");
    let last = sizes.iter().rposition(|size| *size > 3).unwrap();
    assert_eq!(sizes[last + 1], 3, "{sizes:?}");
    // The refusal was no failure of the endpoint: it is not cooling down.
    assert!(!process.worker().rpc.cooling(0));
    process.stop().await;
}

#[tokio::test]
async fn three_endpoints_failing_every_batch_for_three_seconds_defer_the_ticks_and_the_request_is_served()
 {
    let mut rig = free_tier_rig().await;
    let limited = rig.endpoints()[0].clone();
    let refusing = rig.add_endpoint().await;
    let busy = rig.add_endpoint().await;
    let process = rig.process(true, false).await;
    let id = rig.request();
    // The burst: a free plan rate limits, another refuses everything with HTTP 500, and the public endpoint answers
    // -32000 to every eth_call. Ticks follow at POLL_MS 250 for three seconds, as the run loop's back-off would not.
    limited.set(Mode::RateLimited);
    refusing.set(Mode::Down);
    busy.set(Mode::Refusing("eth_call", "server busy"));
    let stuck = Duration::from_secs(2);
    let mut outage = ProviderOutage::default();
    let (mut failures, mut deferred, mut reported) = (0u64, 0u32, 0);
    let began = tokio::time::Instant::now();
    while began.elapsed() < Duration::from_secs(3) {
        rig.chain(|chain| chain.include());
        let error = match process.tick().await {
            Ok(_) => panic!("a tick ran while every endpoint failed"),
            Err(error) => error,
        };
        match FailedTick::of(process.worker().coordinator_kind(), &error) {
            FailedTick::ProviderUnavailable => {
                let deferral = outage.defer(stuck);
                deferred = deferral.attempt;
                if deferral.report {
                    reported += 1;
                    crate::health::rpc_unavailable(&process.worker().journal, true)
                        .await
                        .unwrap();
                }
            }
            FailedTick::RateLimited => panic!("not every endpoint was rate limiting: {error:#}"),
            FailedTick::Counted => failures += 1,
        }
        assert!(
            !tick_failure_ends_the_run(false, false, failures, MAX_TICK_FAILURES),
            "{error:#}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    // More failed ticks than MAX_TICK_FAILURES, none of them counted; reported once, after PROGRESS_STUCK_SECONDS.
    assert!(u64::from(deferred) > MAX_TICK_FAILURES, "{deferred}");
    assert_eq!((failures, reported), (0, 1));
    assert!(outage.reported());
    let health = |journal: &str| journal.lines().last().unwrap().to_owned();
    let pool = rig.journal().await;
    let status: String = sqlx::query_scalar("SELECT value FROM meta WHERE key='health:status'")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert!(status.contains("rpc_unavailable"), "{status}");
    // The endpoints recover: the request is served, and the next tick's observation is without the fault.
    for endpoint in [&limited, &refusing, &busy] {
        endpoint.set(Mode::Up);
    }
    // The rate-limited endpoint's back-off ends.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (lines, served) = serve(&rig, &process, &[id], 60).await;
    assert!(served, "{lines:#?}");
    assert_eq!(outage.end(), Some(deferred));
    let run = process.tick().await.unwrap();
    assert!(
        !health(&run.journal).contains("rpc_unavailable"),
        "{}",
        run.journal
    );
    process.stop().await;
}
