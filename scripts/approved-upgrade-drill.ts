// Approved implementation upgrade drill: real keeper daemons under a restart-on-exit supervisor, on a local chain that
// mines a block every 500 ms. Every coordinator proxy starts on the implementation live on Arc (the de5f82e runtime
// fixture, at its live address) and is upgraded in place to this revision's D20VRFCoordinator, deployed through the
// deterministic CREATE2 factory with its planned salt. Each keeper carries the pins of a production keeper: the live
// implementation's runtime hash as EXPECTED_IMPLEMENTATION_CODE_HASH and the new one's as
// APPROVED_NEXT_IMPLEMENTATION_CODE_HASH.
//
//   npx hardhat run scripts/approved-upgrade-drill.ts
//   PREVIOUS_KEEPER_BINARY=<a keeper build without the setting> npx hardhat run scripts/approved-upgrade-drill.ts
//   DRILL_EXPLORER=1 npx hardhat run scripts/approved-upgrade-drill.ts
//
// preflight   The upgrade lands after the keeper estimated a fulfillment and before it signs: it signs nothing, exits
//             with status 75, and the supervisor's restart serves the pending requests and new ones in time.
// unapproved  The same proxy then moves to an implementation nobody approved: ticks fail as they always have, the
//             keeper exits with status 1 after MAX_TICK_FAILURES, nothing is signed after the move, startup refuses.
// in-flight   The upgrade lands after a batch sized for the live implementation was signed and broadcast, before it is
//             mined: the batch runs on the new implementation, the keeper exits with status 75 at its next check, and
//             the restarted keeper settles every member.
// pins        Startup on the new implementation passes only with the approval or a reviewed pin, and refuses a
//             placeholder approval. With PREVIOUS_KEEPER_BINARY, a keeper build that predates the setting starts and
//             serves with the approval lines present, as after a health-gated update falls back to it.
//
// The supervisor restarts an exited keeper after 100 ms, as Docker's restart policy first does. With DRILL_EXPLORER=1
// the preflight coordinator is also indexed by the local explorer runner (keeper/examples/explorer_local.rs), once with
// the identity the keeper started with before the upgrade and once with the one it restarted with, into a disposable
// PostgreSQL on 127.0.0.1:55439 (user postgres, password public-local-test, database explorer_test, as in CI), and
// read back with psql.
import {createServer} from "node:http";
import {spawn,execFileSync} from "node:child_process";
import {mkdir,mkdtemp,readFile,writeFile} from "node:fs/promises";
import {join,resolve} from "node:path";
import {once} from "node:events";
import {DatabaseSync} from "node:sqlite";
import assert from "node:assert/strict";
import {network} from "hardhat";
import {deployProxy,implementationAddress,implementationCodeHash} from "../test/helpers/proxy.ts";
import {TEST_SECRET,publicKey} from "../test/helpers/proof.ts";
import {epochFixtureData} from "../test/helpers/epoch.ts";
import {canonicalApiRequest} from "../src/sources.ts";
import {buildKeeper} from "./lib/keeper-binary.ts";
import {loadChain} from "./lib/chains.ts";
import {initCode} from "./lib/deployment.ts";

/// keeper/src/proxy.rs APPROVED_UPGRADE_EXIT.
const APPROVED_UPGRADE_EXIT=75;
/// The planned CREATE2 salt of this coordinator implementation.
const NEXT_SALT="0x000000000000000000000000000000000000000000000000468ca50000000000";
/// Runtime code of the deterministic deployment proxy; its hash is chains.json create2.codeHash.
const FACTORY_CODE="0x7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe03601600081602082378035828234f58015156039578182fd5b8082525050506014600cf3";
const FEE=123n,CALLBACK_GAS=100_000n,BLOCK_MS=500;

const {ethers,provider}=await network.create({network:"loadSim"});
assert.equal((await ethers.provider.getNetwork()).chainId,31337n);
const live=JSON.parse(await readFile(new URL("../test/fixtures/coordinator-deployed-de5f82e.json",import.meta.url),"utf8"));
const chain=await loadChain("arc-mainnet");
const binary=process.env.KEEPER_BINARY?resolve(process.env.KEEPER_BINARY):await buildKeeper("debug");
const previousBinary=process.env.PREVIOUS_KEEPER_BINARY?resolve(process.env.PREVIOUS_KEEPER_BINARY):undefined;
await mkdir(".research",{recursive:true});
const dir=await mkdtemp(resolve(".research/approved-upgrade-drill-"));
const [owner,player]=await ethers.getSigners();
const keeperWallet=ethers.HDNodeWallet.fromPhrase("test test test test test test test test test test test junk",undefined,"m/44'/60'/0'/0/2");
const signers=["11","22","33","44"].map(value=>new ethers.Wallet("0x"+value.repeat(32)));
const vrfPath=join(dir,"vrf.key"),txPath=join(dir,"tx.key");
await writeFile(vrfPath,ethers.toBeHex(TEST_SECRET,32),{mode:0o600});
await writeFile(txPath,keeperWallet.privateKey,{mode:0o600});
const sleep=(ms:number)=>new Promise(resolve=>setTimeout(resolve,ms));
async function until(condition:()=>Promise<boolean>|boolean,message:string,ms:number,interval=250){
  const end=Date.now()+ms;
  while(Date.now()<end){if(await condition())return;await sleep(interval);}
  throw new Error(message);
}

