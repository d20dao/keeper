//! The gas of a round coordinator's fulfillment (`COORDINATOR_KIND=round`): the port of the round contracts' model,
//! `scripts/lib/round-gas.ts`, which `test/robinhood/Gas.test.ts` measured on a local chain for every shape.
//!
//! Two numbers come of a shape, and the keeper uses each for one thing:
//! - `gas_limit`: the least gas limit at which the fulfillment passes the coordinator's guards, behind the proxy that
//!   keeps a 64th of the gas. The keeper never sends below it, with the transaction's L1 component and a margin of 1%,
//!   at least 5,000, on top (`chain_limit`), which also covers what a node's `eth_estimateGas` adds to the least limit.
//! - `gas_bound`: an upper bound of the L2 gas the fulfillment can use. The fee gate prices a fulfillment on it, not on
//!   `eth_estimateGas`, whose callback reserves are never spent (design C, review H2).
//!
//! A shape counts every round the fulfillment lists as one it verifies, even a round cached at the decision head: a
//! replaced block can uncache it before the transaction runs, and the fulfillment then verifies it inside the budget
//! (review M1). `keeper/tests/fixtures/round-gas-model.json` holds the TypeScript model's numbers for a table of shapes,
//! and the tests below hold this port to them.
use anyhow::{Result, ensure};

/// Upper bounds of L2 gas used (`ROUND_GAS`), each a little above the largest measurement, with one VRF hash-to-curve
/// candidate per proof and the costliest drand hash-to-curve for every round verified. A member's own callback gas limit
/// comes on top, in full.
/// fulfillRandomness of a request whose round is cached, the round's signature sent anyway.
pub const SINGLE: u64 = 223_400;
/// fulfillRandomness verifying the request's round: the BLS check, the cache write and RoundVerified.
pub const SINGLE_ROUND: u64 = 217_300;
/// fulfillRandomnessBatch's fixed part.
pub const BATCH: u64 = 79_500;
/// Each member a batch serves.
pub const BATCH_MEMBER: u64 = 161_400;
/// Each listed round a batch verifies.
pub const BATCH_ROUND: u64 = 205_600;
/// Each served member for each listed round: finding a member's round in the list.
pub const BATCH_MEMBER_ROUND: u64 = 250;
/// Each VRF hash-to-curve candidate after a proof's first.
pub const VRF_CANDIDATE: u64 = 5_500;
/// Once per transaction whose first served member finds the coordinator's earned fees at zero (after a withdrawal).
pub const FEES_WITHDRAWN: u64 = 17_200;
/// Robinhood's extra cost per verified round over the local measurement (`ROUND_ROBINHOOD_ROUND_EXCESS`): the testnet's
/// drand hash-to-curve, taken whole until the testnet drills measure it on the chain itself.
pub const ROBINHOOD_ROUND_EXCESS: u64 = 17_000;
/// The VRF hash-to-curve candidates a proof is priced with when its own count is not known: a proof needs more with
/// probability 2^-10.
pub const PRICED_VRF_CANDIDATES: u32 = 10;
/// The coordinator's MAX_FULFILL_BATCH.
pub const MAX_FULFILL_BATCH: usize = 16;

/// The coordinator's guards (`CALLBACK_RESERVE`, `BATCH_MEMBER_OVERHEAD`, and `ROUND_VERIFY_GAS_NEEDED` =
/// 400,000 + 400,000 / 63 + 5,000, what a round still to verify asks to be left before its verifier is called).
pub const CALLBACK_RESERVE: u64 = 140_000;
pub const MEMBER_OVERHEAD: u64 = 400_000;
pub const ROUND_VERIFICATION: u64 = 400_000 + 400_000 / 63 + 5_000;

