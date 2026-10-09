// The round replay library (src/round.ts) against the round coordinator's own outputs: the replay vectors that
// test/robinhood/ReplayVectors.test.ts builds on a local chain from real drand evmnet rounds and checks against the contract.
import {test} from "node:test";
import assert from "node:assert/strict";
import {readFileSync} from "node:fs";
import {Interface,ZeroHash,getAddress,id,toBeHex} from "ethers";
import type {MappingSpec} from "../../src/mapping.ts";
import type {VRFProof} from "../../src/vrf.ts";
import {ROUND_BEACON_DOMAIN,ROUND_CONFIG_DOMAIN,ROUND_LEAD,ROUND_RESPONSE_TIMEOUT,ROUND_SEED_DOMAIN,ROUND_TRANSCRIPT_DOMAIN,replayRoundRequest,
  roundAcceptedInTime,roundAt,roundBeaconIdentity,roundConfigurationHash,roundDeadline,roundFor,roundRandomness,roundScheduleFromEvents,roundSeed,
  roundTime,roundTranscriptHash,verifyRoundSignature,type RegisteredRoundBeacon,type RoundBeaconLog,type RoundCheckName,
  type RoundRequestEvidence} from "../../src/round.ts";
import * as index from "../../src/index.ts";

const V=JSON.parse(readFileSync(new URL("../../test/fixtures/robinhood/round-replay-vectors.json",import.meta.url),"utf8"));
const big=(values:string[])=>values.map(value=>BigInt(value));
const proofOf=(p:any):VRFProof=>({pk:[BigInt(p.pk[0]),BigInt(p.pk[1])],gamma:[BigInt(p.gamma[0]),BigInt(p.gamma[1])],c:BigInt(p.c),s:BigInt(p.s),
  seed:BigInt(p.seed),uWitness:p.uWitness,cGammaWitness:[BigInt(p.cGammaWitness[0]),BigInt(p.cGammaWitness[1])],
  sHashWitness:[BigInt(p.sHashWitness[0]),BigInt(p.sHashWitness[1])],zInv:BigInt(p.zInv)});
const mappingOf=(m:any):MappingSpec=>({operation:m.operation,lower:BigInt(m.lower),upper:BigInt(m.upper),count:m.count,population:m.population});
const B=V.beacons[0];
const BEACON:RegisteredRoundBeacon={verifier:B.verifier,chainHash:B.chainHash,publicKey:B.publicKey,genesis:BigInt(B.genesis),period:BigInt(B.period),identity:B.identity};
const CONFIGURATION={publicKey:[BigInt(V.protocolConfiguration.publicKey[0]),BigInt(V.protocolConfiguration.publicKey[1])] as const,
  feeRecipient:V.protocolConfiguration.feeRecipient,initialMinFee:BigInt(V.protocolConfiguration.initialMinFee),beaconIdentity:V.protocolConfiguration.beaconIdentity};
/// A vector as the evidence a replay takes.
function evidenceOf(r:any):RoundRequestEvidence{
  return {chainId:BigInt(V.chainId),coordinator:V.coordinator,configuration:CONFIGURATION,configurationHash:V.protocolConfiguration.hash,keyHash:V.keyHash,
    beacon:BEACON,requestId:BigInt(r.requestId),consumer:r.consumer,clientSeed:r.clientSeed,mapping:mappingOf(r.mapping),mappingHash:r.mappingHash,
    requestBlock:BigInt(r.requestBlock),requestTimestamp:BigInt(r.requestTimestamp),deadline:BigInt(r.deadline),beaconId:r.beaconId,round:BigInt(r.round),
    roundSignature:r.roundSignature,roundRandomness:r.roundRandomness,proof:proofOf(r.proof),randomness:r.randomness,proofHash:r.proofHash,
    transcriptHash:r.transcriptHash,evidencePacket:r.evidencePacket,mappedResult:big(r.mappedResult),acceptanceTimestamp:BigInt(r.acceptanceTimestamp)};
}

