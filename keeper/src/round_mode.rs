//! Round mode against the scripted chain (keeper tasks K1 and K2): a keeper configured with COORDINATOR_KIND=round,
//! before a round coordinator. It starts on the round coordinator's own reads, journals every live request with the round
//! it is bound to, answers round demand, fetches and verifies each round once the wall clock says it is due, takes its
//! publishing right from the coordinator's keeper roles, recovers from a replaced block with the round coordinator's
//! reads, and never asks an epoch coordinator's function or a registry anything. How it serves requests (task K3) is in
//! `round_serving`.
use crate::{
    abi::{Coordinator as C, EpochRegistry as E},
    abi_round::{RoundCoordinator as R, RoundRequest},
    config::{ChainSettings, Config, CoordinatorKind, FinalityMode, GasModel},
    journal::{DemandedRound, Journal},
    rig::{Rig, Tweak},
    scripted::{self, RoundBook},
    soft_finality::{LAG, ROBINHOOD_TESTNET},
    worker::Worker,
};
use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::{SolCall, SolEvent, SolValue};

/// The chain settings of a round coordinator's keeper on Robinhood Chain: soft finality, the arbitrum gas model and the
/// round coordinator. The rig's environment is an epoch keeper's (it pins the registry, as every keeper of 0.4.1 does),
/// so the change also drops the registry's pins, which the loader refuses with COORDINATOR_KIND=round.
pub(crate) struct RoundMode;
impl Tweak for RoundMode {
    fn apply(&self, config: &mut Config) {
        config.chain = round_settings();
        config.registry_implementation_code_hash = None;
        config.approved_next_registry_implementation_code_hash = None;
    }
    fn outside_chain_policy(&self) -> bool {
        true
    }
}
/// `RoundMode` that keeps the registry pin of the rig's environment: a configuration the loader would refuse.
struct RoundModeWithRegistryPin;
impl Tweak for RoundModeWithRegistryPin {
    fn apply(&self, config: &mut Config) {
        config.chain = round_settings();
    }
    fn outside_chain_policy(&self) -> bool {
        true
    }
}
fn round_settings() -> ChainSettings {
    ChainSettings {
        finality_mode: FinalityMode::Soft,
        gas_model: GasModel::Arbitrum,
        coordinator_kind: CoordinatorKind::Round,
        ..ChainSettings::default()
    }
}

/// A round coordinator's keeper on its rig, before a round coordinator whose finalized head is far behind.
pub(crate) async fn round_rig() -> Rig {
    let rig = Rig::new(ROBINHOOD_TESTNET, RoundMode).await;
    rig.chain(|chain| {
        chain.round_coordinator();
        chain.finalized_lag = LAG;
        chain.safe_lag = LAG - 10;
    });
    rig
}
/// The round book of the rig's chain.
pub(crate) fn book(rig: &Rig) -> RoundBook {
    rig.chain(|chain| chain.round.clone().expect("a round coordinator"))
}
/// The journal of the rig's keeper, as a journal.
pub(crate) async fn journal(rig: &Rig) -> Journal {
    Journal {
        pool: rig.journal().await,
    }
}

/// The calls of an epoch coordinator or a registry in trace lines: any function of `EpochRegistry`, the epoch
/// coordinator's own functions (`getRequest`, `epochRegistry`, `confirmationBlocks`, the one-argument
/// `getProofContext` and its two fulfillments), and anything asked of the registry. Selectors are matched as the trace
/// prints them, `0x` and eight hex digits, whatever name the trace gives them. No function of the round coordinator has
/// any of these selectors (`abi_round` tests it), so the gate needs no exception: a round keeper's calls, the drand
/// relays it asks included, pass it, and an epoch keeper's are caught.
pub(crate) fn epoch_calls(lines: &[String]) -> Vec<String> {
    let selectors: Vec<String> = E::EpochRegistryCalls::SELECTORS
        .iter()
        .chain(&[
            C::getRequestCall::SELECTOR,
            C::epochRegistryCall::SELECTOR,
            C::confirmationBlocksCall::SELECTOR,
            C::getProofContextCall::SELECTOR,
            C::fulfillRandomnessCall::SELECTOR,
            C::fulfillRandomnessBatchCall::SELECTOR,
        ])
        .map(|selector| format!("0x{}", hex::encode(selector)))
        .collect();
    lines
        .iter()
        .filter(|line| {
            selectors
                .iter()
                .any(|selector| line.contains(selector.as_str()))
                || line
                    .split_whitespace()
                    .any(|word| word == "registry" || word == "registry_implementation")
        })
        .cloned()
        .collect()
}
pub(crate) fn assert_no_epoch_call(what: &str, lines: &[String]) {
    let found = epoch_calls(lines);
    assert!(
        found.is_empty(),
        "{what}: a round keeper asked an epoch coordinator's function or a registry: {found:#?}"
    );
}
/// The calls that sign, send or price a transaction.
pub(crate) fn sends(lines: &[String]) -> Vec<&String> {
    lines
        .iter()
        .filter(|line| {
            line.contains("eth_sendRawTransaction")
                || line.contains("eth_estimateGas")
                || line.contains("gasEstimateL1Component")
        })
        .collect()
}

