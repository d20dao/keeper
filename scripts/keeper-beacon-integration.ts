import {deployProxy,implementationCodeHash} from "../test/helpers/proxy.ts";
// Local-only: the real Rust keeper against a Hardhat chain, drand beacon epochs served by fake relays whose rounds a real
// D20BeaconVerifier checks. Run it with `npx hardhat run scripts/keeper-beacon-integration.ts`.
import {createServer,type IncomingMessage,type Server,type ServerResponse} from "node:http";
import {spawn} from "node:child_process";
import {existsSync} from "node:fs";
import {mkdir, mkdtemp, writeFile} from "node:fs/promises";
import {join, resolve} from "node:path";
import {once} from "node:events";
import {DatabaseSync} from "node:sqlite";
import assert from "node:assert/strict";
import {buildKeeper} from "./lib/keeper-binary.ts";
import {network} from "hardhat";
import {TEST_SECRET, publicKey} from "../test/helpers/proof.ts";
import {epochFixtureData} from "../test/helpers/epoch.ts";
import {registerTestBeacon,signTestRound,testBeacon} from "../test/helpers/beacon.ts";
import {beaconCanonicalRequest,beaconRoundAt,beaconRoundTime,beaconSlotSigner,decodeBeaconRound,decodeEpochEvidencePacket,readEpochRecipes,replayEpochCommitment,resolveEpochCatalog} from "../src/index.ts";

const {ethers, networkHelpers, provider} = await network.create({network:"loadSim"});
assert.equal((await ethers.provider.getNetwork()).chainId,31337n);
const binary=await buildKeeper("debug");
await mkdir(".research",{recursive:true});const dir=await mkdtemp(resolve(".research/beacon-keeper-e2e-"));
const vrfPath=join(dir,"fixture-vrf.key"),txPath=join(dir,"fixture-tx.key");
const wallet=ethers.HDNodeWallet.fromPhrase("test test test test test test test test test test test junk",undefined,"m/44'/60'/0'/0/2");
await writeFile(vrfPath,ethers.toBeHex(TEST_SECRET,32),{mode:0o600});await writeFile(txPath,wallet.privateKey,{mode:0o600});
const [owner,player]=await ethers.getSigners();
// The initial catalog (four signed recipes, one test Airnode each) serves epoch 1 and every epoch before the switch.
const airnodes=["11","22","33","44"].map(value=>new ethers.Wallet("0x"+value.repeat(32)));
const registry=await deployProxy(ethers,"EpochEntropy",[airnodes.map(s=>s.address),owner.address,wallet.address]);
const registryAddress=await registry.getAddress();
const rng=await deployProxy(ethers,"D20VRFCoordinator",[publicKey(),owner.address,owner.address,123,1,registryAddress,0]);
await rng.setPricing(123,0,300000); // Flat fee: the fixture consumer sends exact values.
const game=await ethers.deployContract("TestConsumer",[await rng.getAddress()]);
// The test beacon: 3-second rounds from a genesis 3000 seconds before now, registered with the real BLS verifier as recipe 6.
const verifier=await ethers.deployContract("D20BeaconVerifier");
const latestBlock=async()=>await provider.request({method:"eth_getBlockByNumber",params:["latest",false]}) as {number:string,timestamp:string};
const chainNow=async()=>BigInt((await latestBlock()).timestamp);
const beacon=testBeacon(await verifier.getAddress(),await chainNow());
await registerTestBeacon(registry,beacon);
const BEACON_RECIPE=6,slot=await registry.slotSigner(BEACON_RECIPE);
assert.equal(slot,beaconSlotSigner(beacon));
const chainHex=beacon.chainHash.slice(2),beaconQuery=ethers.id(beaconCanonicalRequest(beacon.chainHash));
const sleep=(ms:number)=>new Promise<void>(resolve=>setTimeout(resolve,ms));

// Requests the fixture servers did not expect, the calls the keeper made to the chain, and every transaction it sent.
const unexpected:string[]=[],counts:Record<string,number>={},raw:string[]=[];
const signatures=new Map<bigint,string>();
const signRound=(round:bigint)=>{let signature=signatures.get(round);if(signature===undefined){signature=signTestRound(round);signatures.set(round,signature);}return signature;};

