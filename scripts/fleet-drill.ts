// Fleet drill: real keeper processes, a real-time local chain configured like Arc, and load and failure scenarios
// measured from chain data. Every failover or load scenario passes here before it is repeated on Arc Testnet.
//
//   cargo build --release --manifest-path keeper/Cargo.toml
//   npx hardhat run scripts/fleet-drill.ts                       # every scenario at its documented size
//   DRILL_SCENARIOS=healthy-burst DRILL_REQUESTS=100 DRILL_STEADY_SECONDS=30 npx hardhat run scripts/fleet-drill.ts
//   DRILL_SCENARIOS=rpc-idle,rpc-burst npx hardhat run scripts/fleet-drill.ts             # JSON-RPC calls/s per method
//   KEEPER_BINARY=path/to/d20dao-keeper DRILL_SCENARIOS=rpc-idle,rpc-burst npx hardhat run scripts/fleet-drill.ts
//
// It is a drill, not part of `npm run check`: the default run takes about ten minutes of wall-clock time, because the
// 60-second response window, the follower delays and the 20-block fallback windows are only meaningful in real time.
//
// The rpc-* scenarios are measurements and run only when named. They report, per keeper and per phase (an idle window
// with no request open, then the load from its first request until the last one settles), the JSON-RPC calls per
// second by method and by call site (method, contract function, block tag) and the HTTP requests per second, which
// differ once a keeper batches. DRILL_RPC_STATS=1 adds the same report to any scenario, DRILL_IDLE_SECONDS resizes the
// idle window, and KEEPER_BINARY runs another build under the identical harness instead of building this revision.
// DRILL_WS=1 also gives every keeper a WebSocket endpoint (WS_URLS) that pushes new heads and the service contracts'
// logs, so the same scenarios measure the event-driven cadence; pushes are counted apart from the keeper's own calls.
import {createServer} from "node:http";
import {createHash,randomBytes} from "node:crypto";
import type {Socket} from "node:net";
import {spawn} from "node:child_process";
import {access,mkdir,mkdtemp,writeFile} from "node:fs/promises";
import {join,resolve} from "node:path";
import {once} from "node:events";
import assert from "node:assert/strict";
import {network} from "hardhat";
import {deployProxy} from "../test/helpers/proxy.ts";
import {TEST_SECRET,publicKey} from "../test/helpers/proof.ts";
import {epochFixtureData,signSelection} from "../test/helpers/epoch.ts";
import {canonicalApiRequest} from "../src/sources.ts";
import {BUILTIN_EPOCH_RECIPES,type EpochProvider} from "../src/epoch.ts";
import {buildKeeper} from "./lib/keeper-binary.ts";
import {failoverReport} from "./lib/failover-report.ts";

/// A scenario is data: how load arrives, which fault is injected when, and what the result must satisfy. New
/// scenarios, including later heartbeat work, are entries here rather than new code paths.
interface ScenarioSpec {
  name:string;
  purpose:string;
  /// Service lanes: the primary plus one keeper per follower, each with its own wallet and nonce lane.
  lanes:number;
  load:{kind:"burst";requests:number}|{kind:"steady";perSecond:number;seconds:number}|{kind:"none"};
  fault:{kind:"none"}|{kind:"kill"|"block-rpc"|"restart";target:"primary"|"follower";afterSeconds:number;restoreAfterSeconds?:number};
  thresholds:{refunds:number;duplicateRatio?:number;takeoverSeconds?:number;followerTransactions?:number;minServedByFollower?:number};
  /// Before the load, hold this many seconds with no request open and measure the keepers' RPC rates over them.
  idleSeconds?:number;
  /// A measurement rather than a gate: it always prints the RPC rate report and runs only when DRILL_SCENARIOS names it.
  measurement?:boolean;
}
const SCENARIOS:readonly ScenarioSpec[]=[
  {name:"healthy-burst",purpose:"A burst deeper than the join threshold with the whole fleet healthy: both lanes work it from opposite ends, nobody is refunded and the two fronts do not collide.",
    lanes:2,load:{kind:"burst",requests:500},fault:{kind:"none"},thresholds:{refunds:0,duplicateRatio:0.02,minServedByFollower:1}},
  {name:"primary-killed",purpose:"The primary process dies ten seconds into a burst: the follower must finish the burst inside the deadlines.",
    lanes:2,load:{kind:"burst",requests:300},fault:{kind:"kill",target:"primary",afterSeconds:10,restoreAfterSeconds:180},
    thresholds:{refunds:0,duplicateRatio:0.05,takeoverSeconds:30,minServedByFollower:1}},
  {name:"primary-killed-three-lanes",purpose:"The same kill with two followers: the lanes split the tail by request id, so a third wallet raises throughput without duplicating work.",
    lanes:3,load:{kind:"burst",requests:500},fault:{kind:"kill",target:"primary",afterSeconds:10,restoreAfterSeconds:180},
    thresholds:{refunds:0,duplicateRatio:0.05,takeoverSeconds:30,minServedByFollower:1}},
  {name:"primary-killed-light",purpose:"The primary dies under demand too light and too young for the queue rules, so only the chain-dead rule can bring the follower in, and it must before the deadlines.",
    lanes:2,load:{kind:"steady",perSecond:1,seconds:60},fault:{kind:"kill",target:"primary",afterSeconds:20,restoreAfterSeconds:180},
    thresholds:{refunds:0,duplicateRatio:0.02,takeoverSeconds:25,minServedByFollower:1}},
  {name:"primary-rpc-blocked",purpose:"The primary process is alive but its RPC endpoint stops answering: the follower takes over from chain evidence alone.",
    lanes:2,load:{kind:"burst",requests:300},fault:{kind:"block-rpc",target:"primary",afterSeconds:10,restoreAfterSeconds:90},
    thresholds:{refunds:0,duplicateRatio:0.05,takeoverSeconds:30,minServedByFollower:1}},
  {name:"steady-healthy",purpose:"Light steady demand with the fleet healthy: the queue stays young and shallow, so the follower sends nothing at all.",
    lanes:2,load:{kind:"steady",perSecond:0.5,seconds:180},fault:{kind:"none"},thresholds:{refunds:0,followerTransactions:0}},
  {name:"follower-restart",purpose:"The follower restarts under light load: it must not take over early and must not duplicate work.",
    lanes:2,load:{kind:"steady",perSecond:1,seconds:90},fault:{kind:"restart",target:"follower",afterSeconds:20},
    thresholds:{refunds:0,duplicateRatio:0,followerTransactions:0}},
  {name:"rpc-idle",purpose:"The primary alone with nothing to serve: its JSON-RPC calls per second, per method, while it only follows the chain.",
    lanes:1,load:{kind:"none"},fault:{kind:"none"},idleSeconds:80,measurement:true,thresholds:{refunds:0}},
  // The short idle lead-in starts every burst from the same state: the first demand of an epoch whose snapshot is
  // prepared but unpublished, so the burst includes the epoch publication.
  {name:"rpc-burst",purpose:"The primary alone serves a small burst: its JSON-RPC calls per second, per method, from the first request until the last settles.",
    lanes:1,load:{kind:"burst",requests:50},fault:{kind:"none"},idleSeconds:20,measurement:true,thresholds:{refunds:0}},
] as const;

