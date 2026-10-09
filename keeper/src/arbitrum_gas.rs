//! The arbitrum gas model (GAS_MODEL=arbitrum) against the scripted chain: what an Arbitrum chain charges before a
//! transaction runs, its L1 component, is part of every cancellation and of every fulfillment's floor.
//!
//! The L1 components are the ones measured on Robinhood Chain on 3 October 2026 by `NodeInterface.gasEstimateL1Component`:
//! 173 gas for an empty transaction, 732 for a 452-byte fulfillment on mainnet, and 6,214 and 26,134 on the testnet,
//! where a self transfer needs 27,559 gas (21,363 on mainnet) from the node's own estimate.
use crate::{
    abi::NodeInterface,
    config::{ChainSettings, Config, GasModel},
    rig::{Logs, Rig, Tweak},
    rpc::{NODE_INTERFACE, Rpc},
    scripted::Chain,
};
use alloy_primitives::Address;
use alloy_sol_types::SolCall;

const ROBINHOOD_TESTNET: u64 = 46_630;
/// Payloads of a fulfillment of one request, and of a batch of two and of three, in bytes.
const SINGLE: usize = 452;
const BATCH_OF_TWO: usize = 1_028;
const BATCH_OF_THREE: usize = 1_476;

/// The settings under test replace the ones the loader built. They isolate the gas model from the rest of Robinhood
/// Chain's policy, which the loader would refuse for this network.
impl Tweak for ChainSettings {
    fn apply(&self, config: &mut Config) {
        config.chain = *self;
    }
    fn outside_chain_policy(&self) -> bool {
        true
    }
}
fn arbitrum() -> ChainSettings {
    ChainSettings {
        gas_model: GasModel::Arbitrum,
        ..ChainSettings::default()
    }
}
/// The gas limit in a transaction the node was sent, as the scripted chain lists it.
fn gas_of(sent: &str) -> u64 {
    sent.split_whitespace()
        .find_map(|word| word.strip_prefix("gas="))
        .unwrap()
        .parse()
        .unwrap()
}
/// How many times the keeper asked the NodeInterface for an L1 component during a tick.
fn l1_reads(tick: &[String]) -> usize {
    tick.iter()
        .filter(|line| line.contains("gasEstimateL1Component"))
        .count()
}
/// Whether the keeper estimated the zero-value self transfer of a cancellation during a tick.
fn estimated_transfer(tick: &[String]) -> bool {
    tick.iter()
        .any(|line| line.starts_with("eth_estimateGas - keeper transfer from=keeper"))
}
/// A published epoch and a request that is ready to be proved.
fn ready(rig: &Rig) -> u64 {
    rig.chain(|chain| {
        chain.publish_epoch(1, chain.head);
        chain.mine(1);
    });
    let id = rig.request();
    rig.chain(|chain| chain.mine(3));
    id
}
/// A fulfillment that the sequencer never includes, for a request that is refunded meanwhile: the keeper cancels the
/// fulfillment's nonce. The tick that does it, and the cancellation it sent.
async fn cancellation(rig: &Rig) -> (Vec<String>, String) {
    let id = ready(rig);
    rig.run(true, false).await;
    assert_eq!(rig.sent().len(), 1, "the fulfillment is sent first");
    rig.chain(|chain| {
        chain.drop_queue();
        chain.requests.get_mut(&id).unwrap().refunded = true;
        chain.mine(1);
    });
    let run = rig.run(true, false).await;
    (run.tick, rig.sent().pop().unwrap())
}

#[test]
fn the_node_interface_is_at_0xc8_and_its_l1_component_has_the_selector_the_design_names() {
    assert_eq!(
        NodeInterface::gasEstimateL1ComponentCall::SELECTOR,
        [0x77, 0xd4, 0x88, 0xa2]
    );
    assert_eq!(
        NODE_INTERFACE,
        "0x00000000000000000000000000000000000000c8"
            .parse::<Address>()
            .unwrap()
    );
}

