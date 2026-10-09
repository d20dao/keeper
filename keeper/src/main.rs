use alloy_primitives::U256;
use anyhow::{Context, Result};
use d20dao_keeper::{config::Config, prover, worker::Worker};
use fs2::FileExt;
use std::{fs::OpenOptions, path::Path, process::ExitCode};

/// Startup attempts while an approved upgrade propagates across the RPC endpoints, one second apart; a disagreement
/// that outlasts them fails startup as any disagreement always has.
const UPGRADE_SETTLE_ATTEMPTS: u32 = 10;

#[cfg(unix)]
struct Shutdown {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}
#[cfg(unix)]
impl Shutdown {
    fn new() -> Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }
    async fn wait(&mut self) -> Result<()> {
        tokio::select! {
            _ = self.interrupt.recv() => {},
            _ = self.terminate.recv() => {},
        }
        Ok(())
    }
}
#[cfg(not(unix))]
struct Shutdown;
#[cfg(not(unix))]
impl Shutdown {
    fn new() -> Result<Self> {
        Ok(Self)
    }
    async fn wait(&mut self) -> Result<()> {
        Ok(tokio::signal::ctrl_c().await?)
    }
}

/// Pause before the n-th consecutive attempt that every RPC endpoint rate limited: 1 s doubling to 30 s, jittered
/// so keepers sharing endpoints do not return together.
fn rate_limit_pause(attempt: u32) -> std::time::Duration {
    let base = std::time::Duration::from_secs(1u64 << attempt.saturating_sub(1).min(5))
        .min(std::time::Duration::from_secs(30));
    let spread = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos())
        % 1000;
    base / 2 + base.mul_f64(f64::from(spread) / 2000.0)
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("migrate") => {
            let (source, mode) = d20dao_keeper::migration::parse_args(&args[2..])?;
            let cfg = Config::load(true)?;
            d20dao_keeper::migration::run(cfg, &source, mode).await?;
        }
        Some("prove") => {
            let seed: U256 = args.get(2).context("prove requires seed")?.parse()?;
            let key = prover::read_key(Path::new(args.get(3).context("prove requires key file")?))?;
            println!("{}", serde_json::to_string(&prover::prove(seed, &key)?)?);
        }
        Some("public-key") => {
            let key = prover::read_key(Path::new(
                args.get(2).context("public-key requires key file")?,
            ))?;
            println!("{}", serde_json::to_string(&prover::public_key(&key))?);
        }
        Some("health") => {
            let db = args
                .windows(2)
                .find(|a| a[0] == "--db")
                .map(|a| a[1].clone())
                .or_else(|| std::env::var("KEEPER_DB").ok())
                .context("health requires --db <path> or KEEPER_DB")?;
            let max_age: u64 = args
                .windows(2)
                .find(|a| a[0] == "--max-age")
                .map(|a| a[1].as_str())
                .unwrap_or("30")
                .parse()?;
            let mut status = d20dao_keeper::health::read(Path::new(&db)).await?;
            d20dao_keeper::health::check_freshness(
                &mut status,
                d20dao_keeper::health::now()?,
                max_age,
            );
            println!("{}", serde_json::to_string(&status)?);
            d20dao_keeper::health::require_healthy(&status)?;
        }
        Some("sweep") => {
            let flag = |name: &str| args.windows(2).find(|a| a[0] == name).map(|a| a[1].clone());
            let db = flag("--db")
                .or_else(|| std::env::var("KEEPER_DB").ok())
                .context("sweep requires --db <path> or KEEPER_DB")?;
            let pool = d20dao_keeper::sweep::open_existing(Path::new(&db)).await?;
            if args.iter().any(|a| a == "--cancel") {
                let removed = d20dao_keeper::sweep::cancel_request(&pool).await?;
                println!(
                    "{}",
                    serde_json::json!({ "removed_queued_request": removed })
                );
            } else if let Some((mode, value)) = match (flag("--amount"), flag("--keep")) {
                (Some(amount), None) => Some((d20dao_keeper::sweep::Mode::Amount, amount)),
                (None, Some(keep)) => Some((d20dao_keeper::sweep::Mode::Keep, keep)),
                (None, None) => None,
                _ => anyhow::bail!("Use either --amount or --keep, not both"),
            } {
                // In the native token of the chain the journal's keeper serves: USDC on Arc, ETH for a round keeper.
                let unit = d20dao_keeper::sweep::Unit::of_journal(&pool).await?;
                let request = d20dao_keeper::sweep::Request {
                    mode,
                    wei: unit.parse(&value)?.to_string(),
                    requested_at: d20dao_keeper::health::now()?,
                };
                d20dao_keeper::sweep::submit(&pool, &request).await?;
                let mut queued = serde_json::json!({ "mode": request.mode });
                queued[unit.key()] = value.into();
                println!(
                    "{}",
                    serde_json::json!({
                        "queued": queued,
                        "note": "The running keeper sends it to the coordinator fee recipient when its nonce lane is free. Check with --status."
                    })
                );
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&d20dao_keeper::sweep::status(&pool).await?)?
                );
            }
            pool.close().await;
        }
        Some("finality") => {
            let flag = |name: &str| args.windows(2).find(|a| a[0] == name).map(|a| a[1].clone());
            let db = flag("--db")
                .or_else(|| std::env::var("KEEPER_DB").ok())
                .context("finality requires --db <path> or KEEPER_DB")?;
            let pool = d20dao_keeper::sweep::open_existing(Path::new(&db)).await?;
            let status = args.iter().any(|a| a == "--status");
            let acknowledge = args.iter().position(|a| a == "--acknowledge");
            let result = match (status, acknowledge) {
                (true, Some(_)) => Err(anyhow::anyhow!(
                    "Use either --status or --acknowledge <id>, not both"
                )),
                (false, Some(at)) => match args.get(at + 1).filter(|id| !id.starts_with("--")) {
                    Some(id) => {
                        d20dao_keeper::finality::acknowledge(
                            &pool,
                            id,
                            d20dao_keeper::health::now()?,
                        )
                        .await
                    }
                    None => Err(anyhow::anyhow!(
                        "--acknowledge requires the id that --status prints"
                    )),
                },
                _ => d20dao_keeper::finality::status(&pool).await,
            };
            pool.close().await;
            println!("{}", serde_json::to_string_pretty(&result?)?);
        }
        Some("run") => {
            let cfg = Config::load(args.iter().any(|s| s == "--once"))?;
            let mut shutdown = Shutdown::new()?;
            if let Some(parent) = cfg.db.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let lock = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(cfg.db.with_extension("lock"))?;
            FileExt::try_lock_exclusive(&lock).context("Another keeper holds this journal lock")?;
            // Startup verifies every endpoint and the pins; while every endpoint is rate limiting it waits and
            // tries again instead of exiting, since a restart would only add to the load. It also tries again, for a
            // bounded time, while endpoints still disagree about an approved upgrade that is taking effect.
            let mut starts = 0u32;
            let mut settling = 0u32;
            let mut worker = loop {
                let attempt = tokio::select! {
                    biased;
                    signal = shutdown.wait() => { signal?; return Ok(ExitCode::SUCCESS); },
                    result = tokio::time::timeout(std::time::Duration::from_secs(20), Worker::new(cfg.clone())) => {
                        result.context("Keeper startup exceeded 20 second time budget")?
                    }
                };
                match attempt {
                    Ok(worker) => break worker,
                    Err(error) if !cfg.once && d20dao_keeper::rpc::is_rate_limited(&error) => {
                        starts += 1;
                        tracing::warn!(
                            attempt = starts,
                            "Every RPC endpoint is rate limiting at startup; waiting to try again"
                        );
                        tokio::select! {
                            biased;
                            signal = shutdown.wait() => { signal?; return Ok(ExitCode::SUCCESS); },
                            _ = tokio::time::sleep(rate_limit_pause(starts)) => {}
                        }
                    }
                    Err(error)
                        if !cfg.once
                            && settling < UPGRADE_SETTLE_ATTEMPTS
                            && d20dao_keeper::proxy::upgrade_in_progress(&error) =>
                    {
                        settling += 1;
                        tracing::warn!(error=%error,attempt=settling,
                            "An approved implementation upgrade is taking effect on the RPC endpoints; starting again once they agree");
                        tokio::select! {
                            biased;
                            signal = shutdown.wait() => { signal?; return Ok(ExitCode::SUCCESS); },
                            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                        }
                    }
                    Err(error) => return Err(error),
                }
            };
            let telegram = match d20dao_keeper::telegram::Settings::from_env(cfg.role) {
                Ok(Some(settings)) => {
                    match d20dao_keeper::telegram::TelegramNotifier::start(settings) {
                        Ok(notifier) => Some(worker.attach_telegram(notifier)),
                        Err(_) => {
                            tracing::warn!("Optional Telegram disabled: initialization failed");
                            None
                        }
                    }
                }
                Ok(None) => None,
                Err(_) => {
                    tracing::warn!("Optional Telegram disabled: invalid configuration");
                    None
                }
            };
            let mut telegram = telegram;
            match d20dao_keeper::discord::Settings::from_env(cfg.chain_id, cfg.coordinator) {
                Ok(Some(settings)) => match d20dao_keeper::discord::Notifier::start(settings) {
                    Ok(notifier) => worker.attach_discord(notifier),
                    Err(_) => tracing::warn!("Optional Discord disabled: initialization failed"),
                },
                Ok(None) => {}
                Err(_) => tracing::warn!("Optional Discord disabled: invalid configuration"),
            }
            tracing::info!(chain_id=cfg.chain_id,coordinator=%cfg.coordinator,send=cfg.send,consumer_access="public",drand_relays=%cfg.drand_relays,"Outbound-only keeper started");
            let mut telemetry = d20dao_keeper::telemetry::spawn(&cfg, &worker.journal);
            let mut explorer = worker.spawn_explorer();
            let signals = worker.signals();
            // An epoch keeper follows every block and every log of its contracts; a round keeper follows requests, role
            // changes and upgrades, and blocks only while work is open (events::Follow).
            let (follow, keepalive) = match worker.coordinator_kind() {
                d20dao_keeper::config::CoordinatorKind::Epoch => (
                    d20dao_keeper::events::Follow::Everything,
                    d20dao_keeper::events::Keepalive::default(),
                ),
                d20dao_keeper::config::CoordinatorKind::Round => (
                    d20dao_keeper::events::Follow::Demand,
                    d20dao_keeper::events::Keepalive::configured(&cfg.chain),
                ),
            };
            let mut subscription = d20dao_keeper::events::spawn_following(
                cfg.ws_urls.clone(),
                worker.rpc.clone(),
                cfg.chain_id,
                worker.service_contracts(),
                signals.clone(),
                follow,
                keepalive,
            );
            let cadence = d20dao_keeper::events::Cadence {
                poll: std::time::Duration::from_millis(cfg.poll_ms),
                idle_poll: std::time::Duration::from_millis(cfg.idle_poll_ms),
                heartbeat: cfg.idle_heartbeat,
            };
            let mut heads = signals.heads();
            let mut failures = 0u64;
            // Consecutive ticks that could not run only because every RPC endpoint was rate limiting.
            let mut limited = 0u32;
            // A round keeper's consecutive ticks that failed only because the endpoints failed as providers do.
            let mut outage = d20dao_keeper::worker::ProviderOutage::default();
            loop {
                // Poll shutdown before constructing/polling a new tick. In particular,
                // a signal received as startup completed must not start API work.
                tokio::select! {
                    biased;
                    signal = shutdown.wait() => { signal?; break; },
                    _ = std::future::ready(()) => {}
                }
                let mut stopping = false;
                // A block pushed while this tick runs makes the next one due at once.
                heads.borrow_and_update();
                let tick_started = std::cell::Cell::new(false);
                let tick = async {
                    tick_started.set(true);
                    tokio::time::timeout(
                        std::time::Duration::from_secs(cfg.tick_timeout_seconds),
                        worker.tick(),
                    )
                    .await
                    .context("Keeper tick exceeded time budget")?
                };
                tokio::pin!(tick);
                let result = tokio::select! {
                    biased;
                    signal = shutdown.wait() => {
                        signal?;
                        stopping = true;
                        tracing::info!("Shutdown requested; finishing bounded current tick");
                        if tick_started.get() { tick.await } else { Ok(()) }
                    },
                    result = &mut tick => result,
                };
                if let Some(upgrade) = worker.approved_upgrade() {
                    // Not a failed tick and no error notice: this process signs and sends nothing more, and the
                    // supervisor's restart runs every startup check against the new implementation. The journal is
                    // left as any interrupted tick leaves it, and the restart reconciles it.
                    tracing::warn!(service=upgrade.service.name(),proxy=%upgrade.proxy,from=%upgrade.from,to=%upgrade.to,
                        code_hash=%upgrade.code_hash,exit_code=d20dao_keeper::proxy::APPROVED_UPGRADE_EXIT,
                        "Keeper exiting for a restart on the approved next implementation");
                    drop(subscription.take());
                    drop(telemetry.take());
                    drop(explorer.take());
                    drop(telegram.take());
                    // An epoch source fetch still in flight saves its packet first, as at any shutdown; the restart
                    // retries one that failed.
                    if let Err(error) = worker.stop_epoch_fetch().await {
                        tracing::warn!(error=%error,"Epoch source fetch ended with an error during the restart");
                    }
                    worker.journal.pool.close().await;
                    return Ok(ExitCode::from(d20dao_keeper::proxy::APPROVED_UPGRADE_EXIT));
                }
                let failed = match &result {
                    Err(error) if !cfg.once && !stopping => Some(
                        d20dao_keeper::worker::FailedTick::of(worker.coordinator_kind(), error),
                    ),
                    _ => None,
                };
                if failed == Some(d20dao_keeper::worker::FailedTick::RateLimited) {
                    // Not a fault of the chain, the keys or the configuration: the tick is deferred, does not count
                    // toward MAX_TICK_FAILURES, and health reports the condition until a tick runs again.
                    limited += 1;
                    d20dao_keeper::health::rate_limited(&worker.journal, cfg.send).await?;
                    if limited == 1 {
                        worker.notify_error(d20dao_keeper::telegram::ErrorClass::RpcUnavailable);
                        tracing::warn!(
                            "Every RPC endpoint is rate limiting; tick deferred and not counted as a failure"
                        );
                    } else {
                        tracing::debug!(
                            consecutive = limited,
                            "Tick still deferred by RPC rate limits"
                        );
                    }
                    tokio::select! {
                        biased;
                        signal = shutdown.wait() => { signal?; break; },
                        _ = tokio::time::sleep(rate_limit_pause(limited)) => {}
                    }
                    continue;
                }
                if let (Some(d20dao_keeper::worker::FailedTick::ProviderUnavailable), Err(error)) =
                    (failed, &result)
                {
                    // A round keeper's tick that failed only because the endpoints answered with transport or provider
                    // errors (HTTP 429 or 5xx, -32000, no answer in time) is deferred as a rate-limited one is and not
                    // counted: a restart would only add to their load. Health reports it once it has lasted
                    // PROGRESS_STUCK_SECONDS.
                    let deferral =
                        outage.defer(std::time::Duration::from_secs(cfg.progress_stuck_seconds));
                    if deferral.attempt == 1 {
                        tracing::warn!(error=%error,"RPC endpoints are failing as providers do when overloaded or down; tick deferred and not counted as a failure");
                    } else {
                        tracing::debug!(error=%error,consecutive=deferral.attempt,"Tick still deferred by RPC provider errors");
                    }
                    if deferral.report {
                        worker.notify_error(d20dao_keeper::telegram::ErrorClass::RpcUnavailable);
                        tracing::error!(error=%error,consecutive=deferral.attempt,
                            "RPC endpoints have failed every tick for PROGRESS_STUCK_SECONDS; still deferring, health reports rpc_unavailable");
                    }
                    if outage.reported() {
                        d20dao_keeper::health::rpc_unavailable(&worker.journal, cfg.send).await?;
                    }
                    tokio::select! {
                        biased;
                        signal = shutdown.wait() => { signal?; break; },
                        _ = tokio::time::sleep(rate_limit_pause(deferral.attempt)) => {}
                    }
                    continue;
                }
                if limited > 0 && result.is_ok() {
                    tracing::info!(
                        deferred = limited,
                        "RPC endpoints answer again; ticks resumed"
                    );
                    limited = 0;
                }
                // Any tick that was not deferred ends the run of deferred ones.
                if let Some(deferred) = outage.end()
                    && result.is_ok()
                {
                    tracing::info!(
                        deferred,
                        "RPC endpoints answer again after provider errors; ticks resumed"
                    );
                }
                if let Err(error) = result {
                    worker.notify_error(d20dao_keeper::telegram::ErrorClass::KeeperTick);
                    d20dao_keeper::health::tick_failed(&worker.journal, cfg.send).await?;
                    // While a finality mismatch is on record or suspected the process stays up whatever fails: a restart
                    // comes back to the same journal and the same incident, which the running keeper settles and recovers
                    // from by itself. The failure is reported and not counted.
                    let incident = !cfg.once && worker.finality_incident_open().await;
                    if incident {
                        tracing::error!(error=%error,"Keeper tick failed during a finality incident; the process stays up and the journal is retained");
                    } else {
                        failures += 1;
                        tracing::error!(error=%error,consecutive_failures=failures,"Keeper tick failed; journal retained");
                    }
                    if d20dao_keeper::worker::tick_failure_ends_the_run(
                        cfg.once,
                        incident,
                        failures,
                        cfg.max_tick_failures,
                    ) {
                        tracing::error!(
                            "Keeper exiting after failed work; supervisor should restart and operator should inspect persistent failures"
                        );
                        drop(subscription.take());
                        drop(telemetry.take());
                        drop(explorer.take());
                        drop(telegram.take());
                        worker.stop_epoch_fetch().await?;
                        worker.journal.pool.close().await;
                        return Err(error);
                    }
                } else {
                    failures = 0;
                }
                if cfg.once && (!stopping || tick_started.get()) {
                    let status = d20dao_keeper::health::read(&cfg.db).await?;
                    if let Err(error) = d20dao_keeper::health::require_healthy(&status) {
                        drop(subscription.take());
                        drop(telemetry.take());
                        drop(explorer.take());
                        drop(telegram.take());
                        worker.stop_epoch_fetch().await?;
                        worker.journal.pool.close().await;
                        return Err(error);
                    }
                }
                if cfg.once || stopping {
                    break;
                }
                // Open work keeps POLL_MS (or follows pushed blocks); an idle keeper waits for an event or its idle
                // interval. A failed read of the journal counts as open work. A round keeper's subscription follows new
                // blocks only while it is open.
                let busy = worker.open_work().await.unwrap_or(true);
                signals.want_heads(busy);
                tokio::select! {
                    biased;
                    signal = shutdown.wait() => { signal?; break; },
                    _ = signals.next_tick(&mut heads, busy, cadence) => {}
                }
            }
            drop(subscription.take());
            drop(telemetry.take());
            drop(explorer.take());
            drop(telegram.take());
            worker.stop_epoch_fetch().await?;
            worker.journal.pool.close().await;
        }
        _ => println!(
            "d20dao-keeper run [--once]\nd20dao-keeper health --db <path> [--max-age <seconds>]\nd20dao-keeper sweep [--db <path>] --amount <amount> | --keep <amount> | --status | --cancel (USDC on Arc, ETH for a round keeper)\nd20dao-keeper finality [--db <path>] --status | --acknowledge <id>\nd20dao-keeper migrate --from <old-db> --prepare|--apply|--resume\nd20dao-keeper prove <seed> <key-file>\nd20dao-keeper public-key <key-file>\nConfiguration: keeper/.env.example. Sending is OFF by default. Migration requires drained ingress and the reviewed destination environment."
        ),
    }
    Ok(ExitCode::SUCCESS)
}
