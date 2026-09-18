import {deployProxy,implementationCodeHash} from "../test/helpers/proxy.ts";
// Heavy traffic stays on an isolated local EVM. NEVER routes fixtures to Arc/public RPC.
import assert from "node:assert/strict";
import {spawn} from "node:child_process";
import {mkdir, mkdtemp, readFile, writeFile, access} from "node:fs/promises";
import {createServer} from "node:http";
import {once} from "node:events";
import {join,resolve} from "node:path";
import {performance} from "node:perf_hooks";
import {network} from "hardhat";
import {TEST_SECRET,publicKey} from "../test/helpers/proof.ts";
import {canonicalApiRequest,attestationDigest} from "../src/index.ts";
import {EPOCH_TEST_SIGNERS,EPOCH_TEST_WALLETS,epochFixtureData} from "../test/helpers/epoch.ts";
import type {TransactionReceipt,Contract,Log,EventLog} from "ethers";

const binary=resolve("keeper/target/release/d20dao-keeper"+(process.platform==="win32"?".exe":""));
await access(binary);
const sleep=(ms:number)=>new Promise(r=>setTimeout(r,ms));
const quantile=(values:number[],p:number)=>values.length?[...values].sort((a,b)=>a-b)[Math.max(0,Math.ceil(values.length*p)-1)]:null;
await mkdir(".research",{recursive:true});
const output=await mkdtemp(resolve(".research/arc-load-"));
type Scenario={name:string;count:number;spacingMs:number;rpcDelayMs:number;initialBurst?:number;pollMs?:number;epochApiDelayMs?:number};
const suite:Scenario[]=process.env.LOAD_PLAN ? JSON.parse(await readFile(process.env.LOAD_PLAN,"utf8")) : [{name:"steady-1rps",count:20,spacingMs:1000,rpcDelayMs:0},
  {name:"burst-32",count:32,spacingMs:0,rpcDelayMs:0},
  {name:"burst-128-rpc25ms",count:128,spacingMs:0,rpcDelayMs:25}];
