//! A round keeper serving requests (keeper task K3) against the scripted chain: a request whose round is verified is
//! proved over a seed the keeper computes as the coordinator does, journaled with its fingerprint, and sent singly or in
//! a batch with the signature of each of its rounds; a request a replaced block moved is proved again, one it took
//! vanishes; the gas limit is never below the coordinator's guard through its proxy, and the fee gate prices the gas a
//! fulfillment can use. Every test holds the keeper's calls to the round coordinator's own: nothing of an epoch
//! coordinator or a registry is asked (`round_mode::epoch_calls`).
use crate::{
    rig::{Process, Rig},
    round_gas::{self, Shape},
    round_mode::{
        asks, assert_no_epoch_call, bound, journal, lane_clock, mine_to_time, round_rig, row,
    },
    scripted::{round_randomness, round_signature},
    soft_finality::meta,
};
use alloy_primitives::{U256, keccak256};

/// What the scripted chain's NodeInterface answers for the L1 component of every payload, and what the keeper prices it
/// at: with the default margin of L1_GAS_MARGIN_BPS, 2500.
const L1_GAS: u64 = 20_000;
const L1: u64 = 25_000;

/// A round keeper's rig on which every payload has an L1 component.
async fn serving_rig() -> Rig {
    let rig = round_rig().await.unheld();
    rig.chain(|chain| chain.l1_gas = |_| L1_GAS);
    rig
}
/// The model's gas limit of a shape the fixture names (`keeper/tests/fixtures/round-gas-model.json`).
fn model_limit(name: &str) -> u64 {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/round-gas-model.json")).unwrap();
    fixture["gas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == name)
        .unwrap_or_else(|| panic!("the fixture has no {name}"))["gasLimit"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}
/// The model's gas limit on the chain of a shape the fixture names: its limit, the L1 component the keeper reads, and the
/// margin on both (`round_gas::chain_limit`).
fn on_chain(name: &str) -> u64 {
    round_gas::estimate_cover(model_limit(name) + L1)
}
/// The first four bytes of the hash of `data`, as the trace shows a seed or a signature.
fn fp(data: &[u8]) -> String {
    hex::encode(&keccak256(data)[..4])
}
/// The seed of request `id` over its round, as the coordinator computes it now.
fn seed(rig: &Rig, id: u64) -> U256 {
    rig.chain(|chain| {
        let (_, round) = chain.requests[&id].round.unwrap();
        chain
            .round_seed(id, round_randomness(&round_signature(round)))
            .unwrap()
    })
}
/// The VRF hash-to-curve candidates of request `id`'s proof.
fn candidates(rig: &Rig, id: u64) -> u32 {
    let key = rig.chain(|chain| chain.public_key);
    crate::prover::hash_to_curve_candidates(key, seed(rig, id)).unwrap()
}
/// The fee that covers, at FEE_COVERAGE_BPS 12500 and the chain's base fee, the gas a fulfillment of this shape can use
/// with the L1 component the keeper reads: exactly the least a request may have escrowed and still be served.
fn required(rig: &Rig, shape: &Shape) -> u128 {
    let base = rig.chain(|chain| chain.base_fee_at(chain.head));
    let cost = base * u128::from(round_gas::gas_bound(shape).unwrap() + L1);
    cost * 12_500 / 10_000
}
/// Let the round of request `id` become due and verified: the chain's time and the keeper's clock at the round's time, and
/// a tick, whose fetch ends with it. The lines of that tick.
async fn verify_round_of(rig: &Rig, process: &Process<'_>, id: u64) -> Vec<String> {
    let (_, _, due) = bound(rig, id);
    mine_to_time(rig, due);
    lane_clock(process).set(due * 1_000);
    process.tick().await.unwrap().tick
}
/// Let the round of request `id` become due, and the keeper verify it, prove the request and send it: two ticks, since
/// the fetch runs beside the tick and may end before or after the tick's preparation. The lines of both.
async fn serve(rig: &Rig, process: &Process<'_>, id: u64) -> Vec<String> {
    let mut lines = verify_round_of(rig, process, id).await;
    lines.extend(process.tick().await.unwrap().tick);
    lines
}
/// The keeper's transactions: label, gas limit and nonce.
fn sent(rig: &Rig) -> Vec<(String, u64, u64)> {
    rig.chain(|chain| {
        chain
            .sends
            .iter()
            .map(|(_, label, gas, nonce)| (label.clone(), *gas, *nonce))
            .collect()
    })
}
/// The lines that sent a transaction.
fn broadcasts(lines: &[String]) -> Vec<&String> {
    lines
        .iter()
        .filter(|line| line.contains("eth_sendRawTransaction"))
        .collect()
}
/// A job's state.
async fn state(rig: &Rig, id: u64) -> String {
    let journal = journal(rig).await;
    let job = journal.job(&id.to_string()).await.unwrap().unwrap();
    journal.pool.close().await;
    job.state
}

#[tokio::test]
async fn a_request_first_in_its_round_is_proved_over_the_round_and_sent_with_its_signature_at_the_models_gas_limit()
 {
    let rig = serving_rig().await;
    let process = rig.process(true, false).await;
    let id = rig.request();
    let (beacon, round, _) = bound(&rig, id);
    let tick = serve(&rig, &process, id).await;
    assert_eq!(row(&rig, beacon, round).await.unwrap().state, "verified");
    let mut lines = Vec::new();
    // The seed computed here is checked once against the coordinator's proof context, asked with the gas written down.
    let context: Vec<&String> = tick
        .iter()
        .filter(|line| line.contains("getProofContext"))
        .collect();
    assert_eq!(context.len(), 1, "{tick:#?}");
    assert!(context[0].contains(" gas=0xf4240"), "{}", context[0]);
    let sends = broadcasts(&tick);
    assert_eq!(sends.len(), 1, "{tick:#?}");
    assert!(
        sends[0].contains(&format!(
            "fulfillRandomness id={id} seed={} signature={}",
            fp(&seed(&rig, id).to_be_bytes::<32>()),
            fp(&round_signature(round))
        )),
        "{}",
        sends[0]
    );
    // Never below the guard through the proxy: the model's limit of a single first in its round, with the L1 component and
    // the margin.
    assert_eq!(
        sent(&rig),
        [(
            "0x5a58c410 fulfillRandomness".to_owned(),
            on_chain("single first in its round, 100000"),
            0
        )]
    );
    lines.extend(tick);
    // Its proof is journaled with the fingerprint of the request it was made for.
    let journal = journal(&rig).await;
    let job = journal.job(&id.to_string()).await.unwrap().unwrap();
    let prepared: crate::round::Prepared = serde_json::from_str(&job.proof.unwrap()).unwrap();
    let view = rig.chain(|chain| chain.round_view(id).unwrap());
    let request = <crate::abi_round::RoundRequest as alloy_sol_types::SolValue>::abi_decode(
        &alloy_sol_types::SolValue::abi_encode(&view),
    )
    .unwrap();
    assert_eq!(prepared.fingerprint, crate::round::fingerprint(&request));
    assert_eq!(prepared.proof.seed, seed(&rig, id));
    journal.pool.close().await;
    // Mined, it verifies the round and serves the request.
    rig.settle();
    let run = process.tick().await.unwrap();
    assert!(
        run.journal.contains(&format!("jobs [{id}=served]")),
        "{}",
        run.journal
    );
    assert!(rig.chain(|chain| {
        chain
            .round
            .as_ref()
            .unwrap()
            .verified
            .contains_key(&(beacon, round))
    }));
    lines.extend(run.tick);
    // A request of a later round: its seed is computed alone, with no proof context asked.
    let next = rig.request();
    let tick = serve(&rig, &process, next).await;
    assert_eq!(asks(&tick, "getProofContext"), 0, "{tick:#?}");
    assert_eq!(broadcasts(&tick).len(), 1, "{tick:#?}");
    lines.extend(tick);
    assert_no_epoch_call("serving a request", &lines);
    process.stop().await;
}

#[tokio::test]
async fn a_seed_that_differs_from_the_coordinators_proof_context_is_never_proved_and_is_checked_again_until_one_matches()
 {
    let rig = serving_rig().await;
    let process = rig.process(true, false).await;
    let id = rig.request();
    // The coordinator's proof context names another seed than the one computed here.
    rig.chain(|chain| chain.wrong_proof_context = true);
    let mut lines = serve(&rig, &process, id).await;
    lines.extend(process.tick().await.unwrap().tick);
    assert!(broadcasts(&lines).is_empty(), "{lines:#?}");
    assert_eq!(state(&rig, id).await, "pending");
    // Every preparation asks again while none has matched.
    assert!(asks(&lines, "getProofContext") >= 2, "{lines:#?}");
    // Once the two agree the request is proved and sent, and the seeds of this process are computed alone from then on.
    rig.chain(|chain| chain.wrong_proof_context = false);
    let tick = process.tick().await.unwrap().tick;
    assert_eq!(asks(&tick, "getProofContext"), 1, "{tick:#?}");
    assert_eq!(broadcasts(&tick).len(), 1, "{tick:#?}");
    lines.extend(tick);
    assert_no_epoch_call("a seed checked against the proof context", &lines);
    process.stop().await;
}

#[tokio::test]
async fn a_request_whose_round_the_coordinator_has_is_sent_with_the_signature_anyway_and_with_the_rounds_gas()
 {
    let rig = serving_rig().await;
    let process = rig.process(true, false).await;
    let id = rig.request();
    let (beacon, round, due) = bound(&rig, id);
    mine_to_time(&rig, due);
    // Another keeper's fulfillment verified the round: the coordinator has it at the decision head.
    rig.chain(|chain| {
        let block = chain.head;
        chain.verify_round(beacon, round, block);
        chain.mine(1);
    });
    lane_clock(&process).set(due * 1_000);
    let mut lines = Vec::new();
    let mut tick = process.tick().await.unwrap().tick;
    tick.extend(process.tick().await.unwrap().tick);
    let sends = broadcasts(&tick);
    assert_eq!(sends.len(), 1, "{tick:#?}");
    // The signature is sent, so that a block replaced before inclusion that takes the round's verification with it
    // cannot make the fulfillment revert; and the gas limit holds the round's verification for the same reason (M1).
    assert!(
        sends[0].contains(&format!("signature={}", fp(&round_signature(round)))),
        "{}",
        sends[0]
    );
    assert_eq!(
        sent(&rig)[0].1,
        on_chain("single first in its round, 100000")
    );
    lines.extend(tick);
    rig.settle();
    let run = process.tick().await.unwrap();
    assert!(
        run.journal.contains(&format!("jobs [{id}=served]")),
        "{}",
        run.journal
    );
    lines.extend(run.tick);
    assert_no_epoch_call("serving a request of a cached round", &lines);
    process.stop().await;
}

#[tokio::test]
async fn requests_of_one_round_are_sent_in_one_batch_that_lists_the_round_once() {
    let rig = serving_rig().await;
    let process = rig.process(true, false).await;
    let first = rig.request();
    let second = rig.request();
    let (_, round, _) = bound(&rig, first);
    assert_eq!(bound(&rig, second).1, round);
    let mut lines = Vec::new();
    let tick = serve(&rig, &process, first).await;
    let sends = broadcasts(&tick);
    assert_eq!(sends.len(), 1, "{tick:#?}");
    assert!(
        sends[0].contains(&format!(
            "fulfillRandomnessBatch rounds=[0:{round}:{}] ids=[{first},{second}]",
            fp(&round_signature(round))
        )),
        "{}",
        sends[0]
    );
    assert_eq!(
        sent(&rig),
        [(
            "0xfdec2a74 fulfillRandomnessBatch".to_owned(),
            on_chain("batch of 2 over 1 rounds, 100000"),
            0
        )]
    );
    lines.extend(tick);
    rig.settle();
    let run = process.tick().await.unwrap();
    assert!(
        run.journal
            .contains(&format!("jobs [{first}=served {second}=served]")),
        "{}",
        run.journal
    );
    lines.extend(run.tick);
    assert_no_epoch_call("serving a batch of one round", &lines);
    process.stop().await;
}

#[tokio::test]
async fn requests_of_two_rounds_are_sent_in_one_batch_that_lists_both_even_the_one_the_coordinator_has()
 {
    let rig = serving_rig().await;
    let book = crate::round_mode::book(&rig);
    // A keeper that does not send verifies both rounds and proves both requests first. The fetches run beside a tick: a
    // sending keeper that verified one round before its send pass and the other after would send the two singly.
    let process = rig.process(false, false).await;
    let first = rig.request();
    rig.chain(|chain| chain.mine(book.period));
    let second = rig.request();
    let (beacon, one, _) = bound(&rig, first);
    let (_, two, due) = bound(&rig, second);
    assert_eq!(two, one + 1);
    // The coordinator has the first round already.
    mine_to_time(&rig, due);
    rig.chain(|chain| {
        let block = chain.head;
        chain.verify_round(beacon, one, block);
        chain.mine(1);
    });
    lane_clock(&process).set(due * 1_000);
    // A tick ends once its fetches have: the next proves what the first could not.
    let mut lines = process.tick().await.unwrap().tick;
    lines.extend(process.tick().await.unwrap().tick);
    for round in [one, two] {
        assert_eq!(row(&rig, beacon, round).await.unwrap().state, "verified");
    }
    for id in [first, second] {
        assert_eq!(state(&rig, id).await, "prepared");
    }
    process.stop().await;
    let process = rig.process(true, false).await;
    lane_clock(&process).set(due * 1_000);
    let tick = process.tick().await.unwrap().tick;
    let sends = broadcasts(&tick);
    assert_eq!(sends.len(), 1, "{tick:#?}");
    // Both rounds are listed with their signatures, the cached one too, and both count in the gas limit (M1).
    assert!(
        sends[0].contains(&format!(
            "fulfillRandomnessBatch rounds=[0:{one}:{},0:{two}:{}] ids=[{first},{second}]",
            fp(&round_signature(one)),
            fp(&round_signature(two))
        )),
        "{}",
        sends[0]
    );
    assert_eq!(
        sent(&rig)[0].1,
        on_chain("batch of 2 over 2 rounds, 100000")
    );
    lines.extend(tick);
    rig.settle();
    let run = process.tick().await.unwrap();
    assert!(
        run.journal
            .contains(&format!("jobs [{first}=served {second}=served]")),
        "{}",
        run.journal
    );
    lines.extend(run.tick);
    assert_no_epoch_call("serving a batch of two rounds", &lines);
    process.stop().await;
}

#[tokio::test]
async fn a_request_a_replaced_block_moved_loses_its_proof_and_is_proved_again_for_its_new_fields() {
    let rig = serving_rig().await;
    // A keeper that does not send proves the request.
    let process = rig.process(false, false).await;
    let id = rig.request();
    let mut lines = verify_round_of(&rig, &process, id).await;
    lines.extend(process.tick().await.unwrap().tick);
    assert_eq!(state(&rig, id).await, "prepared");
    let old = seed(&rig, id);
    process.stop().await;
    // The sequencer replaces the block: the same id is a request with other fields.
    rig.chain(|chain| chain.move_request(id));
    let new = seed(&rig, id);
    assert_ne!(new, old);
    // The keeper reads it again before it signs: the proof is dropped, the move recorded, and the request proved again
    // for its new fields and sent, in the same tick.
    let process = rig.process(true, false).await;
    let (_, _, due) = bound(&rig, id);
    lane_clock(&process).set(due * 1_000);
    let tick = process.tick().await.unwrap().tick;
    let sends = broadcasts(&tick);
    assert_eq!(sends.len(), 1, "{tick:#?}");
    assert!(
        sends[0].contains(&format!("seed={}", fp(&new.to_be_bytes::<32>()))),
        "{}",
        sends[0]
    );
    let moved: serde_json::Value =
        serde_json::from_str(&meta(&rig, crate::journal::REQUEST_MOVED_KEY).await.unwrap())
            .unwrap();
    assert_eq!(
        (moved["count"].clone(), moved["request"].clone()),
        (1.into(), id.to_string().into())
    );
    let journal = journal(&rig).await;
    let assigned = journal
        .round_assignment(&id.to_string())
        .await
        .unwrap()
        .unwrap();
    let view = rig.chain(|chain| chain.round_view(id).unwrap());
    let request = <crate::abi_round::RoundRequest as alloy_sol_types::SolValue>::abi_decode(
        &alloy_sol_types::SolValue::abi_encode(&view),
    )
    .unwrap();
    assert_eq!(
        assigned.fingerprint,
        crate::round::fingerprint(&request).to_string()
    );
    journal.pool.close().await;
    lines.extend(tick);
    rig.settle();
    let run = process.tick().await.unwrap();
    assert!(
        run.journal.contains(&format!("jobs [{id}=served]")),
        "{}",
        run.journal
    );
    lines.extend(run.tick);
    assert_no_epoch_call("a moved request", &lines);
    process.stop().await;
}

#[tokio::test]
async fn a_batch_member_a_replaced_block_moved_is_left_out_and_the_batch_never_carries_its_stale_proof()
 {
    let rig = serving_rig().await;
    let process = rig.process(false, false).await;
    let first = rig.request();
    let second = rig.request();
    let third = rig.request();
    verify_round_of(&rig, &process, first).await;
    process.tick().await.unwrap();
    for id in [first, second, third] {
        assert_eq!(state(&rig, id).await, "prepared");
    }
    process.stop().await;
    rig.chain(|chain| chain.move_request(second));
    let process = rig.process(true, false).await;
    let (_, round, due) = bound(&rig, first);
    lane_clock(&process).set(due * 1_000);
    let tick = process.tick().await.unwrap().tick;
    let sends = broadcasts(&tick);
    // The two members that are what their proofs were made for go in the batch; the moved one is proved again.
    assert_eq!(sends.len(), 1, "{tick:#?}");
    assert!(
        sends[0].contains(&format!(
            "rounds=[0:{round}:{}] ids=[{first},{third}]",
            fp(&round_signature(round))
        )),
        "{}",
        sends[0]
    );
    assert_eq!(state(&rig, second).await, "prepared");
    assert_no_epoch_call("a moved batch member", &tick);
    process.stop().await;
}

/// A request the coordinator does not have at the decision head (a replaced block took it, and the coordinator's
/// `nextRequestId` there has not reached its id) is left out of a send and its partner sent alone. One read of it is not
/// enough to call it gone: it stays as it is until its deadline, when it expires (review M1); the finality recovery, on a
/// replacement the endpoints confirm, calls it `vanished` sooner.
#[tokio::test]
async fn a_request_the_coordinator_no_longer_has_is_left_out_and_settled_at_its_deadline() {
    let rig = serving_rig().await;
    let process = rig.process(false, false).await;
    let kept = rig.request();
    let id = rig.request();
    verify_round_of(&rig, &process, id).await;
    process.tick().await.unwrap();
    process.stop().await;
    // A replaced block took the newer request: the coordinator refuses to read it (UnknownRequest), and its next id is
    // that request's again.
    rig.chain(|chain| {
        chain.requests.remove(&id);
        chain.next_request = id;
    });
    let process = rig.process(true, false).await;
    let (_, _, due) = bound(&rig, kept);
    lane_clock(&process).set(due * 1_000);
    let run = process.tick().await.unwrap();
    assert_eq!(state(&rig, id).await, "prepared");
    // Its batch partner is sent alone.
    let sends = broadcasts(&run.tick);
    assert_eq!(sends.len(), 1, "{:#?}", run.tick);
    assert!(
        sends[0].contains(&format!("fulfillRandomness id={kept} ")),
        "{}",
        sends[0]
    );
    assert_no_epoch_call("a vanished request", &run.tick);
    // Past its deadline it is settled.
    rig.settle();
    rig.chain(|chain| chain.mine(crate::config::RESPONSE_TIMEOUT_SECONDS + 1));
    process.tick().await.unwrap();
    let journal = journal(&rig).await;
    let job = journal.job(&id.to_string()).await.unwrap().unwrap();
    assert_eq!(job.state, "expired");
    journal.pool.close().await;
    process.stop().await;
}

#[tokio::test]
async fn the_fee_gate_serves_a_request_that_covers_the_gas_it_can_use_and_refuses_one_wei_less_its_round_counted_when_cached()
 {
    let rig = serving_rig().await.setting("FEE_COVERAGE_BPS", "12500");
    let process = rig.process(true, false).await;
    // Exactly covered: sent.
    let covered = rig.request();
    let shape = Shape::single(100_000).with_candidates(vec![candidates(&rig, covered)]);
    let fee = required(&rig, &shape);
    rig.chain(|chain| chain.requests.get_mut(&covered).unwrap().fee_paid = fee);
    let tick = serve(&rig, &process, covered).await;
    assert_eq!(broadcasts(&tick).len(), 1, "{tick:#?}");
    rig.settle();
    process.tick().await.unwrap();
    // One wei less, in a round the coordinator has verified already: refused, although the fee covers the gas without
    // the round's verification. The gate counts it all the same (M1).
    let short = rig.request();
    let (beacon, round, due) = bound(&rig, short);
    let shape = Shape::single(100_000).with_candidates(vec![candidates(&rig, short)]);
    let fee = required(&rig, &shape) - 1;
    let without_round = Shape {
        rounds: 0,
        ..shape.clone()
    };
    assert!(fee > required(&rig, &without_round));
    rig.chain(|chain| chain.requests.get_mut(&short).unwrap().fee_paid = fee);
    mine_to_time(&rig, due);
    rig.chain(|chain| {
        let block = chain.head;
        chain.verify_round(beacon, round, block);
        chain.mine(1);
    });
    lane_clock(&process).set(due * 1_000);
    let mut tick = process.tick().await.unwrap().tick;
    tick.extend(process.tick().await.unwrap().tick);
    assert!(broadcasts(&tick).is_empty(), "{tick:#?}");
    assert_eq!(state(&rig, short).await, "prepared");
    assert!(meta(&rig, "health:blocked:fee_budget").await.is_some());
    // The gate does not price on eth_estimateGas: the request was estimated, and refused on the bound.
    assert!(
        tick.iter().any(|line| line.contains("eth_estimateGas")),
        "{tick:#?}"
    );
    assert_no_epoch_call("the fee gate", &tick);
    process.stop().await;
}

#[tokio::test]
async fn a_batch_whose_fees_do_not_cover_it_drops_its_lowest_fee_member() {
    let rig = serving_rig().await.setting("FEE_COVERAGE_BPS", "12500");
    let process = rig.process(true, false).await;
    let first = rig.request();
    let second = rig.request();
    let third = rig.request();
    // The first two cover a batch of two between them; the third escrowed almost nothing.
    let pair = round_gas::batch_shape(&[
        round_gas::Sized {
            callback_gas_limit: 100_000,
            round: (0, bound(&rig, first).1),
        },
        round_gas::Sized {
            callback_gas_limit: 100_000,
            round: (0, bound(&rig, second).1),
        },
    ])
    .with_candidates(vec![candidates(&rig, first), candidates(&rig, second)]);
    let half = required(&rig, &pair).div_ceil(2);
    rig.chain(|chain| {
        chain.requests.get_mut(&first).unwrap().fee_paid = half;
        chain.requests.get_mut(&second).unwrap().fee_paid = half;
        chain.requests.get_mut(&third).unwrap().fee_paid = 1;
    });
    let mut lines = Vec::new();
    let tick = serve(&rig, &process, first).await;
    let sends = broadcasts(&tick);
    assert_eq!(sends.len(), 1, "{tick:#?}");
    assert!(
        sends[0].contains(&format!("ids=[{first},{second}]")),
        "{}",
        sends[0]
    );
    assert_eq!(
        sent(&rig)[0].1,
        on_chain("batch of 2 over 1 rounds, 100000")
    );
    lines.extend(tick);
    assert_no_epoch_call("a batch dropping a member", &lines);
    process.stop().await;
}

#[tokio::test]
async fn a_batch_holds_only_the_members_whose_gas_limit_fits_max_gas_and_the_transaction_cost_cap()
{
    // MAX_GAS: three members with 1,000,000-gas callbacks fit 6,000,000, and four do not.
    let rig = serving_rig().await.setting("MAX_GAS", "6000000");
    let process = rig.process(true, false).await;
    let ids: Vec<u64> = (0..4).map(|_| rig.request_with(1_000_000)).collect();
    let round = bound(&rig, ids[0]).1;
    let members = vec![
        round_gas::Sized {
            callback_gas_limit: 1_000_000,
            round: (0, round),
        };
        4
    ];
    let fit = round_gas::members_within(&members, 6_000_000, L1.div_ceil(4));
    assert_eq!(fit, 3);
    let tick = serve(&rig, &process, ids[0]).await;
    let sends = broadcasts(&tick);
    assert_eq!(sends.len(), 1, "{tick:#?}");
    assert!(
        sends[0].contains(&format!("ids=[{},{},{}]", ids[0], ids[1], ids[2])),
        "{}",
        sends[0]
    );
    let gas = sent(&rig)[0].1;
    assert!(gas <= 6_000_000, "{gas}");
    assert_no_epoch_call("batch sizing under MAX_GAS", &tick);
    process.stop().await;

    // MAX_TX_COST_WEI: at 3 gwei a gas (twice the base fee and the tip), a cost cap of 0.0135 holds 4,500,000 gas, which
    // two such members fit and three do not.
    let rig = serving_rig().await.settings(&[
        ("MAX_GAS", "13000000"),
        ("MAX_TX_COST_WEI", "13500000000000000"),
    ]);
    let process = rig.process(true, false).await;
    let ids: Vec<u64> = (0..3).map(|_| rig.request_with(1_000_000)).collect();
    let fee = rig.chain(|chain| 2 * chain.base_fee_at(chain.head) + chain.tips[0]);
    let cap = (13_500_000_000_000_000u128 / fee) as u64;
    let members = vec![
        round_gas::Sized {
            callback_gas_limit: 1_000_000,
            round: (0, bound(&rig, ids[0]).1),
        };
        3
    ];
    assert_eq!(round_gas::members_within(&members, cap, L1.div_ceil(3)), 2);
    let tick = serve(&rig, &process, ids[0]).await;
    let sends = broadcasts(&tick);
    assert_eq!(sends.len(), 1, "{tick:#?}");
    assert!(
        sends[0].contains(&format!("ids=[{},{}]", ids[0], ids[1])),
        "{}",
        sends[0]
    );
    assert!(sent(&rig)[0].1 <= cap);
    assert_no_epoch_call("batch sizing under MAX_TX_COST_WEI", &tick);
    process.stop().await;
}

#[tokio::test]
async fn a_fulfillment_of_a_request_a_replaced_block_moved_is_not_sent_again_its_nonce_is_cancelled_and_the_request_proved_again()
 {
    let mut rig = serving_rig().await;
    rig.add_endpoint().await;
    let process = rig.process(true, false).await;
    let id = rig.request();
    let mut lines = verify_round_of(&rig, &process, id).await;
    lines.extend(process.tick().await.unwrap().tick);
    assert_eq!(sent(&rig).len(), 1);
    rig.chain(|chain| chain.include());
    let included = rig.chain(|chain| chain.head);
    let run = process.tick().await.unwrap();
    assert!(
        run.journal.contains(&format!("jobs [{id}=served]")),
        "{}",
        run.journal
    );
    lines.extend(run.tick);
    // The sequencer loses the block that held the fulfillment, and puts another request of the same consumer under the
    // id: the bytes the journal kept carry a proof for a seed the chain does not have, which would only revert.
    rig.chain(|chain| {
        chain.drop_blocks(included, false);
        chain.move_request(id);
    });
    let worker = process.worker();
    let mut cancelled = false;
    for _ in 0..12 {
        let run = process.tick().await.unwrap();
        lines.extend(run.tick);
        if !cancelled
            && sent(&rig)
                .iter()
                .any(|(label, _, nonce)| label == "cancel" && *nonce == 0)
        {
            cancelled = true;
        }
        rig.chain(|chain| chain.include());
        if worker.journal.finality_mismatch().await.unwrap().is_none() && sent(&rig).len() >= 3 {
            break;
        }
    }
    let sends = sent(&rig);
    // The stale fulfillment was never sent again: its nonce was filled by a cancellation, and the request, proved again
    // for its new fields, went at the next nonce.
    assert!(cancelled, "{sends:?}");
    assert_eq!(
        sends
            .iter()
            .filter(|(label, _, _)| label.contains("fulfillRandomness"))
            .map(|(_, _, nonce)| *nonce)
            .collect::<Vec<_>>(),
        [0, 1],
        "{sends:?}"
    );
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
    assert_eq!(
        last["done"]["stale_nonces"],
        serde_json::json!([0]),
        "{last}"
    );
    assert_eq!(last["done"]["jobs_reproved"], 1, "{last}");
    let journal = journal(&rig).await;
    let job = journal.job(&id.to_string()).await.unwrap().unwrap();
    let prepared: crate::round::Prepared = serde_json::from_str(&job.proof.unwrap()).unwrap();
    assert_eq!(prepared.proof.seed, seed(&rig, id));
    journal.pool.close().await;
    for _ in 0..3 {
        rig.chain(|chain| chain.mine(1));
        let run = process.tick().await.unwrap();
        lines.extend(run.tick);
        if run.journal.contains(&format!("jobs [{id}=served]")) {
            break;
        }
    }
    assert_eq!(state(&rig, id).await, "served");
    assert_no_epoch_call("the recovery of a moved request", &lines);
    process.stop().await;
}

#[tokio::test]
async fn a_round_keepers_sweep_is_sent_and_reported_in_eth() {
    let rig = serving_rig().await;
    let process = rig.process(true, false).await;
    process.tick().await.unwrap();
    rig.queue_sweep("1000000000000000000").await;
    let tick = process.tick().await.unwrap().tick;
    let sweep: Vec<&String> = tick
        .iter()
        .filter(|line| line.contains("eth_sendRawTransaction") && line.contains("sweep"))
        .collect();
    assert_eq!(sweep.len(), 1, "{tick:#?}");
    // The fee recipient is read with the round coordinator's binding.
    assert_eq!(asks(&tick, "feeRecipient"), 1, "{tick:#?}");
    rig.settle();
    let run = process.tick().await.unwrap();
    assert!(run.journal.contains("last=sent"), "{}", run.journal);
    // More than the wallet holds is refused, in ETH.
    rig.queue_sweep("100000000000000000000").await;
    let run = process.tick().await.unwrap();
    assert!(run.journal.contains("last=refused"), "{}", run.journal);
    let pool = rig.journal().await;
    let last = crate::sweep::last(&pool).await.unwrap().unwrap();
    assert!(
        last.detail.contains(" ETH ") && !last.detail.contains("USDC"),
        "{}",
        last.detail
    );
    // The operator's status names the amounts in ETH.
    rig.queue_sweep("5").await;
    let status = crate::sweep::status(&pool).await.unwrap();
    assert!(status["queued"].get("eth").is_some(), "{status}");
    assert!(status["queued"].get("usdc").is_none(), "{status}");
    pool.close().await;
    assert_no_epoch_call("a round keeper's sweep", &tick);
    process.stop().await;
}

/// Three requests of one round, which a funded wallet sends in one batch: what that batch costs up front.
async fn batch_of_three_up_front() -> u128 {
    let rig = serving_rig().await;
    let process = rig.process(true, false).await;
    let ids = [rig.request(), rig.request(), rig.request()];
    serve(&rig, &process, ids[0]).await;
    assert_eq!(sent(&rig).len(), 1, "{:?}", sent(&rig));
    assert!(sent(&rig)[0].0.contains("fulfillRandomnessBatch"));
    let up_front = rig.chain(|chain| chain.queued_up_front());
    process.stop().await;
    up_front[0]
}
/// The wallet holds a wei less than a batch of three needs up front: the batch shrinks to the members it can pay for,
/// the rest follow, and nothing fails or waits for the owner.
#[tokio::test]
async fn a_batch_the_wallet_cannot_pay_up_front_shrinks_to_what_it_can() {
    let balance = batch_of_three_up_front().await - 1;
    let rig = serving_rig().await;
    let keeper = rig.chain(|chain| chain.keeper);
    rig.chain(|chain| chain.balances.insert(keeper, U256::from(balance)));
    let process = rig.process(true, false).await;
    let ids = [rig.request(), rig.request(), rig.request()];
    serve(&rig, &process, ids[0]).await;
    let first = sent(&rig);
    assert_eq!(first.len(), 1, "{first:?}");
    assert!(rig.chain(|chain| chain.queued_up_front())[0] <= balance);
    for _ in 0..3 {
        rig.chain(|chain| chain.include());
        process.tick().await.unwrap();
    }
    assert_eq!(sent(&rig).len(), 2, "{:?}", sent(&rig));
    rig.settle();
    let run = process.tick().await.unwrap();
    assert!(
        run.journal.contains(&format!(
            "jobs [{}=served {}=served {}=served]",
            ids[0], ids[1], ids[2]
        )),
        "{}",
        run.journal
    );
    assert!(meta(&rig, crate::worker::LOW_FUNDS_KEY).await.is_none());
    process.stop().await;
}
/// The wallet cannot pay even one fulfillment up front: nothing is signed, no tick fails, the request stays prepared for
/// a follower, and the owner is asked once, in Turkish, to fund the wallet. Funded, the keeper serves it by itself.
#[tokio::test]
async fn a_wallet_that_cannot_pay_one_fulfillment_signs_nothing_and_asks_the_owner_once() {
    let rig = serving_rig().await;
    let keeper = rig.chain(|chain| chain.keeper);
    rig.chain(|chain| chain.balances.insert(keeper, U256::from(1_000_000u64)));
    let mut process = rig.process(true, false).await;
    let mut told = process.told();
    let id = rig.request();
    serve(&rig, &process, id).await;
    for _ in 0..3 {
        rig.chain(|chain| chain.mine(1));
        process.tick().await.unwrap();
    }
    assert!(sent(&rig).is_empty(), "{:?}", sent(&rig));
    assert_eq!(state(&rig, id).await, "prepared");
    let asked: Vec<String> = told
        .events()
        .into_iter()
        .filter_map(|event| match event {
            crate::telegram::Event::Owner(text) if text.contains("ETH azaldı") => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(
        asked[0].contains("Keeper cüzdanında ETH azaldı"),
        "{}",
        asked[0]
    );
    assert!(asked[0].contains(&keeper.to_string()), "{}", asked[0]);
    assert!(meta(&rig, crate::worker::LOW_FUNDS_KEY).await.is_some());
    // Funded: served, and the notice is cleared.
    rig.chain(|chain| {
        chain.balances.insert(
            keeper,
            U256::from(10u64) * U256::from(10u64).pow(U256::from(18)),
        )
    });
    process.tick().await.unwrap();
    assert_eq!(sent(&rig).len(), 1, "{:?}", sent(&rig));
    rig.settle();
    process.tick().await.unwrap();
    assert_eq!(state(&rig, id).await, "served");
    assert!(meta(&rig, crate::worker::LOW_FUNDS_KEY).await.is_none());
    process.stop().await;
}

/// R4 PoC helper: what a single fulfillment of a request with this callback gas costs up front on a funded wallet.
async fn r4_single_up_front(callback_gas: u32) -> u128 {
    let rig = serving_rig().await;
    let process = rig.process(true, false).await;
    let id = rig.request_with(callback_gas);
    serve(&rig, &process, id).await;
    for _ in 0..4 {
        if !sent(&rig).is_empty() {
            break;
        }
        rig.chain(|chain| chain.mine(1));
        process.tick().await.unwrap();
    }
    assert_eq!(sent(&rig).len(), 1, "{:?}", sent(&rig));
    let up_front = rig.chain(|chain| chain.queued_up_front());
    process.stop().await;
    up_front[0]
}
/// R4 M2: the wallet holds enough for a small fulfillment but not for one with a large callback. The small sends go out,
/// the large request keeps waiting, and the owner is asked once for the episode, not once per small send.
#[tokio::test]
async fn the_low_funds_page_is_once_per_episode_while_smaller_sends_stay_affordable() {
    let small = r4_single_up_front(100_000).await;
    let big = r4_single_up_front(1_000_000).await;
    let balance = small * 3 / 2;
    assert!(big > balance, "{big} {balance}");
    let rig = serving_rig().await;
    let keeper = rig.chain(|chain| chain.keeper);
    rig.chain(|chain| chain.balances.insert(keeper, U256::from(balance)));
    let mut process = rig.process(true, false).await;
    let mut told = process.told();
    let big_id = rig.request_with(1_000_000);
    let small_id = rig.request();
    serve(&rig, &process, big_id).await;
    serve(&rig, &process, small_id).await;
    for _ in 0..4 {
        rig.chain(|chain| chain.include());
        process.tick().await.unwrap();
    }
    let third = rig.request();
    serve(&rig, &process, third).await;
    for _ in 0..4 {
        rig.chain(|chain| chain.include());
        process.tick().await.unwrap();
    }
    let asked = told
        .events()
        .into_iter()
        .filter(|event| {
            matches!(event, crate::telegram::Event::Owner(text) if text.contains("ETH azaldı"))
        })
        .count();
    assert_eq!(asked, 1, "{asked} pages; sent: {:?}", sent(&rig));
    assert_eq!(
        sent(&rig).len(),
        2,
        "both small requests are served: {:?}",
        sent(&rig)
    );
    assert!(meta(&rig, crate::worker::LOW_FUNDS_KEY).await.is_some());
    process.stop().await;
}

/// R4 L2: a fulfillment is signed and broadcast, the node loses it, and by the time the keeper broadcasts its bytes again
/// the wallet cannot pay them up front: the node refuses them, and the owner is asked once, in Turkish, to fund the
/// wallet.
#[tokio::test]
async fn a_signed_fulfillment_the_wallet_can_no_longer_pay_asks_the_owner_once() {
    let rig = serving_rig().await;
    let keeper = rig.chain(|chain| chain.keeper);
    let mut process = rig.process(true, false).await;
    let mut told = process.told();
    let id = rig.request();
    serve(&rig, &process, id).await;
    assert_eq!(sent(&rig).len(), 1, "{:?}", sent(&rig));
    rig.chain(|chain| {
        chain.drop_queue();
        chain.balances.insert(keeper, U256::from(1_000u64));
    });
    for _ in 0..4 {
        rig.chain(|chain| chain.mine(1));
        process.tick().await.unwrap();
    }
    let asked = told
        .events()
        .into_iter()
        .filter(|event| {
            matches!(event, crate::telegram::Event::Owner(text) if text.contains("ETH azaldı"))
        })
        .count();
    assert_eq!(asked, 1);
    assert!(meta(&rig, crate::worker::LOW_FUNDS_KEY).await.is_some());
    process.stop().await;
}

/// The proof feed: a round keeper announces each request its accepted fulfillment served, once, with the request's drand
/// round, whether it was sent alone or in a batch.
#[tokio::test]
async fn an_accepted_round_fulfillment_is_announced_once_per_request_with_its_round() {
    let drain = |proofs: &mut tokio::sync::mpsc::Receiver<crate::discord::ProofAccepted>| {
        let mut seen = Vec::new();
        while let Ok(proof) = proofs.try_recv() {
            seen.push((proof.request_id, proof.source));
        }
        seen
    };
    // Alone.
    let rig = serving_rig().await;
    let mut process = rig.process(true, false).await;
    let mut proofs = process.proofs();
    let id = rig.request();
    let (_, round, _) = bound(&rig, id);
    serve(&rig, &process, id).await;
    assert_eq!(sent(&rig).len(), 1, "{:?}", sent(&rig));
    assert!(drain(&mut proofs).is_empty(), "nothing before the receipt");
    for _ in 0..3 {
        rig.settle();
        process.tick().await.unwrap();
    }
    assert_eq!(
        drain(&mut proofs),
        [(U256::from(id), crate::discord::Source::Round(round))]
    );
    process.stop().await;
    // In a batch: one notice for each member.
    let rig = serving_rig().await;
    let mut process = rig.process(true, false).await;
    let mut proofs = process.proofs();
    let (first, second) = (rig.request(), rig.request());
    let (_, round, _) = bound(&rig, first);
    serve(&rig, &process, first).await;
    assert!(
        sent(&rig)[0].0.contains("fulfillRandomnessBatch"),
        "{:?}",
        sent(&rig)
    );
    for _ in 0..3 {
        rig.settle();
        process.tick().await.unwrap();
    }
    let mut seen = drain(&mut proofs);
    seen.sort_by_key(|(id, _)| *id);
    assert_eq!(
        seen,
        [
            (U256::from(first), crate::discord::Source::Round(round)),
            (U256::from(second), crate::discord::Source::Round(round))
        ]
    );
    process.stop().await;
}