#[tokio::test]
async fn the_gate_finds_every_call_an_epoch_keeper_makes_of_the_registry_and_the_epoch_coordinator()
{
    // The gate is only as good as what it catches: an epoch keeper's startup asks epochRegistry, confirmationBlocks, the
    // registry's committer and catalog, and the registry's code.
    let rig = Rig::new(
        ROBINHOOD_TESTNET,
        crate::soft_finality::settings(FinalityMode::Soft, 0),
    )
    .await
    .unheld();
    let found = epoch_calls(&rig.start().await);
    for name in [
        "epochRegistry",
        "confirmationBlocks",
        "committer",
        "catalogHash",
        "registry_implementation",
    ] {
        assert!(
            found.iter().any(|line| line.contains(name)),
            "{name}: {found:#?}"
        );
    }
    // And a getRequest, by selector, whatever the trace calls it.
    let line = format!(
        "eth_call latest coordinator 0x{} unknown args=[1]",
        hex::encode(C::getRequestCall::SELECTOR)
    );
    assert_eq!(epoch_calls(std::slice::from_ref(&line)), vec![line]);
    // The round coordinator's own functions pass, those that share a selector with an epoch coordinator's included.
    for (selector, name) in [
        (R::getRoundRequestCall::SELECTOR, "getRoundRequest"),
        (R::nextRequestIdCall::SELECTOR, "nextRequestId"),
        (
            R::getPendingRequestIdsCall::SELECTOR,
            "getPendingRequestIds",
        ),
        (R::keeperCall::SELECTOR, "keeper"),
        (R::getBeaconCall::SELECTOR, "getBeacon"),
        (R::checkRoundSignatureCall::SELECTOR, "checkRoundSignature"),
        (R::roundRandomnessCall::SELECTOR, "roundRandomness"),
    ] {
        let line = format!(
            "eth_call latest coordinator 0x{} {name}",
            hex::encode(selector)
        );
        assert!(epoch_calls(&[line]).is_empty(), "{name}");
    }
    // The registry's own beacon reads are caught by selector, whatever the trace calls them and whatever is asked.
    for selector in [E::beaconOfCall::SELECTOR, E::verifyBeaconCall::SELECTOR] {
        let line = format!(
            "eth_call latest coordinator 0x{} unknown args=[0]",
            hex::encode(selector)
        );
        assert_eq!(epoch_calls(std::slice::from_ref(&line)), vec![line]);
    }
    // A drand relay asked for a round is a round keeper's call as much as an epoch keeper's.
    assert!(epoch_calls(&["relay GET /<chain hash>/public/7".to_owned()]).is_empty());
}

#[tokio::test]
async fn startup_reads_the_round_coordinator_alone_and_pins_its_key() {
    let _exclusive = crate::golden::exclusive().await;
    let rig = round_rig().await;
    let startup = rig.start().await;
    // The endpoint and code probe, the coordinator's pins (no registry), the VRF key and the configuration pin, the round
    // coordinator's facts and how many beacons it has, each beacon, its keeper, the pins again, and the decision head:
    // nothing else.
    let expected = [
        "eth_chainId",
        "eth_getCode latest coordinator",
        "{",
        "  eth_getCode latest coordinator",
        "  eth_getStorageAt latest coordinator slot=eip1967.implementation",
        "}",
        "eth_getCode latest coordinator_implementation",
        "batch [",
        "  eth_getStorageAt latest coordinator slot=eip1967.implementation",
        "  eth_getCode latest coordinator",
        "  eth_getCode latest coordinator_implementation",
        "]",
        "eth_call latest coordinator 0xfa6df55d publicKeyX",
        "eth_call latest coordinator 0xd7a6f6e8 publicKeyY",
        "eth_call latest coordinator 0x155cf49b protocolConfigurationHash",
        "{",
        "  eth_call latest coordinator 0x463116f7 beaconSchedule",
        "  eth_call latest coordinator 0x61728f39 keyHash",
        "  eth_call latest coordinator 0x7201fa80 beaconCount",
        "  eth_call latest coordinator 0x7ce91411 pricing",
        "  eth_call latest coordinator 0x9da45184 ROUND_LEAD",
        "}",
        "eth_call latest coordinator 0x3e27abc4 getBeacon args=[0]",
        "eth_call latest coordinator 0xaced1661 keeper",
        "batch [",
        "  eth_getStorageAt latest coordinator slot=eip1967.implementation",
        "  eth_getCode latest coordinator",
        "  eth_getCode latest coordinator_implementation",
        "]",
        "eth_getBlockByNumber latest full=false",
    ];
    assert_eq!(startup, expected, "{startup:#?}");
    assert_no_epoch_call("startup", &startup);
    // The selectors the trace shows are the round coordinator's.
    for (selector, name) in [
        (R::publicKeyXCall::SELECTOR, "publicKeyX"),
        (R::publicKeyYCall::SELECTOR, "publicKeyY"),
        (
            R::protocolConfigurationHashCall::SELECTOR,
            "protocolConfigurationHash",
        ),
        (R::beaconScheduleCall::SELECTOR, "beaconSchedule"),
        (R::keyHashCall::SELECTOR, "keyHash"),
        (R::pricingCall::SELECTOR, "pricing"),
        (R::ROUND_LEADCall::SELECTOR, "ROUND_LEAD"),
        (R::beaconCountCall::SELECTOR, "beaconCount"),
        (R::keeperCall::SELECTOR, "keeper"),
    ] {
        let shown = format!("0x{} {name}", hex::encode(selector));
        assert!(startup.iter().any(|line| line.ends_with(&shown)), "{shown}");
    }
}

#[tokio::test]
async fn startup_refuses_a_coordinator_that_holds_another_key_or_is_not_a_round_coordinator() {
    // Another key hash: the coordinator does not hold the operator's VRF key.
    let rig = round_rig().await.unheld();
    rig.chain(|chain| chain.round.as_mut().unwrap().key_hash = Some(B256::repeat_byte(7)));
    rig.node.hold(std::time::Duration::ZERO);
    let error = Worker::new(rig.config(false)).await.err().unwrap();
    assert!(
        format!("{error:#}").contains("Round coordinator key hash does not match the VRF key"),
        "{error:#}"
    );
    // An epoch coordinator: it does not answer the round coordinator's facts, and the keeper does not start. It asked
    // the epoch coordinator nothing of its own on the way.
    let rig = Rig::new(ROBINHOOD_TESTNET, RoundMode).await.unheld();
    rig.node.hold(std::time::Duration::ZERO);
    rig.node.take();
    let error = Worker::new(rig.config(false)).await.err().unwrap();
    let asked = scripted::render(&rig.node.take());
    assert!(
        format!("{error:#}").contains("All configured RPC endpoints failed eth_call"),
        "{error:#}"
    );
    assert_no_epoch_call("startup before an epoch coordinator", &asked);
}

#[tokio::test]
async fn startup_refuses_a_registry_pin_before_it_asks_the_chain_anything() {
    let rig = Rig::new(ROBINHOOD_TESTNET, RoundModeWithRegistryPin)
        .await
        .unheld();
    rig.chain(|chain| chain.round_coordinator());
    let config = rig.config(false);
    assert!(config.registry_implementation_code_hash.is_some());
    rig.node.take();
    let error = Worker::new(config).await.err().unwrap();
    assert!(
        format!("{error:#}").contains("its keeper takes no registry implementation pin"),
        "{error:#}"
    );
    assert_eq!(rig.node.asked(), 0);
}