#[test]
fn the_call_and_its_answer_are_what_robinhood_chain_speaks() {
    // The call as ethers encodes it for a transfer to 0x11...11 with no data and with the data 0x010203, and the
    // answers the public RPCs gave to such a call on 3 October 2026: the testnet charged 6,418 gas of L1 data for an
    // empty transaction, the mainnet none at that moment.
    let call = |data: &[u8]| {
        let call = NodeInterface::gasEstimateL1ComponentCall {
            to: Address::repeat_byte(0x11),
            contractCreation: false,
            data: data.to_vec().into(),
        };
        format!("0x{}", hex::encode(call.abi_encode()))
    };
    let head = "0x77d488a200000000000000000000000011111111111111111111111111111111111111110000000000000000000000000000000000000000000000000000000000000000";
    let offset = "0000000000000000000000000000000000000000000000000000000000000060";
    let zero = "0".repeat(64);
    assert_eq!(call(&[]), format!("{head}{offset}{zero}"));
    assert_eq!(
        call(&[1, 2, 3]),
        format!(
            "{head}{offset}{}{}{}",
            "0".repeat(63) + "3",
            "010203",
            "0".repeat(58)
        )
    );
    let answer = |words: &str| {
        NodeInterface::gasEstimateL1ComponentCall::abi_decode_returns(&hex::decode(words).unwrap())
            .unwrap()
    };
    let testnet = answer(
        "000000000000000000000000000000000000000000000000000000000000191200000000000000000000000000000000000000000000000000000000009896800000000000000000000000000000000000000000000000000000000001841c3c",
    );
    assert_eq!(
        (
            testnet.gasEstimateForL1,
            testnet.baseFee,
            testnet.l1BaseFeeEstimate
        ),
        (
            6_418,
            alloy_primitives::U256::from(10_000_000u64),
            alloy_primitives::U256::from(25_435_196u64)
        )
    );
    let mainnet = answer(
        "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000016387a00000000000000000000000000000000000000000000000000000000000000000",
    );
    assert_eq!(mainnet.gasEstimateForL1, 0);
}

#[tokio::test]
async fn the_l1_component_is_read_from_the_node_interface_at_the_latest_block() {
    let rig = Rig::new(ROBINHOOD_TESTNET, arbitrum()).await.unheld();
    rig.chain(|chain: &mut Chain| {
        chain.l1_gas = |len| match len {
            0 => 6_214,
            452 => 26_134,
            7_332 => 327_332,
            _ => 1,
        }
    });
    let rpc = Rpc::new(vec![rig.node.url.clone()]).unwrap();
    let to = Address::repeat_byte(0xc0);
    for (payload, l1) in [(0, 6_214), (452, 26_134), (7_332, 327_332), (3, 1)] {
        assert_eq!(rpc.l1_gas(to, &vec![7u8; payload]).await.unwrap(), l1);
    }
    let lines = crate::scripted::render(&rig.node.take());
    assert_eq!(lines.len(), 4);
    for line in &lines {
        assert!(
            line.starts_with(
                "eth_call latest node_interface 0x77d488a2 gasEstimateL1Component args="
            ),
            "{line}"
        );
    }
    // The first call asked for the empty transaction: the target, not a contract creation, and no data.
    assert!(
        lines[0].ends_with("args=[coordinator,0,96,0]"),
        "{}",
        lines[0]
    );
    // A node that does not serve it is an error of the read, not a zero.
    rig.chain(|chain| chain.fail_node_interface = true);
    assert!(rpc.l1_gas(to, &[]).await.is_err());
}

#[tokio::test]
async fn a_cancellation_carries_the_bound_whatever_the_node_says_the_transfer_needs() {
    // Mainnet needs 21,363 gas for a self transfer and the testnet 27,559; with the quarter margin that is 26,704 and
    // 34,449, far below the bound. The limit is the bound anyway: the chain charges the gas used, and the L1 price can
    // rise before a replacement. An answer up to the bound itself changes nothing either.
    for estimate in [21_363u64, 27_559, 79_999, 80_000] {
        let rig = Rig::new(ROBINHOOD_TESTNET, arbitrum()).await.unheld();
        rig.chain(|chain| chain.estimates.transfer = estimate);
        let (tick, cancel) = cancellation(&rig).await;
        assert_eq!(gas_of(&cancel), 100_000, "{estimate}");
        // The transfer was estimated as the keeper's own, and no L1 component was read.
        assert!(estimated_transfer(&tick), "{tick:#?}");
        assert_eq!(l1_reads(&tick), 0, "{tick:#?}");
    }
    // The margin is the operator's, and the limit does not move with it.
    for margin in [1_000u64, 10_000] {
        let settings = ChainSettings {
            l1_gas_margin_bps: margin,
            ..arbitrum()
        };
        let rig = Rig::new(ROBINHOOD_TESTNET, settings).await.unheld();
        rig.chain(|chain| chain.estimates.transfer = 21_363);
        assert_eq!(gas_of(&cancellation(&rig).await.1), 100_000, "{margin}");
    }
}