/// A node's `eth_estimateGas` is not the least gas limit at which a call passes but the top of the last window it halved.
/// geth's estimator, which Nitro (Robinhood Chain) runs, tries the gas the call used, padded to
/// `(used + refund + 2,300) × 64/63`, then twice that, and halves the window between the last limit that failed and the
/// last that passed until it is narrower than 1.5% of its top (`estimateGasErrorRatio`), which it returns. When the
/// doubled guess is the first limit that passes, the window is half of its top, and when every halving after it fails it
/// ends at 1/128 of it: the estimate then stands at most 128/127 of the least limit that passes (0.79%). Robinhood
/// testnet's first fulfillments, whose guards ask for about twice the gas they use, took that path: their estimates,
/// 1,145,832 and 2,205,422, are twice the padded guesses 572,916 and 1,102,711, and the model's least limits with the
/// node's L1 component (1,138,073 and 2,190,363) lie in their last windows (above 1,136,880 and 2,188,192). The model was
/// right, and below the estimate. The window of that path, 1/128 of the estimate.
pub const ESTIMATE_WINDOW: u64 = 128;
/// The model's margin over a fulfillment's least limit, in hundredths, and its least (`chain_limit`): 1% and at least
/// 5,000, above the doubled guess's window (0.79%) and above the measured least limits, which the model's own terms
/// clear by only about 800 gas.
pub const LIMIT_MARGIN_PERCENT: u64 = 1;
pub const LIMIT_MARGIN_MIN: u64 = 5_000;
/// Above this share of the least limit, in thousandths, an estimate says the chain needs more than the model: on any path
/// the estimator's last window is narrower than 1.5% of its top, and the warning keeps a tenth of a percent beside it for
/// the L1 component, which the node prices at its own moment.
pub const ESTIMATE_WARN_PER_MILLE: u64 = 16;
/// The model's margin on a least gas limit `least`: 1% of it, rounded up, and at least 5,000.
pub fn estimate_cover(least: u64) -> u64 {
    least.saturating_add(
        least
            .div_ceil(100 / LIMIT_MARGIN_PERCENT)
            .max(LIMIT_MARGIN_MIN),
    )
}
/// The most a node estimates, whatever path its search takes, for a call whose least passing gas limit is `least`, with
/// the warning's tenth of a percent: `least × 1.016`, rounded up.
pub fn estimate_tolerance(least: u64) -> u64 {
    let most = (u128::from(least) * u128::from(1_000 + ESTIMATE_WARN_PER_MILLE)).div_ceil(1_000);
    u64::try_from(most).unwrap_or(u64::MAX)
}

/// A fulfillment as the keeper sends it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shape {
    /// fulfillRandomnessBatch, or fulfillRandomness.
    pub batch: bool,
    /// Each served member's callback gas limit.
    pub callback_gas_limits: Vec<u32>,
    /// The rounds the fulfillment lists: the request's round for a single, every distinct round of the members for a
    /// batch. Each counts as verified, cached at the decision head or not (review M1).
    pub rounds: u64,
    /// Each member's VRF hash-to-curve candidates (`prover::hash_to_curve_candidates`); PRICED_VRF_CANDIDATES each when
    /// not known.
    pub vrf_candidates: Option<Vec<u32>>,
    /// Whether the coordinator's earned fees may be zero when it runs (after a withdrawal). The keeper does not know,
    /// so it prices the worst case, true.
    pub fees_withdrawn: bool,
}
/// What every formula reads of a shape, checked.
struct Parts {
    members: u64,
    rounds: u64,
    callbacks: u64,
    /// Σ (candidates − 1) over the proofs.
    candidates: u64,
}
impl Shape {
    /// A single fulfillment of a request with this callback gas limit, its round counted as verified.
    pub fn single(callback_gas_limit: u32) -> Self {
        Self {
            batch: false,
            callback_gas_limits: vec![callback_gas_limit],
            rounds: 1,
            vrf_candidates: None,
            fees_withdrawn: true,
        }
    }
    /// A batch of members with these callback gas limits over `rounds` distinct rounds.
    pub fn batch(callback_gas_limits: Vec<u32>, rounds: u64) -> Self {
        Self {
            batch: true,
            callback_gas_limits,
            rounds,
            vrf_candidates: None,
            fees_withdrawn: true,
        }
    }
    /// The same shape with each member's VRF hash-to-curve candidates.
    pub fn with_candidates(mut self, candidates: Vec<u32>) -> Self {
        self.vrf_candidates = Some(candidates);
        self
    }
    fn parts(&self) -> Result<Parts> {
        let n = self.callback_gas_limits.len();
        ensure!(
            (1..=MAX_FULFILL_BATCH).contains(&n)
                && self.rounds <= n as u64
                && (self.batch || (n == 1 && self.rounds <= 1))
                && self
                    .vrf_candidates
                    .as_ref()
                    .is_none_or(|k| k.len() == n && k.iter().all(|k| *k >= 1)),
            "Invalid fulfillment shape"
        );
        let candidates = match &self.vrf_candidates {
            Some(k) => k.iter().map(|k| u64::from(*k - 1)).sum(),
            None => n as u64 * u64::from(PRICED_VRF_CANDIDATES - 1),
        };
        Ok(Parts {
            members: n as u64,
            rounds: self.rounds,
            callbacks: self
                .callback_gas_limits
                .iter()
                .map(|limit| u64::from(*limit))
                .sum(),
            candidates,
        })
    }
}

