// Public-data verification only. Contains no secret-key operations or production proof generation.
import { secp256k1 } from "@noble/curves/secp256k1";
import { mod, pow } from "@noble/curves/abstract/modular";
import { AbiCoder, computeAddress, id, keccak256, solidityPacked, toBeHex } from "ethers";
import { hashMapping, type MappingSpec } from "./mapping.ts";

export type XY = readonly [bigint, bigint];
export interface VRFProof {
  pk: XY; gamma: XY; c: bigint; s: bigint; seed: bigint; uWitness: string;
  cGammaWitness: XY; sHashWitness: XY; zInv: bigint;
}
export interface RequestContext {
  chainId: bigint; coordinator: string; keyHash: string; requestId: bigint;
  consumer: string; clientSeed: string; mapping: MappingSpec; requestBlock: bigint; targetBlock: bigint; blockHash: string;
  epochId: bigint; epochHash: string;
}
const abi = AbiCoder.defaultAbiCoder();
const Point = secp256k1.ProjectivePoint;
const P = secp256k1.CURVE.Fp.ORDER;
const N = secp256k1.CURVE.n;
const MAX = (1n << 256n) - 1n;
type PointType = InstanceType<typeof Point>;

export function deriveRequestSeed(r: RequestContext): bigint {
  return BigInt(keccak256(abi.encode(
    ["bytes32", "uint256", "address", "bytes32", "uint256", "address", "bytes32", "bytes32", "uint64", "uint64", "bytes32", "uint64", "bytes32"],
    [id("D20_VRF_SEED"), r.chainId, r.coordinator, r.keyHash, r.requestId,
      r.consumer, r.clientSeed, hashMapping(r.mapping), r.requestBlock, r.targetBlock, r.blockHash, r.epochId, r.epochHash]
  )));
}

export function hashPublicKey(pk: XY): string { return keccak256(abi.encode(["uint256[2]"], [pk])); }
export function hashProof(p: VRFProof): string {
  return keccak256(abi.encode([
    "tuple(uint256[2] pk,uint256[2] gamma,uint256 c,uint256 s,uint256 seed,address uWitness,uint256[2] cGammaWitness,uint256[2] sHashWitness,uint256 zInv)",
  ], [p]));
}
function xy(point: PointType): XY { const a = point.toAffine(); return [a.x, a.y]; }
function point(p: XY): PointType {
  if (p[0] < 0n || p[0] >= P || p[1] < 0n || p[1] >= P) throw new Error("Non-canonical point");
  const q = Point.fromAffine({ x: p[0], y: p[1] });
  q.assertValidity();
  if (q.equals(Point.ZERO)) throw new Error("Point at infinity");
  return q;
}
function fieldHash(bytes: string): bigint {
  let x = BigInt(keccak256(bytes));
  while (x >= P) x = BigInt(keccak256(toBeHex(x, 32)));
  return x;
}
function hashToCurve(pk: XY, input: bigint): PointType {
  let x = fieldHash(solidityPacked(["uint256", "uint256[2]", "uint256"], [1n, pk, input]));
  for (;;) {
    const y2 = mod(x * x * x + 7n, P);
    let y = pow(y2, (P + 1n) / 4n, P);
    if (mod(y * y, P) === y2) {
      if (y % 2n) y = P - y;
      return point([x, y]);
    }
    x = fieldHash(toBeHex(x, 32));
  }
}

/// Verify with EXPECTED key and seed, not values trusted merely because they came in the proof.
export function verifyVRFProof(proof: VRFProof, expectedKey: XY, expectedSeed: bigint):
    { valid: true; randomness: string } | { valid: false; reason: string } {
  try {
    if (proof.pk[0] !== expectedKey[0] || proof.pk[1] !== expectedKey[1]) throw new Error("Wrong public key");
    if (proof.seed !== expectedSeed) throw new Error("Wrong request seed");
    for (const scalar of [proof.c, proof.s, proof.seed, proof.zInv])
      if (scalar < 0n || scalar > MAX) throw new Error("Scalar outside uint256");
    const pk = point(proof.pk), gamma = point(proof.gamma);
    const cg = point(proof.cGammaWitness), sh = point(proof.sHashWitness);
    const c = mod(proof.c, N), s = mod(proof.s, N);
    if (c === 0n || s === 0n) throw new Error("Zero scalar");
    const h = hashToCurve(proof.pk, expectedSeed);
    if (!gamma.multiply(c).equals(cg) || !h.multiply(s).equals(sh)) throw new Error("Invalid multiplication witness");
    if (proof.cGammaWitness[0] === proof.sHashWitness[0]) throw new Error("Witness points not distinct");
    const lz = mod(proof.sHashWitness[0] - proof.cGammaWitness[0], P);
    const dx = mod(lz * lz, P), dy = mod(lz * lz * lz, P);
    if (mod((dx === dy ? dx : mod(dx * dy, P)) * proof.zInv, P) !== 1n) throw new Error("Invalid inverse witness");
    const u = pk.multiply(c).add(Point.BASE.multiply(s));
    const uAddress = computeAddress(`0x${u.toHex(false)}`);
    if (uAddress.toLowerCase() !== proof.uWitness.toLowerCase()) throw new Error("Invalid U witness");
    const v = cg.add(sh);
    const derivedC = BigInt(keccak256(solidityPacked(
      ["uint256", "uint256[2]", "uint256[2]", "uint256[2]", "uint256[2]", "address"],
      [2n, xy(h), proof.pk, proof.gamma, xy(v), uAddress]
    )));
    if (derivedC !== proof.c) throw new Error("Invalid challenge");
    return { valid: true, randomness: keccak256(abi.encode(["uint256", "uint256[2]"], [3n, proof.gamma])) };
  } catch (error) {
    return { valid: false, reason: error instanceof Error ? error.message : "Invalid proof" };
  }
}