test("every replay vector replays, and the replay recomputes the contract's seed, round time and mapped result",()=>{
  assert.equal(V.vectors.length,7);
  for(const r of V.vectors){
    const verdict=replayRoundRequest(evidenceOf(r));
    assert.deepEqual(verdict.failed,[],r.name);
    assert.equal(verdict.valid,true);
    assert.deepEqual(verdict.checks.map(c=>c.name),["keyHash","configuration","beaconIdentity","assignment","roundSignature","roundRandomness","seed","proof",
      "randomness","proofHash","evidencePacket","transcript","mappingHash","mappedResult","deadline","acceptance"]);
    assert.equal(verdict.seed,BigInt(r.seed),r.name);
    assert.equal(verdict.roundTime,BigInt(r.roundTime));
    assert.equal(verdict.roundLeadSeconds,BigInt(r.roundTime)-BigInt(r.requestTimestamp));
    assert.deepEqual(verdict.mappedResult,big(r.mappedResult));
  }
});

test("the identity, configuration, seed, transcript and round randomness equal the contract's",()=>{
  assert.deepEqual([ROUND_SEED_DOMAIN,ROUND_TRANSCRIPT_DOMAIN,ROUND_CONFIG_DOMAIN,ROUND_BEACON_DOMAIN],
    [V.domains.seed,V.domains.transcript,V.domains.configuration,V.domains.beacon]);
  assert.equal(ROUND_LEAD,BigInt(V.roundLead));
  assert.equal(ROUND_RESPONSE_TIMEOUT,BigInt(V.responseTimeout));
  assert.equal(roundBeaconIdentity(BEACON),B.identity);
  assert.equal(roundConfigurationHash(CONFIGURATION),V.protocolConfiguration.hash);
  for(const r of V.vectors){
    const fields={chainId:BigInt(V.chainId),coordinator:V.coordinator,requestId:BigInt(r.requestId),beaconId:r.beaconId,round:BigInt(r.round),
      roundRandomness:r.roundRandomness,mappingHash:r.mappingHash};
    assert.equal(roundSeed({...fields,keyHash:V.keyHash,consumer:r.consumer,clientSeed:r.clientSeed,requestBlock:BigInt(r.requestBlock)}),BigInt(r.seed),r.name);
    assert.equal(roundTranscriptHash({...fields,configurationHash:V.protocolConfiguration.hash,proofHash:r.proofHash,randomness:r.randomness}),r.transcriptHash);
    assert.equal(roundRandomness(r.roundSignature),r.roundRandomness);
    assert.equal(verifyRoundSignature(B.publicKey,BigInt(r.round),r.roundSignature),true);
    assert.equal(verifyRoundSignature(B.publicKey,BigInt(r.round)+1n,r.roundSignature),false);
    assert.equal(roundFor(BEACON,BigInt(r.requestTimestamp)),BigInt(r.round));
    assert.equal(roundTime(BEACON,BigInt(r.round)),BigInt(r.roundTime));
    assert.equal(roundDeadline(BigInt(r.requestTimestamp)),BigInt(r.deadline));
  }
});