// Options come from the environment, so the drill runs under `hardhat run` without argument forwarding.
const option=(name:string)=>{const value=process.env[name];return value===undefined||value===""?undefined:value;};
const selected=option("DRILL_SCENARIOS")?.split(",").map(name=>name.trim())
  ??SCENARIOS.filter(scenario=>!scenario.measurement).map(scenario=>scenario.name);
for(const name of selected)if(!SCENARIOS.some(scenario=>scenario.name===name))throw new Error(`Unknown scenario ${name}`);
const scale=(spec:ScenarioSpec):ScenarioSpec=>({...spec,
  load:spec.load.kind==="burst"?{kind:"burst",requests:Number(option("DRILL_REQUESTS")??spec.load.requests)}
    :spec.load.kind==="steady"?{kind:"steady",perSecond:spec.load.perSecond,seconds:Number(option("DRILL_STEADY_SECONDS")??spec.load.seconds)}
    :spec.load,
  idleSeconds:spec.idleSeconds===undefined?undefined:Number(option("DRILL_IDLE_SECONDS")??spec.idleSeconds)});
const plan=SCENARIOS.filter(scenario=>selected.includes(scenario.name)).map(scale);
const rpcStatsEverywhere=option("DRILL_RPC_STATS")==="1";

const {ethers,networkHelpers,provider}=await network.create({network:"loadSim"});
assert.equal((await ethers.provider.getNetwork()).chainId,31337n);
// KEEPER_BINARY (or the older DRILL_BINARY) measures another build under the identical harness; otherwise this revision.
const binaryOption=option("KEEPER_BINARY")??option("DRILL_BINARY");
const binary=binaryOption?resolve(binaryOption):await buildKeeper("release");
await access(binary).catch(()=>{throw new Error(`Keeper binary ${binary} does not exist`);});
await mkdir(".research",{recursive:true});
const dir=await mkdtemp(resolve(".research/fleet-drill-"));
const FEE=123n,CATALOG=[0,1,2,4,5];
const providerOf=(recipe:number)=>BUILTIN_EPOCH_RECIPES[recipe].provider;
const [owner,player]=await ethers.getSigners();
// One wallet per keeper, from the public test mnemonic: the primary is the registry committer, the follower a backup.
const walletAt=(index:number)=>ethers.HDNodeWallet.fromPhrase("test test test test test test test test test test test junk",undefined,`m/44'/60'/0'/0/${index}`);
const FOLLOWER_NAMES=["follower-a","follower-b","follower-c","follower-d"] as const; // The registry allows four backups.
const MAX_LANES=Math.max(...SCENARIOS.map(scenario=>scenario.lanes));
const keeperWallets:Record<string,ReturnType<typeof walletAt>>={primary:walletAt(2)};
for(let lane=0;lane<MAX_LANES-1;lane++)keeperWallets[FOLLOWER_NAMES[lane]]=walletAt(3+lane);
const followerNames=()=>Object.keys(keeperWallets).filter(name=>name!=="primary");
const signers=["11","22","33","44"].map(value=>new ethers.Wallet("0x"+value.repeat(32)));
const providerSigners:Record<EpochProvider,InstanceType<typeof ethers.Wallet>>={hyperliquid:signers[0],drpc:signers[1],tickerlayer:signers[2],nodary:new ethers.Wallet("0x"+"55".repeat(32))};
const signerFor=(address:string)=>[...signers,...Object.values(providerSigners)].find(w=>w.address===address)!;
const vrfPath=join(dir,"vrf.key");await writeFile(vrfPath,ethers.toBeHex(TEST_SECRET,32),{mode:0o600});
const keyPaths:Record<string,string>={};
for(const [name,wallet] of Object.entries(keeperWallets)){keyPaths[name]=join(dir,`${name}-tx.key`);await writeFile(keyPaths[name],wallet.privateKey,{mode:0o600});}

