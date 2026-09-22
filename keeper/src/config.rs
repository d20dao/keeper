use alloy_primitives::{Address, B256};
use anyhow::{Context, Result, ensure};
use std::{env, path::PathBuf};

/// Which transaction wallet authorization a keeper runs with. A primary is the registry's committer() and sends as
/// soon as work is ready, earliest deadline first. A follower is an allowed backup committer with its own wallet and
/// journal: it prepares the same work but joins the queue from its newest end only when the join rule fires, so the
/// two fronts never work the same requests until they meet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Primary,
    Follower(FollowerPlan),
}
/// How a follower decides to join, and which slice of the tail it takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FollowerPlan {
    /// Age of the oldest unserved request at which this follower joins the queue.
    pub delay: u64,
    /// Visible unserved queue length above which it joins whatever the ages are.
    pub queue_join: u64,
    /// Seconds of sendable work waiting without a committer nonce advance after which the primary counts as dead.
    pub liveness: u64,
    /// This follower's lane: with `lanes` follower lanes it takes the request ids where id % lanes == rank.
    pub rank: u64,
    pub lanes: u64,
}
/// The coordinator's response window: a request's deadline is its creation time plus this many seconds.
pub const RESPONSE_TIMEOUT_SECONDS: u64 = 60;
/// Bounds for FOLLOWER_DELAY_SECONDS: long enough for a healthy primary to be seen serving the oldest request,
/// short enough that at least 30 of a request's 60 seconds remain for a publication, the target block and the proof.
pub const FOLLOWER_DELAY_RANGE: std::ops::RangeInclusive<u64> = 5..=30;
/// Bounds for PRIMARY_LIVENESS_SECONDS, the time sendable work may wait without a committer nonce advance before the
/// primary counts as dead. It must exceed the primary's gap between transactions while it works through a burst
/// (measured at most 3 seconds between batches, about 6 seconds for an idle epoch's publish-then-serve pipeline).
pub const PRIMARY_LIVENESS_RANGE: std::ops::RangeInclusive<u64> = 5..=30;
/// Bounds for FOLLOWER_QUEUE_JOIN, the visible unserved queue above which a follower joins from the tail.
pub const FOLLOWER_QUEUE_JOIN_RANGE: std::ops::RangeInclusive<u64> = 1..=10_000;
/// The registry allows four backup committers, so at most four follower lanes can exist beside the primary.
pub const MAX_FOLLOWER_LANES: u64 = 4;
/// Every node, whatever its role or lane, sends a request this close to its deadline: the last line before a refund.
pub const SAFETY_AGE_SECONDS: u64 = 20;
/// Hysteresis: a follower that has joined leaves again only when the queue is this short and this young.
pub const FOLLOWER_LEAVE_PENDING: u64 = 32;
pub const FOLLOWER_LEAVE_AGE_SECONDS: u64 = 10;
/// Time a node keeps for one fulfillment round: target block, proof transaction and receipt.
pub const FULFILLMENT_ROUND_SECONDS: u64 = 10;
impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Follower { .. } => "follower",
        }
    }
    pub fn is_follower(self) -> bool {
        matches!(self, Self::Follower { .. })
    }
    /// `margin` is SEND_MARGIN_SECONDS: the join delay, the margin and one fulfillment round must fit the response
    /// window, and the margin and that round must fit inside the safety age every node honours.
    fn parse(role: Option<&str>, timing: FollowerSettings<'_>, margin: u64) -> Result<Self> {
        let number = |value: Option<&str>, default: &str, name: &str| -> Result<u64> {
            value
                .unwrap_or(default)
                .trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid {name}"))
        };
        ensure!(
            timing.busy_delay.is_none(),
            "FOLLOWER_BUSY_DELAY_SECONDS was replaced by the join rule: use FOLLOWER_DELAY_SECONDS, FOLLOWER_QUEUE_JOIN and PRIMARY_LIVENESS_SECONDS"
        );
        match role.map(str::trim).unwrap_or("primary") {
            "primary" => {
                ensure!(
                    timing.delay.is_none()
                        && timing.queue_join.is_none()
                        && timing.liveness.is_none()
                        && timing.rank.is_none()
                        && timing.lanes.is_none(),
                    "FOLLOWER_DELAY_SECONDS, FOLLOWER_QUEUE_JOIN, PRIMARY_LIVENESS_SECONDS, FOLLOWER_RANK and FOLLOWER_LANES apply only with KEEPER_ROLE=follower"
                );
                Ok(Self::Primary)
            }
            "follower" => {
                let delay = number(timing.delay, "20", "FOLLOWER_DELAY_SECONDS")?;
                let queue_join = number(timing.queue_join, "150", "FOLLOWER_QUEUE_JOIN")?;
                let liveness = number(timing.liveness, "10", "PRIMARY_LIVENESS_SECONDS")?;
                let rank = number(timing.rank, "0", "FOLLOWER_RANK")?;
                let lanes = number(timing.lanes, "1", "FOLLOWER_LANES")?;
                ensure!(
                    FOLLOWER_DELAY_RANGE.contains(&delay),
                    "FOLLOWER_DELAY_SECONDS must be between {} and {} so a takeover still fits the 60-second request deadline",
                    FOLLOWER_DELAY_RANGE.start(),
                    FOLLOWER_DELAY_RANGE.end()
                );
                ensure!(
                    delay + margin + FULFILLMENT_ROUND_SECONDS <= RESPONSE_TIMEOUT_SECONDS,
                    "FOLLOWER_DELAY_SECONDS, SEND_MARGIN_SECONDS and a {FULFILLMENT_ROUND_SECONDS}-second fulfillment round must fit the 60-second request deadline"
                );
                ensure!(
                    margin + FULFILLMENT_ROUND_SECONDS <= SAFETY_AGE_SECONDS,
                    "SEND_MARGIN_SECONDS must leave the {SAFETY_AGE_SECONDS}-second safety age room for a {FULFILLMENT_ROUND_SECONDS}-second fulfillment round"
                );
                ensure!(
                    FOLLOWER_QUEUE_JOIN_RANGE.contains(&queue_join),
                    "FOLLOWER_QUEUE_JOIN must be between {} and {}",
                    FOLLOWER_QUEUE_JOIN_RANGE.start(),
                    FOLLOWER_QUEUE_JOIN_RANGE.end()
                );
                ensure!(
                    PRIMARY_LIVENESS_RANGE.contains(&liveness),
                    "PRIMARY_LIVENESS_SECONDS must be between {} and {}",
                    PRIMARY_LIVENESS_RANGE.start(),
                    PRIMARY_LIVENESS_RANGE.end()
                );
                ensure!(
                    (1..=MAX_FOLLOWER_LANES).contains(&lanes) && rank < lanes,
                    "FOLLOWER_LANES must be 1 to {MAX_FOLLOWER_LANES} and FOLLOWER_RANK a lane below it: each follower takes the request ids where id % lanes == rank"
                );
                Ok(Self::Follower(FollowerPlan {
                    delay,
                    queue_join,
                    liveness,
                    rank,
                    lanes,
                }))
            }
            _ => anyhow::bail!("KEEPER_ROLE must be primary or follower"),
        }
    }
}
/// The raw follower settings, as read from the environment.
#[derive(Clone, Copy, Default)]
struct FollowerSettings<'a> {
    delay: Option<&'a str>,
    busy_delay: Option<&'a str>,
    queue_join: Option<&'a str>,
    liveness: Option<&'a str>,
    rank: Option<&'a str>,
    lanes: Option<&'a str>,
}
#[derive(Clone)]
pub struct Config {
    pub role: Role,
    pub telemetry: Option<crate::telemetry::Settings>,
    pub rpc_urls: Vec<String>,
    /// Optional WebSocket endpoints for pushed blocks and events; HTTP stays the source of truth.
    pub ws_urls: Vec<String>,
    pub chain_id: u64,
    pub coordinator: Address,
    pub db: PathBuf,
    pub lock_dir: PathBuf,
    pub tx_key_file: PathBuf,
    pub vrf_key_file: PathBuf,
    pub send: bool,
    pub once: bool,
    pub poll_ms: u64,
    /// Polling interval while nothing is open and no event subscription is live.
    pub idle_poll_ms: u64,
    pub max_tick_failures: u64,
    pub tick_timeout_seconds: u64,
    pub margin: u64,
    pub max_gas: u64,
    pub max_fee: u128,
    pub cancel_max_fee: u128,
    pub max_cost: u128,
    /// Bounds for the tip taken from the recent median; the maximum also stays within max_fee.
    pub min_priority_fee: u128,
    pub max_priority_fee: u128,
    /// Escrowed fees must cover this share (bps) of a fulfillment's expected cost; 0 disables.
    pub fee_coverage_bps: u64,
    pub nonce_stuck_seconds: u64,
    pub progress_stuck_seconds: u64,
    /// Prepared requests one fulfillment transaction may carry; 1 keeps the single path only.
    pub fulfill_batch_max: usize,
    pub code_hash: Option<B256>,
    pub protocol_hash: Option<B256>,
    pub implementation_code_hash: Option<B256>,
    pub registry_implementation_code_hash: Option<B256>,
    /// Runtime code hashes of implementations approved ahead of an in-place upgrade. Startup accepts either the pin or
    /// this hash; a running keeper that sees its proxy move to this hash exits for a verified restart.
    pub approved_next_implementation_code_hash: Option<B256>,
    pub approved_next_registry_implementation_code_hash: Option<B256>,
    pub api_override: Option<String>,
    pub api_endpoints: crate::epoch::ApiEndpoints,
}
fn required(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("Missing {name}"))
}
fn number<T: std::str::FromStr>(name: &str, default: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    env::var(name)
        .unwrap_or_else(|_| default.into())
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid {name}: {e}"))
}
impl Config {
    pub fn load(once: bool) -> Result<Self> {
        let chain_id = number("CHAIN_ID", "0")?;
        ensure!(chain_id > 0, "CHAIN_ID must be configured explicitly");
        let rpc_urls: Vec<String> = required("RPC_URLS")?
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        ensure!(!rpc_urls.is_empty(), "At least one RPC required");
        for url in &rpc_urls {
            ensure!(
                url.starts_with("https://")
                    || (chain_id == 31337 && url.starts_with("http://127.0.0.1:")),
                "RPC must use HTTPS (loopback HTTP is local-test only)"
            );
        }
        let ws_urls: Vec<String> = env::var("WS_URLS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        for url in &ws_urls {
            ensure!(
                url.starts_with("wss://")
                    || (chain_id == 31337 && url.starts_with("ws://127.0.0.1:")),
                "WS_URLS must use wss:// (loopback ws is local-test only)"
            );
        }
        let code_hash = env::var("EXPECTED_CODE_HASH")
            .ok()
            .map(|v| v.parse())
            .transpose()?;
        ensure!(
            env::var_os("EXPECTED_SOURCE_HASH").is_none(),
            "Use EXPECTED_PROTOCOL_HASH for the coordinator configuration"
        );
        let protocol_hash = env::var("EXPECTED_PROTOCOL_HASH")
            .ok()
            .map(|v| v.parse())
            .transpose()?;
        let implementation_code_hash = env::var("EXPECTED_IMPLEMENTATION_CODE_HASH")
            .ok()
            .map(|v| v.parse())
            .transpose()?;
        let registry_implementation_code_hash =
            env::var("EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH")
                .ok()
                .map(|v| v.parse())
                .transpose()?;
        ensure!(
            chain_id == 31337
                || (code_hash.is_some()
                    && protocol_hash.is_some()
                    && implementation_code_hash.is_some()
                    && registry_implementation_code_hash.is_some()),
            "Nonlocal networks require proxy, both implementation, and protocol configuration hashes"
        );
        let approved_next_implementation_code_hash = approved_next(
            "APPROVED_NEXT_IMPLEMENTATION_CODE_HASH",
            env::var("APPROVED_NEXT_IMPLEMENTATION_CODE_HASH")
                .ok()
                .as_deref(),
        )?;
        let approved_next_registry_implementation_code_hash = approved_next(
            "APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH",
            env::var("APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH")
                .ok()
                .as_deref(),
        )?;
        let api_override = env::var("TEST_API_BASE").ok();
        if let Some(url) = &api_override {
            ensure!(
                chain_id == 31337 && url.starts_with("http://127.0.0.1:"),
                "API override is local-test only"
            );
        }
        let api_endpoints = crate::epoch::ApiEndpoints::parse(
            env::var("EPOCH_API_ENDPOINTS")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .as_deref(),
        )?;
        let lock_dir = if let Ok(path) = env::var("TEST_LOCK_DIR") {
            ensure!(
                chain_id == 31337,
                "Lock directory override is local-test only"
            );
            PathBuf::from(path)
        } else {
            #[cfg(windows)]
            let path = PathBuf::from(required("ProgramData")?).join("D20DAO/locks");
            #[cfg(not(windows))]
            let path = PathBuf::from("/var/lib/d20dao/locks");
            path
        };
        let margin = number("SEND_MARGIN_SECONDS", "5")?;
        let follower = [
            "FOLLOWER_DELAY_SECONDS",
            "FOLLOWER_BUSY_DELAY_SECONDS",
            "FOLLOWER_QUEUE_JOIN",
            "PRIMARY_LIVENESS_SECONDS",
            "FOLLOWER_RANK",
            "FOLLOWER_LANES",
        ]
        .map(|name| env::var(name).ok());
        let role = Role::parse(
            env::var("KEEPER_ROLE").ok().as_deref(),
            FollowerSettings {
                delay: follower[0].as_deref(),
                busy_delay: follower[1].as_deref(),
                queue_join: follower[2].as_deref(),
                liveness: follower[3].as_deref(),
                rank: follower[4].as_deref(),
                lanes: follower[5].as_deref(),
            },
            margin,
        )?;
        let poll_ms: u64 = number("POLL_MS", "1000")?;
        let result = Self {
            role,
            telemetry: crate::telemetry::Settings::load(chain_id)?,
            rpc_urls,
            ws_urls,
            chain_id,
            coordinator: required("COORDINATOR_ADDRESS")?.parse()?,
            db: required("KEEPER_DB")?.into(),
            lock_dir,
            tx_key_file: required("TX_KEY_FILE")?.into(),
            vrf_key_file: required("VRF_KEY_FILE")?.into(),
            send: env::var("SEND_TRANSACTIONS").as_deref() == Ok("true"),
            once,
            poll_ms,
            idle_poll_ms: number("IDLE_POLL_MS", &poll_ms.max(1000).to_string())?,
            max_tick_failures: number("MAX_TICK_FAILURES", "5")?,
            tick_timeout_seconds: number("TICK_TIMEOUT_SECONDS", "20")?,
            margin,
            // A fulfillment reserves its callbacks' full gas limits (worker::fulfillment_gas): one request
            // with the coordinator's largest callback limit needs about 2.5M, so the default carries it.
            max_gas: number("MAX_GAS", "3000000")?,
            max_fee: number("MAX_FEE_PER_GAS_WEI", "100000000000")?,
            cancel_max_fee: required("CANCEL_MAX_FEE_PER_GAS_WEI")?
                .parse()
                .context("Invalid CANCEL_MAX_FEE_PER_GAS_WEI")?,
            progress_stuck_seconds: number("PROGRESS_STUCK_SECONDS", "20")?,
            nonce_stuck_seconds: number("NONCE_STUCK_SECONDS", "120")?,
            max_cost: number("MAX_TX_COST_WEI", "200000000000000000")?,
            min_priority_fee: number("MIN_PRIORITY_FEE_WEI", "1000000000")?,
            max_priority_fee: number("MAX_PRIORITY_FEE_WEI", "50000000000")?,
            fee_coverage_bps: validate_fee_coverage_bps(number("FEE_COVERAGE_BPS", "10000")?)?,
            fulfill_batch_max: validate_fulfill_batch_max(number("FULFILL_BATCH_MAX", "8")?)?,
            code_hash,
            protocol_hash,
            implementation_code_hash,
            registry_implementation_code_hash,
            approved_next_implementation_code_hash,
            approved_next_registry_implementation_code_hash,
            api_override,
            api_endpoints,
        };
        ensure!(
            result.db.is_absolute() && result.lock_dir.is_absolute(),
            "KEEPER_DB and lock directory must be absolute paths"
        );
        ensure!(
            !result.coordinator.is_zero()
                && result.margin > 0
                && result.margin < 60
                && result.poll_ms >= 100
                && result.idle_poll_ms >= result.poll_ms
                && result.idle_poll_ms <= result.poll_ms.max(20_000)
                && result.max_tick_failures > 0
                && result.tick_timeout_seconds > 0
                && result.tick_timeout_seconds <= 20,
            "Invalid coordinator/timing configuration"
        );
        validate_recovery_budget(result.max_fee, result.cancel_max_fee, result.max_cost)?;
        validate_priority_bounds(
            result.min_priority_fee,
            result.max_priority_fee,
            result.max_fee,
        )?;
        ensure!(
            result.nonce_stuck_seconds > 0 && result.progress_stuck_seconds > 0,
            "NONCE_STUCK_SECONDS and PROGRESS_STUCK_SECONDS must be positive"
        );
        Ok(result)
    }
    /// The implementations approved ahead of an in-place upgrade, per proxy.
    pub fn approved_next(&self) -> crate::proxy::ApprovedNext {
        crate::proxy::ApprovedNext {
            coordinator: self.approved_next_implementation_code_hash,
            registry: self.approved_next_registry_implementation_code_hash,
        }
    }
}