#[tokio::test]
async fn an_estimate_above_the_bound_is_clamped_to_it_and_logged_at_warn() {
    // At the bound there is nothing to say.
    let quiet = Rig::new(ROBINHOOD_TESTNET, arbitrum()).await.unheld();
    quiet.chain(|chain| chain.estimates.transfer = 80_000);
    let logs = Logs::capture(tracing::Level::WARN);
    assert_eq!(gas_of(&cancellation(&quiet).await.1), 100_000);
    assert!(!logs.text().contains("above the bound"), "{}", logs.text());
    drop(logs);
    // An absurd answer: 5,000,000 gas for a self transfer, 6,250,000 with the margin. The cancellation still goes out,
    // with the bound, and the answer is reported.
    let rig = Rig::new(ROBINHOOD_TESTNET, arbitrum()).await.unheld();
    rig.chain(|chain| chain.estimates.transfer = 5_000_000);
    let logs = Logs::capture(tracing::Level::WARN);
    assert_eq!(gas_of(&cancellation(&rig).await.1), 100_000);
    let text = logs.text();
    assert!(
        text.contains("prices a cancellation above the bound"),
        "{text}"
    );
    assert!(text.contains("needed_gas=6250000"), "{text}");
    assert!(text.contains("bound=100000"), "{text}");
    // The warning is level warn: nothing quieter shows it.
    drop(logs);
    let logs = Logs::capture(tracing::Level::ERROR);
    let again = Rig::new(ROBINHOOD_TESTNET, arbitrum()).await.unheld();
    again.chain(|chain| chain.estimates.transfer = 5_000_000);
    cancellation(&again).await;
    assert!(logs.text().is_empty(), "{}", logs.text());
}

#[tokio::test]
async fn a_batch_cancellation_and_an_epoch_cancellation_and_a_sweep_cancellation_carry_the_bound_too()
 {
    // Mainnet's 21,363 gas with the quarter margin is 26,704 gas, and a cancellation carries the bound at every place a
    // nonce is cancelled.
    let rig = Rig::new(ROBINHOOD_TESTNET, arbitrum()).await.unheld();
    rig.chain(|chain| chain.estimates.transfer = 21_363);
    // Two requests in a batch that the sequencer never includes, both refunded meanwhile.
    let first = ready(&rig);
    rig.run(false, false).await;
    let second = rig.request();
    rig.chain(|chain| chain.mine(3));
    rig.run(false, false).await;
    rig.run(true, false).await;
    assert!(
        rig.sent()[0].contains("fulfillRandomnessBatch"),
        "{:?}",
        rig.sent()
    );
    rig.chain(|chain| {
        chain.drop_queue();
        for id in [first, second] {
            chain.requests.get_mut(&id).unwrap().refunded = true;
        }
        chain.mine(1);
    });
    rig.run(true, false).await;
    assert_eq!(rig.sent().pop().unwrap(), "cancel gas=100000 nonce=0");
    rig.settle();
    rig.run(true, false).await;

    // An epoch commit that is never included and loses its only request.
    rig.chain(|chain| chain.mine_to(chain.epoch_start(2) + 2));
    rig.run(true, false).await;
    let third = rig.request();
    rig.chain(|chain| chain.mine(1));
    rig.run(true, false).await;
    assert!(
        rig.sent().pop().unwrap().contains("commitEpoch"),
        "{:?}",
        rig.sent()
    );
    rig.chain(|chain| {
        chain.drop_queue();
        chain.requests.get_mut(&third).unwrap().refunded = true;
        chain.mine(1);
    });
    rig.run(true, false).await;
    assert_eq!(rig.sent().pop().unwrap(), "cancel gas=100000 nonce=1");
    rig.settle();
    rig.run(true, false).await;

    // A sweep that is not included for as long as a sweep waits.
    rig.queue_sweep("2000000000000000000").await;
    rig.run(true, false).await;
    // The transfer itself is priced from its estimate as before, a fifth more.
    assert_eq!(rig.sent().pop().unwrap(), "sweep gas=25635 nonce=2");
    rig.chain(|chain| chain.drop_queue());
    rig.age_sweep().await;
    rig.run(true, false).await;
    assert_eq!(rig.sent().pop().unwrap(), "cancel gas=100000 nonce=2");
}