// A deployer nonce of its own gives every run new registry and proxy addresses, so a reused local explorer database
// never holds this run's deployments from an earlier one.
await provider.request({method:"hardhat_setNonce",params:[owner.address,ethers.toQuantity(1_000+Math.floor(Math.random()*1_000_000))]});
// The registry, the live implementation at its live address (its UUPS self address is embedded in the runtime) and
// the new implementation at its CREATE2 address.
const registry:any=await deployProxy(ethers,"EpochEntropy",[signers.map(s=>s.address),owner.address,keeperWallet.address]);
await provider.request({method:"hardhat_setCode",params:[live.implementation,live.deployedBytecode]});
assert.equal(ethers.keccak256(await ethers.provider.getCode(live.implementation)),live.runtimeCodeHash);
if(await ethers.provider.getCode(chain.create2.factory)==="0x")
  await provider.request({method:"hardhat_setCode",params:[chain.create2.factory,FACTORY_CODE]});
assert.equal(ethers.keccak256(await ethers.provider.getCode(chain.create2.factory)),chain.create2.codeHash);
const nextInitCode=await initCode("D20VRFCoordinator",[]);
const nextImplementation=ethers.getCreate2Address(chain.create2.factory,NEXT_SALT,ethers.keccak256(nextInitCode));
await (await owner.sendTransaction({to:chain.create2.factory,data:ethers.concat([NEXT_SALT,nextInitCode])})).wait();
const nextRuntimeHash=ethers.keccak256(await ethers.provider.getCode(nextImplementation));
assert.notEqual(nextRuntimeHash,ethers.keccak256("0x"),"CREATE2 deployment of the new implementation failed");
const registryImplementationHash=await implementationCodeHash(ethers,registry);
const coordinatorInterface=(await ethers.getContractFactory("D20VRFCoordinator")).interface;
const batchSelector=coordinatorInterface.getFunction("fulfillRandomnessBatch")!.selector;
const singleSelector=coordinatorInterface.getFunction("fulfillRandomness")!.selector;

interface Service {name:string;rng:any;consumer:any;address:string;proxyCodeHash:string;protocolHash:string}
/// A coordinator proxy on the live implementation, initialized like production, with a test consumer.
async function liveCoordinator(name:string):Promise<Service>{
  const init=coordinatorInterface.encodeFunctionData("initialize",[publicKey(),owner.address,owner.address,FEE,1,await registry.getAddress(),5000]);
  const proxy=await ethers.deployContract("D20Proxy",[live.implementation,init]);await proxy.waitForDeployment();
  const rng:any=await ethers.getContractAt("D20VRFCoordinator",await proxy.getAddress());
  await (await rng.setPricing(FEE,0,300000)).wait(); // Flat fee: requests send an exact value.
  const consumer:any=await ethers.deployContract("TestConsumer",[await rng.getAddress()]);await consumer.waitForDeployment();
  const address=(await rng.getAddress()).toLowerCase();
  return {name,rng,consumer,address,proxyCodeHash:ethers.keccak256(await ethers.provider.getCode(address)),protocolHash:await rng.protocolConfigurationHash()};
}
const services={preflight:await liveCoordinator("preflight"),inflight:await liveCoordinator("in-flight"),previous:await liveCoordinator("previous")};

