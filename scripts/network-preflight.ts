// Bounded, read-only network check. Never accesses signer keys or submits a transaction.
import {readFile,mkdir,mkdtemp,writeFile} from "node:fs/promises";
import {resolve,join} from "node:path";
import {loadChain} from "./lib/chains.ts";
const profile=await loadChain(process.argv[2]??"arc-testnet");
const urls=process.env.PREFLIGHT_RPC_URLS?.split(",").map(s=>s.trim()).filter(Boolean) ??
  profile.rpcUrls;
if(urls.length>3||urls.some((url:string)=>!url.startsWith("https://")))throw new Error("Use at most three HTTPS RPCs");
async function rpc(url:string,method:string,params:unknown[]){
  const r=await fetch(url,{method:"POST",headers:{"Content-Type":"application/json","Cache-Control":"no-cache"},
    body:JSON.stringify({jsonrpc:"2.0",id:Date.now(),method,params}),redirect:"error",signal:AbortSignal.timeout(10000)});
  if(!r.ok)throw new Error(`HTTP ${r.status}`);
  const text=await r.text();if(text.length>262144)throw new Error("RPC response exceeds bound");
  const body=JSON.parse(text);if(body.error)throw new Error(`RPC error ${body.error.code}`);return body.result;
}
const results=await Promise.all(urls.map(async(url:string)=>{
  const origin=new URL(url).origin;
  try{
    const chain=BigInt(await rpc(url,"eth_chainId",[]));
    if(chain!==BigInt(profile.chainId))throw new Error("Wrong chain ID");
    const first=await rpc(url,"eth_getBlockByNumber",["latest",false]);
    const gasPrice=await rpc(url,"eth_gasPrice",[]);
    await new Promise(r=>setTimeout(r,5000));
    const last=await rpc(url,"eth_getBlockByNumber",["latest",false]);
    const now=Math.floor(Date.now()/1000),timestamp=Number(BigInt(last.timestamp));
    const advancing=BigInt(last.number)>BigInt(first.number),ageSeconds=now-timestamp;
    return {origin,chainId:Number(chain),firstBlock:Number(BigInt(first.number)),lastBlock:Number(BigInt(last.number)),
      firstTimestamp:Number(BigInt(first.timestamp)),lastTimestamp:timestamp,lastTimestampIso:new Date(timestamp*1000).toISOString(),
      observedAt:new Date().toISOString(),ageSeconds,advancing,fresh:ageSeconds>=-10&&ageSeconds<=120,
      usable:advancing&&ageSeconds>=-10&&ageSeconds<=120,gasLimit:BigInt(last.gasLimit).toString(),
      baseFeeWei:BigInt(last.baseFeePerGas).toString(),gasPriceWei:BigInt(gasPrice).toString()};
  }catch(e){return {origin,usable:false,error:e instanceof Error?e.message:"RPC probe failed"};}
}));
await mkdir(".research",{recursive:true});const dir=await mkdtemp(resolve(".research/arc-preflight-"));
await writeFile(join(dir,"report.json"),JSON.stringify({profile:profile.key,results},null,2)+"\n");
console.log(JSON.stringify({results,report:join(dir,"report.json")},null,2));
if(!results.some(r=>r.usable))process.exitCode=2;
