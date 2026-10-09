// The gas of a round coordinator fulfilment, modelled from test/robinhood/Gas.test.ts, which measures every shape on a local chain (EDR's
// Osaka schedule, MockArbSys) and fails when a measurement leaves its bound. For the keeper's fee gate (review H2 and M1): price a
// fulfilment on the gas it can use, not on eth_estimateGas, whose callback reserves are never spent; keep eth_estimateGas for the gas
// limit. These are L2 gas; Robinhood's L1 component comes on top and is read per transaction.
import {secp256k1} from "@noble/curves/secp256k1";
import {mod,pow} from "@noble/curves/abstract/modular";
import {keccak256,solidityPacked,toBeHex} from "ethers";

/// Upper bounds of L2 gas used, each a little above the largest measurement, with one VRF hash-to-curve candidate per proof and the
/// costliest drand hash-to-curve for every round verified. A member's own callback gas limit comes on top, in full.
export const ROUND_GAS=Object.freeze({
  /// fulfillRandomness of a request whose round is cached, the round's signature sent anyway.
  single:223_400n,
  /// fulfillRandomness verifying the request's round: the BLS check, the cache write and RoundVerified.
  singleRound:217_300n,
  /// fulfillRandomnessBatch's fixed part.
  batch:79_500n,
  /// Each member a batch serves.
  batchMember:161_400n,
  /// Each listed round a batch verifies.
  batchRound:205_600n,
  /// Each served member for each listed round: finding a member's round in the list.
  batchMemberRound:250n,
  /// Each VRF hash-to-curve candidate after a proof's first: one more square root through the modexp precompile.
  vrfCandidate:5_500n,
  /// Once per transaction whose first served member finds earnedFees at zero, as after withdrawFees.
  feesWithdrawn:17_200n,
});
/// Robinhood's extra cost per verified round over the local measurement: the review's testnet hash-to-curve figure, +14,200 to +17,000
/// gas over a local node, taken whole until the testnet drills measure it on the chain itself.
export const ROUND_ROBINHOOD_ROUND_EXCESS=17_000n;
/// The L1 component a fulfilment is priced with, in L2 gas at the floor base fee, where it is largest: 25,000 for one member (Robinhood
/// testnet measured about 20,000 to 24,000; mainnet is priced as testnet), and for a batch 12,000 a further member and 3,000 a further
/// listed round (a testnet batch of 16 measured 162,295, scaled like the single).
export const ROUND_L1_GAS=Object.freeze({single:25_000n,member:12_000n,round:3_000n});
/// VRF hash-to-curve candidates a proof is priced with when its own count is not known: a proof needs more with probability 2^-10.
export const ROUND_PRICED_VRF_CANDIDATES=10;
/// The coordinator's MAX_FULFILL_BATCH.
export const ROUND_MAX_FULFILL_BATCH=16;

/// A fulfilment as the keeper sends it.
export interface RoundFulfilmentShape {
  /// fulfillRandomnessBatch, or fulfillRandomness.
  batch:boolean;
  /// Each served member's callback gas limit.
  callbackGasLimits:readonly (number|bigint)[];
  /// Rounds verified in it: the request's round when not cached (single), or every listed round (batch). Count a listed round even when it
  /// is cached at the decision head: a reorganisation can uncache it before the batch runs.
  roundsToVerify:number;
  /// Each member's VRF hash-to-curve candidates (vrfHashToCurveCandidates); ROUND_PRICED_VRF_CANDIDATES each when absent.
  vrfCandidates?:readonly number[];
  /// Whether earnedFees may be zero when it runs (after withdrawFees). Absent is the worst case, true.
  feesWithdrawn?:boolean;
}
function members(shape:RoundFulfilmentShape){
  const n=shape.callbackGasLimits.length,r=shape.roundsToVerify;
  if(n<1||n>ROUND_MAX_FULFILL_BATCH||!Number.isInteger(r)||r<0||r>n||(!shape.batch&&(n!==1||r>1))||
    (shape.vrfCandidates!==undefined&&(shape.vrfCandidates.length!==n||shape.vrfCandidates.some(k=>!Number.isInteger(k)||k<1))))
    throw new Error("Invalid fulfilment shape");
  const limits=shape.callbackGasLimits.map(BigInt);
  const candidates=(shape.vrfCandidates??limits.map(()=>ROUND_PRICED_VRF_CANDIDATES)).reduce((sum,k)=>sum+BigInt(k-1),0n);
  return {n:BigInt(n),r:BigInt(r),limits,callbacks:limits.reduce((sum,limit)=>sum+limit,0n),candidates,withdrawn:shape.feesWithdrawn!==false};
}
/// The L2 gas a fulfilment can use on Robinhood: the measured bound, its callbacks' limits in full, and ROUND_ROBINHOOD_ROUND_EXCESS for
/// each round it verifies. Add the L1 component (live, with the gate's margin) and multiply by the base fee for its cost.
export function roundFulfilmentGasBound(shape:RoundFulfilmentShape):bigint{
  const {n,r,callbacks,candidates,withdrawn}=members(shape),g=ROUND_GAS;
  const fixed=shape.batch?g.batch+n*g.batchMember+r*g.batchRound+n*r*g.batchMemberRound:g.single+r*g.singleRound;
  return fixed+callbacks+candidates*g.vrfCandidate+(withdrawn?g.feesWithdrawn:0n)+r*ROUND_ROBINHOOD_ROUND_EXCESS;
}
/// The L1 component a fulfilment is priced with (ROUND_L1_GAS), in L2 gas.
export function roundFulfilmentL1Gas(shape:RoundFulfilmentShape):bigint{
  const {n,r}=members(shape);
  return ROUND_L1_GAS.single+(n-1n)*ROUND_L1_GAS.member+(r>1n?r-1n:0n)*ROUND_L1_GAS.round;
}

