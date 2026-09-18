// TEST/LOCAL DEMO ONLY. This BigInt prover is not a production keeper or constant-time signer.
// Uses the unmodified upstream Solidity verifier as the independent acceptance check.
import { secp256k1 } from "@noble/curves/secp256k1";
import { invert, mod, pow } from "@noble/curves/abstract/modular";
import { AbiCoder, computeAddress, keccak256, solidityPacked, toBeHex } from "ethers";

const P = secp256k1.CURVE.Fp.ORDER;
const N = secp256k1.CURVE.n;
const Point = secp256k1.ProjectivePoint;
type PointType = InstanceType<typeof Point>;
type XY = [bigint, bigint];
const abi = AbiCoder.defaultAbiCoder();
export const TEST_SECRET = 123456789n; // PUBLIC TEST FIXTURE, NEVER FUND OR DEPLOY THIS KEY.

function xy(point: PointType): XY {
  const p = point.toAffine();
  return [p.x, p.y];
}
export function publicKey(secret = TEST_SECRET): XY { return xy(Point.BASE.multiply(secret)); }

function fieldHash(bytes: string): bigint {
  let x = BigInt(keccak256(bytes));
  while (x >= P) x = BigInt(keccak256(toBeHex(x, 32)));
  return x;
}

function hashToCurve(pk: XY, seed: bigint): PointType {
  let x = fieldHash(solidityPacked(["uint256", "uint256[2]", "uint256"], [1n, pk, seed]));
  for (;;) {
    const y2 = mod(x * x * x + 7n, P);
    let y = pow(y2, (P + 1n) / 4n, P);
    if (mod(y * y, P) === y2) {
      if (y % 2n) y = P - y;
      return Point.fromAffine({ x, y });
    }
    x = fieldHash(toBeHex(x, 32));
  }
}

export function makeProof(seed: bigint, secret = TEST_SECRET, salt = 0n) {
  const pk = publicKey(secret);
  const h = hashToCurve(pk, seed);
  const gamma = h.multiply(secret);
  // Deterministic TEST nonce. Production signer implementation/review is explicitly out of scope.
  let k = mod(BigInt(keccak256(abi.encode(
    ["string", "uint256", "uint256", "uint256"], ["TEST_ONLY_VRF_NONCE", secret, seed, salt]
  ))), N - 1n) + 1n;
  for (;;) {
    const u = Point.BASE.multiply(k);
    const v = h.multiply(k);
    const uWitness = computeAddress(`0x${u.toHex(false)}`);
    const c = BigInt(keccak256(solidityPacked(
      ["uint256", "uint256[2]", "uint256[2]", "uint256[2]", "uint256[2]", "address"],
      [2n, xy(h), pk, xy(gamma), xy(v), uWitness]
    )));
    const s = mod(k - c * secret, N);
    if (mod(c, N) === 0n || s === 0n) { k = mod(k, N - 1n) + 1n; continue; }
    const cg = xy(gamma.multiply(mod(c, N)));
    const sh = xy(h.multiply(s));
    if (cg[0] === sh[0]) { k = mod(k, N - 1n) + 1n; continue; }
    const lz = mod(sh[0] - cg[0], P);
    const dx = mod(lz * lz, P);
    const dy = mod(dx * lz, P);
    const z = dx === dy ? dx : mod(dx * dy, P);
    return {
      pk, gamma: xy(gamma), c, s, seed, uWitness,
      cGammaWitness: cg, sHashWitness: sh, zInv: invert(z, P),
    };
  }
}

export function proofOutput(proof: ReturnType<typeof makeProof>): string {
  return keccak256(abi.encode(["uint256", "uint256[2]"], [3n, proof.gamma]));
}
