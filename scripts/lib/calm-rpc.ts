// Reads from a rate-limited public node, for the scripts that fork a live network or scan its history. A node that answers "rate limit
// exceeded" is not broken, and the run that hit it should wait and go on, not stop after an hour of work. Only reads are repeated.
import type {JsonRpcProvider} from "ethers";

/// The requests that change nothing anywhere, so that asking again is always safe. A fork's own state changes (evm_mine, sends) never are.
const READS=new Set(["eth_call","eth_estimateGas","eth_getStorageAt","eth_getCode","eth_getBalance","eth_getTransactionCount","eth_getBlockByNumber","eth_getBlockByHash",
  "eth_getTransactionReceipt","eth_getLogs","eth_blockNumber","eth_chainId","eth_gasPrice","eth_feeHistory","net_version"]);
const RATE_LIMITED=/rate limit|too many requests|max retries exceeded|-32005|(^|[^0-9])429([^0-9]|$)/i;

/// Whether a failure says that the node refused for its rate limit: an HTTP 429, JSON-RPC error -32005, or a fork's own report that the
/// node it forks kept saying so. A revert, a missing method or a dropped connection is not one.
export function isRateLimited(error:unknown):boolean{
  const failure=error as {code?:unknown;message?:unknown;shortMessage?:unknown;info?:unknown;error?:unknown}|null;
  const parts:string[]=[String(failure?.message??""),String(failure?.shortMessage??"")];
  for(const detail of [failure?.info,failure?.error]){try{parts.push(JSON.stringify(detail)??"");}catch{/* not printable, and so no evidence */}}
  return failure?.code===-32005||RATE_LIMITED.test(parts.join(" "));
}

/// Make `provider` repeat a read that the node refused for its rate limit, after `pauseMs`, up to `attempts` times in all. A fork retries what
/// it reads from the network before it answers, so what a failed attempt fetched is kept, and every attempt only asks for what is still missing.
export function calmReads<T extends JsonRpcProvider>(provider:T,options:{pauseMs?:number;attempts?:number;onWait?:(method:string,attempt:number,pauseMs:number)=>void}={}):T{
  const {pauseMs=20_000,attempts=12,onWait}=options,send=provider.send.bind(provider);
  provider.send=async(method:string,params:unknown[]|Record<string,unknown>)=>{
    for(let attempt=1;;attempt++){
      try{return await send(method,params);}
      catch(error){
        if(!READS.has(method)||!isRateLimited(error)||attempt>=attempts)throw error;
        onWait?.(method,attempt,pauseMs);
        await new Promise(resolve=>setTimeout(resolve,pauseMs));
      }
    }
  };
  return provider;
}