#[tokio::test]
async fn discovery_journals_every_request_with_the_round_its_events_name() {
    let rig = round_rig().await.unheld();
    let book = book(&rig);
    let first = rig.request();
    rig.chain(|chain| chain.mine(1));
    let second = rig.request_with(250_000);
    rig.chain(|chain| chain.mine(4));
    let third = rig.request();
    rig.chain(|chain| chain.mine(1));
    let run = rig.run(false, false).await;
    assert!(
        run.journal.contains(&format!(
            "jobs [{first}=pending {second}=pending {third}=pending]"
        )),
        "{}",
        run.journal
    );
    assert_no_epoch_call("discovery", &run.tick);
    assert!(
        run.tick.iter().any(|line| line.contains("getRoundRequest")),
        "{:#?}",
        run.tick
    );
    // The chain announced each request twice: RandomnessRequested, then RoundAssigned with its beacon and round.
    let logs = rig.chain(|chain| chain.logs.clone());
    let topic = |log: &serde_json::Value, index: usize| -> B256 {
        serde_json::from_value(log["topics"][index].clone()).unwrap()
    };
    let requested: Vec<u64> = logs
        .iter()
        .filter(|log| topic(log, 0) == R::RandomnessRequested::SIGNATURE_HASH)
        .map(|log| U256::from_be_bytes(topic(log, 1).0).to::<u64>())
        .collect();
    assert_eq!(requested, vec![first, second, third]);
    let assigned: Vec<(u64, u8, u64)> = logs
        .iter()
        .filter(|log| topic(log, 0) == R::RoundAssigned::SIGNATURE_HASH)
        .map(|log| {
            (
                U256::from_be_bytes(topic(log, 1).0).to::<u64>(),
                U256::from_be_bytes(topic(log, 2).0).to::<u8>(),
                U256::from_be_bytes(topic(log, 3).0).to::<u64>(),
            )
        })
        .collect();
    assert_eq!(assigned.len(), 3);
    let journal = journal(&rig).await;
    for (id, beacon, round) in assigned {
        let found = journal
            .round_assignment(&id.to_string())
            .await
            .unwrap()
            .expect("a round row");
        assert_eq!((found.beacon, found.round), (beacon, round), "request {id}");
        // The round is the first scheduled ROUND_LEAD seconds after the request block's time.
        let block = rig.chain(|chain| chain.requests[&id].request_block);
        assert_eq!(round, book.round_at(rig.chain(|chain| chain.time(block))));
        // The fingerprint is the one of the request as getRoundRequest reads it.
        let view = rig.chain(|chain| chain.round_view(id).unwrap());
        let request = RoundRequest::abi_decode(&view.abi_encode()).unwrap();
        assert_eq!(
            found.fingerprint,
            crate::round::fingerprint(&request).to_string()
        );
        // The scripted chain's clock is far ahead of the wall clock: the lag is first sight less the block's time.
        assert!(found.sealing_lag_ms < 0);
    }
    journal.pool.close().await;
    // A second tick discovers nothing new, since the cursor is past the three: it reads no page of pending ids, and
    // reads the three open requests again in one batch to see whether the chain has settled them.
    let run = rig.run(false, false).await;
    assert!(
        !run.tick
            .iter()
            .any(|line| line.contains("getPendingRequestIds")),
        "{:#?}",
        run.tick
    );
    let reads: Vec<&String> = run
        .tick
        .iter()
        .filter(|line| line.contains("getRoundRequest"))
        .collect();
    assert_eq!(reads.len(), 3, "{:#?}", run.tick);
    for (line, id) in reads.iter().zip([first, second, third]) {
        assert!(
            line.starts_with("  ") && line.ends_with(&format!("getRoundRequest args=[{id}]")),
            "{:#?}",
            run.tick
        );
    }
    assert_no_epoch_call("the second tick", &run.tick);
}

#[tokio::test]
async fn round_demand_is_the_set_of_rounds_live_requests_wait_on() {
    let rig = round_rig().await.unheld();
    let book = book(&rig);
    // Two requests in one block share a round; a request a period later binds the next one.
    let a = rig.request();
    let b = rig.request();
    rig.chain(|chain| chain.mine(book.period));
    let c = rig.request();
    rig.chain(|chain| chain.mine(1));
    rig.run(false, false).await;
    let round_of = |id: u64| {
        rig.chain(|chain| {
            let block = chain.requests[&id].request_block;
            book.round_at(chain.time(block))
        })
    };
    let deadline_of = |id: u64| {
        rig.chain(|chain| chain.time(chain.requests[&id].request_block))
            + crate::config::RESPONSE_TIMEOUT_SECONDS
    };
    assert_eq!(round_of(a), round_of(b));
    assert_eq!(round_of(c), round_of(a) + 1);
    let now = rig.chain(|chain| chain.time(chain.head));
    let journal = journal(&rig).await;
    assert_eq!(
        journal.round_demand(now).await.unwrap(),
        vec![
            DemandedRound {
                beacon: 0,
                round: round_of(a),
                requests: 2,
                earliest_deadline: deadline_of(a),
            },
            DemandedRound {
                beacon: 0,
                round: round_of(c),
                requests: 1,
                earliest_deadline: deadline_of(c),
            },
        ]
    );
    journal.pool.close().await;
    // Another submitter serves request c: its round has no live request left once the keeper has seen it.
    rig.chain(|chain| {
        let block = chain.head;
        chain.fulfil(c, block, Address::repeat_byte(0x77));
        chain.mine(1);
    });
    let run = rig.run(false, false).await;
    assert!(
        run.journal
            .contains(&format!("jobs [{a}=pending {b}=pending {c}=served]")),
        "{}",
        run.journal
    );
    let journal = journal_now(&rig).await;
    assert_eq!(
        journal.0.round_demand(journal.1).await.unwrap(),
        vec![DemandedRound {
            beacon: 0,
            round: round_of(a),
            requests: 2,
            earliest_deadline: deadline_of(a),
        }]
    );
    journal.0.pool.close().await;
    // Past their deadlines the requests expire, and no round is awaited.
    rig.chain(|chain| chain.mine(crate::config::RESPONSE_TIMEOUT_SECONDS + 1));
    let run = rig.run(false, false).await;
    assert!(
        run.journal
            .contains(&format!("jobs [{a}=expired {b}=expired {c}=served]")),
        "{}",
        run.journal
    );
    let journal = journal_now(&rig).await;
    assert!(journal.0.round_demand(journal.1).await.unwrap().is_empty());
    journal.0.pool.close().await;
}
/// The journal, and the chain's time at its head.
async fn journal_now(rig: &Rig) -> (Journal, u64) {
    (
        journal(rig).await,
        rig.chain(|chain| chain.time(chain.head)),
    )
}

