// Writes keeper/tests/fixtures/round-coordinator-abi.json: every function, event and error of the compiled round coordinator, with
// its canonical signature, selector or topic, outputs and indexed inputs, for the keeper's binding (keeper/src/abi_round.rs) to be
// checked against. Run from the repository root of a tree with the round contracts (branch rh/round-contracts), after
// `npx hardhat compile`:
//
//   node keeper/tests/fixtures/round-coordinator-abi.gen.mjs \
//     artifacts/contracts/robinhood/D20VRFCoordinatorRobinhood.sol/D20VRFCoordinatorRobinhood.json \
//     > keeper/tests/fixtures/round-coordinator-abi.json
//
// ROUND_CONTRACTS_COMMIT names the commit the artifact was compiled from when the tree is not a git checkout.
import { readFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { id } from "ethers";

const path = process.argv[2];
if (!path) throw new Error("usage: node round-coordinator-abi.gen.mjs <D20VRFCoordinatorRobinhood.json artifact>");
const artifact = JSON.parse(readFileSync(path, "utf8"));
let commit = process.env.ROUND_CONTRACTS_COMMIT ?? "unknown";
if (!process.env.ROUND_CONTRACTS_COMMIT) {
  try {
    commit = execFileSync("git", ["rev-parse", "--short=7", "HEAD"], { encoding: "utf8" }).trim();
  } catch {}
}

/// A parameter's canonical type: a tuple as its components in parentheses, with its array suffix.
const canonical = (param) => {
  if (!param.type.startsWith("tuple")) return param.type;
  return `(${param.components.map(canonical).join(",")})${param.type.slice("tuple".length)}`;
};
const list = (params) => `(${params.map(canonical).join(",")})`;
const byName = (a, b) => (a.signature < b.signature ? -1 : a.signature > b.signature ? 1 : 0);
const functions = artifact.abi.filter((item) => item.type === "function").map((item) => {
  const signature = `${item.name}${list(item.inputs)}`;
  return { signature, selector: id(signature).slice(0, 10), outputs: list(item.outputs), mutability: item.stateMutability };
}).sort(byName);
const events = artifact.abi.filter((item) => item.type === "event").map((item) => {
  const signature = `${item.name}${list(item.inputs)}`;
  return { signature, topic: id(signature), indexed: item.inputs.filter((input) => input.indexed).length, anonymous: item.anonymous };
}).sort(byName);
const errors = artifact.abi.filter((item) => item.type === "error").map((item) => {
  const signature = `${item.name}${list(item.inputs)}`;
  return { signature, selector: id(signature).slice(0, 10) };
}).sort(byName);

const fixture = {
  about: "The ABI of the compiled round coordinator (contracts/robinhood/D20VRFCoordinatorRobinhood.sol with BeaconBook.sol): every function with its selector and outputs, every event with its topic and indexed inputs, every error with its selector. keeper/src/abi_round.rs must match it exactly.",
  regenerate: "npx hardhat compile; node keeper/tests/fixtures/round-coordinator-abi.gen.mjs artifacts/contracts/robinhood/D20VRFCoordinatorRobinhood.sol/D20VRFCoordinatorRobinhood.json > keeper/tests/fixtures/round-coordinator-abi.json, in a tree with the round contracts",
  source: `rh/round-contracts at ${commit}, ${artifact.sourceName}`,
  abiSha256: createHash("sha256").update(JSON.stringify(artifact.abi)).digest("hex"),
  functions,
  events,
  errors,
};
process.stdout.write(`${JSON.stringify(fixture, null, 1)}\n`);
