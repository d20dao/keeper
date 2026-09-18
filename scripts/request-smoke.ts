// Paid end-to-end requests through a deployment's restricted cost client, priced from current gas.
// The running service keeper must fulfill them; nothing is sent without --apply, and mainnet needs --mainnet.
import {parseArgs} from "node:util";
import {readFile} from "node:fs/promises";
import {resolve} from "node:path";
import {Contract,JsonRpcProvider,formatUnits,hexlify,randomBytes} from "ethers";
import {loadDeployer} from "./lib/deployer-env.ts";
import {loadChain} from "./lib/chains.ts";
import {validateNetwork,Stop} from "./lib/deployment.ts";
import {currentFee} from "./lib/gas.ts";

const CALLBACK_GAS=100000,BUFFER_BPS=2000n,REQUEST_GAS_LIMIT=450000n; // A cost client request uses about 370k gas.
const sleep=(ms:number)=>new Promise(done=>setTimeout(done,ms));

async function main(){
  const {values}=parseArgs({options:{chain:{type:"string",default:"arc-testnet"},manifest:{type:"string"},env:{type:"string"},
    count:{type:"string",default:"1"},apply:{type:"boolean",default:false},mainnet:{type:"boolean",default:false}}});
  if(!values.env)throw new Stop("Provide --env with the deployer settings");
  const chain=await loadChain(values.chain);
  if(!chain.testnet&&!values.mainnet)throw new Stop("Mainnet requests require --mainnet");
  const count=Number(values.count);
  if(!Number.isInteger(count)||count<1||count>5)throw new Stop("--count must be between 1 and 5");
  const manifest=JSON.parse(await readFile(resolve(values.manifest??`deployments/private/${chain.key}/deployment.json`),"utf8"));
  if(manifest.chainId!==chain.chainId)throw new Stop("The manifest is for another chain");
  const provider=new JsonRpcProvider(chain.rpcUrls[0],undefined,{batchMaxCount:1});
  try {
    await validateNetwork(provider,chain);
    const wallet=await loadDeployer(resolve(values.env),provider);
    const abi=async(file:string,name:string)=>JSON.parse(await readFile(`artifacts/contracts/${file}.sol/${name}.json`,"utf8")).abi;
    const coordinator=new Contract(manifest.coordinator,await abi("D20VRFCoordinator","D20VRFCoordinator"),provider);
    const client=new Contract(manifest.client,await abi("examples/D20CostClient","D20CostClient"),wallet);
    if(await client.tester()!==wallet.address)throw new Stop("The deployer is not this cost client's tester");
    const {baseFee,tip,maxFee}=await currentFee(provider,BigInt(chain.gas.maxFeePerGasWei));
    const quote=await coordinator.quoteFeeAt(CALLBACK_GAS,baseFee) as bigint,value=quote+quote*BUFFER_BPS/10000n;
    const maxSpend=BigInt(count)*(value+REQUEST_GAS_LIMIT*maxFee),balance=await provider.getBalance(wallet.address);
    console.log(JSON.stringify({chain:chain.key,coordinator:manifest.coordinator,client:manifest.client,count,callbackGas:CALLBACK_GAS,
      baseFeeGwei:formatUnits(baseFee,"gwei"),feePerRequest:formatUnits(quote,18),valuePerRequest:formatUnits(value,18),
      maxSpend:formatUnits(maxSpend,18),balance:formatUnits(balance,18),currency:chain.nativeCurrency.symbol,apply:values.apply},null,2));
    if(!values.apply)return;
    if(balance<maxSpend)throw new Stop("Deployer balance is below the maximum spend");
    const sent:{id:bigint;hash:string;block:number;time:number}[]=[];
    for(let i=0;i<count;i++){
      const tx=await client.request(hexlify(randomBytes(32)),CALLBACK_GAS,false,{value,gasLimit:REQUEST_GAS_LIMIT,maxFeePerGas:maxFee,maxPriorityFeePerGas:tip});
      const receipt=await tx.wait(1,90000);
      if(receipt?.status!==1)throw new Stop("A request transaction failed");
      const event=receipt.logs.filter((log:{address:string})=>log.address.toLowerCase()===manifest.coordinator.toLowerCase())
        .map((log:{topics:string[];data:string})=>coordinator.interface.parseLog(log)).find((parsed:{name:string}|null)=>parsed?.name==="RandomnessRequested");
      if(!event)throw new Stop("RandomnessRequested event missing");
      sent.push({id:event.args.requestId,hash:receipt.hash,block:receipt.blockNumber,time:(await provider.getBlock(receipt.blockNumber))!.timestamp});
    }
    // Poll until each request settles or passes its deadline; the observed block bounds the latency.
    const settled=new Map<bigint,{state:string;block:number;time:number}>();
    for(const stopAt=Date.now()+180000;settled.size<sent.length&&Date.now()<stopAt;await sleep(1000)){
      const head=(await provider.getBlock("latest"))!;
      for(const request of sent){
        if(settled.has(request.id))continue;
        const r=await coordinator.getRequest(request.id);
        const state=r.delivered?"delivered":r.fulfilled?"fulfilled":r.refunded?"refunded":head.timestamp>Number(r.deadline)?"expired":undefined;
        if(state)settled.set(request.id,{state,block:head.number,time:head.timestamp});
      }
    }
    const results=sent.map(request=>{const s=settled.get(request.id);return {requestId:String(request.id),requestTransaction:`${chain.explorerUrl}/tx/${request.hash}`,
      state:s?.state??"pending",observedWithinBlocks:s?s.block-request.block:null,observedWithinSeconds:s?s.time-request.time:null};});
    console.log(JSON.stringify({results},null,2));
    if(results.some(result=>result.state!=="delivered"))process.exitCode=1;
  } finally {provider.destroy();}
}
main().catch(error=>{console.error(error instanceof Stop?`Request smoke stopped: ${error.message}`:"Request smoke stopped. Credentials were not logged; inspect the request transactions before retrying.");process.exitCode=1;});