/// An optional approved next implementation: the runtime code hash an operator reviewed before an in-place upgrade.
/// Unset or empty approves nothing. Every environment variable the keeper does not read is ignored, so a keeper
/// release without this setting runs unchanged beside it.
fn approved_next(name: &str, value: Option<&str>) -> Result<Option<B256>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let hash: B256 = value
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid {name}: expected a 32-byte runtime code hash"))?;
    ensure!(
        !hash.is_zero(),
        "{name} must be the approved implementation's runtime code hash, not a placeholder"
    );
    Ok(Some(hash))
}

/// The coordinator rejects more than MAX_FULFILL_BATCH members; 1 disables batching entirely.
fn validate_fulfill_batch_max(value: usize) -> Result<usize> {
    ensure!(
        (1..=crate::journal::MAX_BATCH_MEMBERS).contains(&value),
        "FULFILL_BATCH_MAX must be between 1 and {}",
        crate::journal::MAX_BATCH_MEMBERS
    );
    Ok(value)
}

fn validate_fee_coverage_bps(value: u64) -> Result<u64> {
    ensure!(
        value <= 100_000,
        "FEE_COVERAGE_BPS must be between 0 (disabled) and 100000"
    );
    Ok(value)
}
fn validate_priority_bounds(min: u128, max: u128, max_fee: u128) -> Result<()> {
    ensure!(
        min <= max && max <= max_fee,
        "MIN_PRIORITY_FEE_WEI must not exceed MAX_PRIORITY_FEE_WEI, which must not exceed MAX_FEE_PER_GAS_WEI"
    );
    Ok(())
}
fn validate_recovery_budget(fulfill: u128, cancel: u128, max_cost: u128) -> Result<()> {
    let minimum = fulfill
        .checked_mul(9)
        .map(|v| v / 8 + 1)
        .ok_or_else(|| anyhow::anyhow!("Recovery fee overflow"))?;
    ensure!(
        fulfill > 0 && cancel >= minimum,
        "CANCEL_MAX_FEE_PER_GAS_WEI must explicitly allow at least a 12.5% + 1 wei replacement above the fulfillment cap"
    );
    ensure!(
        minimum.checked_mul(21000).is_some_and(|v| v <= max_cost),
        "MAX_TX_COST_WEI cannot fund minimum nonce recovery"
    );
    Ok(())
}