/// The L2 gas a fulfillment of this shape can use on Robinhood Chain (`roundFulfilmentGasBound`): the measured bound,
/// its callbacks' limits in full, and ROBINHOOD_ROUND_EXCESS for each round it lists. The fee gate adds the transaction's
/// L1 component, read live with the margin, and multiplies by the base fee.
pub fn gas_bound(shape: &Shape) -> Result<u64> {
    let Parts {
        members: n,
        rounds: r,
        callbacks,
        candidates,
    } = shape.parts()?;
    let fixed = if shape.batch {
        BATCH + n * BATCH_MEMBER + r * BATCH_ROUND + n * r * BATCH_MEMBER_ROUND
    } else {
        SINGLE + r * SINGLE_ROUND
    };
    let withdrawn = if shape.fees_withdrawn {
        FEES_WITHDRAWN
    } else {
        0
    };
    Ok(fixed + callbacks + candidates * VRF_CANDIDATE + withdrawn + r * ROBINHOOD_ROUND_EXCESS)
}

/// What the coordinator's guard asks to be left for one member: its callback, a 63rd of it and the member overhead.
fn member_gas(limit: u32) -> u64 {
    let limit = u64::from(limit);
    limit + limit / 63 + MEMBER_OVERHEAD
}
/// What a guard's budget costs of the transaction's gas limit behind the proxy: its DELEGATECALL forwards at most 63/64
/// of the gas left (EIP-150), so every unit a guard asks for costs 64/63 of a unit, rounded up.
fn via_proxy(gas: u64) -> u64 {
    (gas * 64).div_ceil(63)
}
/// The budget the coordinator's guard asks for before it serves these members over `rounds` rounds to verify:
/// CALLBACK_RESERVE + Σ(limit + limit / 63 + MEMBER_OVERHEAD) + rounds × ROUND_VERIFICATION. A single verifying its round
/// asks the same as a batch of one.
pub fn guard_budget(shape: &Shape) -> Result<u64> {
    let Parts { rounds, .. } = shape.parts()?;
    Ok(CALLBACK_RESERVE
        + shape
            .callback_gas_limits
            .iter()
            .map(|limit| member_gas(*limit))
            .sum::<u64>()
        + rounds * ROUND_VERIFICATION)
}

/// The least gas limit at which a fulfillment of this shape passes the coordinator's guards (`roundFulfilmentGasLimit`),
/// a little above the smallest one measured: 64/63 of what the guard budgets, plus the intrinsic gas, the calldata and
/// the work before the guard. A single of a cached round is guarded only before its callback, after its proof's check.
/// L2 gas: the transaction's L1 component comes on top (`least_limit`), and the margin on both (`chain_limit`).
pub fn gas_limit(shape: &Shape) -> Result<u64> {
    let Parts {
        members: n,
        rounds: r,
        candidates,
        ..
    } = shape.parts()?;
    let limits = &shape.callback_gas_limits;
    Ok(if shape.batch {
        via_proxy(guard_budget(shape)?) + 37_500 + 6_800 * n + 1_800 * r
    } else if r == 1 {
        via_proxy(guard_budget(shape)?) + 48_000
    } else {
        let limit = u64::from(limits[0]);
        let withdrawn = if shape.fees_withdrawn { 17_200 } else { 0 };
        via_proxy(CALLBACK_RESERVE + limit + limit / 63) + 218_000 + candidates * 5_600 + withdrawn
    })
}