// One JSON-RPC endpoint for every keeper and the local AirnodeHub fixture gateway. Hooks: hold the answer to one
// fulfillment estimate for a coordinator, and run an action before a coordinator's batch is forwarded to the chain.
const sends:{to:string;from:string;hash:string;selector:string;at:number}[]=[];
let holdEstimate:{coordinator:string;entered:()=>void;released:Promise<void>}|undefined;
let beforeBatch:{coordinator:string;action:(hash:string)=>Promise<void>}|undefined;
async function forward(call:any):Promise<object>{
  if(call.method==="eth_sendRawTransaction"){
    const tx=ethers.Transaction.from(call.params[0]);
    const to=(tx.to??"").toLowerCase(),selector=tx.data.slice(0,10);
    sends.push({to,from:(tx.from??"").toLowerCase(),hash:tx.hash!,selector,at:Date.now()});
    if(beforeBatch&&to===beforeBatch.coordinator&&selector===batchSelector){const hook=beforeBatch;beforeBatch=undefined;await hook.action(tx.hash!);}
  }
  let answer:object;
  try {answer={jsonrpc:"2.0",id:call.id,result:await provider.request({method:call.method,params:call.params})};}
  catch(error){answer={jsonrpc:"2.0",id:call.id,error:{code:-32000,message:String(error)}};}
  const data=String(call.params?.[0]?.data??"");
  if(call.method==="eth_estimateGas"&&holdEstimate&&String(call.params?.[0]?.to??"").toLowerCase()===holdEstimate.coordinator
    &&(data.startsWith(batchSelector)||data.startsWith(singleSelector))){
    // The estimate ran against the chain as it was; its answer reaches the keeper only after the release.
    const hold=holdEstimate;holdEstimate=undefined;hold.entered();await hold.released;
  }
  return answer;
}
const server=createServer(async(req,res)=>{
  const reply=(value:unknown,status=200)=>{res.writeHead(status,{"Content-Type":"application/json"});res.end(JSON.stringify(value));};
  try {
    const chunks:Buffer[]=[];for await(const chunk of req)chunks.push(Buffer.from(chunk));
    const body=JSON.parse(Buffer.concat(chunks).toString());
    if(req.url?.startsWith("/api/")){
      const recipe=Number(req.url.split("/").at(-1));
      const head=await provider.request({method:"eth_getBlockByNumber",params:["latest",false]}) as {number:string;timestamp:string};
      const epoch=await registry.nextEpochToPrepare(BigInt(head.number));
      let selection;
      const sources=Number(await registry.sourceCountAt(epoch));
      for(let attempt=0;attempt<sources&&!selection;attempt++){
        const s=await registry.getEpochFallbackSelection(epoch,attempt);
        if(Number(s.recipe)===recipe)selection=s;
      }
      if(!selection){reply({error:"not a source of this epoch"},400);return;}
      const canonical=canonicalApiRequest(body);
      assert.equal(canonical,selection.canonicalRequest);
      const data=epochFixtureData(recipe),requestHash=ethers.id(canonical),timestamp=BigInt(head.timestamp);
      const digest=ethers.keccak256(ethers.solidityPacked(["bytes32","uint256","bytes"],[requestHash,timestamp,ethers.toUtf8Bytes(JSON.stringify(data))]));
      const airnode=signers.find(s=>s.address===selection.airnode)!;
      reply({airnode:airnode.address,requestHash,timestamp:String(timestamp),data,signature:await airnode.signMessage(ethers.getBytes(digest))});return;
    }
    if(Array.isArray(body)){reply(await Promise.all(body.map(forward)));return;}
    reply(await forward(body));
  } catch(error){reply({error:String(error)},500);}
});
server.listen(0,"127.0.0.1");await once(server,"listening");
const address=server.address();assert.ok(address&&typeof address!=="string");
const base=`http://127.0.0.1:${address.port}`;