#[tokio::test]
async fn the_coordinators_keeper_roles_decide_the_publishing_right() {
    // The primary: the coordinator's keeper is the transaction wallet.
    let rig = round_rig().await.unheld();
    let run = rig.run(true, true).await;
    let startup = run.startup.unwrap();
    assert!(startup.iter().any(|line| line.ends_with("keeper")));
    assert!(!startup.iter().any(|line| line.contains("isBackupKeeper")));
    assert!(
        !run.journal.contains("wallet_unauthorized"),
        "{}",
        run.journal
    );
    // The coordinator names another keeper: the primary starts with sending disabled, and says so in a round
    // coordinator's words.
    rig.chain(|chain| chain.committer = Address::repeat_byte(0x91));
    let run = rig.run(true, true).await;
    let startup = run.startup.unwrap();
    assert!(
        startup
            .iter()
            .any(|line| line.ends_with("isBackupKeeper args=[keeper]")),
        "{startup:#?}"
    );
    assert!(
        run.journal.contains(
            "wallet_unauthorized:The coordinator's keeper is not the primary keeper wallet"
        ),
        "{}",
        run.journal
    );
    // A primary whose wallet is an allowed backup keeper runs the wrong role, and does not start.
    rig.chain(|chain| {
        chain.backups.insert(chain.keeper);
    });
    rig.node.hold(std::time::Duration::ZERO);
    let error = Worker::new(rig.config(true)).await.err().unwrap();
    assert!(
        format!("{error:#}").contains(
            "The primary transaction wallet is an allowed backup keeper, not the coordinator's keeper; run it with KEEPER_ROLE=follower"
        ),
        "{error:#}"
    );

    // A follower: an allowed backup keeper beside the coordinator's keeper.
    let rig = round_rig().await.unheld().follower();
    let id = rig.request();
    rig.chain(|chain| chain.mine(1));
    let run = rig.run(true, true).await;
    let startup = run.startup.unwrap();
    assert!(
        startup
            .iter()
            .any(|line| line.ends_with("isBackupKeeper args=[keeper]")),
        "{startup:#?}"
    );
    assert!(
        !run.journal.contains("wallet_unauthorized"),
        "{}",
        run.journal
    );
    assert!(
        run.journal.contains(&format!("jobs [{id}=pending]")),
        "{}",
        run.journal
    );
    // Its tick reads the keeper's nonce to judge it, and re-reads its oldest job with getRoundRequest once it is older
    // than half the liveness window.
    rig.chain(|chain| chain.mine(8));
    let run = rig.run(true, false).await;
    assert!(
        run.tick
            .iter()
            .any(|line| line.starts_with("eth_getTransactionCount") && line.ends_with("primary")),
        "{:#?}",
        run.tick
    );
    assert!(
        run.tick
            .iter()
            .any(|line| line.contains(&format!("getRoundRequest args=[{id}]"))),
        "{:#?}",
        run.tick
    );
    assert_no_epoch_call("a follower's tick", &run.tick);
    // A follower that holds the coordinator's keeper wallet runs the wrong role, and does not start.
    rig.chain(|chain| chain.committer = chain.keeper);
    rig.node.hold(std::time::Duration::ZERO);
    let error = Worker::new(rig.config(true)).await.err().unwrap();
    assert!(
        format!("{error:#}").contains(
            "The follower transaction wallet is the coordinator's keeper; run it with KEEPER_ROLE=primary"
        ),
        "{error:#}"
    );
    // The coordinator's role events wake the role check as the registry's do.
    for event in [
        R::KeeperChanged::SIGNATURE_HASH,
        R::BackupKeeperSet::SIGNATURE_HASH,
    ] {
        assert_eq!(
            crate::events::classify(Some(&event)),
            crate::events::Kind::Role
        );
    }
    assert_eq!(
        crate::events::classify(Some(&R::RoundAssigned::SIGNATURE_HASH)),
        crate::events::Kind::Work
    );
}

#[tokio::test]
async fn a_round_keeper_recovers_from_a_replaced_block_with_the_round_coordinators_reads_alone() {
    let mut rig = round_rig().await.unheld();
    rig.add_endpoint().await;
    rig.chain(|chain| chain.mine(2));
    let process = rig.process(false, false).await;
    process.tick().await.unwrap();
    // A request, discovered and journaled with its round.
    let id = rig.request();
    let block = rig.chain(|chain| chain.head);
    rig.chain(|chain| chain.mine(1));
    let run = process.tick().await.unwrap();
    assert!(
        run.journal.contains(&format!("jobs [{id}=pending]")),
        "{}",
        run.journal
    );
    // The sequencer loses the block of the request: the coordinator has no request of that id any more.
    rig.chain(|chain| chain.drop_blocks(block, false));
    let mut ticks = Vec::new();
    let worker = process.worker();
    for _ in 0..6 {
        let run = process.tick().await.unwrap();
        ticks.extend(run.tick);
        if worker.journal.finality_mismatch().await.unwrap().is_none()
            && worker
                .journal
                .meta(crate::journal::LAST_RECOVERY_KEY)
                .await
                .unwrap()
                .is_some()
        {
            break;
        }
    }
    assert!(worker.journal.finality_mismatch().await.unwrap().is_none());
    let last: serde_json::Value = serde_json::from_str(
        &worker
            .journal
            .meta(crate::journal::LAST_RECOVERY_KEY)
            .await
            .unwrap()
            .expect("a recovery was recorded"),
    )
    .unwrap();
    assert_eq!(last["coordinator"], "round");
    // The job of the vanished request is settled from the chain as it is: the coordinator refuses to read it, so it is
    // a request the coordinator does not have, and `vanished`, not expired (design C, 3.7).
    assert_eq!(last["done"]["jobs_settled"], 1, "{last}");
    assert_eq!(last["done"]["epochs_reopened"], 0, "{last}");
    let job = worker.journal.job(&id.to_string()).await.unwrap().unwrap();
    assert_eq!(job.state, "vanished");
    // The recovery read the round coordinator's requests and its request count, and nothing of an epoch coordinator.
    assert!(
        ticks.iter().any(|line| line.contains("getRoundRequest")),
        "{ticks:#?}"
    );
    assert!(
        ticks.iter().any(|line| line.contains("nextRequestId")),
        "{ticks:#?}"
    );
    assert_no_epoch_call("the recovery", &ticks);
    process.stop().await;
}

