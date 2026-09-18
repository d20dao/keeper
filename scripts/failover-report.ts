// Read-only failover drill report for a block range: which wallet published each epoch and fulfilled each request,
// refunds, skipped duplicates, and keeper transactions that reverted or only cancelled a nonce. Sends nothing.
// Usage: node scripts/failover-report.ts --manifest deployments/arc-testnet.json --from-block <n> [--to-block <m>] [--wallets 0x..,0x..]
import {parseArgs} from "node:util";
import {readFile} from "node:fs/promises";
import {resolve} from "node:path";
import {JsonRpcProvider,getAddress} from "ethers";
import {loadChain} from "./lib/chains.ts";
import {failoverReport} from "./lib/failover-report.ts";

async function main(){
  const {values}=parseArgs({options:{manifest:{type:"string"},"from-block":{type:"string"},"to-block":{type:"string"},wallets:{type:"string"}}});
  if(!values.manifest||!/^\d+$/.test(values["from-block"]??""))throw new Error("Usage: node scripts/failover-report.ts --manifest <deployments/network.json> --from-block <n> [--to-block <m>] [--wallets 0x..,0x..]");
  if(values["to-block"]!==undefined&&!/^\d+$/.test(values["to-block"]))throw new Error("--to-block must be a block number");
  const manifest=JSON.parse(await readFile(resolve(values.manifest),"utf8")),chain=await loadChain(manifest.network);
  if(manifest.chainId!==chain.chainId)throw new Error("Manifest and chain profile differ");
  const provider=new JsonRpcProvider(chain.rpcUrls[0],chain.chainId,{staticNetwork:true,batchMaxCount:1});
  try {
    const wallets=values.wallets?.split(",").map(address=>getAddress(address.trim().toLowerCase()));
    const toBlock=values["to-block"]===undefined?await provider.getBlockNumber():Number(values["to-block"]);
    const report=await failoverReport(provider,{registry:manifest.registry,coordinator:manifest.coordinator,fromBlock:Number(values["from-block"]),toBlock,wallets});
    console.log(JSON.stringify({network:chain.key,chainId:chain.chainId,...report},null,2));
  } finally {provider.destroy();}
}
main().catch(error=>{console.error(`Failover report stopped: ${error instanceof Error?error.message:String(error)}`);process.exitCode=1;});