// What each evidence field feeds: the check that must fail when the field alone is wrong, and every check that may fail with it.
const FEEDS:Array<[RegExp,RoundCheckName,RoundCheckName[]]>=[
  [/^chainId$|^coordinator$|^requestId$/,"seed",["seed","transcript"]],
  [/^configuration\.publicKey\.\d$/,"configuration",["keyHash","configuration","proof"]],
  [/^configuration\.(feeRecipient|initialMinFee)$/,"configuration",["configuration"]],
  [/^configuration\.beaconIdentity$/,"configuration",["configuration","beaconIdentity"]],
  [/^configurationHash$/,"configuration",["configuration","transcript"]],
  [/^keyHash$/,"keyHash",["keyHash","seed"]],
  [/^beacon\.(verifier|chainHash|identity)$/,"beaconIdentity",["beaconIdentity"]],
  [/^beacon\.publicKey$/,"beaconIdentity",["beaconIdentity","roundSignature"]],
  [/^beacon\.(genesis|period)$/,"beaconIdentity",["beaconIdentity","assignment"]],
  [/^consumer$|^clientSeed$|^requestBlock$/,"seed",["seed"]],
  [/^mapping\.\w+$/,"mappingHash",["mappingHash","mappedResult"]],
  [/^mappingHash$/,"mappingHash",["mappingHash","seed","transcript"]],
  [/^requestTimestamp$/,"deadline",["deadline","assignment","acceptance"]],
  [/^deadline$/,"deadline",["deadline","acceptance"]],
  [/^beaconId$/,"seed",["seed","transcript","beaconIdentity"]],
  [/^round$/,"assignment",["assignment","roundSignature","seed","transcript"]],
  [/^roundSignature$/,"roundSignature",["roundSignature","roundRandomness"]],
  [/^roundRandomness$/,"roundRandomness",["roundRandomness","seed","transcript"]],
  [/^proof\.gamma\.\d$/,"proof",["proof","randomness","proofHash","evidencePacket"]],
  [/^proof\.seed$/,"seed",["seed","proof","proofHash","evidencePacket"]],
  [/^proof\.(pk\.\d|c|s|uWitness|cGammaWitness\.\d|sHashWitness\.\d|zInv)$/,"proof",["proof","proofHash","evidencePacket"]],
  [/^randomness$/,"randomness",["randomness","transcript","mappedResult"]],
  [/^proofHash$/,"proofHash",["proofHash","transcript"]],
  [/^transcriptHash$/,"transcript",["transcript"]],
  [/^evidencePacket$/,"evidencePacket",["evidencePacket"]],
  [/^mappedResult\.\d+$/,"mappedResult",["mappedResult"]],
  [/^acceptanceTimestamp$/,"acceptance",["acceptance"]],
];
/// Every scalar field of the evidence, by its path.
function leaves(value:unknown,path:string[]=[]):string[][]{
  if(value!==null&&typeof value==="object")return Object.entries(value).flatMap(([key,inner])=>leaves(inner,[...path,key]));
  return [path];
}
/// The evidence with one field changed: an integer by one, a hex string in its last bit, a timestamp past what it may be.
function corrupt(e:RoundRequestEvidence,path:string[]):RoundRequestEvidence{
  const copy=structuredClone(e) as any;
  let parent=copy;
  for(const key of path.slice(0,-1))parent=parent[key];
  const key=path[path.length-1],value=parent[key];
  parent[key]=path.join(".")==="acceptanceTimestamp"?e.deadline+1n:typeof value==="bigint"?value+1n:typeof value==="number"?value+1
    :toBeHex(BigInt(value)^1n,(value.length-2)/2);
  return copy;
}

test("a replay fails when any single field of the evidence is wrong, naming the check that reads it and no check it does not feed",()=>{
  for(const r of V.vectors){
    const e=evidenceOf(r),paths=leaves(e);
    assert.ok(paths.length>=50,r.name);
    for(const path of paths){
      const field=path.join("."),feeds=FEEDS.find(([pattern])=>pattern.test(field));
      assert.ok(feeds,`no expectation for ${field}`);
      const [,primary,allowed]=feeds;
      const verdict=replayRoundRequest(corrupt(e,path));
      assert.equal(verdict.valid,false,`${r.name}: ${field}`);
      assert.ok(verdict.failed.includes(primary),`${r.name}: ${field} fails ${verdict.failed.join(", ")}, not ${primary}`);
      for(const name of verdict.failed)assert.ok(allowed.includes(name),`${r.name}: ${field} also fails ${name}`);
    }
  }
});

test("a replay never throws on malformed evidence: it fails the checks that read it",()=>{
  const e=evidenceOf(V.vectors[1]);
  const broken={...e,coordinator:"not an address",roundSignature:"0x1234",mapping:{...e.mapping,operation:9},proof:{...e.proof,c:-1n},evidencePacket:"0x"};
  const verdict=replayRoundRequest(broken);
  assert.equal(verdict.valid,false);
  for(const name of ["seed","transcript","roundSignature","roundRandomness","mappingHash","mappedResult","proof","proofHash","evidencePacket"] as const)
    assert.ok(verdict.failed.includes(name),name);
  assert.equal(verdict.seed,undefined);
  assert.equal(verdict.mappedResult,undefined);
});

