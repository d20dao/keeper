// Public-data verification only. Contains no secret-key operations or production proof generation.
import { AbiCoder, id, keccak256 } from "ethers";
import { hashMapping, type MappingSpec } from "./mapping.ts";

// The VRF proof check and the proof's hashes live in vrf.ts, which knows no seed layout; this module adds the epoch design's seed.
export { hashPublicKey, hashProof, verifyVRFProof } from "./vrf.ts";
export type { XY, VRFProof } from "./vrf.ts";
export interface RequestContext {
  chainId: bigint; coordinator: string; keyHash: string; requestId: bigint;
  consumer: string; clientSeed: string; mapping: MappingSpec; requestBlock: bigint; targetBlock: bigint; blockHash: string;
  epochId: bigint; epochHash: string;
}
const abi = AbiCoder.defaultAbiCoder();

export function deriveRequestSeed(r: RequestContext): bigint {
  return BigInt(keccak256(abi.encode(
    ["bytes32", "uint256", "address", "bytes32", "uint256", "address", "bytes32", "bytes32", "uint64", "uint64", "bytes32", "uint64", "bytes32"],
    [id("D20_VRF_SEED"), r.chainId, r.coordinator, r.keyHash, r.requestId,
      r.consumer, r.clientSeed, hashMapping(r.mapping), r.requestBlock, r.targetBlock, r.blockHash, r.epochId, r.epochHash]
  )));
}