/// The least gas limit at which a fulfillment of this shape passes on the chain: `gas_limit` and `l1`, the L1 component of
/// its exact payload with the margin, which the chain charges before the transaction runs and `gasleft()` never sees.
pub fn least_limit(shape: &Shape, l1: u64) -> Result<u64> {
    gas_limit(shape)?
        .checked_add(l1)
        .ok_or_else(|| anyhow::anyhow!("Gas overflow"))
}
/// The model's gas limit of a fulfillment of this shape on the chain: its least limit (`least_limit`) and the margin on it
/// (`estimate_cover`), so that the model is at or above the node's estimate (0.77% and 0.79% above Robinhood testnet's
/// first two) and never below the coordinator's guard. The keeper never sends below it.
pub fn chain_limit(shape: &Shape, l1: u64) -> Result<u64> {
    Ok(estimate_cover(least_limit(shape, l1)?))
}

/// The gas limit a fulfillment of this shape is sent with, given its estimate (`eth_estimateGas`, which on an Arbitrum
/// chain holds the L1 component) and `l1`, the L1 component of its exact payload with the margin. The estimate, padded
/// by a fifth of what it holds beyond the guard's budget and at least 50,000, as an epoch keeper pads it; never below
/// `chain_limit`.
pub fn fulfillment_gas(estimate: u64, shape: &Shape, l1: u64) -> Result<u64> {
    let floor = chain_limit(shape, l1)?;
    let padding = (estimate.saturating_sub(guard_budget(shape)?) / 5).max(50_000);
    let padded = estimate
        .checked_add(padding)
        .ok_or_else(|| anyhow::anyhow!("Gas overflow"))?;
    Ok(padded.max(floor))
}