test("assignment binds the first round scheduled at least ROUND_LEAD seconds after the request, round 1 up to genesis, and no round 0",()=>{
  const beacon={genesis:1_000n,period:3n};
  assert.equal(roundFor(beacon,0n),1n);
  assert.equal(roundFor(beacon,997n),1n); // 997 + 3 = genesis
  assert.equal(roundFor(beacon,998n),2n);
  for(let t=990n;t<1_100n;t++){
    const r=roundFor(beacon,t);
    assert.ok(roundTime(beacon,r)>=t+ROUND_LEAD,`${t}`);
    if(r>1n)assert.ok(roundTime(beacon,r-1n)<t+ROUND_LEAD,`${t}`);
  }
  assert.equal(roundFor(beacon,1_000n,0n),1n);
  assert.equal(roundFor(beacon,1_001n,0n),2n);
  assert.throws(()=>roundTime(beacon,0n),/Invalid round/);
  assert.throws(()=>roundFor(beacon,-1n),/Invalid timestamp/);
  assert.throws(()=>roundFor({genesis:1n,period:0n},5n),/Invalid beacon schedule/);
  // Accepted from the request's own second to its deadline, the deadline included.
  assert.deepEqual([99n,100n,160n,161n].map(at=>roundAcceptedInTime(100n,at)),[false,true,true,false]);
});

// ---- the schedule from events
const BOOK=new Interface([
  "event BeaconRegistered(uint8 indexed beaconId, bytes32 indexed identity, address indexed verifier, bytes32 chainHash, bytes publicKey, uint64 genesis, uint64 period)",
  "event BeaconScheduled(uint8 indexed beaconId, uint64 indexed fromTime)",
  "event BeaconScheduleCancelled(uint8 indexed beaconId, uint64 indexed fromTime)",
  "event RoundVerified(uint8 indexed beaconId, uint64 indexed round, bytes32 randomness, bytes signature)",
]);
const COORDINATOR=getAddress(V.coordinator);
/// A beacon like the vectors' on another drand network, with its own genesis and period.
const other=(name:string,genesis:bigint,period:bigint):RegisteredRoundBeacon=>{
  const b={...BEACON,chainHash:id(name),genesis,period};
  return {...b,identity:roundBeaconIdentity(b)};
};
const ONE=other("network one",1_700_000_000n,2n),TWO=other("network two",1_700_000_500n,7n);
/// Logs of a coordinator, one per block, at the given block timestamps.
function chain(){
  const logs:RoundBeaconLog[]=[];
  const add=(name:string,args:unknown[],time:bigint,address=COORDINATOR)=>{
    const {data,topics}=BOOK.encodeEventLog(name,args);
    logs.push({address,topics,data,blockNumber:BigInt(logs.length+1),logIndex:0,blockTimestamp:time});
  };
  return {
    logs,add,
    register:(beaconId:number,b:RegisteredRoundBeacon,time:bigint)=>add("BeaconRegistered",[beaconId,b.identity,b.verifier,b.chainHash,b.publicKey,b.genesis,b.period],time),
    schedule:(beaconId:number,from:bigint,time:bigint)=>add("BeaconScheduled",[beaconId,from],time),
    cancel:(beaconId:number,from:bigint,time:bigint)=>add("BeaconScheduleCancelled",[beaconId,from],time),
  };
}
const INIT=1_790_000_000n;
/// A coordinator initialized at INIT with beacon 0, and beacons 1 and 2 registered an hour later.
function initialized(){
  const c=chain();
  c.register(0,BEACON,INIT);c.schedule(0,INIT,INIT);
  c.register(1,ONE,INIT+3_600n);c.register(2,TWO,INIT+3_600n);
  return c;
}
const beaconAt=(logs:RoundBeaconLog[],t:bigint)=>roundAt(roundScheduleFromEvents(COORDINATOR,logs),t).beaconId;

test("the schedule from events: beacon 0 from initialization, before it too, and a change from exactly its fromTime",()=>{
  const c=initialized();
  let s=roundScheduleFromEvents(COORDINATOR,c.logs);
  assert.deepEqual(s.beacons.map(b=>b.identity),[BEACON.identity,ONE.identity,TWO.identity]);
  assert.deepEqual([s.eras,s.pending],[[{beaconId:0,since:INIT}],undefined]);
  for(const t of [0n,INIT-1n,INIT,INIT+10n**6n])assert.deepEqual(roundAt(s,t),{beaconId:0,round:roundFor(BEACON,t),beacon:s.beacons[0]});
  const from=INIT+7_200n;
  c.schedule(1,from,INIT+7_200n-600n);
  s=roundScheduleFromEvents(COORDINATOR,c.logs);
  assert.deepEqual(s.pending,{beaconId:1,fromTime:from});
  assert.deepEqual(roundAt(s,from-1n),{beaconId:0,round:roundFor(BEACON,from-1n),beacon:s.beacons[0]});
  assert.deepEqual(roundAt(s,from),{beaconId:1,round:roundFor(ONE,from),beacon:s.beacons[1]});
  // Logs of other events, and their order in the input, do not matter.
  c.add("RoundVerified",[0,5n,ZeroHash,"0x"],from+1n);
  assert.deepEqual(roundScheduleFromEvents(COORDINATOR,[...c.logs].reverse()),s);
});