// The coordinator's guards, which a gas limit must satisfy inside the implementation, behind the proxy's DELEGATECALL: EIP-150 forwards at
// most 63/64 of the gas left, so every unit a guard asks for costs 64/63 of a unit of the transaction's gas limit.
const CALLBACK_RESERVE=140_000n,MEMBER_OVERHEAD=400_000n,ROUND_VERIFICATION=400_000n+400_000n/63n+5_000n;
const memberGas=(limit:bigint)=>limit+limit/63n+MEMBER_OVERHEAD;
const viaProxy=(gas:bigint)=>(gas*64n+62n)/63n;
/// A gas limit at which a fulfilment passes its guards, a little above the smallest one measured: 64/63 of what the guard budgets, plus the
/// intrinsic gas, the calldata and the work before the guard. A single of a cached round is guarded only before its callback, after its
/// proof's check. No L1 component: on Robinhood eth_estimateGas adds it, and the chain's per-transaction cap applies to L2 gas.
export function roundFulfilmentGasLimit(shape:RoundFulfilmentShape):bigint{
  const {n,r,limits,candidates,withdrawn}=members(shape);
  if(shape.batch)return viaProxy(CALLBACK_RESERVE+limits.reduce((sum,limit)=>sum+memberGas(limit),0n)+r*ROUND_VERIFICATION)+37_500n+6_800n*n+1_800n*r;
  if(r===1n)return viaProxy(CALLBACK_RESERVE+memberGas(limits[0])+ROUND_VERIFICATION)+48_000n;
  return viaProxy(CALLBACK_RESERVE+limits[0]+limits[0]/63n)+218_000n+candidates*5_600n+(withdrawn?17_200n:0n);
}
/// The largest batch of members with one callback gas limit, over `rounds` rounds to verify, whose gas limit fits a per-transaction cap;
/// never above ROUND_MAX_FULFILL_BATCH. 0 when not even the smallest fits.
export function roundMaxBatch(callbackGasLimit:number|bigint,rounds:number,cap:bigint):number{
  let largest=0;
  for(let n=Math.max(rounds,1);n<=ROUND_MAX_FULFILL_BATCH;n++)
    if(roundFulfilmentGasLimit({batch:true,callbackGasLimits:Array(n).fill(callbackGasLimit),roundsToVerify:rounds})<=cap)largest=n;
  return largest;
}

/// How many candidates the VRF hash-to-curve of a proof tries before one lies on secp256k1, as VRF.sol's _hashToCurve does: each further one
/// costs ROUND_GAS.vrfCandidate. Half the seeds need one, a quarter two; the keeper's prover meets the same count while it builds the proof.
export function vrfHashToCurveCandidates(publicKey:readonly [bigint,bigint],seed:bigint):number{
  const P=secp256k1.CURVE.Fp.ORDER;
  const fieldHash=(bytes:string)=>{let x=BigInt(keccak256(bytes));while(x>=P)x=BigInt(keccak256(toBeHex(x,32)));return x;};
  let x=fieldHash(solidityPacked(["uint256","uint256[2]","uint256"],[1n,publicKey,seed]));
  for(let k=1;;k++){
    const y2=mod(x*x*x+7n,P),y=pow(y2,(P+1n)/4n,P);
    if(mod(y*y,P)===y2)return k;
    x=fieldHash(toBeHex(x,32));
  }
}