assert.ok(Array.isArray(suite)&&suite.length>0&&suite.length<=12,"Bounded local scenario list required");
assert.equal(new Set(suite.map(s=>s.name)).size,suite.length,"Scenario names must be unique");
for(const s of suite){
  assert.match(s.name,/^[a-z0-9-]{1,60}$/);
  for(const n of [s.count,s.spacingMs,s.rpcDelayMs,s.initialBurst??1,s.pollMs??250])assert.ok(Number.isInteger(n));
  assert.ok(s.count>=1&&s.count<=256&&s.spacingMs>=0&&s.spacingMs<=5000&&s.count*s.spacingMs<=180000);
  assert.ok(s.rpcDelayMs>=0&&s.rpcDelayMs<=250&&(s.initialBurst??1)>=1&&(s.initialBurst??1)<=s.count);
  assert.ok((s.pollMs??250)>=100&&(s.pollMs??250)<=1000);
}
const reports=[];
for(const scenario of suite){
  const connection=await network.create("loadSim");
  const {ethers,provider,networkHelpers}=connection;
  assert.equal((await ethers.provider.getNetwork()).chainId,31337n);
  const dir=join(output,scenario.name);await mkdir(dir);
  const vrfPath=join(dir,"public-fixture-vrf.key"),txPath=join(dir,"public-fixture-tx.key");
  const wallet=new ethers.Wallet(ethers.toBeHex(987654321n,32));
  await writeFile(vrfPath,ethers.toBeHex(TEST_SECRET,32),{flag:"wx",mode:0o600});
  await writeFile(txPath,wallet.privateKey,{flag:"wx",mode:0o600});
  await networkHelpers.setBalance(wallet.address,ethers.parseEther("1000"));
  const [owner,player]=await ethers.getSigners();
  const registry=await deployProxy(ethers,"EpochEntropy",[EPOCH_TEST_SIGNERS,owner.address,wallet.address]);
  const rng:Contract=await deployProxy(ethers,"D20VRFCoordinator",[publicKey(),owner.address,owner.address,ethers.parseEther("0.001"),1,await registry.getAddress(),0]);
  await rng.setPricing(ethers.parseEther("0.001"),0,300000); // Flat fee: load requests send exact values.
  const game:Contract=await ethers.deployContract("TestConsumer",[await rng.getAddress()]);
  const counters:Record<string,number>={};let rpcInFlight=0,maxRpcInFlight=0,apiCalls=0;
  const server=createServer(async(req,res)=>{
    rpcInFlight++;maxRpcInFlight=Math.max(maxRpcInFlight,rpcInFlight);
    try{
      const chunks:Buffer[]=[];let size=0;for await(const chunk of req){size+=chunk.length;if(size>131072)throw new Error("RPC request bound");chunks.push(Buffer.from(chunk));}
      const call=JSON.parse(Buffer.concat(chunks).toString());
      if(req.url?.startsWith("/api/")){
        // The keeper posts to /api/<recipe>; this registry keeps its initial catalog, whose slot n is recipe n.
        const source=Number(req.url.slice(5));assert.ok(source>=0&&source<=3);apiCalls++;
        if(scenario.epochApiDelayMs)await sleep(scenario.epochApiDelayMs);
        const text=JSON.stringify(epochFixtureData(source));
        const head=await ethers.provider.getBlock("latest"),queryHash=ethers.id(canonicalApiRequest(call));
        const proof={timestamp:BigInt(head!.timestamp),data:ethers.hexlify(ethers.toUtf8Bytes(text)),signature:"0x"};
        const signature=await EPOCH_TEST_WALLETS[source].signMessage(ethers.getBytes(attestationDigest(queryHash,proof)));
        res.writeHead(200,{"Content-Type":"application/json"});res.end(JSON.stringify({airnode:EPOCH_TEST_SIGNERS[source],requestHash:queryHash,timestamp:proof.timestamp.toString(),data:JSON.parse(text),signature}));return;
      }
      counters[call.method]=(counters[call.method]??0)+1;
      if(scenario.rpcDelayMs)await sleep(scenario.rpcDelayMs);
      let reply;try{reply={jsonrpc:"2.0",id:call.id,result:await provider.request({method:call.method,params:call.params})};}
      catch(e){reply={jsonrpc:"2.0",id:call.id,error:{code:-32000,message:e instanceof Error?e.message:String(e)}};}
      res.writeHead(200,{"Content-Type":"application/json"});res.end(JSON.stringify(reply));
    }catch{res.writeHead(500);res.end();}finally{rpcInFlight--;}
  });
  server.listen(0,"127.0.0.1");await once(server,"listening");const address=server.address();assert.ok(address&&typeof address!=="string");
  const initial=await ethers.provider.getBlock("latest");const miningStarted=performance.now();
  let mining=false,miningPaused=false,miningError:unknown;const blockTimes:number[]=[];
  await provider.request({method:"evm_setAutomine",params:[false]});
  const miner=setInterval(()=>{if(mining||miningPaused)return;mining=true;(async()=>{
    await provider.request({method:"hardhat_setNextBlockBaseFeePerGas",params:[ethers.toBeHex(20000000000n)]});
    await provider.request({method:"evm_mine",params:[initial!.timestamp+Math.floor((performance.now()-miningStarted)/1000)]});
    blockTimes.push(performance.now());
  })().catch(e=>{miningError=e;}).finally(()=>{mining=false;});},480);
  const env={...process.env,NEON_DB:undefined,TELEGRAM_BOT_TOKEN:undefined,TELEGRAM_CHAT_ID:undefined,TELEGRAM_LOW_BALANCE_WEI:undefined,CHAIN_ID:"31337",RPC_URLS:`http://127.0.0.1:${address.port}`,
    COORDINATOR_ADDRESS:await rng.getAddress(),
    EXPECTED_CODE_HASH:ethers.keccak256(await ethers.provider.getCode(await rng.getAddress())),
    EXPECTED_PROTOCOL_HASH:await rng.protocolConfigurationHash(),
    EXPECTED_IMPLEMENTATION_CODE_HASH:await implementationCodeHash(ethers,rng),
    EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH:await implementationCodeHash(ethers,registry),
    KEEPER_DB:join(dir,"keeper.sqlite"),TEST_LOCK_DIR:join(dir,"locks"),TX_KEY_FILE:txPath,VRF_KEY_FILE:vrfPath,
    SEND_TRANSACTIONS:"true",POLL_MS:String(scenario.pollMs??250),TICK_TIMEOUT_SECONDS:"20",MAX_TICK_FAILURES:"5",
    SEND_MARGIN_SECONDS:"5",MAX_GAS:process.env.MAX_GAS??"2000000",FULFILL_BATCH_MAX:process.env.FULFILL_BATCH_MAX??"8",MAX_FEE_PER_GAS_WEI:"100000000000",
    CANCEL_MAX_FEE_PER_GAS_WEI:"150000000000",FEE_COVERAGE_BPS:process.env.FEE_COVERAGE_BPS??"0",MAX_TX_COST_WEI:"200000000000000000",
    NONCE_STUCK_SECONDS:"120",PROGRESS_STUCK_SECONDS:"20",TEST_API_BASE:`http://127.0.0.1:${address.port}/api`,
    HEALTH_API_URL:undefined,HEALTH_API_KEY:undefined,HEALTH_INTERVAL_SECONDS:undefined,RUST_LOG:"warn"};
  const child=spawn(binary,["run"],{env,windowsHide:true,stdio:["ignore","pipe","pipe"]});
  let logs="",exitCode:number|null|undefined;child.stderr.on("data",b=>{logs=(logs+b.toString()).slice(-1000000);});
  child.stdout.on("data",()=>{});child.on("exit",code=>{exitCode=code;});
  const exited=once(child,"exit");
  const submitted:Array<{hash:string;sentAt:number}>=[];
  const receipts:TransactionReceipt[]=[];
  const states:Array<{requestId:string;requestBlock:number;requestTimestamp:number;fulfilled:boolean;delivered:boolean;refunded:boolean;deadlineExpired:boolean;deadline:string;fulfillmentBlock:number|null;blocks:number|null;chainSeconds:number|null;randomness:string}>=[];
  const started=performance.now();
  console.log(`SCENARIO ${scenario.name}: ${scenario.count} requests; RPC +${scenario.rpcDelayMs}ms`);
  try{
    // Activation is outside offered traffic. Idle preparation must not publish onchain.
    miningPaused=true;while(mining)await sleep(10);
    const stamp=(await ethers.provider.getBlock("latest"))!.timestamp;
    let bootstrapBlock=BigInt(await provider.request({method:"eth_blockNumber",params:[]}));
    const firstStart=await registry.firstEpochStart();
    while(bootstrapBlock<firstStart){await provider.request({method:"evm_mine",params:[stamp]});bootstrapBlock++;}
    miningPaused=false;
    const bootstrapEnd=performance.now()+45000;
    while(apiCalls===0){
      if(miningError)throw miningError;if(exitCode!==undefined)throw new Error(`Keeper exited ${exitCode}`);
      if(performance.now()>bootstrapEnd)throw new Error("Local epoch preparation did not start");await sleep(100);
    }
    assert.equal((await registry.getEpoch(1)).epochHash,ethers.ZeroHash,"Idle epoch published before demand");
    assert.equal(await ethers.provider.getTransactionCount(wallet.address,"pending"),0,"Idle keeper consumed a transaction nonce");
    let nonce=await player.getNonce();
    for(let i=0;i<scenario.count;i++){
      if(miningError)throw miningError;if(exitCode!==undefined)throw new Error(`Keeper exited ${exitCode}`);
      const sentAt=performance.now();
      const tx=await game.connect(player).getFunction("request")(ethers.id(`load-${scenario.name}-${i}`),200000,player.address,
        {value:ethers.parseEther("0.001"),gasLimit:350000,maxFeePerGas:100000000000n,maxPriorityFeePerGas:1000000000n,nonce:nonce++});
      submitted.push({hash:tx.hash,sentAt});if(scenario.spacingMs&&i>=(scenario.initialBurst??1)-1)await sleep(scenario.spacingMs);
    }
    const receiptDeadline=performance.now()+15000;
    for(const tx of submitted){let receipt;while(!(receipt=await ethers.provider.getTransactionReceipt(tx.hash))){
      if(performance.now()>receiptDeadline)throw new Error("Request inclusion exceeded bounded local wait");await sleep(100);}
      assert.equal(receipt.status,1);receipts.push(receipt);
    }
    const observationEnd=performance.now()+75000;
    while(performance.now()<observationEnd){
      if(miningError)throw miningError;if(exitCode!==undefined)throw new Error(`Keeper exited ${exitCode}`);
      const head=await ethers.provider.getBlock("latest"),served=Number(await rng.lastServedIndex());
      const last=await rng.getRequest(scenario.count);
      if(served===scenario.count||BigInt(head!.timestamp)>last.deadline+2n)break;
      await sleep(400);
    }
    const fulfilledLogs=await rng.queryFilter(rng.filters.RandomnessFulfilled(),initial!.number);
    const observedHead=await ethers.provider.getBlock("latest");
    const eventById=new Map<string,Log|EventLog>(fulfilledLogs.map(e=>[rng.interface.parseLog(e)!.args.requestId.toString(),e]));
    for(let i=0;i<receipts.length;i++){
      const receipt=receipts[i],event=receipt.logs.filter(l=>l.address.toLowerCase()===(rng.target as string).toLowerCase()).map(l=>rng.interface.parseLog(l)).find(l=>l?.name==="RandomnessRequested")!;
      const id=event.args.requestId,state=await rng.getRequest(id),fulfilled=eventById.get(id.toString());
      const startBlock=await ethers.provider.getBlock(receipt.blockNumber);
      const endBlock=fulfilled?await ethers.provider.getBlock(fulfilled.blockNumber):null;
      if(state.fulfilled){assert.equal(state.delivered,true);assert.equal(await game.results(id),state.randomness);}
      states.push({requestId:id.toString(),requestBlock:receipt.blockNumber,requestTimestamp:startBlock!.timestamp,
        fulfilled:state.fulfilled,delivered:state.delivered,refunded:state.refunded,deadlineExpired:BigInt(observedHead!.timestamp)>state.deadline,deadline:state.deadline.toString(),
        fulfillmentBlock:endBlock?.number??null,blocks:endBlock?endBlock.number-receipt.blockNumber:null,
        chainSeconds:endBlock?endBlock.timestamp-startBlock!.timestamp:null,randomness:state.randomness});
    }
    const successful=states.filter(s=>s.fulfilled),blocks=successful.map(s=>s.blocks!),seconds=successful.map(s=>s.chainSeconds!);
    const claimable=states.filter(s=>!s.fulfilled&&!s.refunded&&s.deadlineExpired);
    const refundTxs=[];
    for(const s of claimable)refundTxs.push(await rng.connect(player).getFunction("refundRequest")(s.requestId,{gasLimit:500000,maxFeePerGas:100000000000n,maxPriorityFeePerGas:1000000000n}));
    for(const tx of refundTxs){const receipt=await tx.wait(1,15000);assert.equal(receipt?.status,1);}
    for(const s of claimable)assert.equal((await rng.getRequest(s.requestId)).refunded,true);
    const fulfillmentReceipts=await Promise.all(fulfilledLogs.map(l=>ethers.provider.getTransactionReceipt(l.transactionHash)));
    const gasUsed=fulfillmentReceipts.map(r=>Number(r!.gasUsed));
    const epochCommits=await registry.queryFilter(registry.filters.EpochCommitted(),initial!.number);
    const report={scenario,environment:"local EDR + signed fixture APIs; NOT Arc testnet benchmark",prover:"release Rust daemon",apiCalls,epochCommits:epochCommits.length,configuration:{targetBlockMs:480,epochBlocks:200,gasPerBlock:30000000,baseFeeGwei:20,pollMs:scenario.pollMs??250,deadlineSeconds:60,maxGas:Number(env.MAX_GAS),fulfillBatchMax:Number(env.FULFILL_BATCH_MAX)},
      sent:states.length,fulfilled:successful.length,expired:claimable.length,pending:states.filter(s=>!s.fulfilled&&!s.refunded&&!s.deadlineExpired).length,
      observedBlock:observedHead!.number,observedTimestamp:observedHead!.timestamp,completedRefunds:refundTxs.length,within10Blocks:blocks.filter(n=>n<=10).length,
      within10BlocksRate:blocks.filter(n=>n<=10).length/states.length,tenBlockGoalMet:successful.length===states.length&&blocks.every(n=>n<=10),
      targetCoverage:[10,20,30,40,60].map(target=>({blocks:target,count:blocks.filter(n=>n<=target).length,rate:blocks.filter(n=>n<=target).length/states.length})),
      maxPendingObservedFromBlocks:states.reduce((m,s)=>Math.max(m,states.filter(x=>x.requestBlock<=s.requestBlock&&(x.fulfillmentBlock===null||x.fulfillmentBlock>s.requestBlock)).length),0),
      blockLatency:{p50:quantile(blocks,.5),p95:quantile(blocks,.95),p99:quantile(blocks,.99),max:blocks.length?Math.max(...blocks):null},
      fulfillmentGas:{p50:quantile(gasUsed,.5),p95:quantile(gasUsed,.95),max:gasUsed.length?Math.max(...gasUsed):null},
      chainSeconds:{p50:quantile(seconds,.5),p95:quantile(seconds,.95),p99:quantile(seconds,.99),max:seconds.length?Math.max(...seconds):null},
      wallSeconds:(performance.now()-started)/1000,observedMeanBlockMs:blockTimes.length>1?(blockTimes.at(-1)!-blockTimes[0])/(blockTimes.length-1):null,
      rpcRequests:counters,maxRpcInFlight,states};
    reports.push(report);await writeFile(join(dir,"report.json"),JSON.stringify(report,null,2)+"\n");
    console.log(JSON.stringify({...report,states:undefined,rpcRequests:undefined}));
  }finally{
    // Abrupt stop is deliberate for this isolated benchmark; never reuse its public keys or journals live.
    child.kill();await exited;clearInterval(miner);while(mining)await sleep(10);
    await writeFile(join(dir,"keeper.log"),logs);server.closeAllConnections();await new Promise<void>(r=>server.close(()=>r()));await connection.close();
  }
}
await writeFile(join(output,"summary.json"),JSON.stringify({generatedAt:new Date().toISOString(),reports},null,2)+"\n");
console.log(`REPORT=${join(output,"summary.json")}`);