/// A configured ceiling that a required transaction can exceed. Each names the
/// environment variable an operator raises; none is bypassed to meet a deadline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeeCap {
    MaxGas,
    MaxFeePerGas,
    CancelMaxFeePerGas,
    MaxTxCost,
    /// Not an operator ceiling: the escrowed fees of the served requests do not cover the expected cost.
    FeeCoverage,
}
impl FeeCap {
    pub fn variable(self) -> &'static str {
        match self {
            Self::MaxGas => "MAX_GAS",
            Self::MaxFeePerGas => "MAX_FEE_PER_GAS_WEI",
            Self::CancelMaxFeePerGas => "CANCEL_MAX_FEE_PER_GAS_WEI",
            Self::MaxTxCost => "MAX_TX_COST_WEI",
            Self::FeeCoverage => "FEE_COVERAGE_BPS",
        }
    }
}
/// Public, operator-facing budget observation: only configured limits and required amounts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeeBudget {
    pub cap: FeeCap,
    pub required: u128,
    pub limit: u128,
}
impl std::fmt::Display for FeeBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.cap == FeeCap::FeeCoverage {
            return write!(
                f,
                "expected cost share {} exceeds escrowed fees {} under FEE_COVERAGE_BPS",
                self.required, self.limit
            );
        }
        write!(
            f,
            "required {} exceeds {}={}",
            self.required,
            self.cap.variable(),
            self.limit
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roles_default_to_primary_and_bound_the_follower_join_rule() {
        let settings = |delay, queue_join, liveness| FollowerSettings {
            delay,
            queue_join,
            liveness,
            ..FollowerSettings::default()
        };
        let lane = |rank, lanes| FollowerSettings {
            rank,
            lanes,
            ..FollowerSettings::default()
        };
        let follower = |delay, queue_join, liveness, rank, lanes| {
            Role::Follower(FollowerPlan {
                delay,
                queue_join,
                liveness,
                rank,
                lanes,
            })
        };
        let none = FollowerSettings::default();
        assert_eq!(Role::parse(None, none, 5).unwrap(), Role::Primary);
        assert_eq!(
            Role::parse(Some("primary"), none, 5).unwrap(),
            Role::Primary
        );
        assert_eq!(
            Role::parse(Some("follower"), none, 5).unwrap(),
            follower(20, 150, 10, 0, 1)
        );
        assert_eq!(
            Role::parse(
                Some("follower"),
                settings(Some("5"), Some("1"), Some("5")),
                5
            )
            .unwrap(),
            follower(5, 1, 5, 0, 1)
        );
        // Three lanes beside the primary: each follower takes one residue class of the request ids.
        assert_eq!(
            Role::parse(Some("follower"), lane(Some("2"), Some("3")), 5).unwrap(),
            follower(20, 150, 10, 2, 3)
        );
        // The longest join delay still fits the deadline with the largest send margin the safety age allows.
        assert_eq!(
            Role::parse(Some("follower"), settings(Some("30"), None, None), 10).unwrap(),
            follower(30, 150, 10, 0, 1)
        );
        for (role, timing) in [
            (Some("follower"), settings(Some("4"), None, None)),
            (Some("follower"), settings(Some("31"), None, None)),
            (Some("follower"), settings(Some("twenty"), None, None)),
            (Some("follower"), settings(None, Some("0"), None)),
            (Some("follower"), settings(None, None, Some("4"))),
            (Some("follower"), settings(None, None, Some("31"))),
            (Some("follower"), lane(Some("1"), Some("1"))),
            (Some("follower"), lane(Some("0"), Some("5"))),
            (Some("follower"), lane(Some("0"), Some("0"))),
            (
                Some("follower"),
                FollowerSettings {
                    busy_delay: Some("40"),
                    ..FollowerSettings::default()
                },
            ),
            (Some("primary"), settings(Some("20"), None, None)),
            (Some("primary"), settings(None, Some("150"), None)),
            (Some("primary"), lane(None, Some("2"))),
            (Some("backup"), none),
        ] {
            assert!(Role::parse(role, timing, 5).is_err(), "{role:?}");
        }
        // A send margin that leaves no room for a fulfillment round inside the safety age is refused.
        assert!(Role::parse(Some("follower"), none, 11).is_err());
        assert_eq!(follower(20, 150, 10, 0, 1).name(), "follower");
    }
    #[test]
    fn approved_next_implementations_are_optional_exact_code_hashes() {
        let name = "APPROVED_NEXT_IMPLEMENTATION_CODE_HASH";
        let hash = "0x3dda400d8360d7e03b8dacd8ba1ffad7ad672e07628bee7de4542d754dd5348c";
        // Unset or empty approves nothing, so the setting can be cleared in place after an upgrade.
        assert_eq!(approved_next(name, None).unwrap(), None);
        assert_eq!(approved_next(name, Some("")).unwrap(), None);
        assert_eq!(approved_next(name, Some("  ")).unwrap(), None);
        assert_eq!(
            approved_next(name, Some(hash)).unwrap(),
            Some(hash.parse().unwrap())
        );
        assert_eq!(
            approved_next(name, Some(&format!(" {hash}\t"))).unwrap(),
            Some(hash.parse().unwrap())
        );
        // A malformed value or a placeholder is refused, naming the setting.
        for invalid in [
            "0x1234",
            "not-a-hash",
            &hash[..65],
            &format!("{hash}00"),
            "0x0000000000000000000000000000000000000000000000000000000000000000",
        ] {
            let error = approved_next(name, Some(invalid)).unwrap_err().to_string();
            assert!(error.contains(name), "{invalid}: {error}");
        }
    }
    #[test]
    fn recovery_requires_explicit_affordable_premium() {
        assert!(validate_recovery_budget(100, 100, 3_000_000).is_err());
        assert!(validate_recovery_budget(100, 113, 113 * 21000).is_ok());
        assert!(validate_recovery_budget(100, 113, 113 * 21000 - 1).is_err());
        assert!(validate_recovery_budget(u128::MAX, u128::MAX, u128::MAX).is_err());
    }
    #[test]
    fn fulfill_batch_max_is_bounded_by_the_coordinator_limit() {
        // Priority bounds: min <= max <= MAX_FEE_PER_GAS_WEI, so a clamped tip is always affordable.
        assert!(validate_priority_bounds(1, 50, 100).is_ok());
        assert!(validate_priority_bounds(50, 50, 50).is_ok());
        assert!(validate_priority_bounds(51, 50, 100).is_err());
        assert!(validate_priority_bounds(1, 101, 100).is_err());
        assert_eq!(validate_fee_coverage_bps(0).unwrap(), 0);
        assert_eq!(validate_fee_coverage_bps(10_000).unwrap(), 10_000);
        assert!(validate_fee_coverage_bps(100_001).is_err());
        assert!(validate_fulfill_batch_max(0).is_err());
        assert_eq!(validate_fulfill_batch_max(1).unwrap(), 1);
        assert_eq!(validate_fulfill_batch_max(8).unwrap(), 8);
        assert_eq!(validate_fulfill_batch_max(16).unwrap(), 16);
        assert!(validate_fulfill_batch_max(17).is_err());
    }
    #[test]
    fn documented_mainnet_budgets_validate_and_cover_observed_base_fees() {
        // keeper/.env.example mainnet guidance: 2000 gwei, 2500 gwei, 4 USDC (native 18).
        let (fulfill, cancel, cost) = (
            2_000_000_000_000u128,
            2_500_000_000_000u128,
            4_000_000_000_000_000_000u128,
        );
        validate_recovery_budget(fulfill, cancel, cost).unwrap();
        assert!(validate_recovery_budget(fulfill, 2_250_000_000_000, cost).is_err());
        // 2 * base fee + 1 gwei at the highest observed Arc base fee (251 gwei) fits the cap.
        let required = 251_000_000_000u128 * 2 + 1_000_000_000;
        assert!(required <= fulfill);
        assert!(required * 600_000 <= cost);
        assert_eq!(
            FeeBudget {
                cap: FeeCap::MaxFeePerGas,
                required,
                limit: 100_000_000_000,
            }
            .to_string(),
            "required 503000000000 exceeds MAX_FEE_PER_GAS_WEI=100000000000"
        );
    }
}