#[tokio::test]
async fn a_round_keeper_makes_no_call_of_an_epoch_coordinator_or_a_registry() {
    let mut lines = Vec::new();
    // A primary: startup, discovery over pages of requests, settlement seen on chain, expiry, and a queued sweep.
    let rig = round_rig().await.unheld();
    let run = rig.run(true, true).await;
    lines.extend(run.startup.unwrap());
    lines.extend(run.tick);
    for _ in 0..20 {
        rig.request();
    }
    rig.chain(|chain| chain.mine(2));
    let served = rig.request();
    rig.chain(|chain| {
        let head = chain.head;
        chain.fulfil(served, head, Address::repeat_byte(0x77));
        chain.mine(1);
    });
    rig.queue_sweep("1").await;
    for _ in 0..2 {
        lines.extend(rig.run(true, false).await.tick);
    }
    rig.chain(|chain| chain.mine(crate::config::RESPONSE_TIMEOUT_SECONDS + 1));
    // A backlog of more than a page: discovery searches for the first live request with getRoundRequest.
    for _ in 0..300 {
        rig.request();
    }
    rig.chain(|chain| chain.mine(crate::config::RESPONSE_TIMEOUT_SECONDS + 1));
    rig.request();
    rig.chain(|chain| chain.mine(1));
    let run = rig.run(true, false).await;
    assert!(
        run.journal.contains("cursor=323"),
        "the backlog was passed: {}",
        run.journal
    );
    lines.extend(run.tick);
    // A follower beside the coordinator's keeper, which stays up across ticks.
    let rig = round_rig().await.unheld().follower();
    let process = rig.process(true, true).await;
    lines.extend(process.startup.clone());
    rig.request();
    rig.chain(|chain| chain.mine(8));
    for _ in 0..3 {
        lines.extend(process.tick().await.unwrap().tick);
        rig.chain(|chain| chain.mine(1));
    }
    process.stop().await;
    assert!(lines.len() > 50, "{}", lines.len());
    assert_no_epoch_call("a round keeper's processes", &lines);
}

// The round lane (task K2). The scripted chain's clock is far ahead of the wall clock, so each test sets the clock the
// lane schedules by; the scripted relays serve a round once the chain's time has reached it.

/// The clock of a process's round lane.
pub(crate) fn lane_clock(process: &crate::rig::Process<'_>) -> crate::round::Clock {
    process
        .worker()
        .round_lane_of()
        .expect("a round lane")
        .clock()
        .clone()
}
/// The beacon and round request `id` is bound to, and the round's scheduled time in seconds.
pub(crate) fn bound(rig: &Rig, id: u64) -> (u8, u64, u64) {
    let book = book(rig);
    let (beacon, round) = rig.chain(|chain| chain.requests[&id].round.expect("a round request"));
    (beacon, round, book.genesis + (round - 1) * book.period)
}
/// Mine until the chain's time has reached `time`: its relays serve the rounds scheduled until then.
pub(crate) fn mine_to_time(rig: &Rig, time: u64) {
    rig.chain(|chain| {
        let now = chain.time(chain.head);
        chain.mine(time.saturating_sub(now) + 1);
    });
}
/// The round lane's row of a round.
pub(crate) async fn row(rig: &Rig, beacon: u8, round: u64) -> Option<crate::round::Work> {
    let pool = rig.journal().await;
    let row = crate::round::work(&pool, beacon, round).await.unwrap();
    pool.close().await;
    row
}
/// The failures a relay's circuit counts in the round keeper's table.
async fn circuit(rig: &Rig, relay: &str) -> Option<i64> {
    let pool = rig.journal().await;
    let failures = sqlx::query_scalar("SELECT failures FROM drand_relay_breaker WHERE url=?")
        .bind(relay)
        .fetch_optional(&pool)
        .await
        .unwrap();
    pool.close().await;
    failures
}
/// How often the lines ask a relay for `round`.
pub(crate) fn relay_asks(lines: &[String], round: u64) -> usize {
    let asked = format!(" GET /<chain hash>/public/{round}");
    lines
        .iter()
        .filter(|line| line.trim_start().starts_with("relay") && line.ends_with(&asked))
        .count()
}
/// How often the lines ask the coordinator for a call of this name.
pub(crate) fn asks(lines: &[String], name: &str) -> usize {
    lines
        .iter()
        .filter(|line| line.contains(&format!(" {name}")))
        .count()
}
/// The signature and randomness a verified row of `round` holds.
fn verified(round: u64) -> (Option<String>, Option<String>) {
    let signature = scripted::round_signature(round);
    (
        Some(alloy_primitives::Bytes::copy_from_slice(&signature).to_string()),
        Some(crate::round::randomness(&signature).to_string()),
    )
}