#[tokio::test]
async fn a_cancellation_whose_estimate_fails_is_priced_from_its_l1_component_and_still_carries_the_bound()
 {
    // 173 and 6,214 are the L1 components of an empty transaction; with the margin 217 and 7,768, and 21,000 gas and
    // twice that is 21,434 and 36,536: the fallback is read once, and the limit is the bound.
    for empty in [173u64, 6_214] {
        let rig = Rig::new(ROBINHOOD_TESTNET, arbitrum()).await.unheld();
        rig.chain(|chain| {
            chain.fail_transfer_estimate = true;
            chain.l1_gas = if empty == 173 {
                |len| if len == 0 { 173 } else { 732 }
            } else {
                |len| if len == 0 { 6_214 } else { 26_134 }
            };
        });
        let (tick, cancel) = cancellation(&rig).await;
        assert_eq!(gas_of(&cancel), 100_000, "{empty}");
        // The estimate was tried, failed, and the empty transaction's L1 component was read once for it.
        assert!(estimated_transfer(&tick), "{tick:#?}");
        assert_eq!(l1_reads(&tick), 1, "{tick:#?}");
    }
    // An L1 component beyond anything measured is clamped like an absurd estimate, and reported.
    let rig = Rig::new(ROBINHOOD_TESTNET, arbitrum()).await.unheld();
    rig.chain(|chain| {
        chain.fail_transfer_estimate = true;
        chain.l1_gas = |len| if len == 0 { 200_000 } else { 1 };
    });
    let logs = Logs::capture(tracing::Level::WARN);
    assert_eq!(gas_of(&cancellation(&rig).await.1), 100_000);
    // 21,000 gas and twice 250,000.
    assert!(logs.text().contains("needed_gas=521000"), "{}", logs.text());
}

#[tokio::test]
async fn a_cancellation_that_cannot_be_priced_leaves_the_nonce_to_the_next_tick() {
    let rig = Rig::new(ROBINHOOD_TESTNET, arbitrum()).await.unheld();
    let id = ready(&rig);
    rig.run(true, false).await;
    rig.chain(|chain| {
        chain.drop_queue();
        chain.requests.get_mut(&id).unwrap().refunded = true;
        chain.mine(1);
        chain.fail_transfer_estimate = true;
        chain.fail_node_interface = true;
    });
    // Neither the estimate nor the L1 component answers: the tick fails, signs nothing, and the fulfillment's
    // attempt stays as it was.
    let error = rig.run_with(true, false, |_| {}).await.err().unwrap();
    assert!(
        format!("{error:#}").contains("failed eth_call"),
        "{error:#}"
    );
    assert_eq!(rig.sent().len(), 1);
    // The node answers again: the next tick cancels.
    rig.chain(|chain| {
        chain.fail_transfer_estimate = false;
        chain.fail_node_interface = false;
        chain.estimates.transfer = 21_363;
    });
    rig.run(true, false).await;
    assert_eq!(gas_of(&rig.sent().pop().unwrap()), 100_000);
}

/// Where a keeper's nonce is held: a single fulfillment, a batch, or an epoch commit.
#[derive(Clone, Copy, Debug)]
enum Lane {
    Single,
    Batch,
    Epoch,
}
const LANES: [Lane; 3] = [Lane::Single, Lane::Batch, Lane::Epoch];