const registry:any=await deployProxy(ethers,"EpochEntropy",[signers.map(s=>s.address),owner.address,keeperWallets.primary.address]);
await registry.scheduleCatalog(CATALOG,CATALOG.map(recipe=>providerSigners[providerOf(recipe)].address),2);
for(const name of followerNames())await registry.setBackupCommitter(keeperWallets[name].address,true);
const rng:any=await deployProxy(ethers,"D20VRFCoordinator",[publicKey(),owner.address,owner.address,FEE,1,await registry.getAddress(),5000]);
await rng.setPricing(FEE,0,300000); // Flat fee: the drill's requests send an exact value.
const consumer:any=await ethers.deployContract("TestConsumer",[await rng.getAddress()]);
for(const wallet of Object.values(keeperWallets))await owner.sendTransaction({to:wallet.address,value:ethers.parseEther("50")});

// One HTTP endpoint per keeper, so a single keeper's RPC can be blocked while its process keeps running, plus the
// local AirnodeHub fixture gateway. No keeper ever reaches the chain except through this proxy.
const blockedRpc=new Set<string>();
const apiCalls:Record<string,number>={};
const snapshotEpochs=new Set<bigint>(); // Epochs whose snapshot a keeper has fetched from the fixture gateway.
/// JSON-RPC traffic each keeper sent to its endpoint, counted on arrival and cumulative for the whole drill; a rate
/// window is the difference of two snapshots. HTTP requests are counted apart from calls, so batching shows as fewer
/// requests for the same calls. `sites` keys a call by method, contract function and block tag, which is how it is
/// traced back to a call site in the keeper.
interface RpcTraffic {http:number;batches:number;unanswered:number;calls:Record<string,number>;sites:Record<string,number>}
const rpcTraffic:Record<string,RpcTraffic>={};
const trafficOf=(keeper:string)=>rpcTraffic[keeper]??={http:0,batches:0,unanswered:0,calls:{},sites:{}};
const functionNames=new Map<string,string>();
for(const [label,contract] of [["Coordinator",rng],["Registry",registry]] as const)
  contract.interface.forEachFunction((fn:{selector:string;name:string})=>functionNames.set(fn.selector,`${label}.${fn.name}`));