#[tokio::test]
async fn a_round_is_fetched_when_the_wall_clock_reaches_its_time_and_not_a_millisecond_before() {
    let rig = round_rig().await.unheld();
    let process = rig.process(false, false).await;
    let clock = lane_clock(&process);
    let id = rig.request();
    let (beacon, round, due) = bound(&rig, id);
    // The relays already have the round: only the keeper's clock holds the fetch back.
    mine_to_time(&rig, due);
    clock.set(due * 1_000 - 1);
    let run = process.tick().await.unwrap();
    assert_eq!(relay_asks(&run.tick, round), 0, "{:#?}", run.tick);
    let pending = row(&rig, beacon, round).await.expect("a row for the round");
    assert_eq!((pending.state.as_str(), pending.attempts), ("pending", 0));
    // At its time, every relay is asked, and the coordinator verifies the signature with the gas written down.
    clock.set(due * 1_000);
    let run = process.tick().await.unwrap();
    assert_eq!(relay_asks(&run.tick, round), 1, "{:#?}", run.tick);
    let checked: Vec<&String> = run
        .tick
        .iter()
        .filter(|line| line.contains("checkRoundSignature"))
        .collect();
    assert_eq!(checked.len(), 1, "{:#?}", run.tick);
    assert!(checked[0].contains(" gas=0xf4240 "), "{}", checked[0]);
    // The coordinator was asked first whether it has the round, at the decision head.
    assert_eq!(asks(&run.tick, "roundRandomness"), 1, "{:#?}", run.tick);
    let done = row(&rig, beacon, round).await.unwrap();
    assert_eq!(done.state, "verified");
    assert_eq!((done.signature, done.randomness), verified(round));
    assert_eq!((done.attempts, done.failing_since), (1, None));
    // A verified round is not fetched again.
    clock.set(due * 1_000 + 5_000);
    let run = process.tick().await.unwrap();
    assert_eq!(relay_asks(&run.tick, round), 0, "{:#?}", run.tick);
    assert_no_epoch_call("the round lane", &run.tick);
    process.stop().await;
}

#[tokio::test]
async fn the_first_signature_the_coordinator_accepts_wins_and_a_forged_one_counts_against_its_relay()
 {
    let mut rig = round_rig().await.unheld();
    let good = rig.add_relay().await;
    let forging = rig.relays()[0].clone();
    forging.set(scripted::Mode::Forging);
    let process = rig.process(false, false).await;
    let clock = lane_clock(&process);
    let id = rig.request();
    let (beacon, round, due) = bound(&rig, id);
    mine_to_time(&rig, due);
    clock.set(due * 1_000);
    let run = process.tick().await.unwrap();
    process.worker().round_lane_of().unwrap().settled().await;
    // Both relays were asked at once; the round's own signature is the one kept, whichever relay answered first.
    assert_eq!(relay_asks(&run.tick, round), 2, "{:#?}", run.tick);
    let done = row(&rig, beacon, round).await.unwrap();
    assert_eq!(done.state, "verified");
    assert_eq!((done.signature, done.randomness), verified(round));
    // The forged signature is the forging relay's failure, whether the coordinator refused it or it differed from the
    // one the coordinator accepted; the relay that served the round is not at fault.
    assert_eq!(circuit(&rig, &forging.url).await, Some(1));
    assert_eq!(circuit(&rig, &good.url).await, None);
    assert!((1..=2).contains(&asks(&run.tick, "checkRoundSignature")));
    // A forging relay alone serves no round: the row stays pending, retried two seconds later.
    good.set(scripted::Mode::Down);
    let next = rig.request();
    let (_, next_round, next_due) = bound(&rig, next);
    assert_ne!(next_round, round);
    mine_to_time(&rig, next_due);
    clock.set(next_due * 1_000);
    process.tick().await.unwrap();
    let failed = row(&rig, beacon, next_round).await.unwrap();
    assert_eq!(failed.state, "pending");
    assert_eq!(failed.retry_at, i64::try_from(next_due + 2).unwrap());
    let error = failed.last_error.unwrap();
    assert!(error.contains("signature does not verify"), "{error}");
    assert!(!failed.failed_rpc);
    process.stop().await;
}

#[tokio::test]
async fn a_server_error_is_excused_while_the_round_is_recent_and_counted_once_it_is_due() {
    let mut rig = round_rig().await.unheld();
    let good = rig.add_relay().await;
    let down = rig.relays()[0].clone();
    down.set(scripted::Mode::Down);
    let process = rig.process(false, false).await;
    let clock = lane_clock(&process);
    // One relay answers HTTP 500, the other serves the round: verified, and the relay with the error is excused.
    let id = rig.request();
    let (beacon, round, due) = bound(&rig, id);
    mine_to_time(&rig, due);
    clock.set(due * 1_000);
    process.tick().await.unwrap();
    process.worker().round_lane_of().unwrap().settled().await;
    assert_eq!(row(&rig, beacon, round).await.unwrap().state, "verified");
    assert_eq!(circuit(&rig, &down.url).await, None);
    // Both relays answer HTTP 500 for a round that is recent: the fetch fails and neither relay is at fault.
    good.set(scripted::Mode::Down);
    let next = rig.request();
    let (_, next_round, next_due) = bound(&rig, next);
    mine_to_time(&rig, next_due);
    clock.set(next_due * 1_000);
    process.tick().await.unwrap();
    let failed = row(&rig, beacon, next_round).await.unwrap();
    assert_eq!(failed.state, "pending");
    let error = failed.last_error.unwrap();
    assert!(
        error.contains("HTTP 500, the round may not be published yet"),
        "{error}"
    );
    assert_eq!(circuit(&rig, &down.url).await, None);
    assert_eq!(circuit(&rig, &good.url).await, None);
    // More than two periods after its time the round is due, and each server error is its relay's failure.
    clock.set((next_due + 7) * 1_000);
    process.tick().await.unwrap();
    assert_eq!(circuit(&rig, &down.url).await, Some(1));
    assert_eq!(circuit(&rig, &good.url).await, Some(1));
    process.stop().await;
}

