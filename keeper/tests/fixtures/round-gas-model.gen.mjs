// Writes keeper/tests/fixtures/round-gas-model.json: the round coordinator's gas model as scripts/lib/round-gas.ts computes it, for
// the keeper's port of it (keeper/src/round_gas.rs) to be checked against. Run from the repository root of a tree that has the
// round contracts' scripts/lib/round-gas.ts (branch rh/round-contracts):
//
//   node keeper/tests/fixtures/round-gas-model.gen.mjs scripts/lib/round-gas.ts > keeper/tests/fixtures/round-gas-model.json
//
// Node 22.18 or later reads the TypeScript module as it is.
import { pathToFileURL } from "node:url";
import { resolve } from "node:path";
import { execFileSync } from "node:child_process";
import { secp256k1 } from "@noble/curves/secp256k1";

const source = process.argv[2];
if (!source) throw new Error("usage: node round-gas-model.gen.mjs <path to scripts/lib/round-gas.ts>");
const model = await import(pathToFileURL(resolve(source)).href);
// The commit of the model: the last one that changed it, or ROUND_GAS_COMMIT for a copy outside a checkout.
let commit = process.env.ROUND_GAS_COMMIT ?? "unknown";
if (!process.env.ROUND_GAS_COMMIT) {
  try {
    commit = execFileSync("git", ["log", "-1", "--format=%h", "--", source], { encoding: "utf8" }).trim() || "unknown";
  } catch {}
}

const callbacks = [30_000, 100_000, 1_000_000];
const shapes = [];
for (const cb of callbacks) {
  shapes.push({ name: `single first in its round, ${cb}`, batch: false, callbackGasLimits: [cb], roundsToVerify: 1 });
  shapes.push({ name: `single of a cached round, ${cb}`, batch: false, callbackGasLimits: [cb], roundsToVerify: 0 });
  for (const n of [2, 4, 8, 16]) {
    for (const r of [1, 2]) {
      shapes.push({ name: `batch of ${n} over ${r} rounds, ${cb}`, batch: true, callbackGasLimits: Array(n).fill(cb), roundsToVerify: r });
    }
  }
}
// Mixed callbacks, every listed round, exact candidate counts and fees known not to be withdrawn.
shapes.push({ name: "batch of 3 mixed over 3 rounds", batch: true, callbackGasLimits: [30_000, 250_000, 1_000_000], roundsToVerify: 3 });
shapes.push({ name: "single first in its round, 2 candidates, fees kept", batch: false, callbackGasLimits: [50_000], roundsToVerify: 1,
  vrfCandidates: [2], feesWithdrawn: false });
shapes.push({ name: "single of a cached round, 3 candidates", batch: false, callbackGasLimits: [50_000], roundsToVerify: 0, vrfCandidates: [3] });
shapes.push({ name: "batch of 4 over 2 rounds, exact candidates, fees kept", batch: true, callbackGasLimits: [30_000, 30_000, 100_000, 100_000],
  roundsToVerify: 2, vrfCandidates: [1, 2, 1, 4], feesWithdrawn: false });

const text = (value) => value.toString();
const gas = shapes.map(({ name, ...shape }) => ({
  name,
  batch: shape.batch,
  callbackGasLimits: shape.callbackGasLimits,
  roundsToVerify: shape.roundsToVerify,
  vrfCandidates: shape.vrfCandidates ?? null,
  feesWithdrawn: shape.feesWithdrawn ?? true,
  gasLimit: text(model.roundFulfilmentGasLimit(shape)),
  gasBound: text(model.roundFulfilmentGasBound(shape)),
  l1Gas: text(model.roundFulfilmentL1Gas(shape)),
}));
const maxBatch = [];
for (const cb of callbacks) {
  for (const rounds of [1, 2]) {
    for (const cap of [13_000_000n, 16_777_216n, 32_000_000n]) {
      maxBatch.push({ callbackGasLimit: cb, rounds, cap: text(cap), members: model.roundMaxBatch(cb, rounds, cap) });
    }
  }
}
// Robinhood testnet's first fulfillments (keeper rh/keeper-round-k3, 2026-10-06), whose eth_estimateGas the keeper logged
// above the model's limit. Evidence, not computed: the node's estimate, the model limit the keeper logged (gasLimit with the
// L1 component, L1_GAS_MARGIN_BPS 2500 on gasEstimateL1Component), and the single's receipt. The L1 component is the one
// the logged limit implies (25,529 = 20,423 with the margin, 52,395 = 41,916 with it). keeper/src/round_gas.rs replays the
// node's estimate from them; gasLimit is the model's, as for the shapes above.
const robinhoodLive = [
  { name: "testnet single first in its round, 100000", batch: false, callbackGasLimits: [100_000], roundsToVerify: 1,
    l1Gas: 20_423, l1MarginBps: 2_500, loggedModelLimit: 1_143_179, estimate: 1_145_832, receiptGasUsed: 556_063, receiptL1Gas: 17_621 },
  { name: "testnet batch of 3 over 1 rounds, 100000", batch: true, callbackGasLimits: [100_000, 100_000, 100_000], roundsToVerify: 1,
    l1Gas: 41_916, l1MarginBps: 2_500, loggedModelLimit: 2_200_842, estimate: 2_205_422, receiptGasUsed: null, receiptL1Gas: null },
].map((evidence) => ({
  ...evidence,
  gasLimit: text(model.roundFulfilmentGasLimit({ batch: evidence.batch, callbackGasLimits: evidence.callbackGasLimits,
    roundsToVerify: evidence.roundsToVerify })),
}));
// The VRF hash-to-curve candidates of the keeper's test key (the prover's fixture key 123456789) for a run of seeds.
const point = secp256k1.ProjectivePoint.BASE.multiply(123456789n).toAffine();
const publicKey = [point.x, point.y];
const candidates = [];
for (let i = 0n; i < 24n; i++) {
  const seed = i * 0x9e3779b97f4a7c15n + 1n;
  candidates.push({ seed: text(seed), candidates: model.vrfHashToCurveCandidates(publicKey, seed) });
}

const fixture = {
  about: "The round coordinator's gas model as scripts/lib/round-gas.ts computes it: roundFulfilmentGasLimit, roundFulfilmentGasBound, roundFulfilmentL1Gas, roundMaxBatch and vrfHashToCurveCandidates. keeper/src/round_gas.rs must give the same numbers. robinhoodLive is evidence from Robinhood testnet, written in the generator: the node's estimates of the keeper's first fulfillments.",
  regenerate: "node keeper/tests/fixtures/round-gas-model.gen.mjs scripts/lib/round-gas.ts > keeper/tests/fixtures/round-gas-model.json, in a tree that has the round contracts' scripts/lib/round-gas.ts",
  source: `scripts/lib/round-gas.ts at ${commit}`,
  constants: Object.fromEntries(Object.entries(model.ROUND_GAS).map(([key, value]) => [key, text(value)])),
  robinhoodRoundExcess: text(model.ROUND_ROBINHOOD_ROUND_EXCESS),
  l1: Object.fromEntries(Object.entries(model.ROUND_L1_GAS).map(([key, value]) => [key, text(value)])),
  pricedVrfCandidates: model.ROUND_PRICED_VRF_CANDIDATES,
  maxFulfillBatch: model.ROUND_MAX_FULFILL_BATCH,
  gas,
  maxBatch,
  vrfCandidates: { publicKey: publicKey.map(text), seeds: candidates },
  robinhoodLive,
};
process.stdout.write(`${JSON.stringify(fixture, null, 1)}\n`);
