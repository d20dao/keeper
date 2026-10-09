// Public computation only, safe to bundle for browsers.
import { AbiCoder, dataSlice, getAddress, getBytes, hexlify, id, keccak256, toUtf8Bytes, toUtf8String, type BytesLike } from "ethers";
import { encodeDataTemplate, matchesDataTemplate } from "./templates.ts";
import { DRAND_DST, drandRoundMessage, verifyDrandRound } from "./drand.ts";

/// A beacon recipe's registration as EpochEntropy.beaconOf returns it: round r is scheduled at genesis + (r - 1) × period,
/// and verifier checks the round's signature under publicKey. chainHash names the beacon network.
export interface BeaconRegistration { verifier: string; chainHash: string; publicKey: string; genesis: bigint; period: bigint; }

/// RFC 9380 domain separation tag of the bls-bn254-unchained-on-g1 hash-to-curve, as in D20BeaconVerifier.
export const BEACON_DST = DRAND_DST;
export const BEACON_DOMAIN = id("D20_EPOCH_BEACON");
/// drand's evmnet, the network D20's beacon recipe follows: its chain hash, group key in drand's word order, and schedule.
export { DRAND_EVMNET } from "./drand.ts";

const abi = AbiCoder.defaultAbiCoder();
const MAX_ROUND = (1n << 64n) - 1n, ROUND_DIGITS = 19;

/// A round's signed data is its number in decimal: 1 to 19 digits, no leading zero.
export const BEACON_TEMPLATE = encodeDataTemplate([{ integer: { minDigits: 1, maxDigits: ROUND_DIGITS } }]);
/// The canonical request, and body, of a beacon recipe: ["drand","<chainHash>"] with the hash in lowercase hex.
export function beaconCanonicalRequest(chainHash: string): string {
  if (getBytes(chainHash).length !== 32) throw new Error("A beacon chain hash is 32 bytes");
  return `["drand","${hexlify(chainHash)}"]`;
}
/// The signer a catalog lists for a beacon recipe: an identity derived from its registration, not a key.
export function beaconSlotSigner(b: BeaconRegistration): string {
  return getAddress(dataSlice(keccak256(abi.encode(["bytes32", "address", "bytes32", "bytes32", "uint64", "uint64"],
    [BEACON_DOMAIN, b.verifier, b.chainHash, keccak256(b.publicKey), b.genesis, b.period])), 12));
}

// Rounds are uint64 onchain. A round the registry commits is at least 1: its template refuses a leading zero.
const isRound = (round: bigint, min: bigint) => typeof round === "bigint" && round >= min && round <= MAX_ROUND;
/// When round is scheduled: genesis + (round - 1) × period, as the registry computes it.
export function beaconRoundTime(b: Pick<BeaconRegistration, "genesis" | "period">, round: bigint): bigint {
  if (!isRound(round, 1n)) throw new Error("Invalid beacon round");
  return b.genesis + (round - 1n) * b.period;
}
/// The latest round scheduled at or before timestamp; 0 before genesis.
export function beaconRoundAt(b: Pick<BeaconRegistration, "genesis" | "period">, timestamp: bigint): bigint {
  return timestamp < b.genesis ? 0n : (timestamp - b.genesis) / b.period + 1n;
}
/// The data a round is committed with: its number in decimal, as ASCII bytes.
export function encodeBeaconRound(round: bigint): string {
  if (round < 1n || round.toString().length > ROUND_DIGITS) throw new Error("Invalid beacon round");
  return hexlify(toUtf8Bytes(round.toString()));
}
/// Throws unless data is exactly a round number in canonical decimal, the only data a beacon recipe accepts.
export function decodeBeaconRound(data: BytesLike): bigint {
  if (!matchesDataTemplate(BEACON_TEMPLATE, data)) throw new Error("Invalid beacon round data");
  return BigInt(toUtf8String(data));
}

// The BN254 and drand math lives in drand.ts, which knows no contract; these are its names in the epoch design.
/// The G1 point that round's signature signs: hash-to-curve of keccak256(round as 8 big-endian bytes) under BEACON_DST.
export function beaconRoundMessage(round: bigint): { x: bigint; y: bigint } {
  return drandRoundMessage(round);
}
/// Whether signature (64 bytes, x ‖ y) is the beacon's signature of round under publicKey (128 bytes, x_im ‖ x_re ‖ y_im
/// ‖ y_re): e(signature, G2) == e(H(round), publicKey). Malformed input is false, never a throw.
export function verifyBeaconRound(publicKey: BytesLike, round: bigint, signature: BytesLike): boolean {
  return verifyDrandRound(publicKey, round, signature);
}
