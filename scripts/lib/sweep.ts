import type {Chain} from "./chains.ts";
import {Stop} from "./deployment.ts";

/** Gas of a plain transfer on an EVM chain: the limit a sweep has always been signed with there. */
export const TRANSFER_GAS=21000n;
/** What the limit adds to the node's estimate on an Arbitrum chain, in basis points: 25%. */
export const ARBITRUM_ESTIMATE_MARGIN_BPS=2500n;
export type SweepTransfer={from:string;to:string;value:bigint};
/** What a sweep reads from the node; a JsonRpcProvider answers both. */
export interface SweepNode {
  getCode(address:string):Promise<string>;
  estimateGas(transfer:SweepTransfer):Promise<bigint>;
}

/** A sweep on a production chain goes to a contract there, the treasury Safe. A destination without code on that chain is the address of a
 * Safe that was never created there, or a wrong one, so the sweep stops before anything is read from the keeper's key. A testnet's owner may be an account. */
export async function requireSweepDestination(node:Pick<SweepNode,"getCode">,chain:Pick<Chain,"name"|"testnet">,destination:string):Promise<void>{
  if(chain.testnet)return;
  if(await node.getCode(destination)==="0x")throw new Stop(`The owner ${destination} has no code on ${chain.name}; a sweep on a production chain goes only to the treasury Safe, which must exist there`);
}

/** The gas limit a sweep transaction is signed with. On an EVM chain it is a transfer's 21,000, as it has always been. On an Arbitrum chain the
 * limit also pays for posting the transaction to the parent chain, so 21,000 is rejected: it is the node's estimate for this transfer plus 25%,
 * rounded up, and never below 21,000. */
export async function sweepGasLimit(node:Pick<SweepNode,"estimateGas">,chain:Pick<Chain,"blockSource">,transfer:SweepTransfer):Promise<bigint>{
  if(chain.blockSource!=="arbitrum-l2")return TRANSFER_GAS;
  const estimate=await node.estimateGas(transfer).catch(()=>{throw new Stop("The node could not estimate the gas of the sweep transaction; nothing was sent");});
  const limit=(estimate*(10000n+ARBITRUM_ESTIMATE_MARGIN_BPS)+9999n)/10000n;
  return limit<TRANSFER_GAS?TRANSFER_GAS:limit;
}

/** What the keeper wallet sends: its balance less what stays behind and less the gas the transfer can cost at the fee cap, `gasLimit × maxFee`,
 * which the node requires the wallet to hold on top of the value. The estimate is taken for the largest amount the sweep could send (the balance
 * above the buffer), so the final transfer is never larger than what was estimated. Nothing above the buffer needs no estimate. */
export async function planSweep(node:Pick<SweepNode,"estimateGas">,chain:Pick<Chain,"blockSource">,
  {keeper,destination,balance,keep,maxFee}:{keeper:string;destination:string;balance:bigint;keep:bigint;maxFee:bigint}):Promise<{gasLimit:bigint;amount:bigint}>{
  const available=balance-keep;
  const gasLimit=available>0n?await sweepGasLimit(node,chain,{from:keeper,to:destination,value:available}):TRANSFER_GAS;
  return {gasLimit,amount:available-gasLimit*maxFee};
}