function callSite(call:{method:string;params?:unknown}){
  const params=Array.isArray(call.params)?call.params:[];
  const tag=(value:unknown)=>value===undefined?"":` @${typeof value==="string"?(value.startsWith("0x")?"number":value):"object"}`;
  switch(call.method){
    case "eth_call":case "eth_estimateGas":{
      const data=String(params[0]?.data??params[0]?.input??"").slice(0,10);
      return `${call.method} ${functionNames.get(data)??(data||"-")}${tag(params[1])}`;
    }
    case "eth_getBlockByNumber":return `${call.method}${tag(params[0])}`;
    case "eth_getTransactionCount":case "eth_getBalance":case "eth_getCode":return `${call.method}${tag(params[1])}`;
    case "eth_getStorageAt":return `${call.method}${tag(params[2])}`;
    default:return call.method;
  }
}
/// Forward one JSON-RPC call, alone or as a batch element, and return its response object. A blocked endpoint never
/// answers: the call waits forever, so a batch holding it is never answered either, like a dead RPC.
async function forwardCall(keeper:string,call:any):Promise<object>{
  if(typeof call!=="object"||call===null||Array.isArray(call)||typeof call.method!=="string")
    return {jsonrpc:"2.0",id:call?.id??null,error:{code:-32600,message:"Invalid Request"}};
  const traffic=trafficOf(keeper),site=callSite(call);
  traffic.calls[call.method]=(traffic.calls[call.method]??0)+1;
  traffic.sites[site]=(traffic.sites[site]??0)+1;
  if(blockedRpc.has(keeper)){traffic.unanswered++;return new Promise<never>(()=>{});}
  try {return {jsonrpc:"2.0",id:call.id,result:await provider.request({method:call.method,params:call.params})};}
  catch(error){return {jsonrpc:"2.0",id:call.id,error:{code:-32000,message:String(error)}};}
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
      apiCalls[String(recipe)]=(apiCalls[String(recipe)]??0)+1;
      snapshotEpochs.add(BigInt(epoch));
      const timestamp=BigInt(head.timestamp);
      const data=epochFixtureData(recipe);
      const requestHash=ethers.id(canonical);
      const digest=ethers.keccak256(ethers.solidityPacked(["bytes32","uint256","bytes"],[requestHash,timestamp,ethers.toUtf8Bytes(JSON.stringify(data))]));
      const airnode=signerFor(selection.airnode);
      reply({airnode:airnode.address,requestHash,timestamp:String(timestamp),data,signature:await airnode.signMessage(ethers.getBytes(digest))});return;
    }
    const keeper=req.url?.slice("/rpc/".length)??"";
    if(!Object.keys(keeperWallets).includes(keeper)){reply({error:"unknown keeper endpoint"},404);return;}
    const traffic=trafficOf(keeper);
    traffic.http++;
    if(!Array.isArray(body)){reply(await forwardCall(keeper,body));return;}
    // A JSON-RPC batch: every element forwarded, the responses returned in the same order.
    traffic.batches++;
    if(body.length===0){reply({jsonrpc:"2.0",id:null,error:{code:-32600,message:"Invalid Request"}});return;}
    reply(await Promise.all(body.map(call=>forwardCall(keeper,call))));
  } catch(error){reply({error:String(error)},500);}
});
// A minimal RFC 6455 endpoint per keeper for eth_subscribe (newHeads, logs), fed from the in-process chain. Other
// methods sent over it are forwarded like HTTP calls. Only enabled with DRILL_WS=1.
interface WsClient {socket:Socket;keeper:string;subs:Map<string,{kind:string;addresses:string[]}>}
const wsClients=new Set<WsClient>();
const wsPushes:Record<string,number>={};
function wsFrame(payload:Buffer,opcode=1){
  const length=payload.length;
  const head=length<126?Buffer.from([0x80|opcode,length]):length<65536?Buffer.from([0x80|opcode,126,length>>8,length&255])
    :(()=>{const h=Buffer.alloc(10);h[0]=0x80|opcode;h[1]=127;h.writeBigUInt64BE(BigInt(length),2);return h;})();
  return Buffer.concat([head,payload]);
}
function wsParse(buffer:Buffer){
  if(buffer.length<2)return null;
  const opcode=buffer[0]&0x0f,masked=(buffer[1]&0x80)!==0;let length=buffer[1]&0x7f,offset=2;
  if(length===126){if(buffer.length<4)return null;length=buffer.readUInt16BE(2);offset=4;}
  else if(length===127){if(buffer.length<10)return null;length=Number(buffer.readBigUInt64BE(2));offset=10;}
  const maskAt=offset;if(masked)offset+=4;
  if(buffer.length<offset+length)return null;
  const payload=Buffer.from(buffer.subarray(offset,offset+length));
  if(masked)for(let i=0;i<payload.length;i++)payload[i]^=buffer[maskAt+(i%4)];
  return {opcode,payload,size:offset+length};
}
const wsSend=(client:WsClient,value:unknown)=>{if(!client.socket.destroyed)client.socket.write(wsFrame(Buffer.from(JSON.stringify(value))));};
server.on("upgrade",(req,socket:Socket)=>{
  const keeper=req.url?.startsWith("/ws/")?req.url.slice("/ws/".length):"";
  if(!Object.keys(keeperWallets).includes(keeper)||blockedRpc.has(keeper)){socket.destroy();return;}
  const accept=createHash("sha1").update(String(req.headers["sec-websocket-key"])+"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest("base64");
  socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
  const client:WsClient={socket,keeper,subs:new Map()};wsClients.add(client);
  let pending=Buffer.alloc(0);
  socket.on("data",async chunk=>{
    pending=Buffer.concat([pending,chunk]);
    for(let frame=wsParse(pending);frame;frame=wsParse(pending)){
      pending=pending.subarray(frame.size);
      if(frame.opcode===8){wsClients.delete(client);socket.end();return;}
      if(frame.opcode===9){socket.write(wsFrame(frame.payload,10));continue;}
      if(frame.opcode!==1)continue;
      const call=JSON.parse(frame.payload.toString());
      if(call.method==="eth_subscribe"){
        const traffic=trafficOf(keeper);traffic.calls[call.method]=(traffic.calls[call.method]??0)+1;
        const id="0x"+randomBytes(16).toString("hex");
        client.subs.set(id,{kind:String(call.params?.[0]),addresses:(call.params?.[1]?.address??[]).map((a:string)=>a.toLowerCase())});
        wsSend(client,{jsonrpc:"2.0",id:call.id,result:id});
      } else if(call.method==="eth_unsubscribe"){wsSend(client,{jsonrpc:"2.0",id:call.id,result:client.subs.delete(call.params?.[0])});}
      else wsSend(client,await forwardCall(keeper,call));
    }
  });
  socket.on("close",()=>wsClients.delete(client));socket.on("error",()=>wsClients.delete(client));
});
let pushedBlock=-1n,pushing=false;
if(process.env.DRILL_WS==="1")setInterval(async()=>{
  if(pushing)return;pushing=true;
  try {
    const latest=BigInt(await provider.request({method:"eth_blockNumber",params:[]}) as string);
    if(pushedBlock<0n)pushedBlock=latest;
    for(let number=pushedBlock+1n;number<=latest;number++){
      const tag=ethers.toQuantity(number);
      const block=await provider.request({method:"eth_getBlockByNumber",params:[tag,false]});
      const logs=await provider.request({method:"eth_getLogs",params:[{fromBlock:tag,toBlock:tag}]}) as {address:string}[];
      for(const client of wsClients){
        if(blockedRpc.has(client.keeper)){client.socket.destroy();wsClients.delete(client);continue;}
        for(const [id,sub] of client.subs){
          const results=sub.kind==="newHeads"?[block]:sub.kind==="logs"?logs.filter(log=>sub.addresses.includes(log.address.toLowerCase())):[];
          for(const result of results){wsSend(client,{jsonrpc:"2.0",method:"eth_subscription",params:{subscription:id,result}});wsPushes[client.keeper]=(wsPushes[client.keeper]??0)+1;}
        }
      }
    }
    pushedBlock=latest;
  } finally {pushing=false;}
},100).unref();
server.listen(0,"127.0.0.1");await once(server,"listening");
const address=server.address();assert.ok(address&&typeof address!=="string");
const base=`http://127.0.0.1:${address.port}`,wsBase=`ws://127.0.0.1:${address.port}`;

const env:NodeJS.ProcessEnv={...process.env,NEON_DB:undefined,TELEGRAM_BOT_TOKEN:undefined,TELEGRAM_CHAT_ID:undefined,DISCORD_BOT_TOKEN:undefined,
  HEALTH_API_URL:undefined,HEALTH_API_KEY:undefined,CHAIN_ID:"31337",COORDINATOR_ADDRESS:await rng.getAddress(),ALLOWED_CONSUMERS:await consumer.getAddress(),
  VRF_KEY_FILE:vrfPath,TEST_API_BASE:`${base}/api`,EXPECTED_PROTOCOL_HASH:await rng.protocolConfigurationHash(),
  CANCEL_MAX_FEE_PER_GAS_WEI:"150000000000",MAX_FEE_PER_GAS_WEI:"100000000000",MAX_TX_COST_WEI:"200000000000000000",MAX_GAS:"6000000",
  FEE_COVERAGE_BPS:"0",SEND_TRANSACTIONS:"true",POLL_MS:"250",RUST_LOG:"warn"};
const children=new Set<ReturnType<typeof spawn>>();
interface Keeper {name:string;child:ReturnType<typeof spawn>;log:()=>string}
const running=new Map<string,Keeper>();
/// A keeper process that ended without the drill stopping it: a crash, or a daemon that gives up on a fault. The
/// scenario that was running fails on it, and the log tail says why.
const unexpectedExits:{keeper:string;code:number|null;at:number;log:string}[]=[];
let fleetLanes=0;
function startKeeper(name:string,lanes=fleetLanes):Keeper {
  const rank=followerNames().indexOf(name);
  const follower=rank>=0?{KEEPER_ROLE:"follower",FOLLOWER_LANES:String(Math.max(lanes-1,1)),FOLLOWER_RANK:String(rank)}:{};
  const overrides:NodeJS.ProcessEnv={KEEPER_DB:join(dir,`${name}.sqlite`),TEST_LOCK_DIR:join(dir,`${name}-locks`),TX_KEY_FILE:keyPaths[name],
    RPC_URLS:`${base}/rpc/${name}`,...(process.env.DRILL_WS==="1"?{WS_URLS:`${wsBase}/ws/${name}`}:{}),...follower};
  const child=spawn(binary,["run"],{env:{...env,...overrides},windowsHide:true,stdio:["ignore","pipe","pipe"]});
  children.add(child);
  let err="";child.stderr.on("data",data=>{err+=data;});child.stdout.on("data",()=>{});
  child.on("exit",code=>{
    children.delete(child);
    if(running.get(name)?.child!==child)return; // Stopped by the drill: stopKeeper already removed it.
    running.delete(name);
    unexpectedExits.push({keeper:name,code,at:Date.now(),log:err.split(/\r?\n/).slice(-12).join(" | ")});
  });
  const keeper={name,child,log:()=>err};
  running.set(name,keeper);
  return keeper;
}
async function stopKeeper(name:string){
  const keeper=running.get(name);
  if(!keeper)return "";
  running.delete(name); // Removed first, so the exit handler does not read the kill as a crash.
  keeper.child.kill();
  await once(keeper.child,"exit").catch(()=>undefined);
  return keeper.log();
}
const sleep=(ms:number)=>new Promise(resolve=>setTimeout(resolve,ms));
/// Run exactly the lanes a scenario asks for. A follower that starts here needs a full liveness window before it may
/// call the primary dead, so the settle wait keeps a restart from being measured as an early takeover.
async function setFleet(lanes:number){
  const wanted=followerNames().slice(0,lanes-1);
  const changed=fleetLanes!==lanes||wanted.some(name=>!running.has(name));
  for(const name of followerNames())if(!wanted.includes(name)||fleetLanes!==lanes)await stopKeeper(name);
  fleetLanes=lanes;
  if(!running.has("primary"))startKeeper("primary");
  for(const name of wanted)if(!running.has(name))startKeeper(name,lanes);
  if(changed||!running.has("primary"))await sleep(15_000);
}
async function until(condition:()=>Promise<boolean>,message:string,ms:number,interval=500){
  const end=Date.now()+ms;
  while(Date.now()<end){if(await condition())return true;await sleep(interval);}
  throw new Error(message);
}

try {
  // Arc-like timing: blocks every 500 ms with no automine, so deadlines, delays and fallback windows run in real time.
  await provider.request({method:"evm_setAutomine",params:[false]});
  await provider.request({method:"evm_setIntervalMining",params:[500]});
  const startedAt=Date.now();
  await setFleet(plan[0]?.lanes??2); // The first scenario's lanes, so a single-lane measurement never starts a follower.
  // Wait for the first epoch and one published packet, so scenarios start from a served service.
  const firstEpochStart=await registry.firstEpochStart() as bigint;
  await until(async()=>BigInt(await ethers.provider.getBlockNumber())>=firstEpochStart,"the first epoch did not start",240_000);
  const warmUp=await request("fleet-drill-warm-up");
  await until(async()=>(await rng.getRequest(warmUp)).fulfilled,"the fleet did not serve the warm-up request",90_000);

  const results=[];
  for(const spec of plan)results.push(await runScenario(spec));
  const failures=results.filter(result=>result.failures.length>0);
  console.log(JSON.stringify({drill:"fleet",chain:"local EDR 31337, 500 ms blocks",binary,pollMs:Number(env.POLL_MS),fixtureDirectory:dir,
    seconds:Math.round((Date.now()-startedAt)/1000),apiCallsByRecipe:apiCalls,scenarios:results},null,2));
  assert.equal(failures.length,0,`Scenarios outside their thresholds: ${failures.map(f=>`${f.scenario}: ${f.failures.join("; ")}`).join(" | ")}`);
} finally {
  for(const child of children)child.kill();
  server.closeAllConnections();server.close();
  await provider.request({method:"evm_setIntervalMining",params:[0]}).catch(()=>undefined);
}

async function request(label:string,nonce?:number){
  const tx=await (consumer.connect(player) as any).request(ethers.id(label),200_000,player.address,{value:FEE,...(nonce!==undefined&&{nonce})});
  if(nonce===undefined)await tx.wait();
  return await consumer.lastRequestId() as bigint;
}
/// Send `count` requests as fast as the node accepts them, with explicit nonces so none is rejected as a duplicate.
async function burst(count:number,label:string){
  const start=await player.getNonce("pending");
  const sent=[];
  for(let n=0;n<count;n++)sent.push((consumer.connect(player) as any).request(ethers.id(`${label}-${n}`),200_000,player.address,{value:FEE,nonce:start+n}));
  await Promise.all(sent);
}
async function steady(perSecond:number,seconds:number,label:string){
  const interval=Math.round(1000/perSecond);
  for(let elapsed=0;elapsed<seconds*1000;elapsed+=interval){
    void request(`${label}-${elapsed}`,await player.getNonce("pending")).catch(()=>undefined);
    await sleep(interval);
  }
}
/// Ids from `fromId` on that are open at the latest block: neither fulfilled nor refunded, deadline not yet passed.
/// One paged view call, so polling it at a short interval barely competes with the keepers for the node.
async function openRequests(fromId:bigint){
  const end=await rng.nextRequestId() as bigint;
  const open:bigint[]=[];
  for(let cursor=fromId;cursor<end;){
    const [ids,next]=await rng.getPendingRequestIds(cursor,256) as [bigint[],bigint];
    open.push(...ids);
    if(next<=cursor)break;
    cursor=next;
  }
  return open;
}

type RateRow={key:string;calls:number;perSecond:number};
interface RateWindow {phase:string;seconds:number;keepers:Record<string,{calls:number;callsPerSecond:number;httpRequests:number;httpPerSecond:number;
  batchRequests:number;unanswered:number;methods:RateRow[];sites:RateRow[]}>}
function snapshotTraffic(){return structuredClone(rpcTraffic);}
/// Rates over the window since `began`: each keeper's traffic now minus its snapshot `before`.
function rateWindow(phase:string,began:number,before:Record<string,RpcTraffic>,keepers:readonly string[]):RateWindow {
  const seconds=(Date.now()-began)/1000;
  const round=(value:number)=>Number(value.toFixed(2));
  const rows=(now:Record<string,number>,then:Record<string,number>)=>Object.entries(now)
    .map(([key,count])=>({key,calls:count-(then[key]??0),perSecond:round((count-(then[key]??0))/seconds)}))
    .filter(row=>row.calls>0).sort((a,b)=>b.calls-a.calls||a.key.localeCompare(b.key));
  return {phase,seconds:round(seconds),keepers:Object.fromEntries(keepers.map(name=>{
    const now=trafficOf(name),then=before[name]??{http:0,batches:0,unanswered:0,calls:{},sites:{}};
    const methods=rows(now.calls,then.calls),calls=methods.reduce((total,row)=>total+row.calls,0),http=now.http-then.http;
    return [name,{calls,callsPerSecond:round(calls/seconds),httpRequests:http,httpPerSecond:round(http/seconds),
      batchRequests:now.batches-then.batches,unanswered:now.unanswered-then.unanswered,methods,sites:rows(now.sites,then.sites)}];
  }))};
}
function printRates(scenario:string,windows:readonly RateWindow[]){
  const line=(row:RateRow)=>`${row.perSecond.toFixed(2).padStart(9)}/s ${String(row.calls).padStart(7)}  ${row.key}`;
  for(const window of windows)for(const [keeper,rates] of Object.entries(window.keepers)){
    console.log(`RPC ${scenario} ${window.phase} ${keeper}: ${window.seconds} s, ${rates.calls} calls ${rates.callsPerSecond}/s, `
      +`${rates.httpRequests} HTTP requests ${rates.httpPerSecond}/s, ${rates.batchRequests} batches, ${rates.unanswered} unanswered`);
    for(const row of rates.methods)console.log(line(row));
    console.log("  by call site:");
    for(const row of rates.sites)console.log(line(row));
  }
}
/// The idle phase: wait until no request is open and the chain is in an epoch no request belongs to, whose snapshot the
/// keeper has already fetched. That is the steady idle state, a prepared and unpublished snapshot, which a published
/// epoch is not: the keeper re-checks an unpublished packet for demand on every tick. Then let the last receipts
/// reconcile and measure a window in which the keepers have nothing to serve and only follow the chain.
async function idleWindow(spec:ScenarioSpec,keepers:readonly string[]){
  await until(async()=>(await openRequests(1n)).length===0,`${spec.name}: requests stayed open before the idle window`,300_000,1000);
  const last=await rng.nextRequestId()-1n;
  const demanded=last===0n?0n:BigInt((await rng.getRequest(last)).epochId);
  await until(async()=>{
    const epoch=BigInt(await registry.epochForBlock(await ethers.provider.getBlockNumber()));
    return epoch>demanded&&snapshotEpochs.has(epoch);
  },`${spec.name}: the keeper did not prepare an idle epoch`,300_000,1000);
  await sleep(6000);
  const began=Date.now(),before=snapshotTraffic();
  await sleep(spec.idleSeconds!*1000);
  const window=rateWindow("idle",began,before,keepers);
  assert.equal((await openRequests(1n)).length,0,`${spec.name}: a request opened during the idle window`);
  return window;
}

async function runScenario(spec:ScenarioSpec){
  await setFleet(spec.lanes);
  const exitsBefore=unexpectedExits.length;
  const reportRates=spec.measurement===true||rpcStatsEverywhere;
  const keepers=["primary",...followerNames().slice(0,spec.lanes-1)];
  const rates:RateWindow[]=[];
  if(spec.idleSeconds!==undefined)rates.push(await idleWindow(spec,keepers));
  const scenarioStart=Date.now();
  const fromBlock=await ethers.provider.getBlockNumber()+1;
  const firstId=await rng.nextRequestId() as bigint;
  let faultAt:number|undefined,restored=false;
  const applyFault=async()=>{
    if(spec.fault.kind==="none")return;
    faultAt=Math.round((Date.now()-scenarioStart)/1000);
    const target=spec.fault.target==="primary"?"primary":followerNames()[0];
    if(spec.fault.kind==="kill")await stopKeeper(target);
    else if(spec.fault.kind==="block-rpc")blockedRpc.add(target);
    else if(spec.fault.kind==="restart"){await stopKeeper(target);startKeeper(target);restored=true;}
  };
  const restore=async()=>{
    if(restored||spec.fault.kind==="none")return;
    const target=spec.fault.target==="primary"?"primary":followerNames()[0];
    if(spec.fault.kind==="block-rpc")blockedRpc.delete(target);
    if(!running.has(target))startKeeper(target);
    restored=true;
  };
  const fault=spec.fault.kind==="none"?undefined:setTimeout(()=>{void applyFault();},spec.fault.afterSeconds*1000);
  // While a fault lasts, restart a lane that gave up, the way a host supervisor does, and no faster: a keeper whose
  // RPC endpoint is dead exits on every start, and restarting it in a tight loop would starve the lanes that work.
  const supervisor=setInterval(()=>{
    if(spec.fault.kind!=="block-rpc"||faultAt===undefined||restored)return;
    const target=spec.fault.target==="primary"?"primary":followerNames()[0];
    if(!running.has(target))startKeeper(target);
  },15_000);
  const restoreTimer=spec.fault.kind!=="none"&&spec.fault.restoreAfterSeconds!==undefined
    ?setTimeout(()=>{void restore();},spec.fault.restoreAfterSeconds*1000):undefined;
  try {
    const expected=spec.load.kind==="burst"?spec.load.requests:spec.load.kind==="steady"?Math.floor(spec.load.perSecond*spec.load.seconds):0;
    const loadStart=Date.now(),trafficBefore=snapshotTraffic();
    if(spec.load.kind==="burst")await burst(spec.load.requests,spec.name);
    else if(spec.load.kind==="steady")await steady(spec.load.perSecond,spec.load.seconds,spec.name);
    // Every request is resolved when it is fulfilled, refunded or past its deadline; refunds are counted, not required.
    // A measured load is polled finely, so its window closes within a quarter second of the last settlement.
    const resolved=async()=>await rng.nextRequestId()-firstId>=BigInt(expected)&&(await openRequests(firstId)).length===0;
    await until(resolved,`${spec.name}: requests were neither served nor expired in time`,300_000,reportRates?250:2000);
    if(reportRates&&spec.load.kind!=="none")rates.push(rateWindow(spec.load.kind,loadStart,trafficBefore,keepers));
    await restore();
    // Let the keepers reconcile their receipts and the chain settle before the accounting read.
    await sleep(6000);
    const toBlock=await ethers.provider.getBlockNumber();
    const report=await failoverReport(ethers.provider,{registry:await registry.getAddress(),coordinator:await rng.getAddress(),fromBlock,toBlock,
      wallets:Object.values(keeperWallets).map(wallet=>wallet.address)});
    const served=report.summary.requestsFulfilledBy,requests=report.requests.length;
    const primaryAddress=ethers.getAddress(keeperWallets.primary.address);
    const followerAddresses=followerNames().slice(0,spec.lanes-1).map(name=>ethers.getAddress(keeperWallets[name].address));
    const sumOver=(by:Record<string,number>|undefined)=>followerAddresses.reduce((total,address)=>total+(by?.[address]??0),0);
    const followerTransactions=sumOver(report.summary.transactionsBy);
    const servedByFollowers=sumOver(served);
    const takeoverSeconds=takeover(report,followerAddresses,scenarioStart,faultAt);
    const duplicateRatio=requests===0?0:report.summary.duplicateAttempts/requests;
    const measured={requests,unserved:report.summary.unserved.length,refunds:report.summary.refunds,duplicateAttempts:report.summary.duplicateAttempts,
      duplicateRatio:Number(duplicateRatio.toFixed(4)),epochsPublishedBy:report.summary.epochsPublishedBy,servedBy:served,
      transactionsBy:report.summary.transactionsBy,gasUsedBy:report.summary.gasUsedBy,nonceCancellations:report.summary.nonceCancellations,
      revertedKeeperTransactions:report.summary.revertedKeeperTransactions,followerTransactions,servedByFollowers,takeoverSeconds,
      lanes:spec.lanes,faultAtSeconds:faultAt,seconds:Math.round((Date.now()-scenarioStart)/1000),
      unexpectedExits:unexpectedExits.slice(exitsBefore).map(exit=>({keeper:exit.keeper,code:exit.code,
        seconds:Math.round((exit.at-scenarioStart)/1000)})),
      ...(reportRates&&{rpc:rates})};
    const failures=[];
    const faultTarget=spec.fault.kind==="none"?undefined:spec.fault.target==="primary"?"primary":followerNames()[0];
    for(const exit of unexpectedExits.slice(exitsBefore))
      if(!(spec.fault.kind==="block-rpc"&&exit.keeper===faultTarget))
        failures.push(`the ${exit.keeper} keeper ended on its own with code ${exit.code}: ${exit.log.trim().slice(-300)}`);
    const refundable=measured.refunds+measured.unserved;
    if(refundable>spec.thresholds.refunds)failures.push(`${refundable} refundable requests over the limit of ${spec.thresholds.refunds}`);
    if(spec.thresholds.duplicateRatio!==undefined&&duplicateRatio>spec.thresholds.duplicateRatio)
      failures.push(`duplicate attempts ${(duplicateRatio*100).toFixed(2)}% over ${(spec.thresholds.duplicateRatio*100).toFixed(2)}%`);
    if(spec.thresholds.takeoverSeconds!==undefined&&(takeoverSeconds===undefined||takeoverSeconds>spec.thresholds.takeoverSeconds))
      failures.push(`takeover ${takeoverSeconds===undefined?"did not happen":`took ${takeoverSeconds}s`}, limit ${spec.thresholds.takeoverSeconds}s`);
    if(spec.thresholds.followerTransactions!==undefined&&followerTransactions>spec.thresholds.followerTransactions)
      failures.push(`the follower sent ${followerTransactions} transactions, limit ${spec.thresholds.followerTransactions}`);
    if(spec.thresholds.minServedByFollower!==undefined&&servedByFollowers<spec.thresholds.minServedByFollower)
      failures.push(`the followers served ${servedByFollowers} requests, expected at least ${spec.thresholds.minServedByFollower}`);
    if((served[primaryAddress]??0)+servedByFollowers<requests-refundable)
      failures.push("some requests were served by a wallet outside the fleet");
    if(reportRates)printRates(spec.name,rates);
    if(reportRates&&process.env.DRILL_WS==="1")console.log(`  WebSocket pushes so far (heads and logs, cumulative): ${JSON.stringify(wsPushes)}`);
    console.log(JSON.stringify({scenario:spec.name,measured,failures}));
    return {scenario:spec.name,purpose:spec.purpose,lanes:spec.lanes,load:spec.load,fault:spec.fault,thresholds:spec.thresholds,measured,failures};
  } finally {
    clearTimeout(fault);clearTimeout(restoreTimer);clearInterval(supervisor);
    await restore();
    if(!running.has("primary"))startKeeper("primary");
    for(const name of followerNames().slice(0,fleetLanes-1))if(!running.has(name))startKeeper(name);
  }
}
/// Seconds from the fault to the first fulfillment a follower lane lands after it, from chain timestamps. A follower
/// that had already joined the queue keeps serving across the fault, which shows here as a takeover of about zero.
function takeover(report:Awaited<ReturnType<typeof failoverReport>>,followers:string[],scenarioStart:number,faultAt:number|undefined){
  if(faultAt===undefined)return undefined;
  const faultWall=Math.round(scenarioStart/1000)+faultAt;
  const served=report.requests.flatMap(entry=>
    entry.fulfilled&&followers.includes(entry.fulfilled.submitter)&&entry.fulfilled.timestamp>=faultWall?[entry.fulfilled.timestamp]:[]);
  if(served.length===0)return undefined;
  return Math.min(...served)-faultWall;
}
