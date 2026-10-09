// Public computation only, safe to bundle for browsers. drand's bls-bn254-unchained-on-g1 scheme, as its evmnet network signs rounds:
// the G1 point a round's signature signs, the check of a signature under a group key, and the randomness a signature gives. It knows
// no contract and no chain.
import { bn254 } from "@noble/curves/bn254";
import { mod, pow } from "@noble/curves/abstract/modular";
import { concat, getBytes, hexlify, keccak256, sha256, toBeHex, toUtf8Bytes, type BytesLike } from "ethers";

/// RFC 9380 domain separation tag of the bls-bn254-unchained-on-g1 hash-to-curve, as in D20BeaconVerifier.
export const DRAND_DST = "BLS_SIG_BN254G1_XMD:KECCAK-256_SVDW_RO_NUL_";
/// drand's evmnet: its chain hash, group key in drand's word order, and schedule (round r at genesis + (r - 1) × period).
export const DRAND_EVMNET = Object.freeze({
  chainHash: "0x04f1e9062b8a81f848fded9c12306733282b2727ecced50032187751166ec8c3",
  publicKey: "0x07e1d1d335df83fa98462005690372c643340060d205306a9aa8106b6bd0b3820557ec32c2ad488e4d4f6008f89a346f18492092ccc0d594610de2732c8b808f0095685ae3a85ba243747b1b2f426049010f6b73a0cf1d389351d5aaaa1047f6297d3a4f9749b33eb2d904c9d9ebf17224150ddd7abd7567a9bec6c74480ee0b",
  genesis: 1727521075n, period: 3n,
});

// The BN254 base field order, written out (it is bn254.fields.Fp.ORDER). bn254 is read only inside the functions that verify or
// hash, never at module scope: a bundler can then drop noble's bn254 setup, about 45 ms to evaluate, from every chunk that
// imports this library without verifying a round, such as server chunks that run under a 10 ms CPU budget.
const P = 21888242871839275222246405745257275088696311157297823662689037894645226208583n;
const MAX_ROUND = (1n << 64n) - 1n;
// Rounds are uint64.
const isRound = (round: bigint) => typeof round === "bigint" && round >= 0n && round <= MAX_ROUND;

// RFC 9380 hash_to_curve for BN254 G1 as BLS.sol computes it, line for line: expand_message_xmd with keccak256, two
// 48-byte field elements, the Shallue-van de Woestijne map with Z = 1, then their sum. G1 has cofactor 1, so no clearing.
const C1 = 4n, C2 = (P - 1n) / 2n; // g(Z), and -Z / 2, which is also the Legendre exponent
const C3 = 0x16789af3a83522eb353c98fc6b36d713d5d8d1cc5dffffffan; // sqrt(-12), the even root
const C4 = 0x10216f7ba065e00de81ac1e7808072c9dd2b2385cd7b438469602eb24829a9bdn; // -16 / 3
const g = (x: bigint) => mod(x * x * x + 3n, P);
const isSquare = (a: bigint) => pow(a, C2, P) === 1n;

