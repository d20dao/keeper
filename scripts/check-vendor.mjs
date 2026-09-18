import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";

const bytes = readFileSync(new URL("../contracts/vendor/VRF.sol", import.meta.url));
const actual = createHash("sha256").update(bytes).digest("hex");
const expected = "98e0345fd2dd42178cad4ad16dd8b18621112078d63d0c64839c8bd01c539350";
if (actual !== expected) throw new Error(`Vendored verifier changed: expected ${expected}, got ${actual}`);
console.log("Chainlink VRF.sol matches pinned upstream source.");
