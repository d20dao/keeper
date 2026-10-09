const GWEI=10n**9n,MIN_TIP=GWEI,MAX_TIP=50n*GWEI;
export type TipBounds={minTip:bigint;maxTip:bigint};
/** A chain profile's bounds for the priority fee (gas.minTipWei, gas.maxTipWei): 1 and 50 gwei where it gives none. Arbitrum chains
 * ignore tips, so their profiles say 0 and 0. */
export function tipBounds(gas:{minTipWei?:string;maxTipWei?:string}):TipBounds{
  const minTip=gas.minTipWei===undefined?MIN_TIP:BigInt(gas.minTipWei),maxTip=gas.maxTipWei===undefined?MAX_TIP:BigInt(gas.maxTipWei);
  if(minTip>maxTip)throw new Error("The chain's minimum tip exceeds its maximum tip");
  return {minTip,maxTip};
}
export type GasSource={getBlock(tag:"latest"):Promise<{baseFeePerGas:bigint|null}|null>;send(method:string,params:unknown[]):Promise<unknown>};
export type OperatorFee={baseFee:bigint;tip:bigint;maxFee:bigint};

/** Fee for operator transactions priced from current gas, like the keeper: the median tip of the last
 * 20 blocks (the chain's tip bounds, 1-50 gwei by default) and a max fee of twice the latest base fee plus that tip. The chain cap
 * bounds the max fee; current gas above the cap refuses rather than underpricing. */
export async function currentFee(provider:GasSource,cap:bigint,{minTip,maxTip}:TipBounds={minTip:MIN_TIP,maxTip:MAX_TIP}):Promise<OperatorFee>{
  const [block,history]=await Promise.all([provider.getBlock("latest"),provider.send("eth_feeHistory",["0x14","latest",[50]]).catch(()=>undefined)]);
  const baseFee=block?.baseFeePerGas;
  if(!baseFee)throw new Error("Latest block has no base fee");
  const rewards=(history as {reward?:string[][]}|undefined)?.reward??[];
  const tips=rewards.map(row=>BigInt(row[0])).sort((a,b)=>a<b?-1:a>b?1:0);
  const median=tips.length?tips[Math.floor(tips.length/2)]:minTip;
  const tip=median<minTip?minTip:median>maxTip?maxTip:median;
  const wanted=2n*baseFee+tip,maxFee=wanted>cap?cap:wanted;
  if(baseFee+tip>maxFee)throw new Error("Current gas exceeds the chain fee cap");
  return {baseFee,tip,maxFee};
}
