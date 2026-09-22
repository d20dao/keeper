import {deployProxy,implementationCodeHash} from "../test/helpers/proxy.ts";
// Local-only: real Rust process, real secp256k1 proofs, epoch registry and EVM.
import {createServer} from "node:http";
import {spawn} from "node:child_process";
import {mkdir, mkdtemp, writeFile} from "node:fs/promises";
import {join, resolve} from "node:path";
import {once} from "node:events";
import {DatabaseSync} from "node:sqlite";
import assert from "node:assert/strict";
import {buildKeeper} from "./lib/keeper-binary.ts";
import {network} from "hardhat";
import {TEST_SECRET, publicKey} from "../test/helpers/proof.ts";
import {epochFixtureData} from "../test/helpers/epoch.ts";
import {canonicalApiRequest} from "../src/sources.ts";
import {BUILTIN_EPOCH_RECIPES,type EpochProvider} from "../src/epoch.ts";
// The exact recipe 1 body AirnodeHub's dRPC gateway accepts: a nested eth_call params object, no responseProjection.
const ETHEREUM_BLOCK_HASH_BODY='{"operation":"jsonRpc","parameters":{"network":"ethereum","method":"eth_call","params":[{"to":"0xcA11bde05977b3631167028862bE2a173976CA11","data":"0x27e86d6e"},"latest"]}}';
// The rollout catalog: five recipes, neighbouring slots never share a provider.
const CATALOG=[0,1,2,4,5];
const providerOf=(recipe:number)=>BUILTIN_EPOCH_RECIPES[recipe].provider;

const {ethers, networkHelpers, provider} = await network.create({network:"loadSim"});
assert.equal((await ethers.provider.getNetwork()).chainId,31337n);
const binary=await buildKeeper("debug");
await mkdir(".research",{recursive:true});const dir=await mkdtemp(resolve(".research/epoch-keeper-e2e-"));
const vrfPath=join(dir,"fixture-vrf.key"),txPath=join(dir,"fixture-tx.key");
const wallet=ethers.HDNodeWallet.fromPhrase("test test test test test test test test test test test junk",undefined,"m/44'/60'/0'/0/2");
const signers=["11","22","33","44"].map(value=>new ethers.Wallet("0x"+value.repeat(32)));
await writeFile(vrfPath,ethers.toBeHex(TEST_SECRET,32),{mode:0o600});await writeFile(txPath,wallet.privateKey,{mode:0o600});
const [owner,player]=await ethers.getSigners();
const registry=await deployProxy(ethers,"EpochEntropy",[signers.map(s=>s.address),owner.address,wallet.address]);
// Epoch 1 keeps the initial four-recipe catalog; from epoch 2 one test Airnode per provider signs the five-source catalog.
const providerSigners:Record<EpochProvider,InstanceType<typeof ethers.Wallet>>={hyperliquid:signers[0],drpc:signers[1],tickerlayer:signers[2],nodary:new ethers.Wallet("0x"+"55".repeat(32))};
await registry.scheduleCatalog(CATALOG,CATALOG.map(recipe=>providerSigners[providerOf(recipe)].address),2);
const signerFor=(address:string)=>[...signers,...Object.values(providerSigners)].find(w=>w.address===address)!;
const rng=await deployProxy(ethers,"D20VRFCoordinator",[publicKey(),owner.address,owner.address,123,1,await registry.getAddress(),0]);
await rng.setPricing(123,0,300000); // Flat fee: the fixture consumers send exact values.
const game=await ethers.deployContract("TestConsumer",[await rng.getAddress()]);
const singleSelector=rng.interface.getFunction("fulfillRandomness")!.selector,batchSelector=rng.interface.getFunction("fulfillRandomnessBatch")!.selector;
const counts:Record<string,number>={},apiCounts:Record<string,number>={},apiBodiesByRecipe=Array(BUILTIN_EPOCH_RECIPES.length).fill(0),apiRecipesByEpoch:Record<string,number[]>={},unexpectedApiBodies:Array<{recipe:number;text:string}>=[];
const raw:string[]=[];let estimateRejects=0,rpcDelayMs=0;
const warmRpcLatency:Array<{rpcDelayMs:number;requestId:string;wallMs:number}>=[];
let holdApi=false,releaseApi:(()=>void)|undefined,apiEntered:(()=>void)|undefined;
const apiRejectRecipes=new Set<number>(),apiUnavailableProviders=new Set<EpochProvider>();
let apiOutage=false,apiRetryAfter="2",apiReject=false,holdEpochSend=false,epochSendEntered:(()=>void)|undefined;
let loseEpochAck=false,unknownEpochSend=false,stallEpochEstimate=false,epochEstimateCalls=0;
let holdProofEstimate=false,proofEstimateEntered:(()=>void)|undefined,releaseProofEstimate:(()=>void)|undefined;
let hangPrimary=false,primaryCalls=0,fallbackCalls=0,proofContextDelayMs=0,rpcRateLimit=false;
let sweepRecipient="",holdSweepSend=false,sweepSendEntered:(()=>void)|undefined;
let holdBatchSend=false,batchSendEntered:(()=>void)|undefined,beforeBatchSend:((tx:ReturnType<typeof ethers.Transaction.from>)=>Promise<void>)|undefined;
let holdSendsFrom=""; // A wallet whose signed transactions the fixture RPC accepts but never forwards: a dead keeper.
// /unusable answers well-formed JSON-RPC whose results nothing can use. "finalized": null for every read at the finalized
// tag, as an endpoint that does not serve that tag answers, and "0x" for every nonce. "pins": null for the proxy
// implementation slots. "receipts": null for every receipt, as an endpoint that has not caught up with the receipt's
// block answers. Every other call, and every call while no mode is set, is answered as /rpc answers it.
let unusableMode:""|"finalized"|"pins"|"receipts"="",unusableCalls=0,unusableAnswers=0;
function unusableAnswer(body:{method:string,params?:unknown[]}):unknown{
  if(unusableMode==="finalized"&&body.method==="eth_getTransactionCount")return "0x";
  if(unusableMode==="finalized"&&(body.params??[]).includes("finalized"))return null;
  if(unusableMode==="pins"&&body.method==="eth_getStorageAt")return null;
  if(unusableMode==="receipts"&&body.method==="eth_getTransactionReceipt")return null;
  return undefined;
}
// A node that estimates no fulfillment batch of more than this many members and refuses larger ones without a revert,
// as a node refuses more gas than it estimates (0: no limit).
let batchEstimateLimit=0,refusedBatchEstimates=0;
const slowProofIds=new Set<string>();
const server=createServer(async(req,res)=>{
  const reply=(value:unknown,status=200)=>{res.writeHead(status,{"Content-Type":"application/json"});res.end(JSON.stringify(value));};
  try {
    const chunks:Buffer[]=[];for await(const chunk of req)chunks.push(Buffer.from(chunk));const text=Buffer.concat(chunks).toString(),body=JSON.parse(text);
    if(req.url?.startsWith("/api/")) {
      const recipe=Number(req.url.split("/").at(-1));assert.ok(recipe>=0&&recipe<Number(await registry.recipeCount()));
      const block=await provider.request({method:"eth_getBlockByNumber",params:["latest",false]}) as {number:string,timestamp:string};
      const epoch=await registry.nextEpochToPrepare(BigInt(block.number));
      // The requested source must be the selected one or a fallback whose window is already open.
      let selection;
      const sources=Number(await registry.sourceCountAt(epoch));
      for(let attempt=0;attempt<sources&&!selection;attempt++){
        const s=await registry.getEpochFallbackSelection(epoch,attempt);
        if(Number(s.recipe)===recipe){assert.ok(BigInt(block.number)>=await registry.fallbackOpensAt(epoch,attempt),"Fallback source fetched before its window");selection=s;}
      }
      assert.ok(selection,"Source is not a fallback of the epoch");
      // The keeper posts the registered recipe body byte for byte; its canonical form is the registry's canonical request.
      const registered=await registry.getRecipe(recipe);
      if(text!==registered.body||(recipe===1&&text!==ETHEREUM_BLOCK_HASH_BODY)){unexpectedApiBodies.push({recipe,text});throw new Error("Unexpected epoch API body");}
      const canonical=canonicalApiRequest(body);
      assert.equal(canonical,selection.canonicalRequest);apiCounts[String(epoch)]=(apiCounts[String(epoch)]??0)+1;apiBodiesByRecipe[recipe]++;(apiRecipesByEpoch[String(epoch)]??=[]).push(recipe);
      apiEntered?.();
      if(holdApi)await new Promise<void>(resolve=>{releaseApi=resolve;});
      if(apiOutage){res.setHeader("Retry-After",apiRetryAfter);reply({error:"fixture unavailable"},429);return;}
      if(apiUnavailableProviders.has(providerOf(recipe))){reply({error:"fixture gateway down"},503);return;} // Transient: counts toward the provider's circuit.
      if(apiReject||apiRejectRecipes.has(recipe)){reply({error:"fixture rejects query"},400);return;} // Permanent: no Retry-After, not a server error.
      const fresh=await provider.request({method:"eth_getBlockByNumber",params:["latest",false]}) as {timestamp:string};
      const timestamp=BigInt(fresh.timestamp);
      const data={...epochFixtureData(recipe),...(recipe===2||recipe===3?{size:0.000001}:{})};
      const requestHash=ethers.id(canonical);
      const digest=ethers.keccak256(ethers.solidityPacked(["bytes32","uint256","bytes"],[requestHash,timestamp,ethers.toUtf8Bytes(JSON.stringify(data))]));
      const airnode=signerFor(selection.airnode),signature=await airnode.signMessage(ethers.getBytes(digest));
      reply({airnode:airnode.address,requestHash,timestamp:String(timestamp),data,signature});return;
    }
    assert.ok(req.url==="/rpc"||req.url==="/primary"||req.url==="/unusable");
    if(rpcRateLimit){reply({jsonrpc:"2.0",id:null,error:{code:429,message:"rate limit exceeded"}},429);return;}
    // A JSON-RPC batch is answered element by element through this same endpoint, in order, so every hold,
    // fault and count applies to each call exactly as it would to a single request.
    if(Array.isArray(body)){
      const answers=await Promise.all(body.map(async item=>{const r=await fetch(`${base}${req.url}`,{method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify(item)});return {status:r.status,value:await r.json().catch(()=>null)};}));
      const failed=answers.find(a=>a.status!==200);
      if(failed){reply(failed.value??{error:"batch element failed"},failed.status);return;}
      reply(answers.map(a=>a.value));return;
    }
    if(req.url==="/primary"){primaryCalls++;if(hangPrimary)return;}
    else if(req.url==="/unusable"){
      unusableCalls++;const unusable=unusableAnswer(body);
      if(unusable!==undefined){unusableAnswers++;reply({jsonrpc:"2.0",id:body.id,result:unusable});return;}
    }
    else{fallbackCalls++;}
    if(rpcDelayMs>0)await new Promise(resolve=>setTimeout(resolve,rpcDelayMs));let name=body.method;
    if(name==="eth_call") {const data=body.params[0].data;const iface=body.params[0].to.toLowerCase()===(await registry.getAddress()).toLowerCase()?registry.interface:rng.interface;name+=":"+(iface.getFunction(data.slice(0,10))?.name??data.slice(0,10));}
    counts[name]=(counts[name]??0)+1;
    if(name==="eth_call:getProofContext"&&slowProofIds.has(String(rng.interface.decodeFunctionData("getProofContext",body.params[0].data)[0])))return;
    if(name==="eth_call:getProofContext"&&proofContextDelayMs>0)await new Promise(resolve=>setTimeout(resolve,proofContextDelayMs));
    if(body.method==="eth_estimateGas"&&body.params[0].to.toLowerCase()===(await registry.getAddress()).toLowerCase()) {
      epochEstimateCalls++;if(stallEpochEstimate)return;
    }
    if(batchEstimateLimit>0&&body.method==="eth_estimateGas"&&String(body.params[0].data??"").startsWith(batchSelector)
      &&(rng.interface.decodeFunctionData("fulfillRandomnessBatch",body.params[0].data)[0] as bigint[]).length>batchEstimateLimit){
      refusedBatchEstimates++;reply({jsonrpc:"2.0",id:body.id,error:{code:-32000,message:"gas required exceeds allowance (16777216)"}});return;
    }
    if(body.method==="eth_estimateGas"&&body.params[0].to.toLowerCase()===(await rng.getAddress()).toLowerCase()&&holdProofEstimate){
      proofEstimateEntered?.();await new Promise<void>(resolve=>{releaseProofEstimate=resolve;});
    }
    if(body.method==="eth_sendRawTransaction") {
      raw.push(body.params[0]);const tx=ethers.Transaction.from(body.params[0]);
      if(holdSendsFrom&&tx.from?.toLowerCase()===holdSendsFrom)return;
      if(sweepRecipient&&tx.to?.toLowerCase()===sweepRecipient&&tx.value>0n){sweepSendEntered?.();if(holdSweepSend)return;}
      if(tx.to?.toLowerCase()===(await registry.getAddress()).toLowerCase()) {
        epochSendEntered?.();if(holdEpochSend)return;
        if(unknownEpochSend){reply({jsonrpc:"2.0",id:body.id,error:{code:-32000,message:"network response unavailable"}});return;}
        if(loseEpochAck){await provider.request({method:body.method,params:body.params});res.destroy();return;}
      }
      if(tx.to?.toLowerCase()===(await rng.getAddress()).toLowerCase()&&tx.data.startsWith(batchSelector)) {
        batchSendEntered?.();if(holdBatchSend)return;
        // Signed and journaled already: whatever the hook does on chain, these bytes are what gets mined.
        if(beforeBatchSend){const hook=beforeBatchSend;beforeBatchSend=undefined;await hook(tx);}
      }
    }
    try {const result=await provider.request({method:body.method,params:body.params});reply({jsonrpc:"2.0",id:body.id,result});}
    catch(error){if(body.method==="eth_estimateGas"){estimateRejects++;}reply({jsonrpc:"2.0",id:body.id,error:{code:-32000,message:String(error)}});}
  } catch(error){reply({error:String(error)},500);}
});
server.listen(0,"127.0.0.1");await once(server,"listening");const addr=server.address();assert.ok(addr&&typeof addr!=="string");const base=`http://127.0.0.1:${addr.port}`;
const env:NodeJS.ProcessEnv={...process.env,NEON_DB:undefined,TELEGRAM_BOT_TOKEN:undefined,TELEGRAM_CHAT_ID:undefined,TELEGRAM_LOW_BALANCE_WEI:undefined,DISCORD_BOT_TOKEN:undefined,DISCORD_PROTOCOL_CHANNEL_ID:undefined,HEALTH_API_URL:undefined,HEALTH_API_KEY:undefined,HEALTH_INTERVAL_SECONDS:undefined,
  RPC_URLS:`${base}/rpc`,CHAIN_ID:"31337",COORDINATOR_ADDRESS:await rng.getAddress(),ALLOWED_CONSUMERS:await game.getAddress(),
  TX_KEY_FILE:txPath,VRF_KEY_FILE:vrfPath,TEST_API_BASE:`${base}/api`,EXPECTED_SOURCE_HASH:undefined,
  EXPECTED_CODE_HASH:undefined,EXPECTED_PROTOCOL_HASH:await rng.protocolConfigurationHash(),
    EXPECTED_IMPLEMENTATION_CODE_HASH:await implementationCodeHash(ethers,rng),
    EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH:await implementationCodeHash(ethers,registry),CANCEL_MAX_FEE_PER_GAS_WEI:"150000000000",FEE_COVERAGE_BPS:"0",
  SEND_TRANSACTIONS:"true",POLL_MS:"100",RUST_LOG:"warn"};