test("a schedule made before the pending change takes effect replaces it, with no cancellation event",()=>{
  const c=initialized();
  const first=INIT+7_200n,second=INIT+9_000n;
  c.schedule(1,first,INIT+4_000n);
  c.schedule(2,second,first-1n); // one second before the first change: it is replaced
  const s=roundScheduleFromEvents(COORDINATOR,c.logs);
  assert.deepEqual([s.eras,s.pending],[[{beaconId:0,since:INIT}],{beaconId:2,fromTime:second}]);
  for(const [t,beaconId] of [[first-1n,0],[first,0],[second-1n,0],[second,2]] as const)assert.equal(roundAt(s,t).beaconId,beaconId,`${t}`);
});

test("a schedule made once the pending change has taken effect keeps it as an era, and roundAt answers across every era",()=>{
  const c=initialized();
  const first=INIT+7_200n,second=INIT+9_000n,third=INIT+20_000n;
  c.schedule(1,first,INIT+4_000n);
  c.schedule(2,second,first); // made at the second the first change took effect
  c.schedule(0,third,second+5_000n);
  const s=roundScheduleFromEvents(COORDINATOR,c.logs);
  assert.deepEqual(s.eras,[{beaconId:0,since:INIT},{beaconId:1,since:first},{beaconId:2,since:second}]);
  assert.deepEqual(s.pending,{beaconId:0,fromTime:third});
  const expected:Array<[bigint,number,RegisteredRoundBeacon]>=[[0n,0,BEACON],[INIT,0,BEACON],[first-1n,0,BEACON],[first,1,ONE],[second-1n,1,ONE],
    [second,2,TWO],[third-1n,2,TWO],[third,0,BEACON],[third+10n**6n,0,BEACON]];
  for(const [t,beaconId,beacon] of expected)assert.deepEqual(roundAt(s,t),{beaconId,round:roundFor(beacon,t),beacon:s.beacons[beaconId]},`${t}`);
});

test("a cancellation drops the pending change until the second before it takes effect",()=>{
  const c=initialized();
  const from=INIT+7_200n;
  c.schedule(1,from,INIT+4_000n);
  c.cancel(1,from,from-1n);
  const s=roundScheduleFromEvents(COORDINATOR,c.logs);
  assert.deepEqual([s.eras,s.pending],[[{beaconId:0,since:INIT}],undefined]);
  assert.equal(roundAt(s,from+10n**6n).beaconId,0);
  // After a cancellation a new change can be scheduled, and it takes effect.
  c.schedule(2,from+600n,from);
  assert.equal(beaconAt(c.logs,from+600n),2);
});

