//! drand beacon epochs. A beacon recipe (EpochEntropy.registerBeacon) names a public randomness network instead of a
//! signed API record: its epoch commits one round of that network, the round number in decimal as the data, at the
//! round's own scheduled time, with the network's BLS signature of it, which the registry's verifier checks. The keeper
//! reads the round from HTTP relays it does not trust and saves it only once the registry itself has verified the
//! signature, so a relay can delay an epoch but never change it.
//!
//! The relays are asked through the chain-neutral drand client (`drand.rs`): this module is what an epoch registry adds
//! to it, the recipe, the round an epoch commits and the registry's verdict.
//!
//! Round policy, from the epoch's start time `start_time` (the timestamp of its start block) and the chain head's time
//! `now` at preparation: the epoch commits the round current at its start, floor((start_time - genesis) / period) + 1,
//! while that round is at most START_ROUND_MAX_AGE seconds old; otherwise it commits the latest scheduled round minus
//! one, which a relay has surely published, and never a round before the start round.
#[cfg(test)]
pub(crate) use crate::drand::fixture;
pub use crate::drand::{
    DrandRelays, MAX_RELAYS, RETRY_SECONDS, RUN_GAP_SECONDS, Stragglers, UNAVAILABLE_SECONDS,
    is_chain_read, loud, relay_client,
};
use crate::{
    abi::{ApiProof, Beacon, EpochRegistry as E},
    drand::{self, Breaker, Network, Trace},
    epoch::RegisteredRecipe,
    rpc::Rpc,
};
use alloy_primitives::{Address, B256, Bytes, U256};
use anyhow::{Result, bail, ensure};
use sqlx::SqlitePool;
use std::sync::Arc;

/// A saved beacon packet older than this when the keeper would sign its commit is discarded and prepared again. The
/// registry accepts a round up to 240 seconds old; the rest is the margin for the block that includes the commit.
pub const BEACON_MAX_AGE: u64 = 200;
/// The start round is committed only while it is at most this old when the epoch is prepared. Taken at the full
/// BEACON_MAX_AGE it would be discarded by the next send check before its commit could be signed.
pub const START_ROUND_MAX_AGE: u64 = BEACON_MAX_AGE - 20;
/// Nothing waits on an epoch without live paid demand, so its retries back off from RETRY_SECONDS, doubling, to this
/// long: an outage of drand costs a fetch every half minute, not every couple of seconds.
pub const BACKOFF_SECONDS: u64 = 30;
/// The table of an epoch keeper's relay circuits, as every release of it has named it.
pub const RELAY_BREAKER: Breaker = Breaker::new("epoch_relay_breaker");
/// DataTemplate.integer(1, 19): the round in decimal, without a leading zero.
const TEMPLATE: [u8; 3] = [0x04, 0x01, 0x13];
const REQUEST_PREFIX: &str = r#"["drand",""#;