const children=new Set<ReturnType<typeof spawn>>();
function start(overrides:NodeJS.ProcessEnv={},args=["run","--once"],db="keeper.sqlite") {
  const child=spawn(binary,args,{env:{...env,KEEPER_DB:join(dir,db),TEST_LOCK_DIR:join(dir,"locks"),...overrides},windowsHide:true,stdio:["ignore","pipe","pipe"]});children.add(child);
  let out="",err="";child.stdout.on("data",d=>{out+=d;});child.stderr.on("data",d=>{err+=d;});
  if(process.env.DEBUG_KEEPER)child.stderr.pipe(process.stderr); // Local diagnosis of a failing scenario.
  const exit=new Promise<{code:number|null,out:string,err:string}>((resolve,reject)=>{child.on("error",reject);child.on("exit",code=>{children.delete(child);resolve({code,out,err});});});return {child,exit};
}
async function run(overrides:NodeJS.ProcessEnv={},args?:string[],db?:string) {
  const p=start(overrides,args,db);const timer=setTimeout(()=>p.child.kill(),30000);
  try{const result=await p.exit;assert.equal(result.code,0,result.err);return result;}finally{clearTimeout(timer);}
}
async function until(condition:()=>Promise<boolean>,message:string,ms=12000){const end=Date.now()+ms;while(Date.now()<end){if(await condition())return;await new Promise(r=>setTimeout(r,60));}throw Error(message);}
async function mineTo(n:bigint){const latest=BigInt(await provider.request({method:"eth_blockNumber",params:[]}) as string);if(n>latest)await provider.request({method:"hardhat_mine",params:[ethers.toQuantity(n-latest),"0x0"]});}
async function request(label:string){
  try{await game.connect(player).getFunction("request")(ethers.id(label),200000,player.address,{value:123});}
  catch(error){
    // A request that will not even be created makes every later assertion unreadable: say where the chain stood.
    const head=await ethers.provider.getBlockNumber(),epochId=await registry.epochForBlock(head);
    throw Error(`Request ${label} reverted at block ${head}, epoch ${epochId} (start ${await registry.epochStart(epochId)}, first ${await registry.firstEpochStart()}): ${error}`);
  }
  await networkHelpers.mine(2);return await game.lastRequestId() as bigint;
}
function rows(sql:string,db="keeper.sqlite"){const sqlite=new DatabaseSync(join(dir,db),{readOnly:true});try{return sqlite.prepare(sql).all();}finally{sqlite.close();}}
async function preparedEpoch(n:number){await until(async()=>rows(`SELECT api FROM epoch_work WHERE epoch=${n}`).some(r=>r.api!==null),`epoch ${n} packet missing`);}
async function requests(label:string,count:number){const ids:bigint[]=[];for(let n=0;n<count;n++)ids.push(await request(`${label}-${n}`));return ids;}
async function advanceTarget(id:bigint){const r=await rng.getRequest(id);if(r.targetBlock>0n)await mineTo(r.targetBlock+await rng.confirmationBlocks());}
async function settle(id:bigint,overrides:NodeJS.ProcessEnv={},db?:string){let last="";for(let attempt=0;attempt<8;attempt++){await advanceTarget(id);last=(await run(overrides,undefined,db)).err;if((await rng.getRequest(id)).fulfilled){await run(overrides,undefined,db);return;}await new Promise(r=>setTimeout(r,300));}throw Error(`Request ${id} did not settle: ${last}`);}
try {
  for(const field of ["EXPECTED_IMPLEMENTATION_CODE_HASH","EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH"]){
    const invalid=await start({[field]:ethers.ZeroHash}).exit;assert.equal(invalid.code,1);assert.match(invalid.err,/Implementation code pin mismatch/);
  }
  const pin=await start({EXPECTED_PROTOCOL_HASH:ethers.ZeroHash}).exit;assert.equal(pin.code,1);assert.match(pin.err,/configuration hash mismatch/);
  const sourcePin=await start({EXPECTED_SOURCE_HASH:ethers.ZeroHash}).exit;assert.equal(sourcePin.code,1);assert.match(sourcePin.err,/EXPECTED_PROTOCOL_HASH/);
  await assert.rejects(game.connect(player).getFunction("request")(ethers.id("before-bootstrap"),200000,player.address,{value:123}));
  await run();assert.equal(Object.values(apiCounts).reduce((a,b)=>a+b,0),0);assert.equal(raw.length,0);
  await mineTo(await registry.firstEpochStart());
  await run({SEND_TRANSACTIONS:"false"});await preparedEpoch(1);await run();
  assert.equal(apiCounts["1"],1);assert.equal(rows("SELECT COUNT(*) n FROM jobs")[0].n,0);
  assert.equal(raw.length,0,"Idle locally prepared epoch consumed a nonce");
  assert.equal((await registry.getEpoch(1)).epochHash,ethers.ZeroHash);
  const packet1=rows("SELECT api FROM epoch_work WHERE epoch=1")[0].api;
  // A separate consumer absent from the old ALLOWED_CONSUMERS value must trigger publication and delivery.
  const outsider=await ethers.deployContract("TestConsumer",[await rng.getAddress()]);
  assert.notEqual(await outsider.getAddress(),await game.getAddress());
  await outsider.connect(player).getFunction("request")(ethers.id("public-first-demand"),200000,player.address,{value:123});
  const first=await outsider.lastRequestId();
  await outsider.connect(player).getFunction("request")(ethers.id("public-shared-epoch-batch"),200000,player.address,{value:123});
  const batch=await outsider.lastRequestId();await networkHelpers.mine(2);
  assert.equal((await rng.getRequest(first)).targetBlock,0n);assert.equal((await rng.getRequest(first)).epochHash,ethers.ZeroHash);
  assert.equal(await ethers.provider.getBalance(await rng.getAddress()),246n);
  loseEpochAck=true;await run();loseEpochAck=false;await run();
  const committed1=await registry.getEpoch(1);assert.notEqual(committed1.epochHash,ethers.ZeroHash);
  assert.equal((await rng.getRequest(first)).targetBlock,committed1.committedBlock+1n);
  assert.equal((await rng.getRequest(batch)).targetBlock,(await rng.getRequest(first)).targetBlock);
  assert.equal(apiCounts["1"],1);assert.equal(rows("SELECT api FROM epoch_work WHERE epoch=1")[0].api,packet1);
  assert.equal(rows("SELECT COUNT(*) n FROM txs WHERE kind='epoch'")[0].n,1);
  await settle(first);await settle(batch);
  assert.equal((await rng.getRequest(first)).consumer,await outsider.getAddress());
  assert.equal((await rng.getRequest(first)).delivered,true);
  assert.equal(rows("SELECT COUNT(*) n FROM audit_events WHERE kind IN ('not_allowlisted','ignored')")[0].n,0);
  assert.equal(rows("SELECT value FROM meta WHERE key='consumer_access'")[0].value,"public");

  // A primary which hangs only AFTER verified startup cannot starve the healthy fallback.
  const beforePrimary=primaryCalls;
  const failover=start({RPC_URLS:`${base}/primary,${base}/rpc`},["run"]);
  try {
    await until(async()=>primaryCalls>beforePrimary+20,"Primary did not warm");
    hangPrimary=true;const beforeFallback=fallbackCalls;
    const id=await request("hung-primary-live-failover");
    // A default-view read already in flight when the primary hangs keeps its full 8 s attempt.
    await until(async()=>(await rng.getRequest(id)).fulfilled,"Healthy fallback did not serve through runtime timeout",25000);
    assert.equal((await rng.getRequest(id)).delivered,true);assert(fallbackCalls>beforeFallback);
    await until(async()=>rows("SELECT COUNT(*) n FROM txs WHERE state IN ('signed','submitted')")[0].n===0,"Failover receipt not reconciled");
  } finally {hangPrimary=false;failover.child.kill("SIGKILL");await failover.exit;}

  // RPC_URLS[0] turns unusable while the daemon reads from it: null for every finalized read and "0x" for every nonce.
  // Each read moves on to the next endpoint, so no tick fails: the daemon outlives MAX_TICK_FAILURES and keeps serving.
  const unusableDaemon=start({RPC_URLS:`${base}/unusable,${base}/rpc`,MAX_TICK_FAILURES:"2"},["run"]);
  let unusableLog="";
  try {
    const warmed=unusableCalls;
    await until(async()=>unusableCalls>warmed+20,"Daemon did not start reading from RPC_URLS[0]");
    unusableMode="finalized";const answeredBefore=unusableAnswers;
    const id=await request("served-past-an-unusable-first-endpoint");
    await until(async()=>(await rng.getRequest(id)).fulfilled,"Daemon did not serve past an unusable RPC_URLS[0]",25000);
    assert.equal((await rng.getRequest(id)).delivered,true);
    assert(unusableAnswers>answeredBefore,"RPC_URLS[0] was not asked once it turned unusable");
    await new Promise(resolve=>setTimeout(resolve,3000)); // Further ticks, well past MAX_TICK_FAILURES=2.
    assert.equal(unusableDaemon.child.exitCode,null,"The daemon exited on an unusable endpoint");
    await until(async()=>rows("SELECT COUNT(*) n FROM txs WHERE state IN ('signed','submitted')")[0].n===0,"Receipt not reconciled past an unusable endpoint");
  } finally {unusableMode="";unusableDaemon.child.kill("SIGKILL");unusableLog=(await unusableDaemon.exit).err;}
  assert.match(unusableLog,/RPC answer unusable/);
  assert.doesNotMatch(unusableLog,/Keeper tick failed/,"An unusable endpoint failed a tick");
  // Unusable from startup on: RPC_URLS[0] answers the implementation slots with null and is left out, or answers every
  // finalized read with null. Startup completes either way and every run serves.
  for(const mode of ["pins","finalized"] as const){
    unusableMode=mode;const answeredBefore=unusableAnswers;
    try {
      const id=await request(`served-with-an-unusable-first-endpoint-from-startup-${mode}`);
      await settle(id,{RPC_URLS:`${base}/unusable,${base}/rpc`});
    } finally {unusableMode="";}
    assert(unusableAnswers>answeredBefore,`RPC_URLS[0] was not asked in ${mode} mode`);
  }
  // RPC_URLS[0] has not caught up with the receipts' blocks and answers null for them. The nonce is used, so the keeper
  // asks every endpoint before it resolves the nonce from contract state alone, and reconciles the receipt with its
  // notices, for a single fulfillment and for a batch.
  for(const [members,reconciled] of [[1,/Receipt reconciled/],[2,/Batch receipt reconciled/]] as const){
    const ids=await requests(`receipt-behind-first-endpoint-${members}`,members);
    const behindEnv={RPC_URLS:`${base}/unusable,${base}/rpc`,RUST_LOG:"d20dao_keeper=info"};
    let log="";unusableMode="receipts";const answeredBefore=unusableAnswers;
    try {
      for(let attempt=0;attempt<8&&!reconciled.test(log);attempt++){for(const id of ids)await advanceTarget(id);log+=(await run(behindEnv)).err;}
    } finally {unusableMode="";}
    assert(unusableAnswers>answeredBefore,"RPC_URLS[0] was not asked for a receipt");
    assert.match(log,reconciled,"A receipt another endpoint served was not reconciled");
    assert.doesNotMatch(log,/no endpoint served its receipt/);
    for(const id of ids)assert.equal((await rng.getRequest(id)).delivered,true);
    assert.equal(rows("SELECT COUNT(*) n FROM txs WHERE state IN ('signed','submitted')")[0].n,0);
  }

  // Rate limits are not faults. While every endpoint answers 429 the daemon defers its ticks without counting them
  // toward MAX_TICK_FAILURES, reports rpc_rate_limited as degraded health, and resumes by itself when they end.
  const limitedSince=Math.floor(Date.now()/1000)+1;
  const limitedDaemon=start({MAX_TICK_FAILURES:"2"},["run"]);
  const healthNow=()=>JSON.parse(String(rows("SELECT value FROM meta WHERE key='health:status'")[0]?.value??"{}")) as {healthy?:boolean,faults?:string[],observed_at?:number};
  try {
    // An observation this daemon made, not the previous one's.
    await until(async()=>healthNow().healthy===true&&(healthNow().observed_at??0)>=limitedSince&&limitedDaemon.child.exitCode===null,"Daemon did not start healthy");
    rpcRateLimit=true;
    await until(async()=>healthNow().faults?.includes("rpc_rate_limited")??false,"Rate limiting did not degrade health",20000);
    await new Promise(resolve=>setTimeout(resolve,8000)); // Several deferred ticks, well past MAX_TICK_FAILURES=2.
    assert.equal(limitedDaemon.child.exitCode,null,"The daemon exited on rate limits alone");
    assert.equal(healthNow().healthy,false);
    rpcRateLimit=false;
    await until(async()=>healthNow().healthy===true,"Daemon did not recover after the rate limit ended",45000);
    const id=await request("after-rate-limit");
    await until(async()=>(await rng.getRequest(id)).fulfilled,"Daemon did not serve after the rate limit ended",25000);
  } finally {rpcRateLimit=false;limitedDaemon.child.kill("SIGKILL");await limitedDaemon.exit;}

  // One fast preparation is sent before seven stalled RPC preparations exhaust the tick.
  // Compare with an idle run on this machine: waiting for the stalled 8 s reads costs far more than the 3 s slice.
  const baselineStarted=Date.now();await run();const baselineMs=Date.now()-baselineStarted;
  const fastPreparation=await request("fast-proof-before-slow-peers"),slowIds:bigint[]=[];
  for(let n=0;n<7;n++){const id=await request(`slow-proof-${n}`);slowIds.push(id);slowProofIds.add(String(id));}
  const preparationStarted=Date.now();await run();
  assert.equal((await rng.getRequest(fastPreparation)).fulfilled,true,"Fast proof waited for the full slow batch");
  assert(Date.now()-preparationStarted<baselineMs+6000,"Preparation did not yield its bounded slices");
  slowProofIds.clear();await run();
  for(const id of slowIds)await settle(id);

  // Preparation reads slower than the 3 s slice must still journal the proof instead of being cancelled every pass.
  const slowContext=await request("slow-proof-context");proofContextDelayMs=3500;
  try {await run();} finally {proofContextDelayMs=0;}
  assert.equal((await rng.getRequest(slowContext)).fulfilled,true,"Slow preparation was cancelled before journaling its proof");

  // Constant provider latency must not consume the entire maintenance budget before paid work runs.
  for(const delay of [200,500]){
    const discoveryBeforeLatency=counts["eth_call:nextRequestId"]??0;
    const warm=start({},["run"]);
    try {
      await until(async()=>(counts["eth_call:nextRequestId"]??0)>discoveryBeforeLatency,"Daemon did not warm before RPC latency test");
      rpcDelayMs=delay;const startedAt=Date.now();const id=await request(`warm-rpc-${delay}ms`);
      await until(async()=>(await rng.getRequest(id)).fulfilled,`Warm request did not fulfill under constant ${delay}ms RPC latency`,45000);
      const elapsed=Date.now()-startedAt;
      assert.equal((await rng.getRequest(id)).delivered,true);
      assert.equal((await rng.queryFilter(rng.filters.RandomnessFulfilled(id))).length,1);
      assert.equal(apiCounts["1"],1,"RPC latency caused snapshot refresh");
      warmRpcLatency.push({rpcDelayMs:delay,requestId:id.toString(),wallMs:elapsed});
      rpcDelayMs=0;
      await until(async()=>rows("SELECT COUNT(*) n FROM txs WHERE state IN ('signed','submitted')")[0].n===0,"Warm latency request receipt did not reconcile");
    } finally {rpcDelayMs=0;warm.child.kill("SIGKILL");await warm.exit;}
  }

  const second=await request("proof-restart");await run({SEND_TRANSACTIONS:"false"});
  const proofBefore=rows(`SELECT proof,call FROM jobs WHERE id='${second}'`)[0];assert.ok(proofBefore.proof);
  const contextBefore=counts["eth_call:getProofContext"];await settle(second);await run();
  assert.deepEqual(rows(`SELECT proof,call FROM jobs WHERE id='${second}'`)[0],proofBefore);
  assert.equal(counts["eth_call:getProofContext"],contextBefore);

  // A real daemon outage recovers undiscovered live gaps, without reviving expired work.
  const expiredBeforeOutage=await request("expired-before-thirty-second-outage");
  await networkHelpers.time.increaseTo((await rng.getRequest(expiredBeforeOutage)).deadline+1n);
  const discoveryBefore=counts["eth_call:nextRequestId"]??0;
  const runningBeforeOutage=start({},["run"]);
  await until(async()=>(counts["eth_call:nextRequestId"]??0)>discoveryBefore,"Daemon never became active before outage");
  runningBeforeOutage.child.kill("SIGKILL");await runningBeforeOutage.exit;
  assert.equal(children.size,0,"Keeper must actually be stopped during outage");
  const callbacksBeforeOutage=await game.callbackCount(),servedBeforeOutage=await rng.lastServedIndex();
  const apiBeforeOutage={...apiCounts},rawBeforeOutage=raw.length;
  const recoveredA=await request("outage-undiscovered-gap-a"),recoveredB=await request("outage-undiscovered-gap-b");
  assert.equal(rows(`SELECT COUNT(*) n FROM jobs WHERE id IN ('${recoveredA}','${recoveredB}')`)[0].n,0,"Stopped daemon discovered requests");
  const outageStart=(await ethers.provider.getBlock("latest"))!;
  await networkHelpers.time.setNextBlockTimestamp(outageStart.timestamp+1);
  await provider.request({method:"hardhat_mine",params:["0x1e","0x1"]});
  const outageEnd=(await ethers.provider.getBlock("latest"))!;
  assert.equal(outageEnd.timestamp-outageStart.timestamp,30,"Outage must advance exactly 30 chain seconds");
  assert.equal(outageEnd.number-outageStart.number,30);assert.equal(raw.length,rawBeforeOutage);
  for(const id of [recoveredA,recoveredB]){
    const r=await rng.getRequest(id);assert.equal(r.fulfilled,false);assert(BigInt(outageEnd.timestamp)<r.deadline,"Recovery fixture has no time remaining");
    assert.equal(r.epochHash,committed1.epochHash);assert.equal(r.targetBlock,r.requestBlock);
  }
  await settle(recoveredB);await settle(recoveredA);await run();
  assert.equal(await rng.lastServedIndex(),servedBeforeOutage+2n);assert.equal(await game.callbackCount(),callbacksBeforeOutage+2n);
  assert.equal(await rng.nextRequestId(),recoveredB+1n,"Outage recovery created replacement requests");
  for(const id of [recoveredA,recoveredB])assert.equal((await rng.queryFilter(rng.filters.RandomnessFulfilled(id))).length,1,"Recovered request fulfilled more than once");
  const expiredState=await rng.getRequest(expiredBeforeOutage);assert.equal(expiredState.fulfilled,false);assert.equal(expiredState.refunded,false);
  assert.equal((await rng.queryFilter(rng.filters.ProofVerified(expiredBeforeOutage))).length,0);
  assert.equal(rows(`SELECT COUNT(*) n FROM txs WHERE job='${expiredBeforeOutage}'`)[0].n,0);
  assert.equal(rows(`SELECT COUNT(*) n FROM jobs WHERE id='${expiredBeforeOutage}' AND proof IS NOT NULL`)[0].n,0);
  assert.deepEqual(apiCounts,apiBeforeOutage,"Recovery refreshed an already committed snapshot");
  const refundBalance=await ethers.provider.getBalance(player.address);await rng.refundRequest(expiredBeforeOutage);
  assert.equal(await ethers.provider.getBalance(player.address),refundBalance+123n);
  const outageRecovery={chainSeconds:30,blocks:30,liveRequestsRecovered:2,expiredRequestsUnserved:1,duplicateFulfillments:0};

  // A current-epoch HTTP fetch cannot block a ready request from the preceding epoch.
  await mineTo(await registry.epochStart(2)-4n);const old=await request("previous-epoch-during-http");
  await mineTo(await registry.epochStart(2));holdApi=true;
  const entered=new Promise<void>(resolve=>{apiEntered=resolve;});const daemon=start({},["run"]);
  await entered;await until(async()=>(await rng.getRequest(old)).fulfilled,"Background epoch HTTP blocked ready proof");
  assert.equal((await registry.getEpoch(2)).epochHash,ethers.ZeroHash);
  holdApi=false;releaseApi?.();apiEntered=undefined;await preparedEpoch(2);
  daemon.child.kill("SIGKILL");await daemon.exit;await run();
  assert.equal((await registry.getEpoch(2)).epochHash,ethers.ZeroHash,"Idle prepared snapshot was published");
  assert.equal(apiCounts["2"],1);assert.equal(rows("SELECT COUNT(*) n FROM pragma_table_info('jobs') WHERE name='api'")[0].n,0);

  // A crash after signing keeps the exact epoch bytes and exclusive wallet nonce.
  const laneRequest=await request("maintenance-lane-exclusivity");await run({SEND_TRANSACTIONS:"false"});
  const apiBeforeCrash={...apiCounts};holdEpochSend=true;const enteredSend=new Promise<void>(resolve=>{epochSendEntered=resolve;});
  const crashing=start();await Promise.race([enteredSend,crashing.exit.then(r=>{throw Error(`Epoch broadcast missing ${r.err}`);})]);
  const savedRaw=raw.at(-1)!;const signed=rows("SELECT raw,kind,nonce FROM txs WHERE state IN ('signed','submitted')");
  assert.equal(signed.length,1);assert.equal(signed[0].raw,savedRaw);assert.equal(signed[0].kind,"epoch");
  assert.equal((await rng.getRequest(laneRequest)).fulfilled,false,"Request used unresolved maintenance nonce");
  crashing.child.kill("SIGKILL");await crashing.exit;holdEpochSend=false;epochSendEntered=undefined;
  await networkHelpers.time.increase(3);await run();assert.equal(raw.at(-1),savedRaw,"Epoch crash recovery changed signed bytes");
  await settle(laneRequest);assert.deepEqual(apiCounts,apiBeforeCrash);
  assert.equal(rows("SELECT COUNT(DISTINCT nonce) n FROM txs WHERE state IN ('signed','submitted')")[0].n,0);

  const bad=await request("bad-proof-preflight");await run({SEND_TRANSACTIONS:"false"});
  const sqlite=new DatabaseSync(join(dir,"keeper.sqlite"));const badProof=JSON.parse(String(sqlite.prepare("SELECT proof FROM jobs WHERE id=?").get(String(bad))!.proof));badProof.s=ethers.toBeHex(BigInt(badProof.s)^1n);
  sqlite.prepare("UPDATE jobs SET proof=?,call=? WHERE id=?").run(JSON.stringify(badProof),rng.interface.encodeFunctionData("fulfillRandomness",[bad,badProof]),String(bad));sqlite.close();
  const beforeBad=raw.length;await run();assert.ok(estimateRejects>0);assert.equal(raw.length,beforeBad);assert.equal(rows(`SELECT COUNT(*) n FROM txs WHERE job='${bad}'`)[0].n,0);
  await networkHelpers.time.increase(61);const estimates=counts.eth_estimateGas;await run();assert.equal(counts.eth_estimateGas,estimates);await rng.refundRequest(bad);

  // Slow publication preflight must leave ready previous-epoch work schedulable.
  await mineTo(await registry.epochStart(3)-4n);const previous=await request("old-ready-during-estimate");
  await mineTo(await registry.epochStart(3));await run({SEND_TRANSACTIONS:"false"});await preparedEpoch(3);
  const current=await request("current-demand-stalled-estimate");
  stallEpochEstimate=true;const stalledBefore=epochEstimateCalls,stallStarted=Date.now();
  await run();assert.equal((await rng.getRequest(previous)).fulfilled,true,"Stalled publication estimate starved ready request");
  assert.ok(Date.now()-stallStarted<8000,"Maintenance estimate exhausted request tick");assert.equal(epochEstimateCalls,stalledBefore+1);
  await run();assert.equal(epochEstimateCalls,stalledBefore+1,"Timed-out epoch estimate lost durable backoff");
  stallEpochEstimate=false;await new Promise(resolve=>setTimeout(resolve,3100));await settle(current);

  // Unpublished demand remains escrowed and cannot acquire substitute epoch data.
  await mineTo(await registry.epochStart(4));apiOutage=true;apiRetryAfter="30";
  const missing=await request("unpublished-request-expiry");await run();const outageCalls=apiCounts["4"];
  await run();assert.equal(apiCounts["4"],outageCalls,"Retry-After was ignored");
  assert.equal((await rng.getRequest(missing)).epochHash,ethers.ZeroHash);
  await networkHelpers.time.increaseTo((await rng.getRequest(missing)).deadline+1n);const rawAtExpiry=raw.length;
  await run();assert.equal(raw.length,rawAtExpiry,"Expired unpublished request caused a transaction");
  assert.equal(apiCounts["4"],outageCalls,"Expired demand retried API work");await rng.refundRequest(missing);
  assert.equal((await registry.getEpoch(4)).epochHash,ethers.ZeroHash);

  // Unknown publication acknowledgment expires with demand, not with epoch start.
  apiOutage=false;await mineTo(await registry.epochStart(5));await run({SEND_TRANSACTIONS:"false"});await preparedEpoch(5);
  const abandoned=await request("ambiguous-publication-demand");unknownEpochSend=true;await run();
  const unresolved=rows("SELECT * FROM txs WHERE state IN ('signed','submitted')");assert.equal(unresolved.length,1);assert.equal(unresolved[0].kind,"epoch");
  const epochRaw=String(unresolved[0].raw),epochNonce=unresolved[0].nonce;
  await networkHelpers.time.increaseTo((await rng.getRequest(abandoned)).deadline+1n);unknownEpochSend=false;const cutoffRaw=raw.length;
  await run();await run();assert.equal(raw.slice(cutoffRaw).includes(epochRaw),false,"Expired demand's epoch bytes were rebroadcast");
  const cancellation=rows(`SELECT kind,nonce FROM txs WHERE nonce=${epochNonce} ORDER BY id DESC`)[0];assert.equal(cancellation.kind,"epoch_cancel");assert.equal(cancellation.nonce,epochNonce);
  assert.equal(rows("SELECT COUNT(*) n FROM txs WHERE state IN ('signed','submitted')")[0].n,0);
  assert.equal((await registry.getEpoch(5)).epochHash,ethers.ZeroHash);await rng.refundRequest(abandoned);
  assert.equal(apiCounts["5"],1,"Cancellation refreshed the fixed local snapshot");

  // First publication can occur after an epoch boundary for its still-live request.
  await mineTo(await registry.epochStart(6));await run({SEND_TRANSACTIONS:"false"});await preparedEpoch(6);
  const rawIdle=raw.length;await run();assert.equal(raw.length,rawIdle);
  await mineTo(await registry.epochStart(7)-4n);const lateEpoch=await request("previous-epoch-first-publication");
  await mineTo(await registry.epochStart(7));await settle(lateEpoch);
  assert.equal((await rng.getRequest(lateEpoch)).epochId,6n);
  assert((await registry.getEpoch(6)).committedBlock>=await registry.epochStart(7));assert.equal(apiCounts["6"],1);
  assert.equal(rows("SELECT COUNT(*) n FROM jobs WHERE id LIKE 'epoch:%'")[0].n,0);
  assert.equal(rows("SELECT COUNT(*) n FROM audit_events WHERE request_id LIKE 'epoch:%'")[0].n,0);
  assert.ok(rows("SELECT nonce,COUNT(DISTINCT job) n FROM txs GROUP BY nonce").every(r=>r.n===1));
  assert.equal(counts["eth_call:verifyRequestProof"]??0,0);assert.equal(counts["eth_call:requestSeed"]??0,0);
  await networkHelpers.time.increase(61);apiOutage=true;
  const compactDb=new DatabaseSync(join(dir,"keeper.sqlite"));compactDb.exec("UPDATE meta SET value='0' WHERE key='history:compact_after'");compactDb.close();await run();
  assert.equal(rows("SELECT api,selection FROM epoch_work WHERE epoch=1")[0].api,null);
  assert.equal(rows("SELECT COUNT(*) n FROM txs WHERE state='resolved' AND (raw!='' OR payload!='')")[0].n,0);
  assert.notEqual((await registry.getEpoch(1)).epochHash,ethers.ZeroHash);
  await run({},["migrate","--from",join(dir,"keeper.sqlite"),"--prepare"],"migrated.sqlite");
  await run({},["migrate","--from",join(dir,"keeper.sqlite"),"--apply"],"migrated.sqlite");await run({},undefined,"migrated.sqlite");
  assert.equal(rows("SELECT api FROM epoch_work WHERE epoch=1","migrated.sqlite")[0].api,null);
  // Upgrade while fulfillment preflight is outstanding: unchanged proxy address must not bypass implementation pins.
  apiOutage=false;await mineTo(await registry.epochStart(8));
  const warmUpgradeEpoch=await request("upgrade-epoch-bootstrap");await settle(warmUpgradeEpoch,{},"migrated.sqlite");
  const upgradeRequest=await request("pending-through-reviewed-proxy-upgrade");
  holdProofEstimate=true;const preflightEntered=new Promise<void>(resolve=>{proofEstimateEntered=resolve;});
  const guarded=start({},["run"],"migrated.sqlite");let upgradeRejected=false;
  guarded.child.stderr.on("data",d=>{if(String(d).includes("Proxy implementation changed"))upgradeRejected=true;});
  await Promise.race([preflightEntered,guarded.exit.then(r=>{throw Error(`Upgrade preflight missing ${r.err}`);})]);
  const rawBeforeUpgrade=raw.length,coordinatorAddress=await rng.getAddress(),registryAddress=await registry.getAddress();
  const coordinatorNext=await ethers.deployContract("CoordinatorUpgradeProbe"),registryNext=await ethers.deployContract("EpochUpgradeProbe");
  await rng.upgradeToAndCall(await coordinatorNext.getAddress(),"0x");await registry.upgradeToAndCall(await registryNext.getAddress(),"0x");
  holdProofEstimate=false;releaseProofEstimate?.();proofEstimateEntered=undefined;
  await until(async()=>upgradeRejected,"Running keeper did not detect proxy implementation change");
  assert.equal(raw.length,rawBeforeUpgrade,"Keeper sent through an implementation change without pin review");
  assert.equal((await rng.getRequest(upgradeRequest)).fulfilled,false);
  assert.equal(await rng.getAddress(),coordinatorAddress);assert.equal(await registry.getAddress(),registryAddress);
  guarded.child.kill("SIGKILL");await guarded.exit;
  assert.equal(rows(`SELECT COUNT(*) n FROM txs WHERE job='${upgradeRequest}'`,"migrated.sqlite")[0].n,0);
  const preparedBeforeReview=rows(`SELECT proof,call FROM jobs WHERE id='${upgradeRequest}'`,"migrated.sqlite")[0];assert.ok(preparedBeforeReview.proof);
  const stale=await start({},["run","--once"],"migrated.sqlite").exit;assert.equal(stale.code,1);assert.match(stale.err,/Implementation code pin mismatch/);
  const reviewedPins={EXPECTED_IMPLEMENTATION_CODE_HASH:await implementationCodeHash(ethers,rng),EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH:await implementationCodeHash(ethers,registry)};
  await new Promise(resolve=>setTimeout(resolve,3100)); // Respect the preflight retry marker retained through the rejected upgrade tick.
  await settle(upgradeRequest,reviewedPins,"migrated.sqlite");
  assert.deepEqual(rows(`SELECT proof,call FROM jobs WHERE id='${upgradeRequest}'`,"migrated.sqlite")[0],preparedBeforeReview);
  // Four transient fetch errors must survive process restart without poisoning this epoch. The third opens the provider's
  // circuit, so the fourth attempt sends no request; once the cooldown has passed the same source recovers.
  await mineTo(await registry.epochStart(9));apiOutage=true;apiRetryAfter="2";
  for(let n=1;n<=4;n++){
    await run({...reviewedPins,SEND_TRANSACTIONS:"false"},undefined,"migrated.sqlite");
    const saved=rows("SELECT state,attempts,api,last_error FROM epoch_work WHERE epoch=9","migrated.sqlite")[0];
    assert.equal(saved.state,"pending");assert.equal(saved.attempts,n);assert.equal(saved.api,null);
    assert.equal(apiCounts["9"],Math.min(n,3));if(n===4)assert.match(String(saved.last_error),/circuit open after 3 consecutive failures/);
    const db=new DatabaseSync(join(dir,"migrated.sqlite"));db.exec("UPDATE epoch_work SET retry_at=0 WHERE epoch=9");db.close(); // Fixture clock acceleration; Retry-After is tested separately.
  }
  {const db=new DatabaseSync(join(dir,"migrated.sqlite"));db.exec("DELETE FROM epoch_breaker");db.close();} // Fixture clock: the circuit cooldown has passed.
  apiOutage=false;const recoveredEpoch=await request("transient-epoch-recovery");await settle(recoveredEpoch,reviewedPins,"migrated.sqlite");
  assert.equal(apiCounts["9"],4);assert.equal((await rng.getRequest(recoveredEpoch)).delivered,true);
  // A base fee pricing sends above the configured cap defers publication and fulfillment by budget:
  // nothing is signed or broadcast, health names the budget, and raising the cap serves the same request.
  const mainnetBudget={MAX_GAS:"6000000",MAX_FEE_PER_GAS_WEI:"2000000000000",CANCEL_MAX_FEE_PER_GAS_WEI:"2500000000000",MAX_TX_COST_WEI:"4000000000000000000"};
  const healthStatus=()=>JSON.parse(String(rows("SELECT value FROM meta WHERE key='health:status'","migrated.sqlite")[0].value)) as {healthy:boolean,faults:string[]};
  async function raiseBaseFee(){
    await provider.request({method:"hardhat_setNextBlockBaseFeePerGas",params:[ethers.toQuantity(400n*10n**9n)]});await networkHelpers.mine(1);
  }
  async function budgetFaultInDaemon(){
    const daemon=start({...reviewedPins,PROGRESS_STUCK_SECONDS:"1"},["run"],"migrated.sqlite");
    try {await until(async()=>healthStatus().faults.includes("fee_budget_exceeded"),"Budget deferral never degraded health");}
    finally {daemon.child.kill("SIGKILL");await daemon.exit;}
    const health=healthStatus();assert.equal(health.healthy,false);return health;
  }
  async function deferredByBudget(id:bigint,kind:string,stalledFault?:string){
    const baseFee=(await ethers.provider.getBlock("latest"))!.baseFeePerGas!;assert(baseFee*2n+10n**9n>100n*10n**9n,"Fixture base fee prices sends under the default cap");
    const rawBefore=raw.length;
    const deferred=await start({...reviewedPins,PROGRESS_STUCK_SECONDS:"1"},undefined,"migrated.sqlite").exit;
    assert.match(deferred.err,/Observed base fee exceeds the fulfillment fee cap/);
    assert.match(deferred.err,/Send deferred by fee budget/);assert.match(deferred.err,/"cap":"MAX_FEE_PER_GAS_WEI"/);assert.match(deferred.err,new RegExp(`"kind":"${kind}"`));
    assert.equal(raw.length,rawBefore,"Budget deferral signed or broadcast a transaction");
    assert.equal(rows("SELECT COUNT(*) n FROM txs WHERE state IN ('signed','submitted')","migrated.sqlite")[0].n,0);
    assert.equal(rows(`SELECT COUNT(*) n FROM txs WHERE job='${id}'`,"migrated.sqlite")[0].n,0);
    // The observation ages only inside a running process; every restart reloads the caps and resets it.
    const health=await budgetFaultInDaemon();
    if(stalledFault)assert.ok(health.faults.includes(stalledFault),health.faults.join(","));
    else assert.deepEqual(health.faults,["fee_budget_exceeded"],"Budget deferral recorded a generic stage");
    assert.equal(raw.length,rawBefore);assert.equal((await rng.getRequest(id)).fulfilled,false);
    await settle(id,{...reviewedPins,...mainnetBudget},"migrated.sqlite");
    assert.equal((await rng.getRequest(id)).delivered,true);assert.equal(healthStatus().healthy,true,healthStatus().faults.join(","));
  }
  await mineTo(await registry.epochStart(10));await run({...reviewedPins,SEND_TRANSACTIONS:"false"},undefined,"migrated.sqlite");
  await until(async()=>rows("SELECT api FROM epoch_work WHERE epoch=10","migrated.sqlite").some(r=>r.api!==null),"epoch 10 packet missing");
  await raiseBaseFee();const budgetedEpoch=await request("fee-budget-epoch-publication");
  await deferredByBudget(budgetedEpoch,"epoch","preparation_stalled");
  assert.notEqual((await registry.getEpoch(10)).epochHash,ethers.ZeroHash);assert.equal(apiCounts["10"],1,"Budget deferral refreshed the fixed snapshot");
  await raiseBaseFee();const budgetedFulfillment=await request("fee-budget-fulfillment");
  await deferredByBudget(budgetedFulfillment,"fulfill");
  assert.ok(rows(`SELECT proof FROM jobs WHERE id='${budgetedFulfillment}'`,"migrated.sqlite")[0].proof,"Proof preparation waited for the fee budget");
  assert.equal(rows(`SELECT COUNT(*) n FROM txs WHERE job IN ('${budgetedEpoch}','${budgetedFulfillment}')`,"migrated.sqlite")[0].n,2,"Deferred requests were not served with exactly one transaction each");
  assert.equal(rows("SELECT COUNT(*) n FROM txs WHERE kind='epoch' AND job LIKE '%:10'","migrated.sqlite")[0].n,1,"Budget deferral re-signed the epoch publication");
  // Fee coverage: a request whose escrowed (fixture-sized) fee cannot cover the expected cost is deferred, never signed.
  {
    const uncoveredId=await request("fee-coverage");let err="";
    for(let n=0;n<8&&!/"cap":"FEE_COVERAGE_BPS"/.test(err);n++)err=(await start({...reviewedPins,...mainnetBudget,FEE_COVERAGE_BPS:"10000"},undefined,"migrated.sqlite").exit).err;
    assert.match(err,/"cap":"FEE_COVERAGE_BPS"/);assert.match(err,/under FEE_COVERAGE_BPS/);
    assert.equal(rows(`SELECT COUNT(*) n FROM txs WHERE job='${uncoveredId}'`,"migrated.sqlite")[0].n,0,"An uncovered fulfillment was signed");
    assert.equal((await rng.getRequest(uncoveredId)).fulfilled,false);
    await settle(uncoveredId,{...reviewedPins,...mainnetBudget},"migrated.sqlite");assert.equal((await rng.getRequest(uncoveredId)).delivered,true);
  }
  // Raised caps take effect at restart: the budget observation describes the previous configuration,
  // so an idle keeper is healthy again immediately, before any send.
  await raiseBaseFee();const clearedAtRestart=await request("fee-budget-cleared-at-restart");const rawBeforeIdle=raw.length;
  const idleDeferred=await start({...reviewedPins,PROGRESS_STUCK_SECONDS:"1"},undefined,"migrated.sqlite").exit;
  assert.match(idleDeferred.err,/Send deferred by fee budget/);
  assert.deepEqual((await budgetFaultInDaemon()).faults,["fee_budget_exceeded"]);
  await networkHelpers.time.increaseTo((await rng.getRequest(clearedAtRestart)).deadline+1n);
  await run({...reviewedPins,...mainnetBudget,PROGRESS_STUCK_SECONDS:"1"},undefined,"migrated.sqlite"); // Exit 0: healthy with no demand and no send.
  assert.equal(healthStatus().healthy,true,healthStatus().faults.join(","));assert.equal(raw.length,rawBeforeIdle,"Restart with raised caps sent for expired demand");
  assert.equal(rows("SELECT COUNT(*) n FROM meta WHERE key='health:blocked:fee_budget'","migrated.sqlite")[0].n,0);
  assert.equal(rows(`SELECT COUNT(*) n FROM txs WHERE job='${clearedAtRestart}'`,"migrated.sqlite")[0].n,0);await rng.refundRequest(clearedAtRestart);
  await provider.request({method:"hardhat_setNextBlockBaseFeePerGas",params:[ethers.toQuantity(20n*10n**9n)]});await networkHelpers.mine(1);
  // Every source rejecting the query walks the five-source fallback ladder once, one source per 20-block window.
  // Live paid demand on the exhausted epoch is an epoch_stalled fault, without refetching or sending,
  // until that demand expires; later demand is served by a fallback source and health is clean again.
  await mineTo(await registry.epochStart(11));apiReject=true;
  const blockedDemand=await request("blocked-epoch-live-demand");const rawBeforeBlocked=raw.length;
  const epochWork=(n:number)=>rows(`SELECT state,fallback FROM epoch_work WHERE epoch=${n}`,"migrated.sqlite").map(r=>`${r.state}:${r.fallback}`)[0];
  await start({...reviewedPins,PROGRESS_STUCK_SECONDS:"1"},undefined,"migrated.sqlite").exit; // Shutdown awaits the permanent fetch failure.
  assert.equal(epochWork(11),"blocked:0");
  let ladderLog="";
  assert.equal(await registry.sourceCountAt(11),5n);
  for(let attempt=1;attempt<=4;attempt++){
    // A blocked source with a fallback still ahead is not an alarm; the last one alarms once it fails.
    if(attempt===4)assert.doesNotMatch(ladderLog,/Live paid demand cannot be published/);
    await mineTo(await registry.epochStart(11)+BigInt(20*attempt+5)); // Zero-interval blocks keep the demand's deadline live.
    ladderLog+=(await start({...reviewedPins,PROGRESS_STUCK_SECONDS:"1"},undefined,"migrated.sqlite").exit).err;
    assert.equal(epochWork(11),`blocked:${attempt}`);
  }
  assert.equal(apiCounts["11"],5);
  // The keeper walked the catalog in slot order from the selected source, changing provider at every step.
  const primary11=Number((await registry.getEpochSelection(11)).source),ladder=apiRecipesByEpoch["11"];
  assert.deepEqual(ladder,CATALOG.map((_,n)=>CATALOG[(primary11+n)%5]));
  assert.ok(ladder.every((recipe,n)=>n===0||providerOf(recipe)!==providerOf(ladder[n-1])),"A fallback stayed with the same provider");
  const alarmed=await start({...reviewedPins,PROGRESS_STUCK_SECONDS:"1"},undefined,"migrated.sqlite").exit;
  assert.match(ladderLog+alarmed.err,/Live paid demand cannot be published/);assert.match(ladderLog+alarmed.err,/"reason":"blocked"/);
  await new Promise(r=>setTimeout(r,1100));
  const stalledEpoch=await start({...reviewedPins,PROGRESS_STUCK_SECONDS:"1"},undefined,"migrated.sqlite").exit;
  assert.equal(stalledEpoch.code,1);assert.match(stalledEpoch.err,/Keeper unhealthy: .*epoch_stalled/);
  assert.ok(healthStatus().faults.includes("epoch_stalled"),healthStatus().faults.join(","));
  assert.equal(raw.length,rawBeforeBlocked,"Blocked epoch demand caused a transaction");assert.equal(apiCounts["11"],5,"Blocked epoch was refetched");
  assert.equal((await registry.getEpoch(11)).epochHash,ethers.ZeroHash);
  await networkHelpers.time.increaseTo((await rng.getRequest(blockedDemand)).deadline+1n);
  await start({...reviewedPins,PROGRESS_STUCK_SECONDS:"1"},undefined,"migrated.sqlite").exit; // Expired demand clears the epoch stage; the unprepared job's stage remains.
  assert.ok(!healthStatus().faults.includes("epoch_stalled"),healthStatus().faults.join(","));
  await rng.refundRequest(blockedDemand);apiReject=false;
  // Every recipe of the selected source's provider rejects in epoch 12: a blocked source with a fallback ahead is not a
  // stall (the run exits healthy), and after 20 blocks the next source, from another provider, is committed with commitEpochFallback.
  await mineTo(await registry.epochStart(12));const selected12=await registry.getEpochSelection(12),primary12=Number(selected12.source);
  const rejectedProvider=providerOf(Number(selected12.recipe));
  for(const recipe of CATALOG)if(providerOf(recipe)===rejectedProvider)apiRejectRecipes.add(recipe);
  const afterBlocked=await request("publishable-epoch-after-blocked");
  await run({...reviewedPins,SEND_TRANSACTIONS:"false"},undefined,"migrated.sqlite");assert.equal(epochWork(12),"blocked:0");
  await mineTo(await registry.epochStart(12)+25n);await settle(afterBlocked,reviewedPins,"migrated.sqlite");
  assert.equal((await rng.getRequest(afterBlocked)).delivered,true);assert.equal(healthStatus().healthy,true,healthStatus().faults.join(","));
  assert.equal(epochWork(12),"committed:1");assert.equal(Number((await registry.getEpoch(12)).source),(primary12+1)%5);
  assert.notEqual(providerOf(CATALOG[(primary12+1)%5]),rejectedProvider);
  const fallbackSelector=registry.interface.getFunction("commitEpochFallback")!.selector;
  assert.equal(raw.filter(hex=>ethers.Transaction.from(hex).data.startsWith(fallbackSelector)).length,1);
  apiRejectRecipes.clear();assert.equal(epochWork(11),"blocked:4");assert.equal(apiCounts["11"],5);

  // Provider circuit breaker. A gateway failing transiently three times opens that Airnode's circuit: while it is open the
  // keeper records its sources as failed without a request, so the next source is published as soon as its window opens.
  const breakerRows=()=>rows("SELECT airnode,failures,open_until FROM epoch_breaker","migrated.sqlite") as Array<{airnode:string;failures:number;open_until:number}>;
  const epochError=(n:bigint)=>String(rows(`SELECT last_error FROM epoch_work WHERE epoch=${n}`,"migrated.sqlite")[0]?.last_error??"");
  {
    let breakerEpoch=await registry.epochForBlock(await ethers.provider.getBlockNumber())+1n;await mineTo(await registry.epochStart(breakerEpoch));
    const downProvider=providerOf(Number((await registry.getEpochSelection(breakerEpoch)).recipe)),downSigner=providerSigners[downProvider].address;
    apiUnavailableProviders.add(downProvider);
    const tripped=await request("breaker-trips");
    for(let n=1;n<=3;n++){
      await run({...reviewedPins,SEND_TRANSACTIONS:"false"},undefined,"migrated.sqlite");
      const db=new DatabaseSync(join(dir,"migrated.sqlite"));db.exec(`UPDATE epoch_work SET retry_at=0 WHERE epoch=${breakerEpoch}`);db.close(); // Fixture clock acceleration.
    }
    assert.equal(apiCounts[String(breakerEpoch)],3);
    const opened=breakerRows().find(row=>row.airnode.toLowerCase()===downSigner.toLowerCase());
    assert.ok(opened&&opened.failures===3&&opened.open_until>Date.now()/1000,"Three transient failures did not open the provider circuit");
    // The next window publishes another provider's source; the down gateway is not asked again.
    await mineTo(await registry.epochStart(breakerEpoch)+20n);await settle(tripped,reviewedPins,"migrated.sqlite");
    assert.equal(epochWork(Number(breakerEpoch)),"committed:1");assert.equal(apiCounts[String(breakerEpoch)],4);
    // A later epoch that selects the same provider skips it without any request and falls back at the first window.
    let skipped:bigint|undefined;
    for(let tries=0;tries<30&&skipped===undefined;tries++){
      breakerEpoch++;await mineTo(await registry.epochStart(breakerEpoch));
      if(providerOf(Number((await registry.getEpochSelection(breakerEpoch)).recipe))===downProvider)skipped=breakerEpoch;
    }
    assert.ok(skipped!==undefined,"No epoch selected the down provider");
    const served=await request("breaker-skips-open-provider");
    await run({...reviewedPins,SEND_TRANSACTIONS:"false"},undefined,"migrated.sqlite");
    assert.equal(apiCounts[String(skipped)]??0,0,"An open circuit still requested the down gateway");
    assert.match(epochError(skipped),/circuit open after 3 consecutive failures/);
    await mineTo(await registry.epochStart(skipped)+20n);await settle(served,reviewedPins,"migrated.sqlite");
    assert.equal(epochWork(Number(skipped)),"committed:1");assert.equal(apiCounts[String(skipped)],1);
    assert.ok(!apiRecipesByEpoch[String(skipped)].some(recipe=>providerOf(recipe)===downProvider));
    assert.equal((await rng.getRequest(served)).delivered,true);
    apiUnavailableProviders.clear();
    // Fixture clock: the cooldown has passed and a probe succeeded, so the circuit is closed for the scenarios below.
    const db=new DatabaseSync(join(dir,"migrated.sqlite"));db.exec("DELETE FROM epoch_breaker");db.close();
    console.log(JSON.stringify({providerCircuitBreaker:{provider:downProvider,trippedAfter:3,skippedEpoch:String(skipped),requestsToOpenGateway:0,fallbackAttempt:1}}));
  }

  // Batched fulfillment. On the guarded coordinator the estimate already budgets every served member, and the keeper's
  // floor adds only the transaction's own cost: about 630k gas per 200,000-gas member, so MAX_GAS=13000000 carries a
  // full batch.
  const batchEpoch=await registry.epochForBlock(await ethers.provider.getBlockNumber())+1n;
  const batchDb="migrated.sqlite",batchEnv={...reviewedPins,MAX_GAS:"13000000"};
  const toCoordinator=(hex:string,selector:string)=>{const tx=ethers.Transaction.from(hex);return tx.to?.toLowerCase()===coordinatorAddress.toLowerCase()&&tx.data.startsWith(selector);};
  const isBatchRaw=(hex:string)=>toCoordinator(hex,batchSelector),isSingleRaw=(hex:string)=>toCoordinator(hex,singleSelector);
  const coordinatorEvents=(receipt:{logs:readonly {address:string;topics:readonly string[];data:string}[]},name:string)=>receipt.logs.filter(l=>l.address.toLowerCase()===coordinatorAddress.toLowerCase()).map(l=>rng.interface.parseLog(l)).filter(e=>e?.name===name).map(e=>e!);
  const jobState=(id:bigint)=>String(rows(`SELECT state FROM jobs WHERE id='${id}'`,batchDb)[0].state);
  async function settleAll(ids:bigint[],overrides:NodeJS.ProcessEnv=batchEnv){
    let last="";
    for(let attempt=0;attempt<14;attempt++){
      for(const id of ids)await advanceTarget(id);
      last=(await run(overrides,undefined,batchDb)).err;
      const states=await Promise.all(ids.map(id=>rng.getRequest(id)));
      if(states.every(s=>s.fulfilled)){await run(overrides,undefined,batchDb);return;}
      await new Promise(r=>setTimeout(r,300));
    }
    throw Error(`Requests ${ids.join(",")} did not settle: ${last}`);
  }
  await mineTo(await registry.epochStart(batchEpoch));await run({...batchEnv,SEND_TRANSACTIONS:"false"},undefined,batchDb);
  await until(async()=>rows(`SELECT api FROM epoch_work WHERE epoch=${batchEpoch}`,batchDb).some(r=>r.api!==null),`epoch ${batchEpoch} packet missing`);
  await rng.setKeeperFeeBps(5000);
  // Six paid requests in one published epoch are served by exactly one fulfillRandomnessBatch on one nonce.
  const batchIds=await requests("batch-member",6);const rawBeforeBatch=raw.length;
  await settleAll(batchIds);
  const batchRaws=raw.slice(rawBeforeBatch).filter(isBatchRaw);
  assert.equal(batchRaws.length,1,"Six ready requests were not served by exactly one batch transaction");
  assert.equal(raw.slice(rawBeforeBatch).filter(isSingleRaw).length,0,"A single fulfillment was sent alongside the batch");
  const batchReceipt=(await ethers.provider.getTransactionReceipt(ethers.keccak256(batchRaws[0])))!;assert.equal(batchReceipt.status,1);
  assert.deepEqual(coordinatorEvents(batchReceipt,"RandomnessFulfilled").map(e=>String(e.args.requestId)).sort(),batchIds.map(String).sort());
  assert.equal(coordinatorEvents(batchReceipt,"FulfillmentSkipped").length,0);
  const keeperPaid=coordinatorEvents(batchReceipt,"KeeperFeePaid");assert.equal(keeperPaid.length,6,"Keeper share was not paid per member");
  for(const e of keeperPaid){assert.equal(e.args.keeper,wallet.address);assert.equal(e.args.amount,61n);assert.equal(e.args.paid,true);}
  for(const id of batchIds){const r=await rng.getRequest(id);assert.equal(r.fulfilled,true);assert.equal(r.delivered,true);assert.equal(jobState(id),"served");assert.equal((await rng.queryFilter(rng.filters.RandomnessFulfilled(id))).length,1);}
  // Earlier scenarios also batch under the default FULFILL_BATCH_MAX; select only this scenario's batch.
  const batchTxs=rows(`SELECT DISTINCT txs.job,txs.nonce,txs.state FROM txs JOIN batch_members ON batch_members.job=txs.job WHERE txs.kind='fulfill_batch' AND batch_members.request_id IN (${batchIds.map(id=>`'${id}'`).join(",")})`,batchDb);
  assert.equal(batchTxs.length,1);assert.equal(batchTxs[0].state,"resolved");
  assert.equal(rows(`SELECT COUNT(*) n FROM batch_members WHERE job='${batchTxs[0].job}'`,batchDb)[0].n,6);
  assert.equal(rows(`SELECT COUNT(DISTINCT nonce) n FROM txs WHERE job='${batchTxs[0].job}'`,batchDb)[0].n,1);
  assert.equal(rows("SELECT COUNT(*) n FROM txs WHERE state IN ('signed','submitted')",batchDb)[0].n,0);
  // A member fulfilled by a third party after signing is skipped on chain; the rest are served and every member resolves.
  const raceIds=await requests("batch-race",4),taken=raceIds[1];
  beforeBatchSend=async tx=>{
    const [ids]=rng.interface.decodeFunctionData("fulfillRandomnessBatch",tx.data);
    assert.ok((ids as bigint[]).map(String).includes(String(taken)),"Race fixture member missing from the signed batch");
    const proof=JSON.parse(String(rows(`SELECT proof FROM jobs WHERE id='${taken}'`,batchDb)[0].proof));
    await(await rng.connect(owner).getFunction("fulfillRandomness")(taken,proof,{gasLimit:2_000_000})).wait();
  };
  const rawBeforeRace=raw.length;
  await settleAll(raceIds);
  assert.equal(beforeBatchSend,undefined,"Third-party fulfillment hook never ran");
  const raceRaws=raw.slice(rawBeforeRace).filter(isBatchRaw);assert.equal(raceRaws.length,1);
  const raceReceipt=(await ethers.provider.getTransactionReceipt(ethers.keccak256(raceRaws[0])))!;assert.equal(raceReceipt.status,1);
  assert.deepEqual(coordinatorEvents(raceReceipt,"FulfillmentSkipped").map(e=>[String(e.args.requestId),Number(e.args.reason)]),[[String(taken),1]]);
  assert.deepEqual(coordinatorEvents(raceReceipt,"RandomnessFulfilled").map(e=>String(e.args.requestId)).sort(),raceIds.filter(id=>id!==taken).map(String).sort());
  assert.equal((await rng.queryFilter(rng.filters.RandomnessFulfilled(taken))).length,1,"Raced member was fulfilled twice");
  for(const id of raceIds){assert.equal((await rng.getRequest(id)).fulfilled,true);assert.equal(jobState(id),"served");}
  assert.equal(rows("SELECT COUNT(*) n FROM txs WHERE state IN ('signed','submitted')",batchDb)[0].n,0);
  // A corrupt journaled proof makes the batch preflight revert: the tick falls back to single sends, the bad
  // member follows the single path (rejected, never signed) and batching resumes without it.
  const mixedIds=await requests("batch-mixed",5),corrupt=mixedIds[1],good=mixedIds.filter(id=>id!==corrupt);
  await run({...batchEnv,SEND_TRANSACTIONS:"false"},undefined,batchDb);
  for(const id of mixedIds)assert.ok(rows(`SELECT proof FROM jobs WHERE id='${id}'`,batchDb)[0].proof,"Mixed fixture not prepared");
  {const db=new DatabaseSync(join(dir,batchDb));const p=JSON.parse(String(db.prepare("SELECT proof FROM jobs WHERE id=?").get(String(corrupt))!.proof));p.s=ethers.toBeHex(BigInt(p.s)^1n);
    db.prepare("UPDATE jobs SET proof=?,call=? WHERE id=?").run(JSON.stringify(p),rng.interface.encodeFunctionData("fulfillRandomness",[corrupt,p]),String(corrupt));db.close();}
  const rawBeforeMixed=raw.length,rejectsBeforeMixed=estimateRejects;
  await settleAll(good);
  assert.ok(estimateRejects>rejectsBeforeMixed,"Corrupt member was never rejected in preflight");
  const mixedRaws=raw.slice(rawBeforeMixed);
  assert.ok(mixedRaws.some(isSingleRaw),"Rejected batch preflight did not fall back to single sends");
  assert.ok(mixedRaws.some(isBatchRaw),"Batching did not resume once the bad member was excluded");
  for(const id of good){assert.equal((await rng.getRequest(id)).delivered,true);assert.equal(jobState(id),"served");}
  assert.equal((await rng.getRequest(corrupt)).fulfilled,false);
  assert.equal(rows(`SELECT COUNT(*) n FROM txs WHERE job='${corrupt}'`,batchDb)[0].n,0);
  assert.equal(rows(`SELECT COUNT(*) n FROM batch_members WHERE request_id='${corrupt}'`,batchDb)[0].n,0,"Corrupt member entered a signed batch");
  assert.equal(rows(`SELECT COUNT(*) n FROM meta WHERE key='batch_exclude:${corrupt}'`,batchDb)[0].n,1);
  await networkHelpers.time.increaseTo((await rng.getRequest(corrupt)).deadline+1n);await run(batchEnv,undefined,batchDb);
  assert.equal(jobState(corrupt),"expired");await rng.refundRequest(corrupt);
  // A node that estimates no batch of more than three members refuses a larger one without a revert: the batch is
  // halved and sent as batches, never one request at a time.
  const refusedIds=await requests("batch-refused-estimate",6),rawBeforeRefused=raw.length,refusedBefore=refusedBatchEstimates;
  batchEstimateLimit=3;
  try {await settleAll(refusedIds);} finally {batchEstimateLimit=0;}
  assert.ok(refusedBatchEstimates>refusedBefore,"The node never refused a batch estimate");
  const refusedRaws=raw.slice(rawBeforeRefused);
  assert.equal(refusedRaws.filter(isSingleRaw).length,0,"A batch estimate refused without a revert fell back to single sends");
  const refusedSizes=refusedRaws.filter(isBatchRaw).map(hex=>(rng.interface.decodeFunctionData("fulfillRandomnessBatch",ethers.Transaction.from(hex).data)[0] as bigint[]).length);
  assert.ok(refusedSizes.length>=2&&refusedSizes.every(n=>n>=2&&n<=3),`Refused batches were not halved: ${refusedSizes}`);
  for(const id of refusedIds){assert.equal((await rng.getRequest(id)).delivered,true);assert.equal(jobState(id),"served");}
  console.log(JSON.stringify({refusedBatchEstimate:{members:6,estimateLimit:3,refused:refusedBatchEstimates-refusedBefore,batchSizes:refusedSizes}}));
  // A crash after a batch is signed but before its receipt: the restart rebroadcasts the identical bytes on the
  // same nonce and resolves every member.
  const crashIds=await requests("batch-crash",3);await run({...batchEnv,SEND_TRANSACTIONS:"false"},undefined,batchDb);
  holdBatchSend=true;const enteredBatch=new Promise<void>(resolve=>{batchSendEntered=resolve;});
  const crashingBatch=start(batchEnv,undefined,batchDb);
  await Promise.race([enteredBatch,crashingBatch.exit.then(r=>{throw Error(`Batch broadcast missing ${r.err}`);})]);
  const savedBatchRaw=raw.at(-1)!;assert.ok(isBatchRaw(savedBatchRaw));
  const signedBatch=rows("SELECT job,raw,kind,nonce FROM txs WHERE state IN ('signed','submitted')",batchDb);
  assert.equal(signedBatch.length,1);assert.equal(signedBatch[0].raw,savedBatchRaw);assert.equal(signedBatch[0].kind,"fulfill_batch");
  assert.equal(rows(`SELECT COUNT(*) n FROM batch_members WHERE job='${signedBatch[0].job}'`,batchDb)[0].n,3);
  for(const id of crashIds)assert.equal(jobState(id),"signed");
  crashingBatch.child.kill("SIGKILL");await crashingBatch.exit;holdBatchSend=false;batchSendEntered=undefined;
  for(const id of crashIds)assert.equal((await rng.getRequest(id)).fulfilled,false,"Held batch was mined");
  const rawAfterCrash=raw.length;await networkHelpers.time.increase(3);
  await settleAll(crashIds);
  const recoveryRaws=raw.slice(rawAfterCrash).filter(hex=>isBatchRaw(hex)||isSingleRaw(hex));
  assert.ok(recoveryRaws.length>=1&&recoveryRaws.every(hex=>hex===savedBatchRaw),"Batch crash recovery changed the signed bytes");
  assert.equal(rows(`SELECT COUNT(DISTINCT nonce) n FROM txs WHERE job='${signedBatch[0].job}'`,batchDb)[0].n,1);
  assert.equal(rows(`SELECT COUNT(*) n FROM txs WHERE job='${signedBatch[0].job}' AND state!='resolved'`,batchDb)[0].n,0);
  for(const id of crashIds){assert.equal((await rng.getRequest(id)).delivered,true);assert.equal(jobState(id),"served");}
  // FULFILL_BATCH_MAX is bounded by the coordinator limit, and 1 is exactly the single path.
  const invalidBatchMax=await start({...batchEnv,FULFILL_BATCH_MAX:"17"},undefined,batchDb).exit;assert.equal(invalidBatchMax.code,1);assert.match(invalidBatchMax.err,/FULFILL_BATCH_MAX/);
  const singlePathIds=await requests("batch-disabled",3),rawBeforeSinglePath=raw.length,batchTxsBeforeSinglePath=rows("SELECT COUNT(*) n FROM txs WHERE kind='fulfill_batch'",batchDb)[0].n;
  await settleAll(singlePathIds,{...batchEnv,FULFILL_BATCH_MAX:"1"});
  assert.equal(raw.slice(rawBeforeSinglePath).filter(isBatchRaw).length,0,"FULFILL_BATCH_MAX=1 still batched");
  assert.equal(raw.slice(rawBeforeSinglePath).filter(isSingleRaw).length,3);
  assert.equal(rows("SELECT COUNT(*) n FROM txs WHERE kind='fulfill_batch'",batchDb)[0].n,batchTxsBeforeSinglePath);
  for(const id of singlePathIds){assert.equal(jobState(id),"served");assert.deepEqual(rows(`SELECT kind FROM txs WHERE job='${id}'`,batchDb).map(r=>r.kind),["fulfill"]);}
  assert.ok(rows("SELECT nonce,COUNT(DISTINCT job) n FROM txs GROUP BY nonce",batchDb).every(r=>r.n===1),"A nonce served more than one job");
  // Operator sweep: the command only queues; the running keeper signs on its own nonce lane, journals before
  // broadcast, rebroadcasts identical bytes after a crash, and service resumes on the next nonce.
  const sweepStatus=async()=>JSON.parse((await run(batchEnv,["sweep","--status"],batchDb)).out);
  const recipient=await rng.feeRecipient();sweepRecipient=recipient.toLowerCase();
  const both=await start(batchEnv,["sweep","--amount","1","--keep","2"],batchDb).exit;assert.equal(both.code,1);assert.match(both.err,/either --amount or --keep/);
  const malformed=await start(batchEnv,["sweep","--amount","1e3"],batchDb).exit;assert.equal(malformed.code,1);
  await run(batchEnv,["sweep","--amount","1"],batchDb);
  const queuedTwice=await start(batchEnv,["sweep","--keep","5"],batchDb).exit;assert.equal(queuedTwice.code,1);assert.match(queuedTwice.err,/already queued/);
  assert.equal(JSON.parse((await run(batchEnv,["sweep","--cancel"],batchDb)).out).removed_queued_request,true);
  assert.equal((await sweepStatus()).queued,null);
  // A request that would leave less than the reserve is refused without signing anything.
  await run(batchEnv,["sweep","--keep","0.5"],batchDb);const rawBeforeRefusal=raw.length;
  await run(batchEnv,undefined,batchDb);
  assert.equal(raw.length,rawBeforeRefusal,"A refused sweep was broadcast");
  const refusal=await sweepStatus();assert.equal(refusal.last.state,"refused");assert.match(refusal.last.detail,/reserve/);assert.equal(refusal.queued,null);
  // SEND_TRANSACTIONS=false never signs a queued sweep.
  await run(batchEnv,["sweep","--amount","2.5"],batchDb);const recipientBefore=await ethers.provider.getBalance(recipient);
  await run({...batchEnv,SEND_TRANSACTIONS:"false"},undefined,batchDb);
  assert.equal(raw.length,rawBeforeRefusal);assert.notEqual((await sweepStatus()).queued,null);
  holdSweepSend=true;const enteredSweep=new Promise<void>(resolve=>{sweepSendEntered=resolve;});
  const crashingSweep=start(batchEnv,undefined,batchDb);
  await Promise.race([enteredSweep,crashingSweep.exit.then(r=>{throw Error(`Sweep broadcast missing ${r.err}`);})]);
  const savedSweepRaw=raw.at(-1)!;
  const inFlight=JSON.parse(String(rows("SELECT value FROM meta WHERE key='sweep:attempt'",batchDb)[0].value));
  assert.equal(inFlight.txs.length,1);assert.equal(inFlight.txs[0].raw,savedSweepRaw,"Sweep broadcast before its bytes were journaled");
  assert.equal(inFlight.to.toLowerCase(),sweepRecipient);assert.equal(inFlight.value,ethers.parseEther("2.5").toString());
  assert.equal(rows("SELECT COUNT(*) n FROM meta WHERE key='sweep:request'",batchDb)[0].n,0);
  const sweepTx=ethers.Transaction.from(savedSweepRaw);assert.equal(sweepTx.from,wallet.address);assert.equal(BigInt(sweepTx.nonce),BigInt(inFlight.nonce));
  crashingSweep.child.kill("SIGKILL");await crashingSweep.exit;holdSweepSend=false;sweepSendEntered=undefined;
  assert.equal(await ethers.provider.getBalance(recipient),recipientBefore,"Held sweep was mined");
  const busyStatus=await sweepStatus();assert.equal(busyStatus.in_flight.nonce,inFlight.nonce);assert.ok(!JSON.stringify(busyStatus).includes(savedSweepRaw.slice(2,40)),"Status exposed signed bytes");
  // Requests that arrive while the sweep is unresolved wait for it; the restart rebroadcasts the identical bytes first.
  const afterSweepIds=await requests("after-sweep",2);await run({...batchEnv,SEND_TRANSACTIONS:"false"},undefined,batchDb);
  await new Promise(r=>setTimeout(r,2100));const rawAfterSweepCrash=raw.length;
  await run(batchEnv,undefined,batchDb);
  assert.deepEqual(raw.slice(rawAfterSweepCrash),[savedSweepRaw],"The sweep lane was not exclusive or its bytes changed");
  await run(batchEnv,undefined,batchDb);
  const sent=await sweepStatus();assert.equal(sent.last.state,"sent");assert.equal(sent.last.tx_hash,ethers.keccak256(savedSweepRaw));assert.equal(sent.in_flight,null);
  assert.equal(await ethers.provider.getBalance(recipient),recipientBefore+ethers.parseEther("2.5"));
  assert.ok(Number(rows("SELECT value FROM meta WHERE key='nonce_floor'",batchDb)[0].value)>=inFlight.nonce+1,"Sweep did not raise the nonce floor");
  await settleAll(afterSweepIds);
  for(const id of afterSweepIds){assert.equal((await rng.getRequest(id)).delivered,true);assert.equal(jobState(id),"served");}
  assert.equal(rows(`SELECT COUNT(*) n FROM txs WHERE nonce=${inFlight.nonce}`,batchDb)[0].n,0,"A game transaction reused the sweep nonce");
  sweepRecipient="";
  console.log(JSON.stringify({operatorSweep:{queuedOnly:true,refusedBelowReserve:true,sendDisabledNeverSigns:true,crashRestartIdenticalBytes:true,exclusiveLane:true,nonceFloorRaised:true,serviceResumed:true}}));
  // Primary and follower keepers. The follower has its own wallet, key file and journal, is an allowed backup
  // committer, and joins the queue only when the join rule fires: the oldest unserved request is older than
  // FOLLOWER_DELAY_SECONDS, the visible queue is deeper than FOLLOWER_QUEUE_JOIN, or the primary is chain-dead
  // (sendable work waiting for PRIMARY_LIVENESS_SECONDS without a committer nonce advance). It then works the
  // newest end of the queue, so the two fronts never meet on the same request until the queue is exhausted.
  {
    const followerWallet=ethers.HDNodeWallet.fromPhrase("test test test test test test test test test test test junk",undefined,"m/44'/60'/0'/0/3");
    const followerKey=join(dir,"follower-tx.key");await writeFile(followerKey,followerWallet.privateKey,{mode:0o600});
    const followerDb="follower.sqlite",primaryDb="migrated.sqlite";
    const delay=20,liveness=10; // The shipped defaults, exercised with chain time under test control.
    // A follower runs on another host: its own host-local scope locks, like its own journal.
    const followerEnv={...batchEnv,TX_KEY_FILE:followerKey,KEEPER_ROLE:"follower",TEST_LOCK_DIR:join(dir,"follower-host-locks")};
    const primaryAddress=wallet.address.toLowerCase(),followerAddress=followerWallet.address.toLowerCase();
    const sentBy=(address:string,from:number)=>raw.slice(from).map(hex=>ethers.Transaction.from(hex)).filter(tx=>tx.from!.toLowerCase()===address);
    const chainNow=async()=>Number((await ethers.provider.getBlock("latest"))!.timestamp);
    const createdAt=async(id:bigint)=>Number((await rng.getRequest(id)).deadline)-60;
    const ticks=async(n=4)=>{await new Promise(r=>setTimeout(r,n*150));};
    // Age a request to `seconds` the way a live chain does, one block per second, so a keeper sees the work wait
    // instead of finding it already old: the liveness rule measures how long work waited while it watched.
    const ageTo=async(id:bigint,seconds:number)=>{
      const created=await createdAt(id);
      for(let guard=0;guard<120&&await chainNow()-created<seconds;guard++){
        await networkHelpers.mine(1);await advanceTarget(id);await ticks(1);
      }
    };
    const servedAge=async(id:bigint)=>{
      const [event]=await rng.queryFilter(rng.filters.RandomnessFulfilled(id));
      return Number((await ethers.provider.getBlock(event.blockNumber))!.timestamp)-await createdAt(id);
    };
    // Keep the chain moving while the follower observes: a finalized head only advances with new blocks.
    const idleBlocks=async(seconds:number)=>{for(let n=0;n<seconds;n++){await networkHelpers.mine(1);await ticks(1);}};
    const followerStatus=()=>JSON.parse(String(rows("SELECT value FROM meta WHERE key='health:status'",followerDb)[0].value));
    const primarySigner=wallet.connect(ethers.provider);
    // A committer transaction lands, which is all a follower sees of a working primary.
    const primaryActs=async()=>{await (await primarySigner.sendTransaction({to:wallet.address,value:0})).wait();};
    // The join rule scenarios below measure fulfillment alone, so the current epoch already has its packet: the
    // committer publishes it here, exactly as the primary keeper would, without any gateway or follower delay.
    const publishEpochAsPrimary=async()=>{
      const epochId=await registry.epochForBlock(await ethers.provider.getBlockNumber());
      if((await registry.getEpoch(epochId)).epochHash!==ethers.ZeroHash)return epochId;
      const selection=await registry.getEpochSelection(epochId);
      const timestamp=BigInt(await chainNow()),data=ethers.toUtf8Bytes(JSON.stringify(epochFixtureData(Number(selection.recipe))));
      const digest=ethers.keccak256(ethers.solidityPacked(["bytes32","uint256","bytes"],[selection.queryHash,timestamp,data]));
      const signature=await signerFor(selection.airnode).signMessage(ethers.getBytes(digest));
      await (registry.connect(primarySigner) as any).commitEpoch(epochId,{timestamp,data:ethers.hexlify(data),signature});
      return epochId;
    };
    /// Anything an earlier scenario left in the follower's queue is served or expired first: these measurements are
    /// about one request and a silent primary, not about a backlog that would make the follower join on age alone.
    const drainFollowerQueue=async()=>{
      // One block per step, like the rest of these scenarios: mining a burst of blocks would age the epoch packets
      // past their attestation window and change what the keepers can do at all.
      for(let step=0;step<120;step++){
        if(rows(`SELECT COUNT(*) n FROM jobs WHERE state IN ('pending','prepared') AND deadline>${await chainNow()}`,followerDb)[0].n===0)return;
        await networkHelpers.mine(1);await ticks(1);
      }
      throw Error("The follower's queue did not drain before the measurement");
    };
    /// A request inside an epoch whose packet is published, created after the primary has been quiet long enough
    /// for the liveness window to be measurable from the moment the request appears.
    const requestInPublishedEpoch=async(label:string,quietSeconds:number)=>{
      await drainFollowerQueue();
      for(let attempt=0;attempt<4;attempt++){
        const epochId=await publishEpochAsPrimary();
        await idleBlocks(quietSeconds);
        const id=await request(`${label}-${attempt}`);
        if((await rng.getRequest(id)).epochId===epochId)return id;
      }
      throw Error(`Could not create ${label} inside a published epoch`);
    };
    // Without the owner's approval the follower runs without sending, which `run --once` reports as unhealthy.
    const refused=await start(followerEnv,["run","--once"],followerDb).exit;
    assert.equal(refused.code,1);assert.match(refused.err,/not an allowed backup committer/);
    await registry.setBackupCommitter(followerWallet.address,true);
    assert.equal(await registry.isBackupCommitter(followerWallet.address),true);

    // Primary down: the follower waits its delay after the fallback window opens, then publishes and serves.
    const takeoverEpoch=await registry.epochForBlock(await ethers.provider.getBlockNumber())+1n;
    await mineTo(await registry.epochStart(takeoverEpoch));
    const rawBeforeTakeover=raw.length;
    let follower=start(followerEnv,["run"],followerDb);
    const takeover=await request("follower-takeover");
    await until(async()=>rows(`SELECT api FROM epoch_work WHERE epoch=${takeoverEpoch}`,followerDb).some(r=>r.api!==null),"follower epoch packet missing");
    await ageTo(takeover,delay-8);await ticks();
    assert.equal((await registry.getEpoch(takeoverEpoch)).epochHash,ethers.ZeroHash,"The follower published before its delay");
    assert.equal(sentBy(followerAddress,rawBeforeTakeover).length,0,"The follower sent before its delay");
    await ageTo(takeover,delay+2);
    await until(async()=>{await networkHelpers.mine(1);await advanceTarget(takeover);return (await rng.getRequest(takeover)).fulfilled;},"The follower did not serve while the primary was down",25000);
    await until(async()=>(await rng.getRequest(takeover)).delivered,"The follower's callback was not delivered",10000);
    const followerTxs=sentBy(followerAddress,rawBeforeTakeover);
    assert.ok(followerTxs.some(tx=>tx.data.startsWith(registry.interface.getFunction("commitEpoch")!.selector)),"The follower did not publish the epoch");
    assert.ok(followerTxs.some(tx=>tx.data.startsWith(singleSelector)||tx.data.startsWith(batchSelector)),"The follower did not fulfill the request");
    assert.equal(sentBy(primaryAddress,rawBeforeTakeover).length,0);
    const [paid]=await rng.queryFilter(rng.filters.KeeperFeePaid(takeover));
    assert.equal(paid.args.keeper,followerWallet.address,"The keeper share did not go to the follower that served the request");
    assert.equal(followerStatus().role,"follower");

    // Diagnostics: a failed assertion in these scenarios prints the follower's own log before it stops.
    const withFollowerLog=async<T>(step:string,run:()=>Promise<T>):Promise<T>=>{
      try{return await run();}
      catch(error){
        follower.child.kill();
        const {err}=await follower.exit;
        console.error(`Follower log for ${step}: `+err.split(String.fromCharCode(10)).slice(-40).join(String.fromCharCode(10)));
        throw error;
      }
    };
    // A working primary with a young queue is left alone: the follower does not join before its delay, whatever
    // the primary is doing, and it does not declare a primary that keeps landing transactions dead.
    await publishEpochAsPrimary();
    const rawBeforeYoung=raw.length,young=await request("primary-working");
    for(const at of [4,8,12,16]){
      await primaryActs();
      await ageTo(young,at);await ticks();
      assert.equal((await rng.getRequest(young)).fulfilled,false,`Request served at ${at}s while the primary was working`);
      assert.equal(sentBy(followerAddress,rawBeforeYoung).length,0,`The follower sent at ${at}s while the primary was working`);
    }
    assert.equal(followerStatus().primary_alive,true,"The follower called a working primary dead");
    // Past the join delay the follower does take the request, however healthy the primary looks.
    await ageTo(young,delay+2);
    await withFollowerLog("join after the delay",()=>until(async()=>{await networkHelpers.mine(1);await advanceTarget(young);return (await rng.getRequest(young)).fulfilled;},"The follower did not join after its delay",25000));
    const youngAge=await servedAge(young);
    assert.ok(youngAge>=delay,`The follower served after ${youngAge}s, before its join delay`);

    // An idle primary is not dead: with nothing waiting, silence alone never makes the follower take over.
    await idleBlocks(liveness*3);
    assert.equal(followerStatus().primary_alive,true,"The follower called an idle primary dead");
    // With work waiting and no committer transaction, the primary is dead after the liveness window and the
    // follower joins at once, well before the join delay would have fired.
    const rawBeforeDead=raw.length,orphan=await requestInPublishedEpoch("primary-dead",liveness+2);
    // The liveness verdict is read from the follower's own log below, not from its status after the fact: once the
    // request is served nothing is waiting any more, so the same primary counts as alive again within a tick.
    const orphanAge=await withFollowerLog("takeover from a dead primary",async()=>{
      await ageTo(orphan,liveness+3);
      await until(async()=>{await networkHelpers.mine(1);await advanceTarget(orphan);return (await rng.getRequest(orphan)).fulfilled;},"The follower did not take over from a dead primary",25000);
      const age=await servedAge(orphan);
      assert.ok(age<delay,`The follower waited ${age}s for a dead primary's request`);
      return age;
    });

    assert.equal(sentBy(primaryAddress,rawBeforeDead).length,0);

    // A restarted follower has not observed the committer yet, so it cannot declare death during that window.
    follower.child.kill();const takeoverLog=(await follower.exit).err;
    await writeFile(join(dir,"follower-takeover.log"),takeoverLog); // Kept for diagnosis of the join-rule scenarios.
    assert.match(takeoverLog,/Follower published epoch/);assert.match(takeoverLog,/Follower served request/);
    assert.match(takeoverLog,/"primary_dead":true/,`The follower never recorded the dead primary; its log is ${join(dir,"follower-takeover.log")}`);
    assert.doesNotMatch(takeoverLog,/Keeper tick failed/);
    const rawBeforeRestart=raw.length,restartRequest=await requestInPublishedEpoch("follower-restart",liveness+2);
    await ageTo(restartRequest,3);
    follower=start(followerEnv,["run"],followerDb);
    await idleBlocks(6);
    assert.equal((await rng.getRequest(restartRequest)).fulfilled,false,"A restarted follower took over before observing the primary");
    assert.equal(sentBy(followerAddress,rawBeforeRestart).length,0);
    await ageTo(restartRequest,liveness+5);
    await until(async()=>{await networkHelpers.mine(1);await advanceTarget(restartRequest);return (await rng.getRequest(restartRequest)).fulfilled;},"The follower did not take over after observing the silent primary",25000);
    const restartAge=await servedAge(restartRequest);
    assert.ok(restartAge>=liveness&&restartAge<delay,`The restarted follower served after ${restartAge}s`);

    // The primary dies mid-burst: the follower serves every request it had not, inside their deadlines.
    const rawBeforeBurst=raw.length;
    const primaryDaemon=start(batchEnv,["run"],primaryDb);
    const servedByPrimary=[await request("burst-a-0"),await request("burst-a-1")];
    for(const id of servedByPrimary)await until(async()=>{await advanceTarget(id);return (await rng.getRequest(id)).fulfilled;},`The primary did not serve ${id}`,30000);
    holdSendsFrom=primaryAddress; // From here the primary can no longer land a transaction: its nonce stops.
    const orphaned=[await request("burst-b-0"),await request("burst-b-1"),await request("burst-b-2")];
    // The epoch of the orphaned requests is published, so this measures the takeover itself and not the wait for a
    // fallback window. The committer publishes it, which is also the last transaction its nonce shows.
    await publishEpochAsPrimary();
    const death=await chainNow();
    await ticks(6);
    primaryDaemon.child.kill();const primaryLog=(await primaryDaemon.exit).err;
    holdSendsFrom="";
    const pending=async()=>(await Promise.all(orphaned.map(async id=>(await rng.getRequest(id)).fulfilled))).filter(fulfilled=>!fulfilled).length;
    // One chain second per step, as a live chain runs it: the takeover below is measured in chain seconds, so
    // fast-forwarding here would count the follower's wall-clock reaction as if it had taken chain time.
    await withFollowerLog("takeover after the primary died mid-burst",async()=>{
      // Chain time follows wall time on this network, so the loop waits in wall time too: the takeover below is
      // measured in chain seconds and must not be shortened by fast-forwarding the clock the follower reads.
      await until(async()=>{
        for(const id of orphaned)await advanceTarget(id);
        await networkHelpers.mine(1);
        return await pending()===0;
      },"The follower did not serve what the primary left before their deadlines",45000);
      const followerView=()=>JSON.stringify({jobs:rows("SELECT id,state,deadline FROM jobs ORDER BY CAST(id AS INTEGER) DESC LIMIT 6",followerDb),
        epochs:rows("SELECT epoch,state,fallback,sources,start,last_error,api IS NOT NULL AS packet FROM epoch_work ORDER BY epoch DESC LIMIT 3",followerDb),
        demand:rows("SELECT job,epoch FROM epoch_demand ORDER BY CAST(job AS INTEGER) DESC LIMIT 6",followerDb),
        status:rows("SELECT value FROM meta WHERE key='health:status'",followerDb)[0]?.value});
      for(const id of orphaned){
        const r=await rng.getRequest(id);
        assert.equal(r.fulfilled,true,`Request ${id} was not served after the primary died (epoch ${r.epochId}, deadline ${r.deadline}, now ${await chainNow()}): ${followerView()}`);
        assert.equal(r.refunded,false,`Request ${id} was refunded after the primary died`);
      }
    });
    const takeoverSeconds=Math.min(...await Promise.all(orphaned.map(async id=>{
      const [event]=await rng.queryFilter(rng.filters.RandomnessFulfilled(id));
      assert.equal(event.args.submitter,followerWallet.address,`Request ${id} was not served by the follower`);
      return Number((await ethers.provider.getBlock(event.blockNumber))!.timestamp)-death;
    })));
    assert.ok(takeoverSeconds<=liveness+delay,`The follower took over ${takeoverSeconds}s after the primary stopped`);
    assert.ok(sentBy(primaryAddress,rawBeforeBurst).length>0,"The primary served nothing before it died");
    assert.doesNotMatch(primaryLog,/Keeper tick failed/);

    // Both up in a new epoch: the primary publishes and serves, neither tick fails, and the follower stays quiet.
    const bothEpoch=await registry.epochForBlock(await ethers.provider.getBlockNumber())+1n;await mineTo(await registry.epochStart(bothEpoch));
    const rawBeforeBoth=raw.length;
    const bothPrimary=start(batchEnv,["run"],primaryDb);
    const both:bigint[]=[];
    try {
      for(let n=0;n<4;n++){both.push(await request(`both-up-${n}`));await new Promise(r=>setTimeout(r,200));}
      for(const id of both){
        await until(async()=>{await advanceTarget(id);return (await rng.getRequest(id)).fulfilled;},`Request ${id} was not served with both keepers up`,30000);
      }
      await until(async()=>rows("SELECT COUNT(*) n FROM txs WHERE state IN ('signed','submitted')",primaryDb)[0].n===0&&rows("SELECT COUNT(*) n FROM txs WHERE state IN ('signed','submitted')",followerDb)[0].n===0,"Receipts were not reconciled with both keepers up",20000);
    } finally {bothPrimary.child.kill();follower.child.kill();}
    const [primaryExit,followerExit]=await Promise.all([bothPrimary.exit,follower.exit]);
    for(const log of [primaryExit.err,followerExit.err])assert.doesNotMatch(log,/Keeper tick failed/);
    const primaryShare=sentBy(primaryAddress,rawBeforeBoth).length,followerShare=sentBy(followerAddress,rawBeforeBoth).length;
    assert.ok(primaryShare>0,"The primary did not serve with both keepers up");
    assert.equal(followerShare,0,`The follower sent ${followerShare} transactions while the primary was healthy`);
    for(const id of both)assert.equal((await rng.getRequest(id)).delivered,true);
    // Losing the approval disables sending without stopping the keeper: it keeps running and reconciling, reports
    // the lost authorization as a health fault, and serves nothing. The operator-facing `run --once` still refuses.
    await registry.setBackupCommitter(followerWallet.address,false);
    const rawBeforeRemoval=raw.length,afterRemoval=await request("after-removal");
    const revoked=start(followerEnv,["run"],followerDb);
    await until(async()=>followerStatus().faults?.some((fault:string)=>fault.startsWith("wallet_unauthorized")),"The follower did not report the lost authorization",20000);
    await ageTo(afterRemoval,delay+5);
    assert.equal((await rng.getRequest(afterRemoval)).fulfilled,false,"A removed follower served a request");
    assert.equal(sentBy(followerAddress,rawBeforeRemoval).length,0,"A removed follower sent a transaction");
    revoked.child.kill();
    const revokedLog=(await revoked.exit).err;
    assert.doesNotMatch(revokedLog,/Keeper tick failed/,"Losing authorization failed the keeper tick");
    assert.match(revokedLog,/sending disabled, reconciliation continues/);
    const removed=await start(followerEnv,["run","--once"],followerDb).exit;
    assert.equal(removed.code,1);assert.match(removed.err,/not an allowed backup committer/);
    // The primary serves the request the revoked follower left, so the scenario ends with an empty queue.
    await registry.setBackupCommitter(followerWallet.address,true);
    const drain=start(batchEnv,["run"],primaryDb);
    try{await until(async()=>{await advanceTarget(afterRemoval);return (await rng.getRequest(afterRemoval)).fulfilled;},"The primary did not serve the request left by the revoked follower",30000);}
    finally{drain.child.kill();await drain.exit;}
    console.log(JSON.stringify({primaryAndFollower:{refusedUntilApproved:true,takeoverPublishedAndServed:true,keeperShareToServingWallet:true,
      workingPrimaryNotJoinedEarly:{waitedPast:16,servedAt:youngAge},idlePrimaryNotDead:true,deadPrimary:{servedAt:orphanAge},restartedFollower:{servedAt:restartAge},
      primaryDiedMidBurst:{orphaned:orphaned.length,takeoverSeconds},bothUp:{requests:both.length,primaryTransactions:primaryShare,followerTransactions:followerShare},refusedAfterRemoval:true}}));
  }
  console.log(JSON.stringify({hungPrimaryFailover:true,unusableFirstEndpointFailover:true,epochSourceFallback:true,boundedPreparation:true,recoveryAfterFourApiFailures:true,feeBudgetDeferral:true,blockedEpochDemandAlarm:true,
    batchedFulfillment:{members:batchIds.length,oneTransaction:true,thirdPartyMemberSkipped:true,corruptMemberFallsBackToSingles:true,crashRestartIdenticalBytes:true,batchMaxOneIsSinglePath:true}}));
  assert.deepEqual(unexpectedApiBodies,[]);assert.ok(CATALOG.every(recipe=>apiBodiesByRecipe[recipe]>0),"The keeper did not request every catalog recipe");
  // Epoch 1 was published under the initial catalog and later epochs under the scheduled one.
  assert.equal((await registry.getEpoch(1)).catalogHash,await registry.catalogHash());
  assert.equal((await registry.getEpoch(12)).catalogHash,(await registry.catalogAt(12)).hash);assert.notEqual((await registry.getEpoch(12)).catalogHash,await registry.catalogHash());
  console.log(JSON.stringify({passed:true,apiBodiesByRecipe,proxyUpgradePins:true,warmRpcLatency,outageRecovery,idleNoTransactions:true,firstDemandFutureBlock:true,sharedSnapshotBatch:true,drainedMigration:true,apiCounts,rpcCounts:counts,estimateRejects,backgroundApiDoesNotBlockRequests:true,stalledEpochEstimateDoesNotStarveRequests:true,immutableEpochRestart:true,singleNonceLane:true,expiredDemandMaintenanceCancelled:true,firstPublicationAcrossBoundary:true,fixtureDirectory:dir},null,2));
} finally {rpcDelayMs=0;holdApi=false;releaseApi?.();holdProofEstimate=false;releaseProofEstimate?.();for(const child of children)child.kill();server.closeAllConnections();server.close();}
