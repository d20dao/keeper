import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";

// Vendored upstream files and the SHA-256 of their original bytes (contracts/vendor/PROVENANCE.md).
const PINNED = {
  "VRF.sol": "98e0345fd2dd42178cad4ad16dd8b18621112078d63d0c64839c8bd01c539350",
  "bls-bn254/BLS.sol": "887fd94553b9d6d0a247900bdb05523d3b9ef3b8ca049e2a0524a0b1a9d37e69",
  "bls-bn254/ModExp.sol": "fd91ae9291511668914b05fece03fb9bf6e2b57f7784d2edddc82456af12b495",
  // The license travels with the vendored code; a changed or missing notice is a change to what was vendored.
  "bls-bn254/LICENSE": "af36460fa628a7aca8c5b1f1b6b3615376f00212284fa0fd61a013ee982a3666",
};
for (const [file, expected] of Object.entries(PINNED)) {
  const bytes = readFileSync(new URL(`../contracts/vendor/${file}`, import.meta.url));
  const actual = createHash("sha256").update(bytes).digest("hex");
  if (actual !== expected) throw new Error(`Vendored ${file} changed: expected ${expected}, got ${actual}`);
}
console.log("Vendored Chainlink VRF.sol and bls-bn254 BLS.sol, ModExp.sol and LICENSE match their pinned upstream sources.");