/// A keeper that has signed the cancellation of its nonce 0, which the sequencer never includes: the tick that sent it.
async fn held_by_a_cancellation(rig: &Rig, lane: Lane) -> Vec<String> {
    let tick = match lane {
        Lane::Single => cancellation(rig).await.0,
        Lane::Batch => {
            let first = ready(rig);
            rig.run(false, false).await;
            let second = rig.request();
            rig.chain(|chain| chain.mine(3));
            rig.run(false, false).await;
            rig.run(true, false).await;
            assert!(
                rig.sent()[0].contains("fulfillRandomnessBatch"),
                "{:?}",
                rig.sent()
            );
            rig.chain(|chain| {
                chain.drop_queue();
                for id in [first, second] {
                    chain.requests.get_mut(&id).unwrap().refunded = true;
                }
                chain.mine(1);
            });
            rig.run(true, false).await.tick
        }
        Lane::Epoch => {
            rig.chain(|chain| {
                chain.publish_epoch(1, chain.head);
                chain.mine(1);
            });
            rig.chain(|chain| chain.mine_to(chain.epoch_start(2) + 2));
            rig.run(true, false).await;
            let request = rig.request();
            rig.chain(|chain| chain.mine(1));
            rig.run(true, false).await;
            assert!(
                rig.sent().last().unwrap().contains("commitEpoch"),
                "{:?}",
                rig.sent()
            );
            rig.chain(|chain| {
                chain.drop_queue();
                chain.requests.get_mut(&request).unwrap().refunded = true;
                chain.mine(1);
            });
            rig.run(true, false).await.tick
        }
    };
    assert_eq!(
        rig.sent().pop().unwrap(),
        "cancel gas=100000 nonce=0",
        "{lane:?}"
    );
    tick
}
/// The line of a trace that sends a transaction.
fn sending(tick: &[String]) -> &str {
    tick.iter()
        .find(|line| line.starts_with("eth_sendRawTransaction"))
        .unwrap_or_else(|| panic!("nothing was sent: {tick:#?}"))
}
/// A number in a recorded line: `maxPriority=2250000001`.
fn field(line: &str, name: &str) -> u128 {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("no {name} in {line}"))
        .parse()
        .unwrap()
}

#[tokio::test]
async fn a_replacement_of_a_cancellation_is_priced_afresh_when_the_l1_price_rose_in_between() {
    for lane in LANES {
        let rig = Rig::new(ROBINHOOD_TESTNET, arbitrum()).await.unheld();
        rig.chain(|chain| {
            chain.estimates.transfer = 21_363;
            chain.enforce_transfer_gas = true;
        });
        let first = held_by_a_cancellation(&rig, lane).await;
        // The L1 price rises while the sequencer is silent: the transfer needs 70,000 gas now, 87,500 with the margin.
        // A replacement that reused the limit of the first, priced from 21,363 gas, would be rejected as below its
        // intrinsic gas and hold the lane for good.
        rig.chain(|chain| {
            chain.estimates.transfer = 70_000;
            chain.mine(10);
        });
        let before = rig.sent().len();
        let run = rig.run(true, false).await;
        assert_eq!(rig.sent().len(), before + 1, "{lane:?}: {:#?}", run.tick);
        assert_eq!(rig.sent().pop().unwrap(), "cancel gas=100000 nonce=0");
        // The same nonce, signed again with a fee 12.5% above the first.
        let replacement = sending(&run.tick);
        assert!(
            field(replacement, "maxPriority") * 8 >= field(sending(&first), "maxPriority") * 9,
            "{lane:?}: {replacement}"
        );
        assert!(field(replacement, "maxFee") > field(sending(&first), "maxFee"));
        // It was priced afresh: the transfer was estimated again.
        assert!(estimated_transfer(&run.tick), "{lane:?}: {:#?}", run.tick);
    }
}

