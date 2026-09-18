import { AbiCoder, getBytes, keccak256 } from "ethers";
import type { VRFProof, XY } from "./verification.ts";
const abi=AbiCoder.defaultAbiCoder();
const types=["tuple(uint256[2] pk,uint256[2] gamma,uint256 c,uint256 s,uint256 seed,address uWitness,uint256[2] cGammaWitness,uint256[2] sHashWitness,uint256 zInv)"];
export const EVIDENCE_PACKET_BYTES=416;
export function encodeEvidencePacket(proof:VRFProof):string { return abi.encode(types,[proof]); }
/// Decoding is not verification. Replay with independently trusted chain context.
export function decodeEvidencePacket(packet:string):{proof:VRFProof} {
  if(getBytes(packet).length!==EVIDENCE_PACKET_BYTES) throw new Error("Evidence packet must be exactly 416 bytes");
  const [p]=abi.decode(types,packet);
  const pair=(v:readonly bigint[]):XY=>[v[0],v[1]];
  const proof:VRFProof={pk:pair(p.pk),gamma:pair(p.gamma),c:p.c,s:p.s,seed:p.seed,uWitness:p.uWitness,cGammaWitness:pair(p.cGammaWitness),sHashWitness:pair(p.sHashWitness),zInv:p.zInv};
  if(keccak256(encodeEvidencePacket(proof))!==keccak256(packet)) throw new Error("Noncanonical evidence packet");
  return {proof};
}
