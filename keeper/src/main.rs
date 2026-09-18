use alloy_primitives::U256;
use anyhow::{Context, Result};
use d20dao_keeper::{config::Config, prover, worker::Worker};
use fs2::FileExt;
use std::{fs::OpenOptions, path::Path};

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
async fn main() -> Result<()> {
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
                let request = d20dao_keeper::sweep::Request {
                    mode,
                    wei: d20dao_keeper::sweep::parse_usdc(&value)?.to_string(),
                    requested_at: d20dao_keeper::health::now()?,
                };
                d20dao_keeper::sweep::submit(&pool, &request).await?;
                println!(
                    "{}",
                    serde_json::json!({
                        "queued": { "mode": request.mode, "usdc": value },
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
            // tries again instead of exiting, since a restart would only add to the load.
            let mut starts = 0u32;
            let mut worker = loop {
                let attempt = tokio::select! {
                    biased;
                    signal = shutdown.wait() => { signal?; return Ok(()); },
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
                            signal = shutdown.wait() => { signal?; return Ok(()); },
                            _ = tokio::time::sleep(rate_limit_pause(starts)) => {}
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
            tracing::info!(chain_id=cfg.chain_id,coordinator=%cfg.coordinator,send=cfg.send,consumer_access="public","Outbound-only keeper started");
            let mut telemetry = d20dao_keeper::telemetry::spawn(&cfg, &worker.journal);
            let mut explorer = worker.spawn_explorer();
            let signals = worker.signals();
            let mut subscription = d20dao_keeper::events::spawn(
                cfg.ws_urls.clone(),
                worker.rpc.clone(),
                cfg.chain_id,
                [cfg.coordinator, worker.registry()],
                signals.clone(),
            );
            let mut heads = signals.heads();
            let mut failures = 0u64;
            // Consecutive ticks that could not run only because every RPC endpoint was rate limiting.
            let mut limited = 0u32;
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
                if let Err(error) = &result
                    && !cfg.once
                    && !stopping
                    && d20dao_keeper::rpc::is_rate_limited(error)
                {
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
                if limited > 0 && result.is_ok() {
                    tracing::info!(
                        deferred = limited,
                        "RPC endpoints answer again; ticks resumed"
                    );
                    limited = 0;
                }
                if let Err(error) = result {
                    worker.notify_error(d20dao_keeper::telegram::ErrorClass::KeeperTick);
                    d20dao_keeper::health::tick_failed(&worker.journal, cfg.send).await?;
                    failures += 1;
                    tracing::error!(error=%error,consecutive_failures=failures,"Keeper tick failed; journal retained");
                    if cfg.once || failures >= cfg.max_tick_failures {
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
                // interval. A failed read of the journal counts as open work.
                let busy = worker.open_work().await.unwrap_or(true);
                tokio::select! {
                    biased;
                    signal = shutdown.wait() => { signal?; break; },
                    _ = signals.next_tick(
                        &mut heads,
                        busy,
                        std::time::Duration::from_millis(cfg.poll_ms),
                        std::time::Duration::from_millis(cfg.idle_poll_ms),
                    ) => {}
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
            "d20dao-keeper run [--once]\nd20dao-keeper health --db <path> [--max-age <seconds>]\nd20dao-keeper sweep [--db <path>] --amount <USDC> | --keep <USDC> | --status | --cancel\nd20dao-keeper migrate --from <old-db> --prepare|--apply|--resume\nd20dao-keeper prove <seed> <key-file>\nd20dao-keeper public-key <key-file>\nConfiguration: keeper/.env.example. Sending is OFF by default. Migration requires drained ingress and the reviewed destination environment."
        ),
    }
    Ok(())
}