function keeperEnv(service:Service,overrides:NodeJS.ProcessEnv={}):NodeJS.ProcessEnv {
  return {...process.env,NEON_DB:undefined,TELEGRAM_BOT_TOKEN:undefined,TELEGRAM_CHAT_ID:undefined,DISCORD_BOT_TOKEN:undefined,
    DISCORD_PROTOCOL_CHANNEL_ID:undefined,HEALTH_API_URL:undefined,HEALTH_API_KEY:undefined,WS_URLS:undefined,
    CHAIN_ID:"31337",RPC_URLS:`${base}/rpc`,COORDINATOR_ADDRESS:service.address,TX_KEY_FILE:txPath,VRF_KEY_FILE:vrfPath,
    KEEPER_DB:join(dir,`${service.name}.sqlite`),TEST_LOCK_DIR:join(dir,`${service.name}-locks`),TEST_API_BASE:`${base}/api`,
    EXPECTED_CODE_HASH:service.proxyCodeHash,EXPECTED_PROTOCOL_HASH:service.protocolHash,
    EXPECTED_IMPLEMENTATION_CODE_HASH:live.runtimeCodeHash,EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH:registryImplementationHash,
    APPROVED_NEXT_IMPLEMENTATION_CODE_HASH:nextRuntimeHash,APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH:undefined,
    SEND_TRANSACTIONS:"true",POLL_MS:"500",FULFILL_BATCH_MAX:"16",MAX_GAS:"6000000",MAX_FEE_PER_GAS_WEI:"100000000000",
    CANCEL_MAX_FEE_PER_GAS_WEI:"150000000000",MAX_TX_COST_WEI:"4000000000000000000",FEE_COVERAGE_BPS:"0",
    RUST_LOG:"d20dao_keeper=info",...overrides};
}
interface Run {started:number;ready?:number;exited?:number;code?:number|null;log:string}
/// Restarts an exited keeper after 100 ms, as Docker's unless-stopped policy first does, until told to stop or until
/// `giveUp` says so. `onExit` sees each exit before the restart.
class Supervisor {
  runs:Run[]=[];
  private stopped=false;
  private child?:ReturnType<typeof spawn>;
  private finished:Promise<void>;
  constructor(readonly env:NodeJS.ProcessEnv,readonly program=binary,readonly giveUp:(runs:Run[])=>boolean=()=>false,
    readonly onExit?:(run:Run)=>void){this.finished=this.loop();}
  private async loop(){
    while(!this.stopped){
      const run:Run={started:Date.now(),log:""};this.runs.push(run);
      const child=spawn(this.program,["run"],{env:this.env,windowsHide:true,stdio:["ignore","pipe","pipe"]});this.child=child;
      child.stdout!.on("data",()=>{});
      child.stderr!.on("data",data=>{run.log+=data;if(!run.ready&&run.log.includes("Outbound-only keeper started"))run.ready=Date.now();});
      const [code]=await once(child,"exit") as [number|null];
      run.exited=Date.now();run.code=code;
      if(this.stopped)break;
      this.onExit?.(run);
      if(this.giveUp(this.runs))break;
      await sleep(100);
    }
  }
  get current(){return this.runs.at(-1)!;}
  async stop(){this.stopped=true;this.child?.kill();await this.finished;}
}
/// The structured log lines of a run whose message matches, with their fields.
function logs(run:Run,pattern:RegExp){
  return run.log.split(/\r?\n/).flatMap(line=>{
    try {const entry=JSON.parse(line);return pattern.test(String(entry.fields?.message??""))?[{level:entry.level,...entry.fields}]:[];}
    catch {return pattern.test(line)?[{level:"text",message:line}]:[];}
  });
}
function journal(service:Service,sql:string,file=`${service.name}.sqlite`){
  const db=new DatabaseSync(join(dir,file),{readOnly:true});
  try{return db.prepare(sql).all();}finally{db.close();}
}
/// The journal as the interrupted tick left it: jobs by state and the transactions not yet resolved.
function journalAtExit(service:Service){
  return {jobs:Object.fromEntries(journal(service,"SELECT state,COUNT(*) n FROM jobs GROUP BY state").map(r=>[String(r.state),Number(r.n)])),
    unresolvedTransactions:journal(service,"SELECT kind,state,nonce FROM txs WHERE state IN ('signed','submitted')")};
}
async function request(service:Service,label:string){
  await (await service.consumer.connect(player).request(ethers.id(label),CALLBACK_GAS,player.address,{value:FEE})).wait();
  return await service.consumer.lastRequestId() as bigint;
}
/// `count` requests sent together with explicit nonces, so they land in one or two blocks.
async function burst(service:Service,count:number,label:string){
  const start=await player.getNonce("pending"),first=await service.rng.nextRequestId() as bigint;
  const sent=[];
  for(let n=0;n<count;n++)sent.push(service.consumer.connect(player).request(ethers.id(`${label}-${n}`),CALLBACK_GAS,player.address,{value:FEE,nonce:start+n}));
  await Promise.all((await Promise.all(sent)).map((tx:any)=>tx.wait()));
  return Array.from({length:count},(_,n)=>first+BigInt(n));
}
/// Chain facts per request: fulfilled, delivered, not refunded, fulfilled exactly once, and how long it took.
async function settled(service:Service,ids:bigint[]){
  const out=[];
  for(const id of ids){
    const r=await service.rng.getRequest(id);
    const events=await service.rng.queryFilter(service.rng.filters.RandomnessFulfilled(id));
    let secondsToServe:number|undefined;
    if(events.length===1){
      const [requested]=await service.rng.queryFilter(service.rng.filters.RandomnessRequested(id));
      const [at,done]=await Promise.all([ethers.provider.getBlock(requested.blockNumber),ethers.provider.getBlock(events[0].blockNumber)]);
      secondsToServe=done!.timestamp-at!.timestamp;
    }
    out.push({id:String(id),fulfilled:r.fulfilled as boolean,delivered:r.delivered as boolean,refunded:r.refunded as boolean,fulfillments:events.length,secondsToServe});
  }
  return out;
}
const served=async(service:Service,ids:bigint[])=>(await settled(service,ids)).every(r=>r.fulfilled);
function assertServedInTime(rows:Awaited<ReturnType<typeof settled>>,what:string){
  for(const r of rows){
    assert.ok(r.fulfilled&&r.delivered&&!r.refunded&&r.fulfillments===1,`${what}: request ${r.id} ${JSON.stringify(r)}`);
    assert.ok((r.secondsToServe??99)<60,`${what}: request ${r.id} took ${r.secondsToServe}s`);
  }
}
async function upgrade(service:Service,to:string){
  const receipt=await (await service.rng.connect(owner).upgradeToAndCall(to,"0x")).wait();
  assert.equal(await implementationAddress(ethers,service.rng),ethers.getAddress(to));
  return {block:receipt.blockNumber as number,at:Date.now()};
}
function psql(sql:string){
  return execFileSync("psql",["-h","127.0.0.1","-p","55439","-U","postgres","-d","explorer_test","-At","-c",sql],
    {env:{...process.env,PGPASSWORD:"public-local-test"},encoding:"utf8",windowsHide:true}).trim();
}
/// A child process run to completion without blocking this process, which serves the chain the child reads.
async function completed(program:string,args:string[]){
  const child=spawn(program,args,{stdio:["ignore","ignore","pipe"],windowsHide:true});
  let err="";child.stderr!.on("data",d=>{err+=d;});
  const [code]=await once(child,"exit") as [number|null];
  assert.equal(code,0,`${program} ${args.join(" ")}: ${err.slice(-1500)}`);
}
const explorerRunner=["--quiet","--locked","--manifest-path","keeper/Cargo.toml","--example","explorer_local"];
/// Index the service's coordinator with the identity its keeper last started with, up to the current head.
async function explorerIndex(service:Service,label:string){
  const pins=JSON.parse(String(journal(service,"SELECT value FROM meta WHERE key='runtime:pins'")[0].value));
  const fixture=join(dir,`explorer-${label}.json`);
  await writeFile(fixture,JSON.stringify({rpc_url:`${base}/rpc`,chain_id:31337,pins}));
  const cursor=()=>Number(psql(`SELECT next_block FROM d20dao_explorer.cursors WHERE chain_id='31337' AND coordinator='${service.address}'`)||0);
  const head=await ethers.provider.getBlockNumber();
  for(let round=0;round<8&&(round===0||cursor()<=head);round++)await completed("cargo",["run",...explorerRunner,"--",fixture]);
  return {implementation:pins.coordinator.implementation,cursor:cursor(),head};
}