#[tokio::test]
async fn a_failing_round_is_retried_every_two_seconds_and_is_unavailable_after_ten() {
    let rig = round_rig().await.unheld();
    let relay = rig.relays()[0].clone();
    relay.set(scripted::Mode::Down);
    let process = rig.process(false, false).await;
    let clock = lane_clock(&process);
    let id = rig.request();
    let (beacon, round, due) = bound(&rig, id);
    mine_to_time(&rig, due);
    let mut asked = Vec::new();
    let mut faults = Vec::new();
    for second in 0..=12 {
        clock.set((due + second) * 1_000);
        let run = process.tick().await.unwrap();
        if relay_asks(&run.tick, round) > 0 {
            asked.push(second);
        }
        if run.journal.contains("\"round_unavailable\"") {
            faults.push(second);
        }
    }
    // A fetch every two seconds while the request waits, never in between.
    assert_eq!(asked, [0, 2, 4, 6, 8, 10, 12]);
    // The fault stands once the failures have gone on for ten seconds, and not before.
    assert_eq!(faults, [10, 11, 12]);
    let failing = row(&rig, beacon, round).await.unwrap();
    assert_eq!(failing.failing_since, Some(i64::try_from(due).unwrap()));
    assert_eq!(failing.attempts, 7);
    // The relay serves the round again: it is verified and the fault clears.
    relay.set(scripted::Mode::Up);
    clock.set((due + 14) * 1_000);
    process.tick().await.unwrap();
    assert_eq!(row(&rig, beacon, round).await.unwrap().state, "verified");
    clock.set((due + 15) * 1_000);
    let run = process.tick().await.unwrap();
    assert!(
        run.journal
            .contains("health healthy=true faults=[] send_enabled=false"),
        "{}",
        run.journal
    );
    process.stop().await;
    // Without live demand there is no stall to report: a round whose requests expired is no fault, and its row goes.
    let process = rig.process(false, false).await;
    let clock = lane_clock(&process);
    relay.set(scripted::Mode::Down);
    let id = rig.request();
    let (beacon, round, due) = bound(&rig, id);
    mine_to_time(&rig, due);
    for second in [0, 2, 4, 6, 8, 10] {
        clock.set((due + second) * 1_000);
        process.tick().await.unwrap();
    }
    assert!(row(&rig, beacon, round).await.is_some());
    rig.chain(|chain| chain.mine(crate::config::RESPONSE_TIMEOUT_SECONDS + 1));
    clock.set((due + 12) * 1_000);
    let run = process.tick().await.unwrap();
    assert!(
        run.journal.contains(&format!("{id}=expired"))
            && !run.journal.contains("round_unavailable"),
        "{}",
        run.journal
    );
    assert_eq!(row(&rig, beacon, round).await, None);
    process.stop().await;
}

#[tokio::test]
async fn a_round_the_keeper_cannot_check_on_the_chain_is_a_round_rpc_error_not_unavailable() {
    let rig = round_rig().await.unheld();
    let process = rig.process(false, false).await;
    let clock = lane_clock(&process);
    let id = rig.request();
    let (beacon, round, due) = bound(&rig, id);
    mine_to_time(&rig, due);
    // The relays serve the round, but the coordinator's verdict on its signature cannot be read.
    rig.chain(|chain| chain.fail_round_check = true);
    let mut faults = Vec::new();
    for second in 0..=12 {
        clock.set((due + second) * 1_000);
        let run = process.tick().await.unwrap();
        faults.push((
            run.journal.contains("\"round_rpc_error\""),
            run.journal.contains("\"round_unavailable\""),
        ));
    }
    let failing = row(&rig, beacon, round).await.unwrap();
    assert!(failing.failed_rpc, "{failing:?}");
    // After ten seconds of failures the fault is the keeper's own reads, and never drand's.
    assert!(
        faults[..10].iter().all(|fault| *fault == (false, false)),
        "{faults:?}"
    );
    assert!(
        faults[10..].iter().all(|fault| *fault == (true, false)),
        "{faults:?}"
    );
    // The relays alone fail now: the same wait is drand's, and the fault is the other one once that fetch has ended.
    rig.chain(|chain| chain.fail_round_check = false);
    rig.relays()[0].set(scripted::Mode::Down);
    clock.set((due + 14) * 1_000);
    process.tick().await.unwrap();
    clock.set((due + 15) * 1_000);
    let run = process.tick().await.unwrap();
    assert!(
        run.journal.contains("\"round_unavailable\"") && !run.journal.contains("round_rpc_error"),
        "{}",
        run.journal
    );
    // Served at last: neither stands.
    rig.relays()[0].set(scripted::Mode::Up);
    clock.set((due + 16) * 1_000);
    process.tick().await.unwrap();
    clock.set((due + 17) * 1_000);
    let run = process.tick().await.unwrap();
    assert!(
        !run.journal.contains("round_unavailable") && !run.journal.contains("round_rpc_error"),
        "{}",
        run.journal
    );
    process.stop().await;
}

#[tokio::test]
async fn at_most_two_rounds_are_fetched_at_once() {
    let rig = round_rig().await.unheld();
    let book = book(&rig);
    let process = rig.process(false, false).await;
    let clock = lane_clock(&process);
    // Three requests, each bound to its own round.
    let mut rounds = Vec::new();
    for _ in 0..3 {
        let id = rig.request();
        rounds.push(bound(&rig, id));
        rig.chain(|chain| chain.mine(book.period));
    }
    let latest = rounds.iter().map(|(_, _, due)| *due).max().unwrap();
    mine_to_time(&rig, latest);
    clock.set(latest * 1_000);
    // The two whose requests' deadlines come first are fetched; the third waits for a free fetch.
    let run = process.tick().await.unwrap();
    let asked: Vec<usize> = rounds
        .iter()
        .map(|(_, round, _)| relay_asks(&run.tick, *round))
        .collect();
    assert_eq!(asked, [1, 1, 0], "{:#?}", run.tick);
    let run = process.tick().await.unwrap();
    let asked: Vec<usize> = rounds
        .iter()
        .map(|(_, round, _)| relay_asks(&run.tick, *round))
        .collect();
    assert_eq!(asked, [0, 0, 1], "{:#?}", run.tick);
    for (beacon, round, _) in rounds {
        assert_eq!(row(&rig, beacon, round).await.unwrap().state, "verified");
    }
    assert_eq!(crate::round::MAX_FETCHES, 2);
    process.stop().await;
}