// A fake drand relay: GET <prefix>/<chain hash>/public/<round> answers a round signed by the test key, unless its mode says
// otherwise. A round scheduled after the chain's latest block time is not published yet, as on a real relay: HTTP 425. A relay
// holds a request for `delayMs` first, as a relay long-polling for the next round does, and does not answer a request the
// keeper has given up on.
type RelayMode="good"|"wrong"|"down"|"missing"|"error";
interface Relay {url:string;server?:Server;mode:RelayMode;delayMs:number;hits:bigint[];}
const hold=(ms:number,res:ServerResponse)=>new Promise<void>(resolve=>{const timer=setTimeout(resolve,ms);res.once("close",()=>{clearTimeout(timer);resolve();});});
const relayHandler=(relay:Relay,prefix:string)=>async(req:IncomingMessage,res:ServerResponse)=>{
  const match=new RegExp(`^${prefix}/([0-9a-f]{64})/public/([1-9][0-9]*)$`).exec(req.url??"");
  if(!match||match[1]!==chainHex||req.method!=="GET"){unexpected.push(`${req.method} ${req.url}`);res.writeHead(404);res.end();return;}
  const round=BigInt(match[2]);relay.hits.push(round);
  if(relay.mode==="down"){req.socket.destroy();return;} // As a relay that drops its connections does.
  if(relay.delayMs>0){await hold(relay.delayMs,res);if(res.destroyed)return;}
  // As a relay whose cache lacks the round, or that is failing: 404 for every round however old, or 503.
  if(relay.mode==="missing"){res.writeHead(404,{"Content-Type":"text/plain"});res.end("Not found");return;}
  if(relay.mode==="error"){res.writeHead(503,{"Content-Type":"text/plain"});res.end("Unavailable");return;}
  if(beaconRoundTime(beacon,round)>await chainNow()){res.writeHead(425,{"Content-Type":"text/plain"});res.end("Too early");return;}
  // A wrong signature is well formed: the beacon's signature of the next round, which only the registry can tell apart.
  const signature=signRound(relay.mode==="wrong"?round+1n:round);
  res.writeHead(200,{"Content-Type":"application/json"});
  res.end(JSON.stringify({round:Number(round),randomness:ethers.keccak256(signature).slice(2),signature:signature.slice(2)}));
};
const relays:Relay[]=[];
async function startRelay(mode:RelayMode,delayMs=0):Promise<Relay>{
  const relay:Relay={url:"",mode,delayMs,hits:[]};relays.push(relay);
  const relayServer=createServer(relayHandler(relay,""));relay.server=relayServer;
  relayServer.listen(0,"127.0.0.1");await once(relayServer,"listening");
  const address=relayServer.address();assert.ok(address&&typeof address!=="string");relay.url=`http://127.0.0.1:${address.port}`;
  return relay;
}
// The keeper's RPC endpoint and, for the local override, its one relay: /rpc is the chain; /api/<chain hash>/public/<round> is
// the relay `overrideRelay`.
let holdCommits=false,held=0;
const overrideRelay:Relay={url:"",mode:"good",delayMs:0,hits:[]};
const overrideHandler=relayHandler(overrideRelay,"");
const server=createServer(async(req,res)=>{
  const reply=(value:unknown,status=200)=>{res.writeHead(status,{"Content-Type":"application/json"});res.end(JSON.stringify(value));};
  try {
    const chunks:Buffer[]=[];for await(const chunk of req)chunks.push(Buffer.from(chunk));const text=Buffer.concat(chunks).toString();
    if(req.url?.startsWith("/api/")){
      const path=req.url.slice("/api".length);
      if(/^\/[0-9a-f]{64}\//.test(path)){req.url=path;await overrideHandler(req,res);return;}
      unexpected.push(`${req.method} ${req.url}`);throw new Error("Unexpected relay request");
    }
    assert.equal(req.url,"/rpc");
    // A JSON-RPC batch is answered element by element, in order.
    const one=async(item:any)=>{
      let name:string=item.method;
      if(name==="eth_call"){const call=item.params[0];const iface=call.to.toLowerCase()===registryAddress.toLowerCase()?registry.interface:rng.interface;name+=":"+(iface.getFunction(call.data.slice(0,10))?.name??call.data.slice(0,10));}
      counts[name]=(counts[name]??0)+1;
      if(item.method==="eth_sendRawTransaction"){
        raw.push(item.params[0]);
        // A commit the chain never sees, as when the network drops it: the keeper cannot tell and keeps its signed bytes.
        if(holdCommits&&ethers.Transaction.from(item.params[0]).to?.toLowerCase()===registryAddress.toLowerCase()){held++;return {jsonrpc:"2.0",id:item.id,error:{code:-32000,message:"network response unavailable"}};}
      }
      try {return {jsonrpc:"2.0",id:item.id,result:await provider.request({method:item.method,params:item.params})};}
      catch(error){return {jsonrpc:"2.0",id:item.id,error:{code:-32000,message:String(error)}};}
    };
    const body=JSON.parse(text);
    reply(Array.isArray(body)?await Promise.all(body.map(one)):await one(body));
  } catch(error){reply({error:String(error)},500);}
});
server.listen(0,"127.0.0.1");await once(server,"listening");const addr=server.address();assert.ok(addr&&typeof addr!=="string");const base=`http://127.0.0.1:${addr.port}`;
const env:NodeJS.ProcessEnv={...process.env,NEON_DB:undefined,TELEGRAM_BOT_TOKEN:undefined,TELEGRAM_CHAT_ID:undefined,TELEGRAM_LOW_BALANCE_WEI:undefined,DISCORD_BOT_TOKEN:undefined,DISCORD_PROTOCOL_CHANNEL_ID:undefined,HEALTH_API_URL:undefined,HEALTH_API_KEY:undefined,HEALTH_INTERVAL_SECONDS:undefined,
  RPC_URLS:`${base}/rpc`,CHAIN_ID:"31337",COORDINATOR_ADDRESS:await rng.getAddress(),TX_KEY_FILE:txPath,VRF_KEY_FILE:vrfPath,
  TEST_API_BASE:undefined,DRAND_RELAYS:undefined,EPOCH_API_ENDPOINTS:undefined,EXPECTED_SOURCE_HASH:undefined,EXPECTED_CODE_HASH:undefined,EXPECTED_PROTOCOL_HASH:await rng.protocolConfigurationHash(),
  EXPECTED_IMPLEMENTATION_CODE_HASH:await implementationCodeHash(ethers,rng),EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH:await implementationCodeHash(ethers,registry),
  CANCEL_MAX_FEE_PER_GAS_WEI:"150000000000",FEE_COVERAGE_BPS:"0",SEND_TRANSACTIONS:"true",POLL_MS:"100",RUST_LOG:"warn"};
const children=new Set<ReturnType<typeof spawn>>();
/// A keeper daemon on the shared journal. Only one runs at a time.
function start(overrides:NodeJS.ProcessEnv={}) {
  const child=spawn(binary,["run"],{env:{...env,KEEPER_DB:join(dir,"keeper.sqlite"),TEST_LOCK_DIR:join(dir,"locks"),...overrides},windowsHide:true,stdio:["ignore","pipe","pipe"]});children.add(child);
  let err="";child.stderr.on("data",d=>{err+=d;});child.stdout.resume();
  if(process.env.DEBUG_KEEPER)child.stderr.pipe(process.stderr); // Local diagnosis of a failing scenario.
  const exit=new Promise<{code:number|null,err:string}>((resolve,reject)=>{child.on("error",reject);child.on("exit",code=>{children.delete(child);resolve({code,err});});});
  return {child,exit,log:()=>err};
}
async function stop(keeper:ReturnType<typeof start>){keeper.child.kill("SIGKILL");return (await keeper.exit).err;}
async function until(condition:()=>Promise<boolean>|boolean,message:string,ms=20000){const end=Date.now()+ms;while(Date.now()<end){if(await condition())return;await sleep(80);}throw Error(message);}
async function mineTo(n:bigint){const latest=BigInt(await provider.request({method:"eth_blockNumber",params:[]}) as string);if(n>latest)await provider.request({method:"hardhat_mine",params:[ethers.toQuantity(n-latest),"0x0"]});}
async function request(label:string){
  await game.connect(player).getFunction("request")(ethers.id(label),200000,player.address,{value:123});
  await networkHelpers.mine(2);return await game.lastRequestId() as bigint;
}
/// Open the keeper's journal and wait out its locks: a keeper started after a SIGKILL recovers the WAL, and a reader without a busy timeout fails meanwhile with SQLITE_BUSY_RECOVERY.
function openJournal(path:string,readOnly=false){const db=new DatabaseSync(path,{readOnly});db.exec("PRAGMA busy_timeout=5000");return db;}
function rows(sql:string){
  if(!existsSync(join(dir,"keeper.sqlite")))return []; // The keeper has not created its journal yet.
  const sqlite=openJournal(join(dir,"keeper.sqlite"),true);
  try{return sqlite.prepare(sql).all();}
  catch(error){if(/no such table/.test(String(error)))return [];throw error;} // Nor its tables.
  finally{sqlite.close();}
}
/// Wait for the running keeper to serve a request, mining the blocks its target needs.
async function served(id:bigint,message:string,ms=30000){
  await until(async()=>{const r=await rng.getRequest(id);if(r.targetBlock>0n)await mineTo(r.targetBlock+await rng.confirmationBlocks());return (await rng.getRequest(id)).fulfilled;},message,ms);
  assert.equal((await rng.getRequest(id)).delivered,true);
}
interface EpochRow {state:string;attempts:number;api:string|null;last_error:string|null;failing_since:number|null;fallback:number}
const epochRow=(epoch:bigint)=>rows(`SELECT state,attempts,api,last_error,failing_since,fallback FROM epoch_work WHERE epoch=${epoch}`)[0] as unknown as EpochRow|undefined;
// The work of an epoch before the switch whose signed source was refused (scenario (a)) stays blocked for good, whether or not the keeper
// noticed that the committer published the epoch by hand. No other epoch is ever to be left blocked: a beacon epoch, one from the switch
// on, is not even when it is wrongly refused as an unsupported recipe.
const blockedBeaconEpochs=(switchEpoch:bigint)=>Number(rows(`SELECT COUNT(*) n FROM epoch_work WHERE state='blocked' AND NOT (epoch<${switchEpoch} AND COALESCE(last_error,'') LIKE '%signed API recipes are not supported%')`)[0].n);
const packetRound=(api:string)=>BigInt(ethers.toUtf8String(JSON.parse(api).data));
const health=()=>JSON.parse(String(rows("SELECT value FROM meta WHERE key='health:status'")[0]?.value??"{}")) as {healthy?:boolean,faults?:string[]};
// The initial catalog's signed record for an epoch, signed by the test Airnode its selection names, and its publication by the
// committer: this keeper does not prepare such epochs, so whoever has the record publishes it.
const committer=wallet.connect(ethers.provider);
async function publishSignedEpoch(epoch:bigint){
  const selection=await registry.getEpochSelection(epoch),recipe=Number(selection.recipe),airnode=airnodes.find(a=>a.address===selection.airnode)!;
  const data=ethers.toUtf8Bytes(JSON.stringify({...epochFixtureData(recipe),...(recipe===2||recipe===3?{size:0.000001}:{})})),timestamp=await chainNow();
  const digest=ethers.keccak256(ethers.solidityPacked(["bytes32","uint256","bytes"],[selection.queryHash,timestamp,data]));
  await (await (registry.connect(committer) as any).commitEpoch(epoch,{timestamp,data:ethers.hexlify(data),signature:await airnode.signMessage(ethers.getBytes(digest))})).wait();
}
const startBlockTime=async(epoch:bigint)=>BigInt((await ethers.provider.getBlock(Number(await registry.epochStart(epoch))))!.timestamp);
const epochStartOf=async(epoch:bigint)=>await registry.epochStart(epoch) as bigint;
const currentEpoch=async()=>await registry.epochForBlock(await ethers.provider.getBlockNumber()) as bigint;
/// Replay a published beacon epoch from public data alone, as an independent verifier would: the catalog and beacon
/// registration the registry holds, the packet of the EpochCommitted event and the block the commit landed in.
async function replayBeaconEpoch(epoch:bigint){
  const record=await registry.getEpoch(epoch),[event]=await registry.queryFilter(registry.filters.EpochCommitted(epoch));
  assert.ok(event,`Epoch ${epoch} was not published`);
  const [hash,recipes,signers]=await registry.catalogAt(epoch);
  const recipeBook=await readEpochRecipes(ethers.provider,registryAddress,recipes.map(Number));
  assert.ok(recipeBook[BEACON_RECIPE]&&"beacon" in recipeBook[BEACON_RECIPE],"The recipe book lacks the beacon registration");
  const catalog=resolveEpochCatalog({registry:registryAddress,chainId:31337n,firstEpochStart:await registry.firstEpochStart(),recipeBook},{hash,recipes,signers});
  const commit=await event.getBlock(),replayed=replayEpochCommitment({catalog,epochId:epoch,record,commitTimestamp:BigInt(commit.timestamp),packet:event.args.packet});
  assert.equal(replayed.epochHash,record.epochHash);assert.equal(record.queryHash,beaconQuery);
  const {attestation}=decodeEpochEvidencePacket(event.args.packet),round=decodeBeaconRound(attestation.data);
  assert.equal(BigInt(attestation.timestamp),beaconRoundTime(beacon,round));assert.equal(BigInt(record.signedAt),beaconRoundTime(beacon,round));
  return {round,ageAtCommit:BigInt(commit.timestamp)-BigInt(record.signedAt)};
}

// Local diagnosis: BEACON_SCENARIOS=e runs scenario (e) alone, after a plain switch to beacon epochs.
const only=process.env.BEACON_SCENARIOS?.split(",");
const selected=(name:string)=>only===undefined||only.includes(name);
try {
  let switchEpoch:bigint;
  if(!selected("a")){
    await mineTo(await registry.firstEpochStart());switchEpoch=await currentEpoch()+2n;
    await registry.scheduleCatalog([BEACON_RECIPE],[slot],switchEpoch);await mineTo(await epochStartOf(switchEpoch));
  } else {
  // ---- (a) The initial catalog is signed, which this keeper does not prepare; the owner schedules the beacon catalog while it runs. ------
  // Epoch 1 and every epoch up to the switch use the initial catalog, four signed API recipes. The keeper refuses their sources with an
  // error and a health fault, does not exit, serves what the registry has published (here, by the committer) and prepares the epochs
  // from the switch on, with no restart. A setting that release 0.4.0 read and this one does not is a warning, even if it is nonsense.
  // The local override is the keeper's one relay.
  const keeper=start({TEST_API_BASE:`${base}/api`,EPOCH_API_ENDPOINTS:"not a list of gateways",PROGRESS_STUCK_SECONDS:"2"});
  await mineTo(await registry.firstEpochStart());
  await until(()=>epochRow(1n)?.state==="blocked","The signed source of epoch 1 was not refused");
  assert.match(String(epochRow(1n)!.last_error),/is not a drand beacon: signed API recipes are not supported since 0\.4\.1/);
  assert.equal(epochRow(1n)!.api,null);
  await until(()=>(health().faults??[]).some(fault=>/^epoch_recipe_unsupported:[0-3]$/.test(fault)),"The unsupported recipe was not a health fault");
  assert.match(keeper.log(),/"level":"ERROR".*signed API recipes are not supported since 0\.4\.1/);
  assert.equal(keeper.log().split("\n").filter(line=>line.includes("EPOCH_API_ENDPOINTS")).length,1,"The removed setting was not named by exactly one warning");
  assert.match(keeper.log(),/EPOCH_API_ENDPOINTS is ignored: signed API recipes are not supported since 0\.4\.1/);
  assert.equal(keeper.child.exitCode,null,"The keeper exited on an unsupported recipe");
  // Demand on the unpublished epoch is a stall of its own reason once the ladder has run out: every source of the initial catalog is
  // signed, one source per 20-block window, and nobody on the keeper's side can serve it.
  const stuck=await request("signed-epoch-unpublished"),start1=await epochStartOf(1n);
  for(const [rung,blocks] of [[1,25n],[2,45n],[3,65n]] as const){
    await mineTo(start1+blocks);
    await until(()=>epochRow(1n)?.fallback===rung&&epochRow(1n)?.state==="blocked",`The signed source ${rung} of epoch 1 was not refused`);
  }
  await until(()=>(health().faults??[]).includes("epoch_stalled"),"Live demand on the refused epoch was not an epoch stall",20000);
  assert.match(keeper.log(),/Live paid demand cannot be published/);assert.match(keeper.log(),/"reason":"unsupported_recipe"/);
  assert.equal((await registry.getEpoch(1n)).epochHash,ethers.ZeroHash);assert.equal(raw.length,0,"A transaction was sent for an epoch that cannot be prepared");
  // The committer publishes the epoch from the record it signed, and the keeper serves the requests of the published epoch, the one
  // that waited for it and a later one, as it serves any: nothing about the epoch's source matters once it is on chain.
  await publishSignedEpoch(1n);
  assert.notEqual((await registry.getEpoch(1n)).epochHash,ethers.ZeroHash);assert.notEqual((await registry.getEpoch(1n)).queryHash,beaconQuery);
  await served(stuck,"The request that waited for the signed epoch was not served once it was published");
  const before=await request("signed-epoch-published-by-the-committer");
  await served(before,"The request of the published signed epoch was not served");
  assert.equal((await rng.getRequest(stuck)).epochId,1n);assert.equal((await rng.getRequest(before)).epochId,1n);
  switchEpoch=await currentEpoch()+2n;
  await registry.scheduleCatalog([BEACON_RECIPE],[slot],switchEpoch);
  const switched:Array<{epoch:string;round:string;startRound:string;ageAtCommitSeconds:string}>=[];
  for(let n=0n;n<3n;n++){
    const epoch=switchEpoch+n;
    await mineTo(await epochStartOf(epoch));
    // The keeper prepares the beacon epoch on its own; the request then publishes it.
    await until(()=>epochRow(epoch)?.api!=null,`The beacon epoch ${epoch} was not prepared`);
    // A source that is supported again clears the fault.
    await until(()=>(health().faults??[]).length===0,"The faults stayed after a beacon epoch was prepared");
    const id=await request(`beacon-epoch-${n}`);await served(id,`The request of beacon epoch ${epoch} was not served`);
    assert.equal((await rng.getRequest(id)).epochId,epoch);
    const replayed=await replayBeaconEpoch(epoch),startRound=beaconRoundAt(beacon,await startBlockTime(epoch));
    // The round current at the epoch's start: prepared within seconds of it and committed a few seconds later.
    assert.equal(replayed.round,startRound,`Epoch ${epoch} committed round ${replayed.round}, not its start round ${startRound}`);
    assert.ok(replayed.ageAtCommit<=200n,`The round was ${replayed.ageAtCommit} seconds old at its commit`);
    await until(()=>epochRow(epoch)?.state==="committed",`Epoch ${epoch} was not recorded as committed`);
    assert.equal(epochRow(epoch)?.failing_since,null);
    switched.push({epoch:String(epoch),round:String(replayed.round),startRound:String(startRound),ageAtCommitSeconds:String(replayed.ageAtCommit)});
  }
  assert.ok(counts["eth_call:verifyBeacon"]>=3,"The keeper never asked the registry to verify a relay's round");
  // Once everything is served an idle epoch costs no transaction and no second fetch of a saved round.
  const sentBeforeIdle=raw.length,hitsBeforeIdle=overrideRelay.hits.length;
  await sleep(1500);assert.equal(raw.length,sentBeforeIdle);assert.equal(overrideRelay.hits.length,hitsBeforeIdle);
  const logA=await stop(keeper);
  assert.doesNotMatch(logA,/Keeper tick failed|Epoch API preparation deferred/,logA);
  console.log(JSON.stringify({signedCatalogThenBeacon:{noRestart:true,unsupportedRecipeRefused:true,healthFault:"epoch_recipe_unsupported",removedSettingWarned:"EPOCH_API_ENDPOINTS",
    publishedSignedEpoch:"1",servedPublishedSignedEpoch:true,beaconEpochs:switched,relayRequests:overrideRelay.hits.length,verifyBeaconCalls:counts["eth_call:verifyBeacon"]}}));
  }

  // ---- (b) One relay drops connections, another serves a wrong signature, a third serves the round. ---------------------------
  const dropping=await startRelay("down"),wrong=await startRelay("wrong"),good=await startRelay("good",400);
  const failing=(relay:Relay)=>rows(`SELECT failures,open_until FROM epoch_relay_breaker WHERE url='${relay.url}'`)[0] as unknown as {failures:number;open_until:number}|undefined;
  if(selected("b")){
    const keeperB=start({DRAND_RELAYS:[dropping,wrong,good].map(r=>r.url).join(","),RUST_LOG:"d20dao_keeper=info"});
    const verifiedBefore=counts["eth_call:verifyBeacon"]??0,epochs:string[]=[];
    for(let n=0;n<3;n++){
      const epoch=await currentEpoch()+1n;await mineTo(await epochStartOf(epoch));epochs.push(String(epoch));
      await until(()=>epochRow(epoch)?.api!=null,`The beacon epoch ${epoch} was not prepared past two bad relays`);
      const id=await request(`bad-relays-${n}`);await served(id,`The request of epoch ${epoch} was not served past two bad relays`);
      await replayBeaconEpoch(epoch);
      // Each failure counts against its own relay alone, before the good relay's slower answer wins.
      assert.equal(failing(dropping)?.failures,n+1);assert.equal(failing(wrong)?.failures,n+1);
    }
    for(const relay of [dropping,wrong])assert.ok(failing(relay)!.open_until>Date.now()/1000,`Three failures did not open the circuit of ${relay.url}`);
    assert.equal(failing(good),undefined,"A relay that served the round has a circuit with failures");
    // Only the wrong and the good signature of each epoch reached the registry: the dropped connection never gave one.
    assert.equal((counts["eth_call:verifyBeacon"]??0)-verifiedBefore,6);
    // With both bad circuits open the good relay serves, and of the two only the one whose cooldown ends first is asked, once, as
    // the half-open probe: the other is left alone.
    const asked=[dropping.hits.length,wrong.hits.length,good.hits.length];
    const epoch=await currentEpoch()+1n;await mineTo(await epochStartOf(epoch));
    await until(()=>epochRow(epoch)?.api!=null,"The epoch was not prepared with two circuits open");
    const id=await request("open-circuits");await served(id,"The request was not served with two circuits open");
    const probes=[dropping.hits.length-asked[0],wrong.hits.length-asked[1]];
    assert.equal(probes[0]+probes[1],1,`Of two open circuits ${probes[0]} and ${probes[1]} requests were made: only the probe is asked`);
    assert.ok(good.hits.length>asked[2]);
    const logB=await stop(keeperB);
    for(const relay of [dropping,wrong])assert.match(logB,new RegExp(`"relay":"${relay.url}"`),`The log does not name ${relay.url}`);
    assert.match(logB,/signature does not verify/);assert.match(logB,/transport:/);assert.match(logB,/drand relay circuit opened/);
    assert.doesNotMatch(logB,/Keeper tick failed/);
    console.log(JSON.stringify({badRelays:{epochs,failures:{dropping:failing(dropping)?.failures,wrong:failing(wrong)?.failures},circuitsOpen:true,probesWhileOpen:probes[0]+probes[1]}}));
    // Fixture clock: the cooldown has passed, so the circuits are closed for the scenarios below.
    const db=openJournal(join(dir,"keeper.sqlite"));db.exec("DELETE FROM epoch_relay_breaker");db.close();
  }
  for(const relay of [dropping,wrong,good])relay.server?.close();

  // ---- (c) Every relay is down while a request waits. ------------------------------------------------------------------------
  const relayA=await startRelay("down"),relayB=await startRelay("down");
  if(selected("c")){
    const keeperC=start({DRAND_RELAYS:[relayA,relayB].map(r=>r.url).join(","),PROGRESS_STUCK_SECONDS:"2"});
    const epoch=await currentEpoch()+1n;await mineTo(await epochStartOf(epoch));
    const rawBefore=raw.length,stalledId=await request("no-relay-serves-this");
    // A plain retry is not an alarm: a few seconds of retries are not reported.
    await sleep(4000);
    assert.doesNotMatch(keeperC.log(),/Live paid demand cannot be published/,"A transient retry was reported as a stall");
    assert.ok(!(health().faults??[]).includes("epoch_stalled"));
    // Once they have gone on for about ten seconds while the demand waits, the epoch stage reports it with its reason.
    await until(()=>/"reason":"beacon_unavailable"/.test(keeperC.log()),"The keeper never reported that the beacon was unavailable",30000);
    await until(()=>(health().faults??[]).includes("epoch_stalled"),"The stall was not a health fault",15000);
    assert.equal((await registry.getEpoch(epoch)).epochHash,ethers.ZeroHash,"An epoch was published without a round");
    assert.equal(raw.length,rawBefore,"A transaction was sent for an epoch without a round");
    const waiting=epochRow(epoch)!;assert.equal(waiting.state,"pending");assert.equal(waiting.api,null);assert.match(String(waiting.last_error),/No drand relay served round/);
    assert.ok(waiting.failing_since!==null&&waiting.attempts>=3,`Only ${waiting.attempts} attempts in the wait`);
    // The request expires unserved and is refunded; then the relays come back and the next request is served, with no
    // change to the keeper or its journal: the circuits of the two relays are open, and a probe finds them working.
    await networkHelpers.time.increaseTo((await rng.getRequest(stalledId)).deadline+1n);
    await rng.refundRequest(stalledId);
    relayA.mode="good";relayB.mode="good";
    const next=await request("relays-are-back");
    await served(next,"The next request was not served once the relays were back",40000);
    assert.equal((await rng.getRequest(next)).epochId,epoch);
    await replayBeaconEpoch(epoch);
    await until(()=>health().healthy===true,"The stall did not clear once the epoch was published",15000);
    await until(()=>epochRow(epoch)?.state==="committed","The epoch was not recorded as committed");
    assert.equal(epochRow(epoch)?.failing_since,null);
    const logC=await stop(keeperC);
    assert.match(logC,/"reason":"beacon_unavailable"/);assert.match(logC,/No drand relay served round/);
    console.log(JSON.stringify({allRelaysDown:{epoch:String(epoch),reason:"beacon_unavailable",fault:"epoch_stalled",transactionsSent:0,attemptsWhileDown:waiting.attempts,servedAfterRecovery:true}}));
  }
  for(const relay of [relayA,relayB])relay.server?.close();

  // ---- (d) A packet that ages out before demand arrives is refreshed, not blocked. --------------------------------------------
  const relay=await startRelay("good");
  if(selected("d")){
    const keeperD=start({DRAND_RELAYS:relay.url,RUST_LOG:"d20dao_keeper=info"});
    const epoch=await currentEpoch()+1n;await mineTo(await epochStartOf(epoch));
    await until(()=>epochRow(epoch)?.api!=null,"The beacon epoch was not prepared");
    const first=packetRound(epochRow(epoch)!.api!),firstPacket=epochRow(epoch)!.api;
    assert.equal(first,relay.hits.at(-1));
    // Chain time moves more than 200 seconds before any demand arrives.
    await networkHelpers.time.increase(250);
    const hitsBefore=relay.hits.length;
    const id=await request("demand-after-the-packet-aged");
    await served(id,"The request was not served after its epoch's packet aged out");
    assert.equal((await rng.getRequest(id)).epochId,epoch);
    const replayed=await replayBeaconEpoch(epoch);
    assert.ok(replayed.round>first+60n,`The keeper committed round ${replayed.round} after round ${first}`);
    assert.ok(replayed.ageAtCommit<100n,`The refreshed round was ${replayed.ageAtCommit} seconds old at its commit`);
    assert.equal(relay.hits.at(-1),replayed.round);assert.ok(relay.hits.length>hitsBefore,"No fresh round was fetched");
    await until(()=>epochRow(epoch)?.state==="committed","The epoch was not recorded as committed");
    assert.notEqual(epochRow(epoch)!.api,firstPacket);
    assert.equal(blockedBeaconEpochs(switchEpoch),0,"An epoch was blocked");
    await until(()=>(health().faults??[]).length===0,"The keeper reported a fault");
    const logD=await stop(keeperD);
    assert.match(logD,/Saved beacon packet is too old, its epoch is unpublished and no commit for it is in flight; discarded/);
    assert.doesNotMatch(logD,/Live paid demand cannot be published|Keeper tick failed/);
    console.log(JSON.stringify({refreshedPacket:{epoch:String(epoch),firstRound:String(first),committedRound:String(replayed.round),ageAtCommitSeconds:String(replayed.ageAtCommit),blocked:false}}));
  }

  // ---- (e) A commit that is still in flight is never replaced, however stale its packet becomes. -----------------------------------
  if(selected("e")){
    const keeperE=start({DRAND_RELAYS:relay.url,RUST_LOG:"d20dao_keeper=info"});
    const epoch=await currentEpoch()+1n;await mineTo(await epochStartOf(epoch));
    await until(()=>epochRow(epoch)?.api!=null,"The beacon epoch was not prepared");
    const first=packetRound(epochRow(epoch)!.api!),firstPacket=epochRow(epoch)!.api,fetches=relay.hits.length;
    holdCommits=true;
    const orphan=await request("commit-signed-and-lost");
    const commitsOf=()=>rows(`SELECT payload,state FROM txs WHERE kind='epoch' AND job LIKE '%:${epoch}' ORDER BY id`) as unknown as Array<{payload:string;state:string}>;
    await until(()=>held>=1&&commitsOf().length>=1,"The keeper did not sign the commit");
    const signed=commitsOf()[0];
    // Chain time passes the beacon bound but not the registry's while some request is always live, as with steady demand: the
    // packet is 205 seconds old with its commit still in flight, and a request in the same epoch needs that commit. (With no live
    // request the keeper would cancel the commit and the packet could then be refreshed: scenario (f).)
    // Each request is waited for until the keeper has journaled it, so that no jump finds it without a live request it knows of.
    const asked=[orphan];
    for(let step=0;step<5;step++){
      await networkHelpers.time.increase(41);const id=await request(`demand-after-the-signed-commit-aged-${step}`);asked.push(id);
      await until(()=>rows(`SELECT COUNT(*) n FROM jobs WHERE id='${id}'`)[0].n===1,`The keeper did not discover request ${id}`);
    }
    const late=asked.at(-1)!;
    await sleep(3000);
    assert.equal(epochRow(epoch)!.api,firstPacket,"A packet with a commit in flight was replaced");
    assert.equal(relay.hits.length,fetches,"A packet with a commit in flight was fetched again");
    assert.equal(rows(`SELECT COUNT(*) n FROM txs WHERE kind='epoch_cancel' AND job LIKE '%:${epoch}'`)[0].n,0,"The commit was cancelled while a request was live");
    // The chain moves on again, as a live chain does, so the keeper's rebroadcast of its signed commit is due.
    holdCommits=false;await networkHelpers.time.increase(3);
    await served(late,"The request was not served with the original commit",40000);
    const replayed=await replayBeaconEpoch(epoch);
    assert.equal(replayed.round,first,"The epoch did not publish the round of the packet its commit was signed for");
    assert.ok(replayed.ageAtCommit>200n&&replayed.ageAtCommit<=240n,`The published round was ${replayed.ageAtCommit} seconds old`);
    assert.equal(relay.hits.length,fetches);
    // Every commit the keeper signed for this epoch carries the same packet; the requests that expired are refunded.
    for(const commit of commitsOf())assert.ok(commit.payload===""||commit.payload===signed.payload,"A replacement changed the commit's bytes");
    for(const id of asked.slice(0,-1)){const r=await rng.getRequest(id);if(!r.fulfilled&&!r.refunded&&r.deadline<await chainNow())await rng.refundRequest(id);}
    const logE=await stop(keeperE);
    assert.doesNotMatch(logE,/Saved beacon packet is too old|Keeper tick failed|Live paid demand cannot be published/);
    console.log(JSON.stringify({signedCommitKept:{epoch:String(epoch),round:String(first),ageAtCommitSeconds:String(replayed.ageAtCommit),commitsHeld:held,refetched:false}}));
  }

  // ---- (f) A commit that was cancelled no longer holds its epoch's packet, once its nonce has resolved. --------------------------
  // The keeper signs a commit and the network drops it. The keeper then cancels its nonce with a transaction to its own address:
  // either the request expired with the commit unmined and the packet is still young (the epoch is left prepared), or the chain
  // passed the registry's 240 second bound for the packet (the epoch is left blocked). Nothing of the epoch is published, so when
  // demand returns after the packet has aged out, a current round is committed and served instead of the epoch staying blocked.
  if(selected("f")){
    const keeperF=start({DRAND_RELAYS:relay.url,RUST_LOG:"d20dao_keeper=info"});
    const cancelled:Array<{after:string;left:string;epoch:string;firstRound:string;committedRound:string;ageAtCommitSeconds:string;refunded:boolean}>=[];
    for(const [after,left] of [["request-expired","prepared"],["packet-past-the-bound","blocked"]] as const){
      const epoch=await currentEpoch()+1n;await mineTo(await epochStartOf(epoch));
      await until(()=>epochRow(epoch)?.api!=null,`The beacon epoch ${epoch} was not prepared`);
      const first=packetRound(epochRow(epoch)!.api!),firstPacket=epochRow(epoch)!.api,fetches=relay.hits.length,heldBefore=held;
      const txsOf=()=>rows(`SELECT kind,state,payload FROM txs WHERE job LIKE '%:${epoch}' ORDER BY id`) as unknown as Array<{kind:string;state:string;payload:string}>;
      holdCommits=true;
      const orphan=await request(`commit-cancelled-after-${after}`);
      await until(()=>held>heldBefore&&txsOf().some(tx=>tx.kind==="epoch"),`The keeper did not sign the commit of epoch ${epoch}`);
      if(left==="prepared")await networkHelpers.time.increaseTo((await rng.getRequest(orphan)).deadline+1n);
      else await networkHelpers.time.increase(250);
      await networkHelpers.mine(2);
      await until(()=>txsOf().some(tx=>tx.kind==="epoch_cancel")&&txsOf().every(tx=>tx.state==="resolved"),`The commit of epoch ${epoch} was not cancelled`);
      await until(()=>epochRow(epoch)?.state===left,`The cancelled epoch ${epoch} is ${epochRow(epoch)?.state}, not ${left}`);
      assert.equal(epochRow(epoch)!.api,firstPacket,"The packet changed before any demand came back");
      assert.equal((await registry.getEpoch(epoch)).epochHash,ethers.ZeroHash,"The cancelled commit published its epoch");
      assert.equal(relay.hits.length,fetches,"The packet was fetched again before any demand came back");
      // Chain time passes the packet's bound with nothing in flight, and a request needs the same epoch's commit.
      await networkHelpers.time.increase(250);holdCommits=false;
      const late=await request(`demand-after-the-cancelled-commit-${after}`);
      await served(late,`The request of epoch ${epoch} was not served after its commit was cancelled`,40000);
      assert.equal((await rng.getRequest(late)).epochId,epoch);assert.equal((await rng.getRequest(late)).refunded,false);
      const replayed=await replayBeaconEpoch(epoch);
      assert.ok(replayed.round>first+60n,`The keeper committed round ${replayed.round} after round ${first}`);
      assert.ok(replayed.ageAtCommit<100n,`The refreshed round was ${replayed.ageAtCommit} seconds old at its commit`);
      assert.equal(relay.hits.at(-1),replayed.round);assert.ok(relay.hits.length>fetches,"No fresh round was fetched");
      await until(()=>epochRow(epoch)?.state==="committed","The epoch was not recorded as committed");
      assert.notEqual(epochRow(epoch)!.api,firstPacket);
      // The commit that was signed and cancelled never reached the chain: a second commit, for the round replayed above, published.
      assert.ok(txsOf().filter(tx=>tx.kind==="epoch").length>=2&&txsOf().every(tx=>tx.state==="resolved"));
      // The request that expired with its commit is the fixture's to refund; the one that came back was served, not refunded.
      await rng.refundRequest(orphan);
      cancelled.push({after,left,epoch:String(epoch),firstRound:String(first),committedRound:String(replayed.round),ageAtCommitSeconds:String(replayed.ageAtCommit),refunded:(await rng.getRequest(late)).refunded});
    }
    assert.equal(blockedBeaconEpochs(switchEpoch),0,"An epoch was left blocked");
    await until(()=>(health().faults??[]).length===0,"The keeper reported a fault");
    const logF=await stop(keeperF);
    assert.match(logF,/Saved beacon packet is too old, its epoch is unpublished and no commit for it is in flight; discarded/);
    assert.doesNotMatch(logF,/Live paid demand cannot be published|Keeper tick failed/);
    console.log(JSON.stringify({cancelledCommitRefreshed:cancelled}));
  }

  // ---- (g) A relay that answers 404 for every round: an old round is due, so it is at fault and its circuit opens. ---------------------
  // Each epoch is left alone for 30 seconds of chain time, without a keeper, before the keeper first looks at it: the round current at
  // its start has been due for ten rounds by then. The other relay serves it, and the keeper publishes.
  if(selected("g")){
    const missing=await startRelay("missing"),serving=await startRelay("good");
    const epochs:string[]=[];let logG="";
    for(let n=0;n<3;n++){
      const epoch=await currentEpoch()+1n;await mineTo(await epochStartOf(epoch));await networkHelpers.time.increase(30);
      const keeperG=start({DRAND_RELAYS:[missing,serving].map(r=>r.url).join(","),RUST_LOG:"d20dao_keeper=info"});
      await until(()=>epochRow(epoch)?.api!=null,`The beacon epoch ${epoch} was not prepared past a relay that answers 404`);
      const id=await request(`relay-without-rounds-${n}`);await served(id,`The request of epoch ${epoch} was not served past a relay that answers 404`);
      const replayed=await replayBeaconEpoch(epoch);assert.equal(replayed.round,beaconRoundAt(beacon,await startBlockTime(epoch)));
      // Each epoch adds one failure against it, before or after the good relay's answer is in.
      await until(()=>failing(missing)?.failures===n+1,`The 404 relay has ${failing(missing)?.failures} failures after ${n+1} epochs`);
      assert.equal(failing(serving),undefined,"A relay that served the round has a circuit with failures");
      epochs.push(String(epoch));logG+=await stop(keeperG);
    }
    assert.ok(failing(missing)!.open_until>Date.now()/1000,"Three answers of 404 for a round that was due did not open the circuit");
    assert.match(logG,/round already due but not served/);assert.match(logG,/drand relay circuit opened/);assert.doesNotMatch(logG,/Keeper tick failed/);
    console.log(JSON.stringify({relayAnsweringNotFound:{epochs,failures:failing(missing)?.failures,circuitOpen:true,publishedThroughTheOtherRelay:true}}));
    for(const relay of [missing,serving])relay.server?.close();
  }

  // ---- (h) A relay that is valid but always slower than another loses every race, and is credited for it all the same. ---------------
  // Its failures are forgiven once it serves the round, so a single 503 afterwards is its first failure, not the one that opens it.
  if(selected("h")){
    const fast=await startRelay("good"),slow=await startRelay("down");
    const keeperH=start({DRAND_RELAYS:[fast,slow].map(r=>r.url).join(","),RUST_LOG:"d20dao_keeper=info"});
    const prepared=async(what:string)=>{const epoch=await currentEpoch()+1n;await mineTo(await epochStartOf(epoch));await until(()=>epochRow(epoch)?.api!=null,`The beacon epoch ${epoch} was not prepared (${what})`);return epoch;};
    // Two epochs in which its connections are dropped: two failures, one short of opening its circuit.
    for(const failures of [1,2]){await prepared(`dropped connection ${failures}`);await until(()=>failing(slow)?.failures===failures,`The slow relay has ${failing(slow)?.failures} failures, not ${failures}`);}
    // It serves the round now, 600 ms after the fast relay has won it: the keeper does not wait for it, and still credits its answer.
    slow.mode="good";slow.delayMs=600;const hits=slow.hits.length;
    await prepared("slow and valid");
    await until(()=>failing(slow)===undefined,"The valid answer of the slow relay did not close its circuit");
    assert.ok(slow.hits.length>hits,"The slow relay was not asked");
    // One 503 after that is a first failure: without the credit it would be the third in a row and open the circuit.
    slow.mode="error";slow.delayMs=0;
    await prepared("one error");await until(()=>failing(slow)?.failures===1,`The slow relay has ${failing(slow)?.failures} failures after a valid answer and one 503`);
    assert.ok(failing(slow)!.open_until<=Date.now()/1000,"One failure after a valid answer opened the circuit");
    const logH=await stop(keeperH);assert.doesNotMatch(logH,/Keeper tick failed|drand relay circuit opened/);
    console.log(JSON.stringify({slowValidRelay:{failuresBeforeItServed:2,failuresAfterTheNextError:failing(slow)?.failures,circuitOpen:false}}));
    for(const relay of [fast,slow])relay.server?.close();
  }

  // ---- (i) A relay that long-polls: it holds the request for three seconds and then serves the round, which is not a failure. -----------
  // Beside it, one that never answers is timed out at four seconds for a round that is recent, which counts against nobody.
  if(selected("i")){
    const polling=await startRelay("good",3000),silent=await startRelay("good",30000);
    const keeperI=start({DRAND_RELAYS:[polling,silent].map(r=>r.url).join(","),RUST_LOG:"d20dao_keeper=info"});
    const epoch=await currentEpoch()+1n;await mineTo(await epochStartOf(epoch));const began=Date.now();
    await until(()=>epochRow(epoch)?.api!=null,"The round of the long-polling relay was not used");
    assert.ok(Date.now()-began>=2500,`The round was used after ${Date.now()-began} ms, before the long-polling relay could have served it`);
    assert.equal(packetRound(epochRow(epoch)!.api!),beaconRoundAt(beacon,await startBlockTime(epoch)));
    const id=await request("long-polling-relay");await served(id,"The request was not served with a long-polling relay");await replayBeaconEpoch(epoch);
    // The relay that never answers is given up on at four seconds of the fetch, which are over well before this.
    await sleep(2000);
    assert.equal(failing(polling),undefined,"A relay that held the request for three seconds and served the round has failures");
    assert.equal(failing(silent),undefined,"A relay that has not answered a recent round in time has failures");
    assert.equal(polling.hits.length,1);assert.equal(silent.hits.length,1);
    const logI=await stop(keeperI);
    for(const relay of [polling,silent])assert.doesNotMatch(logI,new RegExp(`"relay":"${relay.url}"`),`The log reports a failure of ${relay.url}`);
    assert.doesNotMatch(logI,/Keeper tick failed|Epoch API preparation deferred/);
    console.log(JSON.stringify({longPollingRelay:{epoch:String(epoch),heldForMs:3000,failures:0,silentRelayFailures:0}}));
    for(const relay of [polling,silent])relay.server?.close();
  }
  relay.server?.close();
  assert.deepEqual(unexpected,[],"The fixture servers saw requests they did not expect");
  console.log(JSON.stringify({passed:true,rpcCounts:counts,fixtureDirectory:dir}));
} finally {
  for(const child of children)child.kill();
  for(const relay of relays){relay.server?.closeAllConnections();relay.server?.close();}
  server.closeAllConnections();server.close();
}