const report:Record<string,unknown>={chain:"local EDR 31337, one block every 500 ms",binary,fixtureDirectory:dir,
  implementations:{live:{address:live.implementation,runtimeCodeHash:live.runtimeCodeHash},
    next:{address:nextImplementation,runtimeCodeHash:nextRuntimeHash,initCodeHash:ethers.keccak256(nextInitCode),salt:NEXT_SALT,factory:chain.create2.factory}}};
const supervisors:Supervisor[]=[];
try {
  if(process.env.DRILL_EXPLORER==="1")await completed("cargo",["build",...explorerRunner.filter(arg=>arg!=="--quiet")]);
  // Epoch 1 starts 200 blocks after the registry; reach it at once, then mine in real time.
  const firstEpoch=Number(await registry.firstEpochStart());
  await provider.request({method:"hardhat_mine",params:[ethers.toQuantity(firstEpoch-await ethers.provider.getBlockNumber()),"0x0"]});
  await provider.request({method:"evm_setAutomine",params:[false]});
  await provider.request({method:"evm_setIntervalMining",params:[BLOCK_MS]});

  // preflight: the upgrade lands between the keeper's estimate and its signature.
  {
    const service=services.preflight;
    const exits:Record<string,unknown>[]=[];
    const keeper=new Supervisor(keeperEnv(service),binary,runs=>runs.length>=8,run=>{
      exits.push({code:run.code,journal:run.code===APPROVED_UPGRADE_EXIT?journalAtExit(service):undefined});
    });
    supervisors.push(keeper);
    await until(()=>keeper.current.ready!==undefined,"preflight: the keeper did not start",60_000);
    const warm=[await request(service,"preflight-warm-a"),await request(service,"preflight-warm-b")];
    await until(()=>served(service,warm),"preflight: warm-up requests were not served",60_000);
    const indexedBefore=process.env.DRILL_EXPLORER==="1"?await explorerIndex(service,"before"):undefined;
    let entered!:()=>void,release!:()=>void;
    const estimating=new Promise<void>(resolve=>{entered=resolve;});
    holdEstimate={coordinator:service.address,entered,released:new Promise<void>(resolve=>{release=resolve;})};
    const pending=await burst(service,6,"preflight-burst");
    await Promise.race([estimating,sleep(45_000).then(()=>{throw new Error("preflight: the keeper never estimated the burst");})]);
    const sendsBefore=sends.length;
    const upgraded=await upgrade(service,nextImplementation);
    release();
    await until(()=>keeper.runs.length>=2,"preflight: the keeper did not exit after the approved upgrade",30_000);
    const first=keeper.runs[0];
    const sentBeforeExit=sends.slice(sendsBefore).filter(s=>s.at<first.exited!).length;
    await until(()=>keeper.current.ready!==undefined,"preflight: the restarted keeper did not start",60_000);
    const later=[await request(service,"preflight-after-a"),await request(service,"preflight-after-b"),await request(service,"preflight-after-c")];
    const all=[...warm,...pending,...later];
    await until(()=>served(service,all),"preflight: requests were not served after the restart",90_000);
    const rows=await settled(service,all);
    assertServedInTime(rows,"preflight");
    assert.equal(first.code,APPROVED_UPGRADE_EXIT,`preflight: exit code ${first.code}\n${first.log.slice(-2000)}`);
    assert.equal(sentBeforeExit,0,"preflight: the keeper sent a transaction after the approved upgrade");
    assert.equal(logs(first,/Keeper tick failed/).length,0,"preflight: the approved upgrade failed a tick");
    const observed=logs(first,/Proxy moved to its approved next implementation/),exiting=logs(first,/Keeper exiting for a restart/);
    assert.equal(observed.length,1);assert.equal(exiting.length,1);
    const restarted=logs(keeper.runs[1],/Running on the approved next coordinator implementation/);
    assert.equal(restarted.length,1);
    report.preflight={upgradeBlock:upgraded.block,upgradeToExitMs:first.exited!-upgraded.at,exitToReadyMs:keeper.runs[1].ready!-first.exited!,
      exits,sentAfterUpgradeBeforeExit:sentBeforeExit,failedTicks:0,logs:{observed:observed[0],exiting:exiting[0],restarted:restarted[0]},
      requests:rows.length,burstAtUpgrade:(await settled(service,pending)).map(r=>r.secondsToServe),maxSecondsToServe:Math.max(...rows.map(r=>r.secondsToServe??0))};
    if(indexedBefore){
      const indexedAfter=await explorerIndex(service,"after");
      const where=`chain_id='31337' AND coordinator='${service.address}'`;
      const identities=JSON.parse(psql(`SELECT implementation_pins FROM d20dao_explorer.deployments WHERE ${where}`));
      const misattributed=Number(psql(`SELECT COUNT(*) FROM d20dao_explorer.requests WHERE ${where} AND (
        lower(request_receipt->'implementations'->>'implementation')<>lower(CASE WHEN request_block<${upgraded.block} THEN '${live.implementation}' ELSE '${nextImplementation}' END)
        OR (fulfillment_block IS NOT NULL AND lower(fulfillment_receipt->'implementations'->>'implementation')<>lower(CASE WHEN fulfillment_block<${upgraded.block} THEN '${live.implementation}' ELSE '${nextImplementation}' END)))`));
      const byImplementation=psql(`SELECT lower(fulfillment_receipt->'implementations'->>'implementation'),COUNT(*) FROM d20dao_explorer.requests WHERE ${where} GROUP BY 1 ORDER BY 1`);
      const indexed=Number(psql(`SELECT COUNT(*) FROM d20dao_explorer.requests WHERE ${where} AND fulfillment_block IS NOT NULL`));
      assert.deepEqual(identities.map((p:any)=>String(p.coordinator.implementation).toLowerCase()),[live.implementation.toLowerCase(),nextImplementation.toLowerCase()]);
      assert.equal(misattributed,0,"explorer: a request or fulfillment is attributed to the wrong implementation");
      assert.equal(indexed,all.length,"explorer: not every served request is indexed");
      report.explorer={before:indexedBefore,after:indexedAfter,identities:identities.length,servedRequestsIndexed:indexed,
        fulfillmentsByImplementation:byImplementation.split(/\r?\n/),misattributed};
    }

    // unapproved: the same keeper, now running the approved implementation, sees its proxy move to an unapproved one.
    const probe=await ethers.deployContract("CoordinatorUpgradeProbe");await probe.waitForDeployment();
    const runsBefore=keeper.runs.length,sendsAtMove=sends.length;
    const moved=await upgrade(service,await probe.getAddress());
    const stranded=await request(service,"unapproved-after-move");
    await until(()=>keeper.runs.length>=runsBefore+3,"unapproved: the keeper did not stop and refuse",150_000);
    await keeper.stop();
    const [stopped,refusedA,refusedB]=keeper.runs.slice(runsBefore-1,runsBefore+2);
    const failedTicks=logs(stopped,/Keeper tick failed/);
    assert.equal(stopped.code,1,`unapproved: exit code ${stopped.code}`);
    assert.ok(failedTicks.length>=5,"unapproved: fewer than MAX_TICK_FAILURES failed ticks");
    assert.ok(failedTicks.every(entry=>String((entry as any).error).includes("Proxy implementation changed; review and update pins before restarting")),JSON.stringify(failedTicks));
    assert.equal(logs(stopped,/Proxy moved to its approved next implementation|Keeper exiting for a restart/).length,0,"unapproved: treated as approved");
    assert.equal(logs(stopped,/Keeper exiting after failed work/).length,1);
    for(const refused of [refusedA,refusedB]){
      assert.equal(refused.code,1);assert.equal(refused.ready,undefined);
      assert.match(refused.log,/Implementation code pin mismatch/);
    }
    assert.equal(sends.slice(sendsAtMove).filter(s=>s.from===keeperWallet.address.toLowerCase()).length,0,"unapproved: a transaction was sent after the move");
    assert.equal((await service.rng.getRequest(stranded)).fulfilled,false);
    report.unapproved={moveBlock:moved.block,exitCode:stopped.code,failedTicks:failedTicks.length,moveToExitMs:stopped.exited!-moved.at,
      startupRefusals:[refusedA.code,refusedB.code],startupError:"Implementation code pin mismatch",sentAfterMove:0,requestAfterMoveFulfilled:false};
  }

  // in-flight: a batch sized against the live implementation is signed and broadcast; the upgrade is mined first.
  {
    const service=services.inflight;
    const exits:Record<string,unknown>[]=[];
    const keeper=new Supervisor(keeperEnv(service),binary,runs=>runs.length>=6,run=>{
      exits.push({code:run.code,journal:run.code===APPROVED_UPGRADE_EXIT?journalAtExit(service):undefined});
    });
    supervisors.push(keeper);
    await until(()=>keeper.current.ready!==undefined,"in-flight: the keeper did not start",60_000);
    const warm=[await request(service,"inflight-warm-a"),await request(service,"inflight-warm-b")];
    await until(()=>served(service,warm),"in-flight: warm-up requests were not served",60_000);
    let batch:{hash:string;upgrade:{block:number;at:number}}|undefined;
    beforeBatch={coordinator:service.address,action:async hash=>{batch={hash,upgrade:await upgrade(service,nextImplementation)};}};
    const pending:bigint[]=[];
    for(let round=0;round<4&&!batch;round++){
      pending.push(...await burst(service,6,`inflight-burst-${round}`));
      await until(async()=>batch!==undefined||await served(service,pending),"in-flight: the burst was neither batched nor served",60_000);
    }
    assert.ok(batch,"in-flight: the keeper never sent a batch");
    beforeBatch=undefined;
    // New demand after the move: its first send verifies the pins, whatever became of the batch.
    const later=[await request(service,"inflight-after-a"),await request(service,"inflight-after-b")];
    await until(()=>keeper.runs.length>=2,"in-flight: the keeper did not exit after the approved upgrade",90_000);
    await until(()=>keeper.current.ready!==undefined,"in-flight: the restarted keeper did not start",60_000);
    const all=[...warm,...pending,...later];
    await until(()=>served(service,all),"in-flight: requests were not served after the restart",120_000);
    await keeper.stop();
    const rows=await settled(service,all);
    assertServedInTime(rows,"in-flight");
    const first=keeper.runs[0];
    assert.equal(first.code,APPROVED_UPGRADE_EXIT,`in-flight: exit code ${first.code}\n${first.log.slice(-2000)}`);
    assert.equal(logs(first,/Keeper tick failed/).length,0,"in-flight: the approved upgrade failed a tick");
    const tx=await ethers.provider.getTransaction(batch.hash),receipt=await ethers.provider.getTransactionReceipt(batch.hash);
    let revert:string|undefined;
    if(receipt!.status===0){
      try {await provider.request({method:"eth_call",params:[{from:tx!.from,to:tx!.to,data:tx!.data,gas:ethers.toQuantity(tx!.gasLimit)},ethers.toQuantity(receipt!.blockNumber-1)]});revert="none on replay";}
      catch(error:any){
        const text=`${error?.data??""} ${error?.message??error}`;
        revert=text.includes(coordinatorInterface.getError("InsufficientCallbackGas")!.selector.slice(2))||text.includes("InsufficientCallbackGas")?"InsufficientCallbackGas":text.slice(0,200);
      }
    }
    const members=coordinatorInterface.decodeFunctionData("fulfillRandomnessBatch",tx!.data)[0] as bigint[];
    const membersServedBy=await Promise.all(members.map(async id=>{
      const [event]=await service.rng.queryFilter(service.rng.filters.RandomnessFulfilled(id));
      const servedTx=await ethers.provider.getTransaction(event.transactionHash);
      return servedTx!.data.startsWith(batchSelector)?(event.transactionHash===batch!.hash?"this batch":"a later batch"):"single";
    }));
    report.inflight={upgradeBlock:batch.upgrade.block,batch:{block:receipt!.blockNumber,status:receipt!.status,gasLimit:String(tx!.gasLimit),
      gasUsed:String(receipt!.gasUsed),members:members.length,revert},membersServedBy,exits,exitToReadyMs:keeper.runs[1].ready!-first.exited!,
      requests:rows.length,maxSecondsToServe:Math.max(...rows.map(r=>r.secondsToServe??0))};
  }

  // pins: startup on the new implementation passes only with the approval or a reviewed pin; an older build ignores it.
  {
    const service=services.inflight;
    const runOnce=(label:string,overrides:NodeJS.ProcessEnv)=>new Promise<{code:number|null;log:string}>((resolve,reject)=>{
      const env=keeperEnv(service,{KEEPER_DB:join(dir,`pins-${label}.sqlite`),TEST_LOCK_DIR:join(dir,`pins-${label}-locks`),...overrides});
      const child=spawn(binary,["run","--once"],{env,windowsHide:true,stdio:["ignore","pipe","pipe"]});
      let log="";child.stderr!.on("data",d=>{log+=d;});child.stdout!.on("data",()=>{});
      child.on("error",reject);child.on("exit",code=>resolve({code,log}));
    });
    const approved=await runOnce("approved",{});
    const unapproved=await runOnce("unapproved",{APPROVED_NEXT_IMPLEMENTATION_CODE_HASH:undefined});
    const reviewed=await runOnce("reviewed",{EXPECTED_IMPLEMENTATION_CODE_HASH:nextRuntimeHash,APPROVED_NEXT_IMPLEMENTATION_CODE_HASH:undefined});
    const cleared=await runOnce("cleared",{EXPECTED_IMPLEMENTATION_CODE_HASH:nextRuntimeHash,APPROVED_NEXT_IMPLEMENTATION_CODE_HASH:""});
    const placeholder=await runOnce("placeholder",{APPROVED_NEXT_IMPLEMENTATION_CODE_HASH:ethers.ZeroHash});
    assert.equal(approved.code,0,approved.log.slice(-1500));
    assert.equal(unapproved.code,1);assert.match(unapproved.log,/Implementation code pin mismatch/);
    for(const run of [reviewed,cleared]){assert.equal(run.code,0,run.log.slice(-1500));assert.doesNotMatch(run.log,/Running on the approved next/);}
    assert.equal(placeholder.code,1);assert.match(placeholder.log,/APPROVED_NEXT_IMPLEMENTATION_CODE_HASH must be/);
    const pins:Record<string,unknown>={exitCodes:{withApproval:approved.code,withoutApproval:unapproved.code,reviewedPin:reviewed.code,
      reviewedPinWithEmptyApproval:cleared.code,placeholderApproval:placeholder.code},withoutApprovalError:"Implementation code pin mismatch"};
    if(previousBinary){
      // Before the upgrade, with both approval lines present, as after a health-gated update falls back to this build.
      const target=services.previous;
      const keeper=new Supervisor(keeperEnv(target,{APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH:ethers.keccak256("0x01")}),previousBinary,runs=>runs.length>=2);
      supervisors.push(keeper);
      await until(()=>keeper.current.ready!==undefined||keeper.current.code!==undefined,"previous: the older keeper did not start",60_000);
      assert.notEqual(keeper.current.ready,undefined,keeper.current.log.slice(-1500));
      const ids=[await request(target,"previous-release-a"),await request(target,"previous-release-b")];
      await until(()=>served(target,ids),"previous: the older keeper did not serve",60_000);
      await keeper.stop();
      assertServedInTime(await settled(target,ids),"previous");
      assert.equal(keeper.runs.length,1,"previous: the older keeper exited");
      pins.previousRelease={binary:previousBinary,startedWithBothApprovalLines:true,served:ids.length,exits:0};
    }
    report.pins=pins;
  }
  console.log(JSON.stringify({approvedUpgradeDrill:report},null,2));
} finally {
  for(const keeper of supervisors)await keeper.stop().catch(()=>undefined);
  server.closeAllConnections();server.close();
  await provider.request({method:"evm_setIntervalMining",params:[0]}).catch(()=>undefined);
}