test("the schedule refuses logs the coordinator could not have emitted, from another address, repeated, removed or reordered in time",()=>{
  const refused=(name:string,build:(c:ReturnType<typeof chain>)=>void,message:RegExp)=>{
    const c=chain();
    build(c);
    assert.throws(()=>roundScheduleFromEvents(COORDINATOR,c.logs),message,name);
  };
  const init=(c:ReturnType<typeof chain>)=>{c.register(0,BEACON,INIT);c.schedule(0,INIT,INIT);c.register(1,ONE,INIT+1n);};
  refused("no initialization",c=>c.register(0,BEACON,INIT),/do not reach initialization/);
  refused("a first schedule that is not its block's time",c=>{c.register(0,BEACON,INIT);c.schedule(0,INIT+1n,INIT);},/first beacon schedule/);
  refused("a registration out of order",c=>{init(c);c.register(3,TWO,INIT+2n);},/Invalid beacon registration/);
  refused("a registration whose identity is not its own",c=>{init(c);c.register(2,{...TWO,identity:ONE.identity},INIT+2n);},/Invalid beacon registration/);
  refused("a registration twice",c=>{init(c);c.register(2,ONE,INIT+2n);},/Invalid beacon registration/);
  refused("a period above ROUND_MAX_BEACON_PERIOD",c=>{init(c);c.register(2,other("slow",1n,11n),INIT+2n);},/Invalid beacon registration/);
  refused("an unregistered beacon",c=>{init(c);c.schedule(2,INIT+10_000n,INIT+2n);},/unregistered beacon/);
  refused("a change less than ROUND_MIN_SCHEDULE_LEAD ahead",c=>{init(c);c.schedule(1,INIT+2n+599n,INIT+2n);},/Invalid beacon schedule/);
  refused("a change beyond uint40",c=>{init(c);c.schedule(1,1n<<40n,INIT+2n);},/Invalid beacon schedule/);
  refused("a cancellation with nothing pending",c=>{init(c);c.cancel(1,INIT+1_000n,INIT+2n);},/cancellation/);
  refused("a cancellation of another change",c=>{init(c);c.schedule(1,INIT+1_000n,INIT+2n);c.cancel(1,INIT+1_001n,INIT+3n);},/cancellation/);
  refused("a cancellation once the change took effect",c=>{init(c);c.schedule(1,INIT+1_000n,INIT+2n);c.cancel(1,INIT+1_000n,INIT+1_000n);},/cancellation/);
  refused("a log from another address",c=>{init(c);c.add("BeaconScheduled",[1,INIT+1_000n],INIT+2n,getAddress(`0x${"11".repeat(20)}`));},/another address/);
  refused("a repeated log",c=>{init(c);c.logs.push({...c.logs[2]});},/repeated/);
  refused("a removed log",c=>{init(c);c.logs[2]={...c.logs[2],removed:true};},/removed/);
  refused("a later block with an earlier timestamp",c=>{init(c);c.schedule(1,INIT+1_000n,INIT-1n);},/go back/);
  refused("extra data",c=>{init(c);c.logs[2]={...c.logs[2],data:c.logs[2].data+"00"};},/Invalid beacon event|Noncanonical/);
  refused("a log without its block's timestamp",c=>{init(c);c.logs[2]={...c.logs[2],blockTimestamp:"soon"};},/block timestamp/);
});

test("given the schedule, a replay also checks that the request's beacon was in force",()=>{
  const r=V.vectors[0],e=evidenceOf(r),t=BigInt(r.requestTimestamp);
  const c=chain();
  c.register(0,BEACON,t-7_200n);c.schedule(0,t-7_200n,t-7_200n);
  const verdict=replayRoundRequest(e,{schedule:roundScheduleFromEvents(COORDINATOR,c.logs)});
  assert.deepEqual(verdict.failed,[]);
  assert.ok(verdict.checks.some(check=>check.name==="beaconInForce"&&check.ok));
  // Another beacon in force from the request's second: its beacon was not.
  c.register(1,ONE,t-7_000n);c.schedule(1,t,t-600n);
  const refused=replayRoundRequest(e,{schedule:roundScheduleFromEvents(COORDINATOR,c.logs)});
  assert.deepEqual(refused.failed,["beaconInForce"]);
});

test("src/index.ts exports the round API under round names only, beside every name it exported before",()=>{
  const names=Object.keys(index);
  const round=["ROUND_SEED_DOMAIN","ROUND_TRANSCRIPT_DOMAIN","ROUND_CONFIG_DOMAIN","ROUND_BEACON_DOMAIN","ROUND_LEAD","ROUND_RESPONSE_TIMEOUT",
    "ROUND_MIN_SCHEDULE_LEAD","ROUND_MAX_BEACON_PERIOD","ROUND_MAX_BEACONS","roundTime","roundFor","roundBeaconIdentity","roundScheduleFromEvents","roundAt",
    "roundRandomness","verifyRoundSignature","roundSeed","roundTranscriptHash","roundConfigurationHash","roundDeadline","roundAcceptedInTime","replayRoundRequest"];
  for(const name of round)assert.ok(names.includes(name),name);
  for(const name of names.filter(name=>/round/i.test(name)&&!round.includes(name)))
    assert.match(name,/^(beacon|encodeBeacon|decodeBeacon|verifyBeacon)Round/,`${name} is neither a round name nor one of the beacon helpers`);
  assert.equal(index.replayRoundRequest,replayRoundRequest);
  assert.equal(index.ROUND_BEACON_DOMAIN,id("D20_ROUND_BEACON_V1"));
});