#[tokio::test]
async fn a_round_the_coordinator_has_verified_is_taken_from_its_event_without_asking_a_relay() {
    let rig = round_rig().await.unheld();
    let process = rig.process(false, false).await;
    let clock = lane_clock(&process);
    let id = rig.request();
    let (beacon, round, due) = bound(&rig, id);
    mine_to_time(&rig, due);
    // Another keeper's fulfillment verified the round; the relays are down.
    rig.chain(|chain| {
        let block = chain.head;
        chain.verify_round(beacon, round, block);
        chain.mine(1);
    });
    rig.relays()[0].set(scripted::Mode::Down);
    clock.set(due * 1_000);
    let run = process.tick().await.unwrap();
    assert_eq!(relay_asks(&run.tick, round), 0, "{:#?}", run.tick);
    assert_eq!(asks(&run.tick, "checkRoundSignature"), 0, "{:#?}", run.tick);
    // The round's randomness at the decision head, the oldest request of the round for its block, and the round's event
    // from that block on.
    assert_eq!(asks(&run.tick, "roundRandomness"), 1, "{:#?}", run.tick);
    assert!(
        run.tick
            .iter()
            .any(|line| line.ends_with(&format!("getRoundRequest args=[{id}]"))),
        "{:#?}",
        run.tick
    );
    let logs: Vec<&String> = run
        .tick
        .iter()
        .filter(|line| line.contains("eth_getLogs"))
        .collect();
    let topic = format!("{:?}", R::RoundVerified::SIGNATURE_HASH);
    assert!(logs.iter().any(|line| line.contains(&topic)), "{logs:#?}");
    let done = row(&rig, beacon, round).await.unwrap();
    assert_eq!(done.state, "verified");
    assert_eq!((done.signature, done.randomness), verified(round));
    assert_no_epoch_call("a round taken from the chain", &run.tick);
    process.stop().await;
}

#[tokio::test]
async fn a_beacon_a_request_names_that_startup_did_not_read_is_read_when_its_round_is_due() {
    let rig = round_rig().await.unheld();
    let process = rig.process(false, false).await;
    let clock = lane_clock(&process);
    // After startup, the coordinator registers a second beacon and switches to it.
    rig.chain(|chain| {
        let book = chain.round.as_mut().unwrap();
        book.beacons = 2;
        book.beacon = 1;
    });
    let id = rig.request();
    let (beacon, round, due) = bound(&rig, id);
    assert_eq!(beacon, 1);
    mine_to_time(&rig, due);
    clock.set(due * 1_000);
    let run = process.tick().await.unwrap();
    assert!(
        run.tick
            .iter()
            .any(|line| line.ends_with("getBeacon args=[1]")),
        "{:#?}",
        run.tick
    );
    assert_eq!(row(&rig, beacon, round).await.unwrap().state, "verified");
    // It is read once, and kept for the process.
    let next = rig.request();
    let (_, next_round, next_due) = bound(&rig, next);
    mine_to_time(&rig, next_due);
    clock.set(next_due * 1_000);
    let run = process.tick().await.unwrap();
    assert_eq!(asks(&run.tick, "getBeacon"), 0, "{:#?}", run.tick);
    assert_eq!(
        row(&rig, beacon, next_round).await.unwrap().state,
        "verified"
    );
    process.stop().await;
}

#[tokio::test]
async fn the_sealing_lag_is_measured_when_the_request_blocks_header_arrived() {
    let rig = round_rig().await.unheld();
    let process = rig.process(false, false).await;
    // A request in the block that is the decision head: its header arrives with the tick's head read. The process saw the
    // header of that block 1.25 seconds after its time (as a pushed head would tell it), before the tick.
    let id = rig.request();
    let block = rig.chain(|chain| chain.head);
    let time = rig.chain(|chain| chain.time(block));
    process
        .worker()
        .signals()
        .header(block, time, time * 1_000 + 1_250);
    process.tick().await.unwrap();
    let journal = journal(&rig).await;
    let found = journal
        .round_assignment(&id.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.sealing_lag_ms, 1_250);
    // A request whose block's header this process never saw: its lag is measured at discovery, by the wall clock, which
    // is far behind the scripted chain's.
    rig.chain(|chain| chain.mine(1));
    let other = rig.request();
    rig.chain(|chain| chain.mine(2));
    process.tick().await.unwrap();
    let found = journal
        .round_assignment(&other.to_string())
        .await
        .unwrap()
        .unwrap();
    assert!(
        found.sealing_lag_ms < -1_000_000_000,
        "{}",
        found.sealing_lag_ms
    );
    journal.pool.close().await;
    process.stop().await;
}

#[tokio::test]
async fn a_round_keepers_lane_makes_no_call_of_an_epoch_coordinator_or_a_registry() {
    let mut rig = round_rig().await.unheld();
    rig.add_relay().await;
    rig.relays()[1].set(scripted::Mode::Forging);
    let process = rig.process(false, false).await;
    let clock = lane_clock(&process);
    let mut lines = process.startup.clone();
    // A round served by the relays, one that fails and is retried, one the coordinator has already, and one of a beacon
    // startup did not read.
    let first = rig.request();
    let (_, _, due) = bound(&rig, first);
    mine_to_time(&rig, due);
    clock.set(due * 1_000);
    lines.extend(process.tick().await.unwrap().tick);
    rig.relays()[0].set(scripted::Mode::Down);
    let second = rig.request();
    let (_, _, due) = bound(&rig, second);
    mine_to_time(&rig, due);
    for second in [0, 2] {
        clock.set((due + second) * 1_000);
        lines.extend(process.tick().await.unwrap().tick);
    }
    let third = rig.request();
    let (beacon, round, due) = bound(&rig, third);
    mine_to_time(&rig, due);
    rig.chain(|chain| {
        let block = chain.head;
        chain.verify_round(beacon, round, block);
        chain.mine(1);
    });
    clock.set(due * 1_000);
    lines.extend(process.tick().await.unwrap().tick);
    rig.relays()[0].set(scripted::Mode::Up);
    rig.chain(|chain| {
        let book = chain.round.as_mut().unwrap();
        book.beacons = 2;
        book.beacon = 1;
    });
    let fourth = rig.request();
    let (_, _, due) = bound(&rig, fourth);
    mine_to_time(&rig, due);
    clock.set(due * 1_000);
    lines.extend(process.tick().await.unwrap().tick);
    process.stop().await;
    for name in [
        "checkRoundSignature",
        "roundRandomness",
        "eth_getLogs",
        "getBeacon args=[1]",
    ] {
        assert!(
            lines.iter().any(|line| line.contains(name)),
            "{name}: {lines:#?}"
        );
    }
    assert!(
        lines
            .iter()
            .any(|line| line.trim_start().starts_with("relay"))
    );
    assert_no_epoch_call("a round keeper's lane", &lines);
    assert!(sends(&lines).is_empty(), "{:#?}", sends(&lines));
}