#[tokio::test]
async fn a_replacement_is_never_below_the_cancellation_it_replaces() {
    for lane in LANES {
        let rig = Rig::new(ROBINHOOD_TESTNET, arbitrum()).await.unheld();
        held_by_a_cancellation(&rig, lane).await;
        // A cancellation signed with more than the bound, by another release: max(previous, fresh) keeps it.
        let pool = rig.journal().await;
        sqlx::query("UPDATE txs SET gas=120000 WHERE kind IN ('cancel','epoch_cancel')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        rig.chain(|chain| chain.mine(10));
        rig.run(true, false).await;
        assert_eq!(
            rig.sent().pop().unwrap(),
            "cancel gas=120000 nonce=0",
            "{lane:?}"
        );
        // One signed with less, as the arbitrum model before the bound did, is raised to the bound.
        let pool = rig.journal().await;
        sqlx::query("UPDATE txs SET gas=26704 WHERE kind IN ('cancel','epoch_cancel')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        rig.chain(|chain| chain.mine(10));
        rig.run(true, false).await;
        assert_eq!(
            rig.sent().pop().unwrap(),
            "cancel gas=100000 nonce=0",
            "{lane:?}"
        );
    }
}

#[tokio::test]
async fn a_replacement_whose_pricing_fails_keeps_the_previous_limit_and_the_tick_succeeds() {
    for lane in LANES {
        let rig = Rig::new(ROBINHOOD_TESTNET, arbitrum()).await.unheld();
        held_by_a_cancellation(&rig, lane).await;
        // Neither the estimate nor the L1 component answers now.
        rig.chain(|chain| {
            chain.fail_transfer_estimate = true;
            chain.fail_node_interface = true;
            chain.mine(10);
        });
        let logs = Logs::capture(tracing::Level::WARN);
        let before = rig.sent().len();
        let run = rig.run_with(true, false, |_| {}).await;
        let run = run.unwrap_or_else(|error| panic!("{lane:?}: the tick failed: {error:#}"));
        assert_eq!(rig.sent().len(), before + 1, "{lane:?}: {:#?}", run.tick);
        assert_eq!(
            rig.sent().pop().unwrap(),
            "cancel gas=100000 nonce=0",
            "{lane:?}"
        );
        assert!(
            logs.text().contains("keeping the previous limit"),
            "{lane:?}: {}",
            logs.text()
        );
    }
}

#[tokio::test]
async fn the_standard_model_replaces_a_cancellation_with_the_gas_it_had_and_asks_for_no_estimate() {
    let rig = Rig::new(ROBINHOOD_TESTNET, ChainSettings::default())
        .await
        .unheld();
    rig.chain(|chain| chain.estimates.transfer = 21_363);
    cancellation(&rig).await;
    assert_eq!(rig.sent().pop().unwrap(), "cancel gas=21000 nonce=0");
    rig.chain(|chain| chain.mine(10));
    let run = rig.run(true, false).await;
    assert_eq!(rig.sent().pop().unwrap(), "cancel gas=21000 nonce=0");
    assert_eq!(rig.sent().len(), 3);
    assert!(
        !run.tick
            .iter()
            .any(|line| line.starts_with("eth_estimateGas")),
        "{:#?}",
        run.tick
    );
    assert_eq!(l1_reads(&run.tick), 0);
}

#[tokio::test]
async fn the_standard_model_still_cancels_with_21000_gas_and_asks_for_no_estimate() {
    let rig = Rig::new(ROBINHOOD_TESTNET, ChainSettings::default())
        .await
        .unheld();
    rig.chain(|chain| chain.estimates.transfer = 21_363);
    let (tick, cancel) = cancellation(&rig).await;
    assert_eq!(gas_of(&cancel), 21_000);
    assert!(
        !tick.iter().any(|line| line.starts_with("eth_estimateGas")),
        "{tick:#?}"
    );
    assert_eq!(l1_reads(&tick), 0);
}

#[tokio::test]
async fn a_fulfillment_holds_the_l1_component_of_its_payload_when_the_floor_wins() {
    // A callback that is cheap in the simulation: the estimate is below the floor of 753,643 gas, which then holds the
    // payload's L1 component with the margin, 915 on mainnet and 32,668 on the testnet.
    for (l1, expected) in [(732u64, 754_558u64), (26_134, 786_311)] {
        let rig = Rig::new(ROBINHOOD_TESTNET, arbitrum()).await.unheld();
        rig.chain(|chain| {
            chain.estimates.fulfill = 500_000;
            chain.l1_gas = if l1 == 732 {
                |len| if len == SINGLE { 732 } else { 1 }
            } else {
                |len| if len == SINGLE { 26_134 } else { 1 }
            };
        });
        ready(&rig);
        let run = rig.run(true, false).await;
        assert_eq!(gas_of(&rig.sent()[0]), expected, "{l1}");
        assert_eq!(l1_reads(&run.tick), 1, "{:#?}", run.tick);
    }
    // The same request in the standard model is the floor alone; an estimate above the floor is padded as before in both.
    let standard = Rig::new(ROBINHOOD_TESTNET, ChainSettings::default())
        .await
        .unheld();
    standard.chain(|chain| chain.estimates.fulfill = 500_000);
    ready(&standard);
    standard.run(true, false).await;
    assert_eq!(gas_of(&standard.sent()[0]), 753_643);
    let rig = Rig::new(ROBINHOOD_TESTNET, arbitrum()).await.unheld();
    rig.chain(|chain| chain.l1_gas = |_| 26_134);
    ready(&rig);
    rig.run(true, false).await;
    assert_eq!(gas_of(&rig.sent()[0]), 810_000);
}

/// A rig with three requests proved and not sent.
async fn three_prepared(rig: &Rig) {
    rig.chain(|chain| {
        chain.publish_epoch(1, chain.head);
        chain.mine(1);
        // A cheap callback in the simulation, so that the floors decide the gas limit.
        chain.estimates.batch_member = 100_000;
    });
    for _ in 0..3 {
        rig.request();
        rig.chain(|chain| chain.mine(3));
        rig.run(false, false).await;
    }
}

#[tokio::test]
async fn a_batch_reads_its_l1_component_twice_and_its_floor_holds_it() {
    let rig = Rig::new(ROBINHOOD_TESTNET, arbitrum()).await.unheld();
    rig.chain(|chain| {
        chain.estimates.batch_member = 100_000;
        chain.l1_gas = |len| if len == BATCH_OF_TWO { 200_000 } else { 1 };
    });
    ready(&rig);
    rig.run(false, false).await;
    rig.request();
    rig.chain(|chain| chain.mine(3));
    rig.run(false, false).await;
    let run = rig.run(true, false).await;
    // Once for the candidate payload, which sizes the batch, and once for the payload that was estimated.
    assert_eq!(l1_reads(&run.tick), 2, "{:#?}", run.tick);
    // Two members' floor of 1,290,791 and the payload's 200,000 with the margin, 250,000.
    let sent = rig.sent();
    assert!(sent[0].contains("fulfillRandomnessBatch"), "{sent:?}");
    assert_eq!(gas_of(&sent[0]), 1_540_791);
}

#[tokio::test]
async fn a_batch_loses_the_members_whose_l1_component_does_not_fit_the_cap() {
    // Three members at the standard model's floor need 1,827,939 gas, and fit a 1,900,000 cap. Their payload's L1
    // component of 300,000 (375,000 with the margin, 125,000 a member) leaves room for two.
    let cap = |config: &mut crate::config::Config| config.max_gas = 1_900_000;
    let standard = Rig::new(ROBINHOOD_TESTNET, ChainSettings::default())
        .await
        .unheld();
    three_prepared(&standard).await;
    standard.run_with(true, false, cap).await.unwrap();
    let sent = standard.sent();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(gas_of(&sent[0]), 1_827_939);

    let rig = Rig::new(ROBINHOOD_TESTNET, arbitrum()).await.unheld();
    rig.chain(|chain| {
        chain.l1_gas = |len| match len {
            BATCH_OF_THREE => 300_000,
            BATCH_OF_TWO => 200_000,
            _ => 1,
        }
    });
    three_prepared(&rig).await;
    let run = rig.run_with(true, false, cap).await.unwrap();
    let sent = rig.sent();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("fulfillRandomnessBatch"), "{sent:?}");
    // The batch of two: their floor, 1,290,791, and the L1 component of that payload with the margin, 250,000.
    assert_eq!(gas_of(&sent[0]), 1_540_791);
    assert!(
        run.journal
            .contains("jobs [1=submitted 2=submitted 3=prepared]"),
        "{}",
        run.journal
    );
    // The candidate payload of three was read once, and the payload that was estimated, of two, once.
    assert_eq!(l1_reads(&run.tick), 2, "{:#?}", run.tick);
}

#[tokio::test]
async fn a_node_interface_that_does_not_answer_defers_the_send_and_the_next_tick_sends() {
    let rig = Rig::new(ROBINHOOD_TESTNET, arbitrum()).await.unheld();
    rig.chain(|chain| chain.fail_node_interface = true);
    ready(&rig);
    // The tick is not a failed one: the fulfillment waits, as it does for any read it could not make.
    let run = rig.run(true, false).await;
    assert!(rig.sent().is_empty(), "{:#?}", run.tick);
    assert!(run.journal.contains("jobs [1=prepared]"), "{}", run.journal);
    rig.chain(|chain| {
        chain.fail_node_interface = false;
        chain.l1_gas = |_| 732;
    });
    rig.run(true, false).await;
    assert_eq!(rig.sent().len(), 1);
}