/// One member of a batch as its size is judged: its callback gas limit and the round it lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sized {
    pub callback_gas_limit: u32,
    pub round: (u8, u64),
}
/// The batch shape of the members in `members`: their callbacks, and their distinct rounds.
pub fn batch_shape(members: &[Sized]) -> Shape {
    let mut rounds: Vec<(u8, u64)> = members.iter().map(|member| member.round).collect();
    rounds.sort_unstable();
    rounds.dedup();
    Shape::batch(
        members
            .iter()
            .map(|member| member.callback_gas_limit)
            .collect(),
        rounds.len() as u64,
    )
}
/// How many members, from the front, a batch can hold under `cap` (the configured caps at this price per gas, and the
/// chain's own limit): the most whose gas limit on the chain (`chain_limit`), with every distinct round of theirs counted
/// as verified and each member's share `l1_each` of the payload's L1 component, fits. Never above MAX_FULFILL_BATCH; 0
/// when not even one member fits. With one callback gas limit throughout and the rounds the members bring, this is the
/// round contracts' `roundMaxBatch` for every cap of the fixture: the margin moves none of them.
pub fn members_within(members: &[Sized], cap: u64, l1_each: u64) -> usize {
    (1..=members.len().min(MAX_FULFILL_BATCH))
        .take_while(|&count| {
            let shape = batch_shape(&members[..count]);
            chain_limit(&shape, l1_each.saturating_mul(count as u64)).is_ok_and(|gas| gas <= cap)
        })
        .last()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use serde_json::Value;

    /// The TypeScript model's numbers (see the module documentation).
    fn fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/round-gas-model.json")).unwrap()
    }
    fn number(value: &Value) -> u64 {
        value.as_str().unwrap().parse().unwrap()
    }
    fn shape_of(case: &Value) -> Shape {
        Shape {
            batch: case["batch"].as_bool().unwrap(),
            callback_gas_limits: case["callbackGasLimits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|limit| limit.as_u64().unwrap() as u32)
                .collect(),
            rounds: case["roundsToVerify"].as_u64().unwrap(),
            vrf_candidates: case["vrfCandidates"].as_array().map(|k| {
                k.iter()
                    .map(|candidates| candidates.as_u64().unwrap() as u32)
                    .collect()
            }),
            fees_withdrawn: case["feesWithdrawn"].as_bool().unwrap(),
        }
    }
    /// The L1 component the round contracts' pricing assumes (`roundFulfilmentL1Gas`): 25,000 for one member, 12,000 a
    /// further member and 3,000 a further listed round. The keeper reads the live one; the pricing test uses this.
    fn priced_l1(shape: &Shape) -> u64 {
        let n = shape.callback_gas_limits.len() as u64;
        25_000 + (n - 1) * 12_000 + shape.rounds.saturating_sub(1) * 3_000
    }

    #[test]
    fn the_constants_are_the_models() {
        let model = fixture();
        let constants = &model["constants"];
        for (name, value) in [
            ("single", SINGLE),
            ("singleRound", SINGLE_ROUND),
            ("batch", BATCH),
            ("batchMember", BATCH_MEMBER),
            ("batchRound", BATCH_ROUND),
            ("batchMemberRound", BATCH_MEMBER_ROUND),
            ("vrfCandidate", VRF_CANDIDATE),
            ("feesWithdrawn", FEES_WITHDRAWN),
        ] {
            assert_eq!(number(&constants[name]), value, "{name}");
        }
        assert_eq!(
            number(&model["robinhoodRoundExcess"]),
            ROBINHOOD_ROUND_EXCESS
        );
        assert_eq!(
            model["pricedVrfCandidates"].as_u64(),
            Some(u64::from(PRICED_VRF_CANDIDATES))
        );
        assert_eq!(
            model["maxFulfillBatch"].as_u64(),
            Some(MAX_FULFILL_BATCH as u64)
        );
        assert_eq!(ROUND_VERIFICATION, 411_349);
    }

    #[test]
    fn the_gas_limit_and_the_bound_of_every_shape_are_the_models() {
        let model = fixture();
        let cases = model["gas"].as_array().unwrap();
        assert!(cases.len() >= 30, "{}", cases.len());
        for case in cases {
            let shape = shape_of(case);
            let name = case["name"].as_str().unwrap();
            assert_eq!(
                gas_limit(&shape).unwrap(),
                number(&case["gasLimit"]),
                "{name}"
            );
            assert_eq!(
                gas_bound(&shape).unwrap(),
                number(&case["gasBound"]),
                "{name}"
            );
            assert_eq!(priced_l1(&shape), number(&case["l1Gas"]), "{name}");
        }
        // The shapes the task names, by name: a single first in its round and of a cached round, and batches over one
        // and two rounds, with callbacks of 30,000, 100,000 and 1,000,000.
        for cb in [30_000, 100_000, 1_000_000] {
            for name in [
                format!("single first in its round, {cb}"),
                format!("single of a cached round, {cb}"),
                format!("batch of 2 over 1 rounds, {cb}"),
                format!("batch of 2 over 2 rounds, {cb}"),
                format!("batch of 16 over 2 rounds, {cb}"),
            ] {
                assert!(
                    cases.iter().any(|case| case["name"] == name.as_str()),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn the_largest_batch_under_a_cap_is_the_models() {
        let model = fixture();
        for case in model["maxBatch"].as_array().unwrap() {
            let cb = case["callbackGasLimit"].as_u64().unwrap() as u32;
            let rounds = case["rounds"].as_u64().unwrap();
            let cap = number(&case["cap"]);
            let expected = case["members"].as_u64().unwrap() as usize;
            // `roundMaxBatch`: members of one callback limit, the first `rounds` of them in rounds of their own.
            let members: Vec<Sized> = (0..MAX_FULFILL_BATCH as u64)
                .map(|i| Sized {
                    callback_gas_limit: cb,
                    round: (0, 100 + i.min(rounds.saturating_sub(1))),
                })
                .collect();
            let fit = members_within(&members, cap, 0);
            // The model counts no batch of fewer members than rounds: when even that one does not fit it answers 0, and
            // the front of the batch that fits here is shorter than the rounds.
            if expected == 0 {
                assert!(fit < rounds as usize, "{case}: {fit}");
            } else {
                assert_eq!(fit, expected, "{case}");
            }
        }
    }

    #[test]
    fn the_vrf_candidates_the_prover_counts_are_the_models() {
        let model = fixture();
        let key = &model["vrfCandidates"];
        let public_key = [
            key["publicKey"][0]
                .as_str()
                .unwrap()
                .parse::<U256>()
                .unwrap(),
            key["publicKey"][1]
                .as_str()
                .unwrap()
                .parse::<U256>()
                .unwrap(),
        ];
        let fixture_key =
            k256::SecretKey::from_slice(&U256::from(123_456_789u64).to_be_bytes::<32>()).unwrap();
        assert_eq!(crate::prover::public_key(&fixture_key), public_key);
        let mut seen = std::collections::BTreeSet::new();
        for case in key["seeds"].as_array().unwrap() {
            let seed: U256 = case["seed"].as_str().unwrap().parse().unwrap();
            let expected = case["candidates"].as_u64().unwrap() as u32;
            assert_eq!(
                crate::prover::hash_to_curve_candidates(public_key, seed).unwrap(),
                expected,
                "{seed}"
            );
            seen.insert(expected);
        }
        // The seeds meet more than one count, so the count is not a constant that happens to agree.
        assert!(seen.len() > 1, "{seen:?}");
    }

    #[test]
    fn a_fulfillment_is_sent_at_its_estimate_padded_and_never_below_the_models_limit_with_its_l1_component()
     {
        let shape = Shape::single(100_000);
        let floor = gas_limit(&shape).unwrap();
        // An estimate below the floor: the floor with the L1 component, and the margin on both.
        assert_eq!(
            fulfillment_gas(300_000, &shape, 20_000).unwrap(),
            estimate_cover(floor + 20_000)
        );
        assert_eq!(
            chain_limit(&shape, 20_000).unwrap(),
            floor + 20_000 + (floor + 20_000).div_ceil(100)
        );
        // An estimate above it: padded by a fifth of what it holds beyond the guard's budget, at least 50,000.
        let guard = guard_budget(&shape).unwrap();
        assert_eq!(guard, 140_000 + 100_000 + 100_000 / 63 + 400_000 + 411_349);
        let high = floor + 1_000_000;
        assert_eq!(
            fulfillment_gas(high, &shape, 20_000).unwrap(),
            high + (high - guard) / 5
        );
        assert_eq!(
            fulfillment_gas(floor + 30_000, &shape, 0).unwrap(),
            (floor + 30_000 + 50_000).max(floor)
        );
        // Shapes the coordinator would refuse are refused here.
        for broken in [
            Shape::batch(vec![], 0),
            Shape::batch(vec![30_000; 17], 1),
            Shape::batch(vec![30_000; 2], 3),
            Shape {
                rounds: 2,
                ..Shape::single(30_000)
            },
            Shape::single(30_000).with_candidates(vec![0]),
            Shape::batch(vec![30_000; 2], 1).with_candidates(vec![1]),
        ] {
            assert!(gas_limit(&broken).is_err(), "{broken:?}");
            assert!(gas_bound(&broken).is_err(), "{broken:?}");
        }
    }

    /// geth's `eth_estimateGas` (`gasestimator.Estimate`, error ratio 0.015), which Nitro runs, for a call that passes at a
    /// gas limit of `least` or more, used `used` gas with a refund of `refund` when run at the cap, and may have at most
    /// `cap`: the optimistic guess, then the window halved, each step skewed to at most twice the limit that last failed.
    fn node_estimate(used: u64, refund: u64, least: u64, cap: u64) -> u64 {
        let passes = |limit: u64| limit >= least;
        let (mut lo, mut hi) = (used - 1, cap);
        let optimistic = (used + refund + 2_300) * 64 / 63;
        if optimistic < hi {
            if passes(optimistic) {
                hi = optimistic;
            } else {
                lo = optimistic;
            }
        }
        while lo + 1 < hi {
            if ((hi - lo) as f64) / (hi as f64) < 0.015 {
                break;
            }
            let mid = ((hi + lo) / 2).min(lo * 2);
            if passes(mid) {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        hi
    }
    /// A live case of the fixture: its shape, and the L1 component the keeper priced it with (the node's, with the margin).
    fn live(case: &Value) -> (Shape, u64) {
        let limits: Vec<u32> = case["callbackGasLimits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|limit| limit.as_u64().unwrap() as u32)
            .collect();
        let rounds = case["roundsToVerify"].as_u64().unwrap();
        let shape = if case["batch"].as_bool().unwrap() {
            Shape::batch(limits, rounds)
        } else {
            Shape::single(limits[0])
        };
        let l1 = case["l1Gas"].as_u64().unwrap();
        let margin = case["l1MarginBps"].as_u64().unwrap();
        (shape, (l1 * (10_000 + margin)).div_ceil(10_000))
    }

    /// Robinhood testnet's first two fulfillments, shapes and L1 components as the keeper priced them: the node estimated
    /// 1,145,832 and 2,205,422, above the model's 1,143,179 and 2,200,842, and the keeper warned. The node's estimator
    /// replays both estimates exactly from the model's own least limit (with the node's L1 component, which the node
    /// estimates without the keeper's margin) and the gas the fulfillment used: the estimate is the doubled optimistic
    /// guess, and every later halving failed. Nothing is missing from the model; the node rounds up. The model's limit on
    /// the chain now covers both estimates, by under 1%, and the guard's floor is where it was.
    #[test]
    fn the_node_estimates_of_robinhood_testnets_first_fulfillments_are_its_rounding_of_the_models_least_limit()
     {
        let model = fixture();
        let cases = model["robinhoodLive"].as_array().unwrap();
        assert_eq!(cases.len(), 2);
        // The gas the node's run used, refund and L1 component included: the single's receipt (556,063, of which 17,621 is
        // L1) less its L1 component at inclusion, the 2,800 refund of the coordinator's reentrancy guard put back, and the
        // L1 component the node priced in its estimate. The batch has no receipt in the fixture: its estimate fixes its
        // use at 1,083,182.
        let single = &cases[0];
        let node_l1 = single["l1Gas"].as_u64().unwrap();
        let single_used = single["receiptGasUsed"].as_u64().unwrap()
            - single["receiptL1Gas"].as_u64().unwrap()
            + node_l1;
        let used = [(single_used, 2_800), (1_083_182 - 2_800, 2_800)];
        for (case, (used, refund)) in cases.iter().zip(used) {
            let name = case["name"].as_str().unwrap();
            let (shape, l1) = live(case);
            assert_eq!(
                gas_limit(&shape).unwrap(),
                number(&case["gasLimit"]),
                "{name}"
            );
            // What the keeper logged: the model's least limit with the L1 component and its margin.
            let logged = case["loggedModelLimit"].as_u64().unwrap();
            assert_eq!(least_limit(&shape, l1).unwrap(), logged, "{name}");
            let estimate = case["estimate"].as_u64().unwrap();
            assert!(logged < estimate, "{name}");
            // The node's estimate of a call whose least limit is the model's, with the node's own L1 component.
            let least = gas_limit(&shape).unwrap() + case["l1Gas"].as_u64().unwrap();
            assert_eq!(
                node_estimate(used, refund, least, 32_000_000),
                estimate,
                "{name}"
            );
            // The doubled guess, and the window every halving left: the least limit lies in it, the model's too.
            let doubled = 2 * ((used + refund + 2_300) * 64 / 63);
            assert_eq!(doubled, estimate, "{name}");
            let window = estimate - estimate / ESTIMATE_WINDOW;
            assert!(least > window && least <= estimate, "{name}: {least}");
            // The model's limit on the chain: at or above the estimate, by less than 1%, and above the guard.
            let limit = chain_limit(&shape, l1).unwrap();
            assert!(limit >= estimate, "{name}: {limit}");
            assert!(limit * 100 <= estimate * 101, "{name}: {limit}");
            assert!(
                limit > via_proxy(guard_budget(&shape).unwrap()) + l1,
                "{name}"
            );
            assert!(estimate <= estimate_tolerance(least_limit(&shape, l1).unwrap()));
        }
        let (single, single_l1) = live(&cases[0]);
        let (batch, batch_l1) = live(&cases[1]);
        assert_eq!(chain_limit(&single, single_l1).unwrap(), 1_154_611);
        assert_eq!(chain_limit(&batch, batch_l1).unwrap(), 2_222_851);
    }

    /// Whatever gas a fulfillment uses and whatever its shape, the node's estimate of the model's least limit never stands
    /// above `estimate_tolerance` of it; on the doubled guess's path, where every halving fails, it never stands above the
    /// model's limit on the chain. The cover never moves the limit below the least one.
    #[test]
    fn the_nodes_estimate_of_any_shape_stays_within_the_models_cover_on_its_path_and_its_tolerance_on_any()
     {
        let model = fixture();
        let mut paths = (0, 0);
        for case in model["gas"].as_array().unwrap() {
            let shape = shape_of(case);
            for l1 in [0, 25_000, 160_000] {
                let least = least_limit(&shape, l1).unwrap();
                assert!(chain_limit(&shape, l1).unwrap() >= least);
                // From a twentieth of the least limit to all of it, in 997 steps.
                for step in 1..=997u64 {
                    let used = (least / 20).max(21_001) + (least - least / 20) * step / 997;
                    let estimate = node_estimate(used.min(least), 0, least, 32_000_000);
                    assert!(estimate >= least, "{case}");
                    assert!(estimate <= estimate_tolerance(least), "{case} {used}");
                    let doubled = 2 * ((used.min(least) + 2_300) * 64 / 63);
                    if estimate == doubled {
                        paths.0 += 1;
                        assert!(estimate <= estimate_cover(least), "{case} {used}");
                    } else {
                        paths.1 += 1;
                    }
                }
            }
        }
        // Both paths are met.
        assert!(paths.0 > 0 && paths.1 > 0, "{paths:?}");
        assert_eq!(estimate_cover(1_143_179), 1_154_611);
        // At least 5,000 over a small least limit.
        assert_eq!(estimate_cover(300_000), 305_000);
        assert_eq!(estimate_tolerance(1_143_179), 1_161_470);
        assert_eq!(estimate_tolerance(u64::MAX), u64::MAX);
        assert_eq!(estimate_cover(u64::MAX), u64::MAX);
    }

    #[test]
    fn a_batch_counts_each_distinct_round_of_its_members_once_and_fits_its_cap_from_the_front() {
        let member = |cb: u32, round: u64| Sized {
            callback_gas_limit: cb,
            round: (0, round),
        };
        let members = [
            member(100_000, 7),
            member(100_000, 7),
            member(100_000, 8),
            member(1_000_000, 8),
            member(100_000, 9),
        ];
        assert_eq!(batch_shape(&members[..2]).rounds, 1);
        assert_eq!(batch_shape(&members[..4]).rounds, 2);
        assert_eq!(batch_shape(&members).rounds, 3);
        // The same round of another beacon is another round.
        let other = [
            member(100_000, 7),
            Sized {
                round: (1, 7),
                ..member(100_000, 7)
            },
        ];
        assert_eq!(batch_shape(&other).rounds, 2);
        // The cap at exactly the limit on the chain of the first three members, with their L1 shares, holds three and not
        // four.
        let three = chain_limit(&batch_shape(&members[..3]), 3 * 10_000).unwrap();
        assert_eq!(members_within(&members, three, 10_000), 3);
        assert_eq!(members_within(&members, three - 1, 10_000), 2);
        assert_eq!(members_within(&members, u64::MAX, 10_000), 5);
        assert_eq!(members_within(&members, 1_000, 0), 0);
        // Never more than the coordinator takes.
        let many = vec![member(30_000, 7); 20];
        assert_eq!(members_within(&many, u64::MAX, 0), MAX_FULFILL_BATCH);
        // Robinhood testnet: MAX_GAS 13,000,000 under the chain's 32,000,000 holds 8 members with 1,000,000-gas callbacks
        // over two rounds, and every batch of 100,000-gas callbacks.
        let heavy: Vec<Sized> = (0..16).map(|i| member(1_000_000, 7 + i % 2)).collect();
        assert_eq!(members_within(&heavy, 13_000_000, 0), 8);
        let light: Vec<Sized> = (0..16).map(|i| member(100_000, 7 + i % 2)).collect();
        assert_eq!(members_within(&light, 13_000_000, 12_000), 16);
    }
}