// expand_message_xmd (RFC 9380 section 5.3.1) to 96 bytes: keccak256 has 32-byte digests and 136-byte blocks.
function expandMessage(message: Uint8Array): Uint8Array {
  const dst = toUtf8Bytes(DRAND_DST), suffix = concat([dst, Uint8Array.of(dst.length)]);
  const b0 = getBytes(keccak256(concat([new Uint8Array(136), message, Uint8Array.of(0, 96, 0), suffix])));
  const blocks: Uint8Array[] = [];
  for (let i = 1; i <= 3; i++)
    blocks.push(getBytes(keccak256(concat([i === 1 ? b0 : b0.map((byte, at) => byte ^ blocks[i - 2][at]), Uint8Array.of(i), suffix]))));
  return getBytes(concat(blocks));
}
function mapToPoint(u: bigint): { x: bigint; y: bigint } {
  let tv1 = mod(u * u * C1, P);
  const tv2 = mod(1n + tv1, P);
  tv1 = mod(1n - tv1, P);
  const tv3 = pow(mod(tv1 * tv2, P), P - 2n, P); // inv0: zero maps to zero
  const tv5 = mod(mod(mod(u * tv1, P) * tv3, P) * C3, P);
  const x1 = mod(C2 - tv5, P), x2 = mod(C2 + tv5, P);
  const tv8 = mod(mod(tv2 * tv2, P) * tv3, P);
  const x3 = mod(1n + C4 * mod(tv8 * tv8, P), P);
  const x = isSquare(g(x1)) ? x1 : isSquare(g(x2)) ? x2 : x3;
  const y = pow(g(x), (P + 1n) / 4n, P);
  if (mod(y * y, P) !== g(x)) throw new Error("Map to curve failed");
  return { x, y: (y & 1n) === (u & 1n) ? y : P - y }; // sgn0 is parity
}
/// The G1 point that round's signature signs: hash-to-curve of keccak256(round as 8 big-endian bytes) under DRAND_DST.
export function drandRoundMessage(round: bigint): { x: bigint; y: bigint } {
  if (!isRound(round)) throw new Error("Invalid beacon round");
  const uniform = expandMessage(getBytes(keccak256(toBeHex(round, 8)))), G1 = bn254.G1.Point;
  const [p0, p1] = [uniform.subarray(0, 48), uniform.subarray(48)].map(bytes => G1.fromAffine(mapToPoint(BigInt(hexlify(bytes)) % P)));
  const { x, y } = p0.add(p1).toAffine();
  return { x, y };
}

const words = (bytes: Uint8Array) => Array.from({ length: bytes.length / 32 }, (_, i) => BigInt(hexlify(bytes.subarray(32 * i, 32 * i + 32))));
// Points must be canonical (coordinates below the field order), not the infinity encoding, and on the curve; a G2 key must
// also lie in the prime-order subgroup, which D20BeaconVerifier leaves to the pairing precompile.
function g1Point(bytes: Uint8Array) {
  const [x, y] = words(bytes);
  if (x >= P || y >= P) throw new Error("Non-canonical G1 point");
  const point = bn254.G1.Point.fromAffine({ x, y });
  if (point.is0()) throw new Error("G1 point at infinity");
  point.assertValidity();
  return point;
}
function g2Point(bytes: Uint8Array) {
  const [xIm, xRe, yIm, yRe] = words(bytes); // drand's order: the imaginary word first
  if ([xIm, xRe, yIm, yRe].some(word => word >= P)) throw new Error("Non-canonical G2 point");
  const point = bn254.G2.Point.fromAffine({ x: { c0: xRe, c1: xIm }, y: { c0: yRe, c1: yIm } });
  if (point.is0()) throw new Error("G2 point at infinity");
  point.assertValidity();
  return point;
}
/// Whether signature (64 bytes, x ‖ y) is drand's signature of round under publicKey (128 bytes, x_im ‖ x_re ‖ y_im ‖ y_re):
/// e(signature, G2) == e(H(round), publicKey). Malformed input is false, never a throw.
export function verifyDrandRound(publicKey: BytesLike, round: bigint, signature: BytesLike): boolean {
  try {
    const key = getBytes(publicKey), sig = getBytes(signature);
    if (key.length !== 128 || sig.length !== 64 || !isRound(round)) return false;
    const pk = g2Point(key), sigma = g1Point(sig), { Fp12 } = bn254.fields;
    return Fp12.eql(bn254.pairing(sigma, bn254.G2.Point.BASE), bn254.pairing(bn254.G1.Point.fromAffine(drandRoundMessage(round)), pk));
  } catch {
    return false;
  }
}
/// A round's randomness as drand publishes it: sha256 of the round's signature bytes.
export function drandRandomness(signature: BytesLike): string {
  return sha256(signature);
}