/// A beacon recipe's registration in EpochEntropy: round r is scheduled at genesis + (r - 1) * period and `verifier`
/// checks its signature under `public_key`; `chain_hash` names the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeaconRegistration {
    pub verifier: Address,
    pub genesis: u64,
    pub period: u64,
    pub chain_hash: B256,
    pub public_key: Bytes,
}
impl From<Beacon> for BeaconRegistration {
    fn from(beacon: Beacon) -> Self {
        Self {
            verifier: beacon.verifier,
            genesis: beacon.genesis,
            period: beacon.period,
            chain_hash: beacon.chainHash,
            public_key: beacon.publicKey,
        }
    }
}
impl BeaconRegistration {
    /// The canonical request, which is also the body, of the recipe that registers this network.
    fn request(&self) -> String {
        format!(r#"["drand","0x{}"]"#, hex::encode(self.chain_hash))
    }
    /// The drand network the relays are asked about.
    pub fn network(&self) -> Network {
        Network {
            chain_hash: self.chain_hash,
            genesis: self.genesis,
            period: self.period,
        }
    }
    /// When a round is scheduled: genesis + (round - 1) * period, as the registry computes it.
    pub fn round_time(&self, round: u64) -> u64 {
        self.network().round_time(round)
    }
}
/// Whether a canonical request names a beacon. Only registries that can hold beacon recipes answer `beaconOf`, so the
/// keeper asks for a registration exactly when this holds.
pub fn is_beacon_request(canonical_request: &str) -> bool {
    canonical_request.starts_with(REQUEST_PREFIX)
}
/// A beacon recipe is fetched only when its registration is consistent: a verifier, a schedule with a period shorter
/// than BEACON_MAX_AGE, the canonical request and body of its chain hash and the round template. The selection's own
/// agreement with the recipe is checked by the caller.
pub fn check(id: u8, recipe: &RegisteredRecipe) -> Result<()> {
    let refused = |what: &str| {
        anyhow::anyhow!(
            "Registered recipe {id} is not a consistent beacon ({what}); refusing to fetch"
        )
    };
    let beacon = recipe
        .beacon
        .as_ref()
        .ok_or_else(|| refused("the registry has no beacon registration for it"))?;
    ensure!(
        !beacon.verifier.is_zero() && beacon.genesis > 0 && beacon.period > 0,
        refused("it has no verifier or no schedule")
    );
    // Rounds this far apart are already older than BEACON_MAX_AGE when the next one is due: every packet would be
    // discarded as stale and fetched again, and the epoch would never be published.
    ensure!(
        beacon.period < BEACON_MAX_AGE,
        refused(&format!(
            "its period of {} s is not shorter than the {BEACON_MAX_AGE} s a packet may age",
            beacon.period
        ))
    );
    let request = beacon.request();
    ensure!(
        recipe.canonical_request == request && recipe.body == request,
        refused("its request and body are not the canonical request of its chain hash")
    );
    ensure!(
        recipe.template[..] == TEMPLATE,
        refused("its data template is not a round number")
    );
    Ok(())
}
/// The round an epoch commits: see the module documentation. `None` while the network has no round to commit, before
/// its genesis, whether that is the epoch's start or the chain head.
pub fn choose_round(beacon: &BeaconRegistration, start_time: u64, now: u64) -> Option<u64> {
    if beacon.period == 0 || start_time < beacon.genesis || now < beacon.genesis {
        return None;
    }
    let start = (start_time - beacon.genesis) / beacon.period + 1;
    if now.saturating_sub(beacon.round_time(start)) <= START_ROUND_MAX_AGE {
        return Some(start);
    }
    let latest = (now - beacon.genesis) / beacon.period + 1;
    Some(latest.saturating_sub(1).max(start))
}
/// The wait after the `attempt`-th failed fetch of a beacon epoch that has no live paid demand: RETRY_SECONDS,
/// doubling up to BACKOFF_SECONDS. Live demand retries every RETRY_SECONDS whatever this says (see
/// epoch::Work::retry_due).
pub fn backoff(attempt: i64) -> u64 {
    (RETRY_SECONDS << attempt.saturating_sub(1).clamp(0, 4) as u32).min(BACKOFF_SECONDS)
}

/// Everything a background fetch of one beacon epoch's round needs. It is moved into the fetch task.
pub struct Fetcher {
    pub rpc: Rpc,
    pub client: reqwest::Client,
    pub pool: SqlitePool,
    pub relays: Vec<String>,
    /// The keeper's trusted, pinned registry: it verifies every signature before the keeper saves it.
    pub registry: Address,
    pub recipe_id: u8,
    pub recipe: Arc<RegisteredRecipe>,
    /// Where the answers of the relays that lose the race are settled once the fetch has returned.
    pub stragglers: Stragglers,
}
impl Fetcher {
    /// The attestation of the round the policy picks for an epoch starting at block `start`, from the first relay whose
    /// answer the registry verifies, within drand::FETCH_TIMEOUT altogether. Any failure to get one is retryable, and the
    /// error names each relay's reason; see `is_chain_read` for the failures that are not the relays'.
    pub async fn fetch(&self, start: u64, now: u64) -> Result<ApiProof> {
        let mut trace = Trace::default();
        match tokio::time::timeout(drand::FETCH_TIMEOUT, self.attempt(start, now, &mut trace)).await
        {
            Ok(result) => result,
            Err(_) => Err(trace.error(
                format!("No drand round within {} s", drand::FETCH_TIMEOUT.as_secs()),
                true,
            )),
        }
    }
    async fn attempt(&self, start: u64, now: u64, trace: &mut Trace) -> Result<ApiProof> {
        let beacon = self.recipe.beacon.as_ref().ok_or_else(|| {
            anyhow::anyhow!("Recipe {} is not a registered beacon", self.recipe_id)
        })?;
        trace.reading = true;
        let block = self.rpc.block(start).await;
        trace.reading = false;
        let start_time = block
            .map_err(|error| {
                drand::chain_read(format!(
                    "Epoch start block {start} could not be read: {error:#}"
                ))
            })?
            .timestamp;
        let Some(round) = choose_round(beacon, start_time, now) else {
            bail!(
                "The beacon has no round yet: its genesis {} is after the epoch start {start_time} or the chain time {now}",
                beacon.genesis
            );
        };
        let data = round.to_string();
        ensure!(
            crate::template::matches(&self.recipe.template, data.as_bytes()),
            "Round {round} does not match the beacon recipe's data template"
        );
        let Some(signature) = self
            .relays()
            .collect(
                &beacon.network(),
                round,
                now,
                |signature| self.verify(round, signature),
                trace,
            )
            .await?
        else {
            return Err(trace.error(format!("No drand relay served round {round}"), false));
        };
        Ok(ApiProof {
            timestamp: U256::from(beacon.round_time(round)),
            data: data.into_bytes().into(),
            signature: signature.to_vec().into(),
        })
    }
    /// The relays as the drand client asks them for an epoch: their circuits in the epoch keeper's table, a server error
    /// held against the relay whatever the round's age, and the registry as the verifier.
    fn relays(&self) -> drand::Client {
        drand::Client {
            http: self.client.clone(),
            pool: self.pool.clone(),
            breaker: RELAY_BREAKER,
            relays: self.relays.clone(),
            stragglers: self.stragglers.clone(),
            server_errors: false,
            verifier: "the registry",
        }
    }
    /// Whether the registry, the keeper's trusted registry, verifies this signature of `round`.
    async fn verify(&self, round: u64, signature: [u8; 64]) -> Result<bool> {
        self.rpc
            .call(
                self.registry,
                E::verifyBeaconCall {
                    recipe: self.recipe_id,
                    round,
                    signature: Bytes::copy_from_slice(&signature),
                },
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::{fixture::*, *};
    use crate::{
        drand::{FETCH_TIMEOUT, RELAY_TIMEOUT},
        epoch::{relay_open, relay_record},
    };
    use alloy_sol_types::SolCall;
    use serde_json::Value;
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    const GENESIS: u64 = 1_000_000;
    fn registration() -> BeaconRegistration {
        BeaconRegistration {
            verifier: Address::repeat_byte(0xbb),
            genesis: GENESIS,
            period: 3,
            chain_hash: B256::repeat_byte(0x11),
            public_key: Bytes::from(vec![7u8; 128]),
        }
    }
    /// The beacon recipe the registry registers for a registration.
    fn recipe(beacon: BeaconRegistration) -> RegisteredRecipe {
        RegisteredRecipe {
            canonical_request: beacon.request(),
            template: Bytes::from_static(&TEMPLATE),
            body: beacon.request(),
            beacon: Some(beacon),
        }
    }
    #[test]
    fn requests_name_their_chain_hash_in_lowercase_hex() {
        let beacon = BeaconRegistration {
            chain_hash: B256::from(U256::from(0xABCDEF_u64)),
            ..registration()
        };
        assert_eq!(
            beacon.request(),
            r#"["drand","0x0000000000000000000000000000000000000000000000000000000000abcdef"]"#
        );
        assert!(is_beacon_request(&beacon.request()));
        for other in [
            r#"["passthrough","GET","/feed/latest",[],""]"#,
            r#"["drand",["chain","x"]]"#,
            r#"["latestFeeds",[["name","ETH/USD"]]]"#,
            "",
        ] {
            assert!(!is_beacon_request(other), "{other}");
        }
    }
    #[test]
    fn round_times_follow_genesis_plus_rounds_minus_one_periods() {
        let beacon = registration();
        assert_eq!(beacon.round_time(1), GENESIS);
        assert_eq!(beacon.round_time(11), GENESIS + 30);
        // evmnet as registered: round 1 at its genesis, one round every 3 seconds.
        let evmnet = BeaconRegistration {
            genesis: 1_727_521_075,
            ..registration()
        };
        assert_eq!(evmnet.round_time(2), 1_727_521_078);
    }
    #[test]
    fn the_round_is_the_start_round_until_it_is_180_seconds_old_and_then_the_latest_minus_one() {
        let beacon = registration();
        // The start round is taken while it is 20 seconds younger than a packet may get, so that the next send check
        // does not discard it.
        assert_eq!(START_ROUND_MAX_AGE, 180);
        assert_eq!(START_ROUND_MAX_AGE, BEACON_MAX_AGE - 20);
        // The round current at the start block: round 11 is scheduled at genesis + 30, and a start block up to two
        // seconds later is still in it.
        for start in [GENESIS + 30, GENESIS + 31, GENESIS + 32] {
            assert_eq!(choose_round(&beacon, start, start), Some(11));
            assert_eq!(choose_round(&beacon, start, GENESIS + 210), Some(11));
        }
        assert_eq!(choose_round(&beacon, GENESIS + 33, GENESIS + 33), Some(12));
        // Exactly 180 seconds old still counts; one more second does not: the latest scheduled round is 71 then, and
        // 70 is the one a relay has surely published. At 200 seconds, the old bound, it is no longer the start round.
        assert_eq!(choose_round(&beacon, GENESIS + 30, GENESIS + 210), Some(11));
        assert_eq!(choose_round(&beacon, GENESIS + 30, GENESIS + 211), Some(70));
        assert_eq!(choose_round(&beacon, GENESIS + 30, GENESIS + 212), Some(70));
        assert_eq!(choose_round(&beacon, GENESIS + 30, GENESIS + 213), Some(71));
        assert_eq!(choose_round(&beacon, GENESIS + 30, GENESIS + 230), Some(76));
        assert_eq!(
            choose_round(&beacon, GENESIS + 30, GENESIS + 100_000),
            Some(33_333)
        );
        // Whatever the time, the round is at most 180 seconds old: the 20 seconds that remain of the 200 a packet may
        // age cover the commit's own preparation before the send check would discard it.
        for now in GENESIS + 30..GENESIS + 400 {
            let round = choose_round(&beacon, GENESIS + 30, now).unwrap();
            assert!(
                now - beacon.round_time(round) <= START_ROUND_MAX_AGE,
                "{now}"
            );
        }
        // A round never precedes the epoch's own: with rounds 300 seconds apart the round before the latest would.
        let slow = BeaconRegistration {
            period: 300,
            ..registration()
        };
        for now in [1500, 1750, 1800, 2099] {
            assert_eq!(choose_round(&slow, GENESIS + 1500, GENESIS + now), Some(6));
        }
        assert_eq!(choose_round(&slow, GENESIS + 1500, GENESIS + 2100), Some(7));
    }
    #[test]
    fn there_is_no_round_before_the_genesis() {
        let beacon = registration();
        assert_eq!(choose_round(&beacon, GENESIS, GENESIS), Some(1));
        // The epoch started, or the chain head is, before the network's first round.
        assert_eq!(choose_round(&beacon, GENESIS - 1, GENESIS + 100), None);
        assert_eq!(choose_round(&beacon, GENESIS, GENESIS - 1), None);
        assert_eq!(choose_round(&beacon, 0, 0), None);
        let unscheduled = BeaconRegistration {
            period: 0,
            ..registration()
        };
        assert_eq!(choose_round(&unscheduled, GENESIS, GENESIS), None);
    }
    #[test]
    fn beacon_recipes_are_fetched_only_when_consistent_with_their_registration() {
        let consistent = recipe(registration());
        check(6, &consistent).unwrap();
        let other = BeaconRegistration {
            chain_hash: B256::repeat_byte(0x22),
            ..registration()
        };
        let refused = [
            RegisteredRecipe {
                beacon: None,
                ..consistent.clone()
            },
            recipe(BeaconRegistration {
                verifier: Address::ZERO,
                ..registration()
            }),
            recipe(BeaconRegistration {
                genesis: 0,
                ..registration()
            }),
            recipe(BeaconRegistration {
                period: 0,
                ..registration()
            }),
            // Rounds this far apart are stale before the next is due: every packet would be discarded and fetched again.
            recipe(BeaconRegistration {
                period: BEACON_MAX_AGE,
                ..registration()
            }),
            recipe(BeaconRegistration {
                period: BEACON_MAX_AGE + 1,
                ..registration()
            }),
            recipe(BeaconRegistration {
                period: u64::MAX,
                ..registration()
            }),
            // The request or the body of another network, or in another spelling.
            RegisteredRecipe {
                canonical_request: other.request(),
                ..consistent.clone()
            },
            RegisteredRecipe {
                body: other.request(),
                ..consistent.clone()
            },
            RegisteredRecipe {
                body: consistent.body.replace(',', ", "),
                ..consistent.clone()
            },
            RegisteredRecipe {
                canonical_request: consistent.canonical_request.replace("0x", "0X"),
                body: consistent.body.replace("0x", "0X"),
                ..consistent.clone()
            },
            // Another data shape than a round number.
            RegisteredRecipe {
                template: Bytes::from_static(&[0x04, 0x01, 0x12]),
                ..consistent.clone()
            },
            RegisteredRecipe {
                template: Bytes::from_static(&[0x04, 0x02, 0x13]),
                ..consistent.clone()
            },
            RegisteredRecipe {
                template: Bytes::from_static(&[0x03, 0x00]),
                ..consistent.clone()
            },
            RegisteredRecipe {
                template: Bytes::new(),
                ..consistent.clone()
            },
        ];
        for recipe in refused {
            let error = check(6, &recipe).unwrap_err().to_string();
            assert!(
                error.contains("Registered recipe 6") && error.contains("refusing to fetch"),
                "{error}"
            );
        }
        // A period that is too long says so, and the longest that is not is still fetched.
        let error = check(
            6,
            &recipe(BeaconRegistration {
                period: BEACON_MAX_AGE,
                ..registration()
            }),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("period of 200 s is not shorter than the 200 s a packet may age"),
            "{error}"
        );
        check(
            6,
            &recipe(BeaconRegistration {
                period: BEACON_MAX_AGE - 1,
                ..registration()
            }),
        )
        .unwrap();
        // The template is the registry's own DataTemplate.integer(1, 19).
        assert!(crate::template::is_valid(&TEMPLATE));
        assert!(crate::template::matches(&TEMPLATE, b"1"));
        assert!(crate::template::matches(&TEMPLATE, b"9999999999999999999"));
        for round in ["0", "01", "10000000000000000000", "", "1x"] {
            assert!(
                !crate::template::matches(&TEMPLATE, round.as_bytes()),
                "{round}"
            );
        }
    }
    #[test]
    fn the_registry_registration_converts_field_for_field() {
        let beacon: BeaconRegistration = Beacon {
            verifier: Address::repeat_byte(3),
            genesis: 9,
            period: 4,
            chainHash: B256::repeat_byte(5),
            publicKey: Bytes::from_static(&[1, 2, 3]),
        }
        .into();
        assert_eq!(
            beacon,
            BeaconRegistration {
                verifier: Address::repeat_byte(3),
                genesis: 9,
                period: 4,
                chain_hash: B256::repeat_byte(5),
                public_key: Bytes::from_static(&[1, 2, 3]),
            }
        );
    }
    #[test]
    fn fetches_of_a_beacon_epoch_back_off_from_two_to_thirty_seconds_and_a_long_run_warns_every_five_minutes()
     {
        // The wait after the nth failed fetch: 2, 4, 8, 16, then 30 whatever the count.
        assert_eq!((RETRY_SECONDS, BACKOFF_SECONDS), (2, 30));
        let waits: Vec<u64> = (0..9).map(backoff).collect();
        assert_eq!(waits, [2, 2, 4, 8, 16, 30, 30, 30, 30]);
        assert_eq!(backoff(i64::MAX), 30);
        assert_eq!(backoff(i64::MIN), 2);
        // The first attempt of a work warns, whenever it was; later ones only when they are the first in five
        // minutes of the run's clock (`previous` is the failure before).
        assert!(loud(1, None, 1000));
        assert!(loud(1, Some(999), 1000));
        assert!(loud(2, None, 1000));
        assert!(!loud(2, Some(998), 1000));
        assert!(!loud(9, Some(990), 1000));
        // Attempts 30 seconds apart cross a boundary of five minutes (1200, 1500, ...) once each: one warning for
        // every five minutes that go on, on the first failure after the boundary.
        let mut warned = Vec::new();
        let mut previous = 1000;
        for (attempt, now) in (2..).zip((1000..2500).step_by(30)) {
            if loud(attempt, Some(previous), now) {
                warned.push(now);
            }
            previous = now;
        }
        assert_eq!(warned, [1210, 1510, 1810, 2110, 2410]);
    }
    /// A JSON-RPC node for the registry: block `start` at `start_time`, and `verifyBeacon` true for the one signature
    /// `valid`; the count is of `verifyBeacon` calls.
    async fn node(start: u64, start_time: u64, valid: [u8; 64]) -> (String, Arc<AtomicUsize>) {
        node_with(start, start_time, valid, 0).await
    }
    /// `node`, except that the first `failing` calls of `verifyBeacon` (which the count includes) are answered with a
    /// JSON-RPC error, as by a node that has failed.
    async fn node_with(
        start: u64,
        start_time: u64,
        valid: [u8; 64],
        failing: usize,
    ) -> (String, Arc<AtomicUsize>) {
        let verified = Arc::new(AtomicUsize::new(0));
        let counter = verified.clone();
        let (url, _) = serve(move |_, body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            let result = match request["method"].as_str().unwrap() {
                "eth_getBlockByNumber" => {
                    assert_eq!(request["params"][0], format!("0x{start:x}"));
                    serde_json::json!({"number":format!("0x{start:x}"),"hash":B256::repeat_byte(9),"timestamp":format!("0x{start_time:x}"),"baseFeePerGas":"0x1"})
                }
                "eth_call" => {
                    let data: Bytes =
                        serde_json::from_value(request["params"][0]["data"].clone()).unwrap();
                    let call = E::verifyBeaconCall::abi_decode(&data).unwrap();
                    assert_eq!(call.recipe, 6);
                    if counter.fetch_add(1, Ordering::SeqCst) < failing {
                        return answer(
                            200,
                            serde_json::json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"node failure"}}).to_string(),
                        );
                    }
                    let valid = call.signature[..] == valid;
                    serde_json::json!(Bytes::from(E::verifyBeaconCall::abi_encode_returns(
                        &valid
                    )))
                }
                method => panic!("unexpected {method}"),
            };
            answer(
                200,
                serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                    .to_string(),
            )
        })
        .await;
        (url, verified)
    }
    /// A fetcher of the test beacon: it reads the start block and the registry's verdicts from `rpc_url`, keeps its
    /// relay circuits in `pool` and asks `relays`.
    fn test_fetcher(rpc_url: &str, pool: &SqlitePool, relays: Vec<String>) -> Fetcher {
        Fetcher {
            rpc: Rpc::new(vec![rpc_url.to_owned()]).unwrap(),
            client: relay_client().unwrap(),
            pool: pool.clone(),
            relays,
            registry: Address::repeat_byte(0xaa),
            recipe_id: 6,
            recipe: Arc::new(recipe(registration())),
            stragglers: Stragglers::default(),
        }
    }
    async fn open_journal(dir: &tempfile::TempDir, name: &str) -> crate::journal::Journal {
        crate::journal::Journal::open(&dir.path().join(name), "scope")
            .await
            .unwrap()
    }
    async fn failures(pool: &SqlitePool, relay: &str) -> Option<i64> {
        sqlx::query_scalar("SELECT failures FROM epoch_relay_breaker WHERE url=?")
            .bind(relay)
            .fetch_optional(pool)
            .await
            .unwrap()
    }
    async fn is_open(pool: &SqlitePool, relay: &str) -> bool {
        relay_open(pool, relay, crate::health::now().unwrap())
            .await
            .unwrap()
            .is_some()
    }
    /// Open a relay's circuit, as of now.
    async fn open_now(pool: &SqlitePool, relay: &str) {
        let now = crate::health::now().unwrap();
        for _ in 0..3 {
            relay_record(pool, relay, true, now).await.unwrap();
        }
        assert!(is_open(pool, relay).await);
    }
    /// The epoch of these tests starts at block 500 in the time of round 101.
    const START: u64 = 500;
    const ROUND: u64 = 101;
    const ROUND_TIME: u64 = GENESIS + 3 * (ROUND - 1);
    #[tokio::test]
    async fn the_first_verified_answer_wins_and_each_relay_keeps_a_circuit_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let journal = open_journal(&dir, "relays.sqlite").await;
        let beacon = registration();
        let (rpc_url, verified) = node(START, ROUND_TIME + 1, [0x11; 64]).await;
        let line = asked_line(beacon.chain_hash, ROUND);
        // The relays, in the order they are configured: one that refuses connections, one with a wrong signature, one
        // that answers another round, one that has not published the round and, slowest, the good one.
        let refusing = refused().await;
        let (wrong, wrong_hits) = serve({
            let (line, body) = (line.clone(), round_json(ROUND, &signature(0x22)));
            move |request, _| {
                assert_eq!(request, line);
                answer(200, body.clone())
            }
        })
        .await;
        let (stale, stale_hits) = serve({
            let (line, body) = (line.clone(), round_json(ROUND - 1, &signature(0x11)));
            move |request, _| {
                assert_eq!(request, line);
                answer(200, body.clone())
            }
        })
        .await;
        let (early, early_hits) = serve(|_, _| answer(425, "")).await;
        let (right, right_hits) = serve({
            let body = round_json(ROUND, &signature(0x11));
            move |request, _| {
                assert_eq!(request, line);
                Answer {
                    delay: Duration::from_millis(400),
                    ..answer(200, body.clone())
                }
            }
        })
        .await;
        let fetcher = test_fetcher(
            &rpc_url,
            &journal.pool,
            vec![
                refusing.clone(),
                wrong.clone(),
                stale.clone(),
                early.clone(),
                right.clone(),
            ],
        );
        let attestation = fetcher.fetch(START, ROUND_TIME + 5).await.unwrap();
        // The relay that refuses connections may be slow to fail, after the winner has returned: each fetch is settled
        // before the next, so that the circuits are what the test says they are whatever the network stack does.
        fetcher.stragglers.settled().await;
        // The epoch's round, at its own scheduled time, with the signature of the relay the registry verified.
        assert_eq!(attestation.timestamp, U256::from(ROUND_TIME));
        assert_eq!(attestation.data.as_ref(), b"101");
        assert_eq!(attestation.signature.as_ref(), [0x11; 64]);
        assert!(crate::template::matches(&TEMPLATE, &attestation.data));
        // Every relay was asked once. The registry verified the wrong signature and the good one, and never the
        // answer for another round. Failures count against their relay only, and a round not yet published against none.
        for hits in [&wrong_hits, &stale_hits, &early_hits, &right_hits] {
            assert_eq!(hits.load(Ordering::SeqCst), 1);
        }
        assert_eq!(verified.load(Ordering::SeqCst), 2);
        assert_eq!(failures(&journal.pool, &wrong).await, Some(1));
        assert_eq!(failures(&journal.pool, &stale).await, Some(1));
        assert_eq!(failures(&journal.pool, &early).await, None);
        assert_eq!(failures(&journal.pool, &right).await, None);
        // Three failures in a row open a relay's circuit. An open circuit is asked only as the half-open probe of the
        // one that closes first; the closed ones are asked every time.
        for _ in 0..2 {
            fetcher.fetch(START, ROUND_TIME + 5).await.unwrap();
            fetcher.stragglers.settled().await;
        }
        for relay in [&wrong, &stale] {
            assert_eq!(failures(&journal.pool, relay).await, Some(3));
            assert!(is_open(&journal.pool, relay).await);
        }
        assert!(!is_open(&journal.pool, &early).await);
        assert!(!is_open(&journal.pool, &right).await);
        fetcher.fetch(START, ROUND_TIME + 5).await.unwrap();
        fetcher.stragglers.settled().await;
        let [wrong_asked, stale_asked, early_asked, right_asked] =
            [&wrong_hits, &stale_hits, &early_hits, &right_hits]
                .map(|hits| hits.load(Ordering::SeqCst));
        assert_eq!(
            [early_asked, right_asked],
            [4, 4],
            "a closed circuit was not asked"
        );
        // Three circuits are open, the refusing relay's too, and exactly one of them was asked again: the probe.
        let probed = [
            failures(&journal.pool, &refusing).await == Some(4),
            wrong_asked == 4,
            stale_asked == 4,
        ];
        assert_eq!(
            probed.iter().filter(|probed| **probed).count(),
            1,
            "{probed:?}: only the probe of the open circuits is asked"
        );
        // The registry's verdict, not the relay's word, decides: a relay's signature it does not verify is no round.
        let (rpc_url, _) = node(START, ROUND_TIME + 1, [0x33; 64]).await;
        let only_right = Fetcher {
            rpc: Rpc::new(vec![rpc_url]).unwrap(),
            relays: vec![right.clone()],
            ..fetcher
        };
        let error = only_right.fetch(START, ROUND_TIME + 5).await.unwrap_err();
        assert!(!is_chain_read(&error));
        let error = error.to_string();
        assert!(
            error.starts_with("No drand relay served round 101: "),
            "{error}"
        );
        assert!(
            error.contains(&format!("{right}: signature does not verify")),
            "{error}"
        );
        assert_eq!(failures(&journal.pool, &right).await, Some(1));
        journal.pool.close().await;
    }
    #[tokio::test]
    async fn an_open_circuit_is_probed_on_every_attempt_and_a_valid_answer_closes_it() {
        let dir = tempfile::tempdir().unwrap();
        let journal = open_journal(&dir, "probe.sqlite").await;
        let (rpc_url, verified) = node(START, ROUND_TIME + 1, [0x11; 64]).await;
        let body = round_json(ROUND, &signature(0x11));
        let (fast, _) = serve({
            let body = body.clone();
            move |_, _| answer(200, body.clone())
        })
        .await;
        let (recovered, recovered_hits) = serve(move |_, _| Answer {
            delay: Duration::from_millis(200),
            ..answer(200, body.clone())
        })
        .await;
        open_now(&journal.pool, &recovered).await;
        let fetcher = test_fetcher(
            &rpc_url,
            &journal.pool,
            vec![fast.clone(), recovered.clone()],
        );
        // The open relay is asked all the same, and it gives the round the fast one has already won: it is credited for
        // it, its circuit is closed and the registry is not asked about the same signature twice.
        fetcher.fetch(START, ROUND_TIME + 5).await.unwrap();
        fetcher.stragglers.settled().await;
        assert_eq!(recovered_hits.load(Ordering::SeqCst), 1);
        assert_eq!(failures(&journal.pool, &recovered).await, None);
        assert_eq!(verified.load(Ordering::SeqCst), 1);
        // Alone, an open circuit is asked as well, and if it wins the round it is closed.
        open_now(&journal.pool, &recovered).await;
        let alone = Fetcher {
            relays: vec![recovered.clone()],
            ..fetcher
        };
        let attestation = alone.fetch(START, ROUND_TIME + 5).await.unwrap();
        assert_eq!(attestation.signature.as_ref(), [0x11; 64]);
        assert_eq!(recovered_hits.load(Ordering::SeqCst), 2);
        assert_eq!(failures(&journal.pool, &recovered).await, None);
        assert!(!is_open(&journal.pool, &recovered).await);
        journal.pool.close().await;
    }
    /// One fetch that a relay wins at once while seven others answer 200 milliseconds later, or never, `after` seconds
    /// after the round's time. Returns how long the fetch took, the failures each relay has counted once the fetch has
    /// settled, in the order below, and how often the registry was asked.
    async fn late_answers(after: u64) -> (Duration, Vec<(&'static str, Option<i64>)>, usize) {
        let dir = tempfile::tempdir().unwrap();
        let journal = open_journal(&dir, "late.sqlite").await;
        let (rpc_url, verified) = node(START, ROUND_TIME, [0x11; 64]).await;
        let late = |status: u16, body: String| {
            serve(move |_, _| Answer {
                delay: Duration::from_millis(200),
                ..answer(status, body.clone())
            })
        };
        let good = round_json(ROUND, &signature(0x11));
        let (winner, _) = serve({
            let good = good.clone();
            move |_, _| answer(200, good.clone())
        })
        .await;
        let (agrees, _) = late(200, good.clone()).await;
        let (disagrees, _) = late(200, round_json(ROUND, &signature(0x22))).await;
        let (other_round, _) = late(200, round_json(ROUND - 1, &signature(0x11))).await;
        let (errors, _) = late(503, String::new()).await;
        let (early, _) = late(425, String::new()).await;
        let (missing, _) = late(404, String::new()).await;
        let (silent, _) = serve(|_, _| Answer {
            delay: Duration::from_secs(30),
            ..answer(200, "")
        })
        .await;
        // The relay that agrees has failed twice before: the round it serves late resets its count.
        let now = crate::health::now().unwrap();
        for _ in 0..2 {
            relay_record(&journal.pool, &agrees, true, now)
                .await
                .unwrap();
        }
        let labelled = [
            ("winner", &winner),
            ("agrees", &agrees),
            ("disagrees", &disagrees),
            ("other round", &other_round),
            ("errors", &errors),
            ("early", &early),
            ("missing", &missing),
            ("silent", &silent),
        ];
        let fetcher = test_fetcher(
            &rpc_url,
            &journal.pool,
            labelled.iter().map(|(_, relay)| (*relay).clone()).collect(),
        );
        let started = tokio::time::Instant::now();
        let attestation = fetcher.fetch(START, ROUND_TIME + after).await.unwrap();
        let took = started.elapsed();
        assert_eq!(attestation.signature.as_ref(), [0x11; 64]);
        // The others are heard afterwards, at most RELAY_TIMEOUT after the fetch began.
        fetcher.stragglers.settled().await;
        assert!(started.elapsed() < RELAY_TIMEOUT + Duration::from_secs(2));
        let mut counted = Vec::new();
        for (label, relay) in labelled {
            counted.push((label, failures(&journal.pool, relay).await));
        }
        journal.pool.close().await;
        (took, counted, verified.load(Ordering::SeqCst))
    }
    #[tokio::test]
    async fn the_relays_that_lose_the_race_are_credited_afterwards_without_holding_the_fetch() {
        // Five seconds after the round's time it may still be on its way to a relay, at 60 it is due.
        let ((recent_took, recent, recent_verified), (due_took, due, due_verified)) =
            tokio::join!(late_answers(5), late_answers(60));
        // The fetch returns with the winner, without waiting for the relay that never answers.
        for took in [recent_took, due_took] {
            assert!(took < Duration::from_secs(3), "{took:?}");
        }
        // The registry verified the winner's signature and nothing else: the same bytes from another relay are the
        // same answer, and other bytes are wrong, since a signature is unique.
        assert_eq!((recent_verified, due_verified), (1, 1));
        // Identical bytes are a success, which resets a count of failures; different bytes, another round, and an
        // error are failures. "Not published" and silence count for nothing while the round is recent, and against the
        // relay once it is due.
        assert_eq!(
            recent,
            [
                ("winner", None),
                ("agrees", None),
                ("disagrees", Some(1)),
                ("other round", Some(1)),
                ("errors", Some(1)),
                ("early", None),
                ("missing", None),
                ("silent", None),
            ]
        );
        assert_eq!(
            due,
            [
                ("winner", None),
                ("agrees", None),
                ("disagrees", Some(1)),
                ("other round", Some(1)),
                ("errors", Some(1)),
                ("early", Some(1)),
                ("missing", Some(1)),
                ("silent", Some(1)),
            ]
        );
    }
    #[tokio::test]
    async fn the_relays_left_over_by_a_fetch_are_aborted_and_never_outlive_it() {
        let dir = tempfile::tempdir().unwrap();
        let journal = open_journal(&dir, "abort.sqlite").await;
        let (rpc_url, _) = node(START, ROUND_TIME, [0x11; 64]).await;
        let good = round_json(ROUND, &signature(0x11));
        let (winner, _) = serve(move |_, _| answer(200, good.clone())).await;
        let (silent, silent_hits) = serve(|_, _| Answer {
            delay: Duration::from_secs(30),
            ..answer(200, "")
        })
        .await;
        let fetcher = test_fetcher(&rpc_url, &journal.pool, vec![winner, silent.clone()]);
        // The round is long due, so the relay that stays silent would be failed at the time limit.
        fetcher.fetch(START, ROUND_TIME + 60).await.unwrap();
        // Its request reaches the relay that stays silent, whatever the order of the two.
        for _ in 0..200 {
            if silent_hits.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(silent_hits.load(Ordering::SeqCst), 1);
        // The publisher aborts what is left when it goes: nothing is recorded, and nothing waits for the time limit.
        let started = tokio::time::Instant::now();
        fetcher.stragglers.abort();
        fetcher.stragglers.settled().await;
        assert!(started.elapsed() < Duration::from_secs(2));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(failures(&journal.pool, &silent).await, None);
        journal.pool.close().await;
    }
    #[tokio::test]
    async fn relays_that_give_the_same_signature_cost_the_registry_one_call() {
        let dir = tempfile::tempdir().unwrap();
        let journal = open_journal(&dir, "cache.sqlite").await;
        // Three relays give the same wrong signature at once: it is verified once, and each of them fails.
        let (rpc_url, verified) = node(START, ROUND_TIME, [0x11; 64]).await;
        let wrong = round_json(ROUND, &signature(0x22));
        let mut same = Vec::new();
        for _ in 0..3 {
            let wrong = wrong.clone();
            same.push(serve(move |_, _| answer(200, wrong.clone())).await.0);
        }
        let error = test_fetcher(&rpc_url, &journal.pool, same.clone())
            .fetch(START, ROUND_TIME + 5)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(verified.load(Ordering::SeqCst), 1);
        for relay in &same {
            assert!(
                error.contains(&format!("{relay}: signature does not verify")),
                "{error}"
            );
            assert_eq!(failures(&journal.pool, relay).await, Some(1));
        }
        // A verdict is remembered, an error is not: the registry failed to answer for the first relay's signature and
        // is asked again for the second relay's, which is the same.
        let (rpc_url, verified) = node_with(START, ROUND_TIME, [0x11; 64], 1).await;
        let good = round_json(ROUND, &signature(0x11));
        let mut same = Vec::new();
        for _ in 0..2 {
            let good = good.clone();
            same.push(serve(move |_, _| answer(200, good.clone())).await.0);
        }
        let fetcher = test_fetcher(&rpc_url, &journal.pool, same.clone());
        let attestation = fetcher.fetch(START, ROUND_TIME + 5).await.unwrap();
        assert_eq!(attestation.signature.as_ref(), [0x11; 64]);
        assert_eq!(verified.load(Ordering::SeqCst), 2);
        fetcher.stragglers.settled().await;
        for relay in &same {
            assert_eq!(failures(&journal.pool, relay).await, None);
        }
        journal.pool.close().await;
    }
    #[tokio::test]
    async fn a_fetch_without_a_round_says_why_per_relay_and_never_counts_a_late_round_against_one()
    {
        let dir = tempfile::tempdir().unwrap();
        let journal = open_journal(&dir, "none.sqlite").await;
        let (start, round_time) = (500, GENESIS + 300);
        let (rpc_url, verified) = node(start, round_time, [0x11; 64]).await;
        let (early, _) = serve(|_, _| answer(425, "")).await;
        let (missing, _) = serve(|_, _| answer(404, "")).await;
        let fetcher = test_fetcher(
            &rpc_url,
            &journal.pool,
            vec![early.clone(), missing.clone()],
        );
        for _ in 0..5 {
            let error = fetcher
                .fetch(start, round_time + 5)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.starts_with("No drand relay served round 101: "),
                "{error}"
            );
            for relay in [&early, &missing] {
                assert!(
                    error.contains(&format!("{relay}: round not published yet")),
                    "{error}"
                );
            }
        }
        // Five rounds of "not yet" opened nothing and asked the registry nothing.
        assert_eq!(failures(&journal.pool, &early).await, None);
        assert_eq!(failures(&journal.pool, &missing).await, None);
        assert_eq!(verified.load(Ordering::SeqCst), 0);
        // Before the network's genesis there is no round to ask for at all.
        let error = fetcher
            .fetch(start, GENESIS - 1)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("The beacon has no round yet"), "{error}");
        // A recipe that is not a registered beacon is never fetched.
        let plain = Fetcher {
            recipe: Arc::new(RegisteredRecipe {
                beacon: None,
                ..recipe(registration())
            }),
            ..fetcher
        };
        assert!(plain.fetch(start, round_time).await.is_err());
        journal.pool.close().await;
    }
    #[tokio::test]
    async fn a_relay_that_does_not_serve_a_round_that_is_due_is_at_fault_and_one_that_lacks_a_recent_round_is_not()
     {
        let dir = tempfile::tempdir().unwrap();
        let journal = open_journal(&dir, "due.sqlite").await;
        let (rpc_url, verified) = node(START, ROUND_TIME, [0x11; 64]).await;
        let (early, early_hits) = serve(|_, _| answer(425, "")).await;
        let (missing, missing_hits) = serve(|_, _| answer(404, "")).await;
        let fetcher = test_fetcher(
            &rpc_url,
            &journal.pool,
            vec![early.clone(), missing.clone()],
        );
        // Up to two periods after its time, 6 seconds, the round may still be on its way to the relay.
        for _ in 0..3 {
            let error = fetcher
                .fetch(START, ROUND_TIME + 6)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("round not published yet"), "{error}");
        }
        assert_eq!(failures(&journal.pool, &early).await, None);
        assert_eq!(failures(&journal.pool, &missing).await, None);
        // A second later it is due, and each answer of "not published" is that relay's failure; the third opens its
        // circuit, so a relay that answers 404 for every round is found out.
        for count in 1..=3 {
            let error = fetcher
                .fetch(START, ROUND_TIME + 7)
                .await
                .unwrap_err()
                .to_string();
            for relay in [&early, &missing] {
                assert!(
                    error.contains(&format!("{relay}: round already due but not served")),
                    "{error}"
                );
                assert_eq!(failures(&journal.pool, relay).await, Some(count));
            }
        }
        assert!(is_open(&journal.pool, &early).await && is_open(&journal.pool, &missing).await);
        // Both circuits are open: one probe is made, of the relay that closes first, and it fails again.
        let asked = early_hits.load(Ordering::SeqCst) + missing_hits.load(Ordering::SeqCst);
        fetcher.fetch(START, ROUND_TIME + 7).await.unwrap_err();
        assert_eq!(
            early_hits.load(Ordering::SeqCst) + missing_hits.load(Ordering::SeqCst),
            asked + 1
        );
        assert_eq!(verified.load(Ordering::SeqCst), 0);
        journal.pool.close().await;
    }
    #[tokio::test]
    async fn a_failure_of_the_keepers_own_chain_reads_is_not_blamed_on_the_relays() {
        let dir = tempfile::tempdir().unwrap();
        let journal = open_journal(&dir, "chain.sqlite").await;
        let good = round_json(ROUND, &signature(0x11));
        let (relay, relay_hits) = serve(move |_, _| answer(200, good.clone())).await;
        // The epoch's start block cannot be read: no relay is asked, and the failure is the chain's.
        let (rpc_url, _) = serve(|_, body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            answer(
                200,
                serde_json::json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"node failure"}}).to_string(),
            )
        })
        .await;
        let error = test_fetcher(&rpc_url, &journal.pool, vec![relay.clone()])
            .fetch(START, ROUND_TIME + 5)
            .await
            .unwrap_err();
        assert!(is_chain_read(&error));
        assert!(
            error
                .to_string()
                .starts_with("Epoch start block 500 could not be read: "),
            "{error}"
        );
        assert_eq!(relay_hits.load(Ordering::SeqCst), 0);
        // The registry cannot say whether the signature is valid: it is unverified, not wrong, so the relay that gave
        // it is not blamed and the failure is the chain's again.
        let (rpc_url, verified) = node_with(START, ROUND_TIME, [0x11; 64], usize::MAX).await;
        let error = test_fetcher(&rpc_url, &journal.pool, vec![relay.clone()])
            .fetch(START, ROUND_TIME + 5)
            .await
            .unwrap_err();
        assert!(is_chain_read(&error));
        let error = error.to_string();
        assert!(
            error.starts_with("No drand relay served round 101: ")
                && error.contains(&format!("{relay}: signature not verified: ")),
            "{error}"
        );
        assert_eq!(verified.load(Ordering::SeqCst), 1);
        assert_eq!(failures(&journal.pool, &relay).await, None);
        // A failure of the relays alone is not: relays that answer with errors, or that have no round.
        let (rpc_url, _) = node(START, ROUND_TIME, [0x11; 64]).await;
        let (down, _) = serve(|_, _| answer(503, "")).await;
        let error = test_fetcher(&rpc_url, &journal.pool, vec![down.clone()])
            .fetch(START, ROUND_TIME + 5)
            .await
            .unwrap_err();
        assert!(!is_chain_read(&error));
        assert!(error.to_string().contains(&format!("{down}: HTTP 503")));
        assert_eq!(failures(&journal.pool, &down).await, Some(1));
        journal.pool.close().await;
    }
    #[tokio::test]
    async fn a_registry_that_never_answers_ends_the_fetch_at_its_bound_with_the_reasons_so_far() {
        let dir = tempfile::tempdir().unwrap();
        let journal = open_journal(&dir, "slow.sqlite").await;
        let (start, round_time) = (500, GENESIS + 300);
        // A node that answers the start block and then never the verification, beside a relay that has not the round yet.
        let (rpc_url, _) = serve(move |_, body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            let block = serde_json::json!({"number":format!("0x{start:x}"),"hash":B256::repeat_byte(9),"timestamp":format!("0x{round_time:x}"),"baseFeePerGas":"0x1"});
            let answer = |result: Value, delay| Answer {
                delay,
                ..answer(
                    200,
                    serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                        .to_string(),
                )
            };
            match request["method"].as_str().unwrap() {
                "eth_getBlockByNumber" => answer(block, Duration::ZERO),
                _ => answer(serde_json::json!("0x"), Duration::from_secs(60)),
            }
        })
        .await;
        let (early, _) = serve(|_, _| answer(425, "")).await;
        // The right relay answers late, so that the early one's reason is heard before the verification that never ends.
        let (right, _) = serve({
            let body = round_json(101, &signature(0x11));
            move |_, _| Answer {
                delay: Duration::from_secs(2),
                ..answer(200, body.clone())
            }
        })
        .await;
        let fetcher = test_fetcher(&rpc_url, &journal.pool, vec![early.clone(), right.clone()]);
        let started = tokio::time::Instant::now();
        let error = fetcher.fetch(start, round_time + 5).await.unwrap_err();
        assert!(
            started.elapsed() >= FETCH_TIMEOUT
                && started.elapsed() < FETCH_TIMEOUT + Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        // It was waiting for the chain, not for a relay.
        assert!(is_chain_read(&error));
        let error = error.to_string();
        assert!(error.starts_with("No drand round within 8 s: "), "{error}");
        assert!(
            error.contains(&format!("{early}: round not published yet")),
            "{error}"
        );
        // Neither a relay that could not be verified nor one that had no round is at fault.
        assert_eq!(failures(&journal.pool, &early).await, None);
        assert_eq!(failures(&journal.pool, &right).await, None);
        journal.pool.close().await;
    }
}
