import {test} from "node:test";
import assert from "node:assert/strict";
import {mkdtempSync,readFileSync,writeFileSync} from "node:fs";
import {spawnSync} from "node:child_process";
import {tmpdir} from "node:os";
import {join} from "node:path";
import {createServer,type ServerResponse} from "node:http";
import type {AddressInfo} from "node:net";
import {Contract,FetchRequest,JsonRpcProvider,Network,getAddress,id,toBeHex,zeroPadValue,type Log} from "ethers";
import {DRAND_EVMNET,beaconCanonicalRequest,beaconRoundTime,beaconSlotSigner,type BeaconRegistration} from "../../src/beacon.ts";
import {BUILTIN_EPOCH_RECIPES} from "../../src/epoch.ts";
import {Stop} from "../lib/deployment.ts";
import {BEACON_PRESETS,DEFAULT_RELAY,SAMPLE_LAG,checkBeaconSample,fetchBeaconSample,relayBase} from "../lib/drand-relay.ts";
import {answerOrMissing,findDuplicateBeacon,isEmptyRevert,parseRegisterBeaconOptions,registerBeaconArgs} from "../lib/beacon-admin.ts";
import {checkBeaconRecipes,loadServiceCatalog,requireExplicitSchedule,resolveSlotSigners} from "../lib/catalog.ts";
import {LOG_PAGE_BLOCKS,readBackupCommitters,type LogSource} from "../lib/backup-committers.ts";
import {calmReads,isRateLimited} from "../lib/calm-rpc.ts";

// Real evmnet chain info and rounds, as four public relays served them on 2026-09-29.
const fixture=JSON.parse(readFileSync("test/fixtures/drand-evmnet-2026-09-29.json","utf8")) as {info:Record<string,unknown>;rounds:Array<{round:number;randomness:string;signature:string}>};
const preset=BEACON_PRESETS.evmnet,HASH=preset.chainHash.slice(2),RELAY="https://relay.example/drand";
const rounds=new Map(fixture.rounds.map(record=>[record.round,record]));
const ROUND=21056750,LATEST=ROUND+Number(SAMPLE_LAG);
const json=(body:unknown,status=200)=>new Response(JSON.stringify(body),{status,headers:{"content-type":"application/json"}});
const rejectsStop=(promise:Promise<unknown>,message:RegExp)=>assert.rejects(promise,(error:Error)=>error instanceof Stop&&message.test(error.message),String(message));
type Answer=(path:string,init?:RequestInit)=>Response|undefined|Promise<Response|undefined>;
/// A relay serving the fixture's chain info, its rounds and `LATEST` as its latest round; `override` may answer a path itself. Every request is recorded.
function fakeRelay(override:Answer=()=>undefined){
  const requests:string[]=[],signals:Array<AbortSignal|null|undefined>=[];
  const send:typeof fetch=async(input,init)=>{
    const url=String(input),path=url.slice(RELAY.length);
    requests.push(url);signals.push(init?.signal);
    const custom=await override(path,init);
    if(custom)return custom;
    if(path===`/${HASH}/info`)return json(fixture.info);
    if(path===`/${HASH}/public/latest`)return json({round:LATEST,signature:"00"});
    const round=/^\/[0-9a-f]{64}\/public\/(\d+)$/.exec(path)?.[1],record=round===undefined?undefined:rounds.get(Number(round));
    return record?json({round:record.round,randomness:record.randomness,signature:record.signature}):json({error:"not found"},404);
  };
  return {send,requests,signals};
}
const sample=(relay:ReturnType<typeof fakeRelay>,options:{round?:bigint;timeoutMs?:number}={})=>fetchBeaconSample(RELAY,preset,{...options,fetch:relay.send});

test("a relay sample is the round 100 behind its latest, read from the paths of drand's HTTP API and verified",async()=>{
  const relay=fakeRelay(),got=await sample(relay);
  assert.deepEqual(got,{round:BigInt(ROUND),signature:`0x${rounds.get(ROUND)!.signature}`});
  assert.deepEqual(relay.requests,[`${RELAY}/${HASH}/info`,`${RELAY}/${HASH}/public/latest`,`${RELAY}/${HASH}/public/${ROUND}`]);
  for(const signal of relay.signals)assert.ok(signal instanceof AbortSignal,"every request is bounded in time");
  // A requested round is fetched without asking for the latest, and a trailing slash on the relay makes no difference.
  const explicit=fakeRelay();
  assert.equal((await fetchBeaconSample(`${RELAY}/`,preset,{round:21056714n,fetch:explicit.send})).round,21056714n);
  assert.deepEqual(explicit.requests,[`${RELAY}/${HASH}/info`,`${RELAY}/${HASH}/public/21056714`]);
  // The sample is scheduled at its round's time, which checkBeaconSample returns while it is not after the chain's clock.
  const time=beaconRoundTime(preset,BigInt(ROUND));
  assert.equal(checkBeaconSample(preset,got,time),time);assert.equal(checkBeaconSample(preset,got,time+1000n),time);
  await rejectsStop(Promise.resolve().then(()=>checkBeaconSample(preset,got,time-1n)),/scheduled at \d+, after the latest block time \d+: registerBeacon refuses it/);
  const other=rounds.get(21056714)!;
  await rejectsStop(Promise.resolve().then(()=>checkBeaconSample(preset,{round:got.round,signature:`0x${other.signature}`},time)),/does not verify under the evmnet group key/);
});

test("a relay whose chain info is not the preset's network is refused before any round is read",async()=>{
  const info=fixture.info,wrong:Array<[string,unknown]>=[["hash",`${"0".repeat(63)}1`],["public_key",`${info.public_key}00`],["genesis_time",1727521076],["period",6],["period","3"],["schemeID","pedersen-bls-chained"],["hash",undefined]];
  for(const [key,value] of wrong){
    const relay=fakeRelay(path=>path.endsWith("/info")?json({...info,[key]:value}):undefined);
    await rejectsStop(sample(relay),new RegExp(`The relay's ${key} is .*not the evmnet preset's .*another network or another relay`));
    assert.equal(relay.requests.length,1,`${key}: nothing but the info was requested`);
  }
  // The hash and the key may be in any case; extra fields, such as the group hash and metadata, do not matter.
  const upper=fakeRelay(path=>path.endsWith("/info")?json({...info,hash:HASH.toUpperCase(),public_key:String(info.public_key).toUpperCase()}):undefined);
  assert.equal((await sample(upper)).round,BigInt(ROUND));
});

test("a relay answer for another round, a wrong or malformed signature, or a latest round too early is refused",async()=>{
  const at=(round:number)=>`/${HASH}/public/${round}`,mix=rounds.get(21056714)!.signature;
  const answer=(patch:object)=>fakeRelay(path=>path===at(ROUND)?json({...rounds.get(ROUND),...patch}):undefined);
  await rejectsStop(sample(answer({round:ROUND+1})),new RegExp(`answered round ${ROUND+1} when asked for round ${ROUND}`));
  for(const round of [undefined,"21056750",0,-1,1.5])await rejectsStop(sample(answer({round})),/is not a round number/);
  await rejectsStop(sample(answer({signature:mix})),/signature of round 21056750 does not verify under the evmnet group key/);
  const good=rounds.get(ROUND)!.signature;
  await rejectsStop(sample(answer({signature:`${good.slice(0,-1)}${good.endsWith("0")?"1":"0"}`})),/does not verify/);
  for(const signature of [undefined,42,"",mix.slice(2),`${mix}00`,`zz${mix.slice(2)}`])
    await rejectsStop(sample(answer({signature})),/is not 64 bytes of hex/);
  // A 0x prefix and upper case are accepted, and normalized.
  const prefixed=await sample(answer({signature:`0x${rounds.get(ROUND)!.signature.toUpperCase()}`}));
  assert.equal(prefixed.signature,`0x${rounds.get(ROUND)!.signature}`);
  await rejectsStop(sample(fakeRelay(path=>path.endsWith("/public/latest")?json({round:Number(SAMPLE_LAG)}):undefined)),/latest round 100 is too early to sample 100 rounds behind it/);
  await rejectsStop(sample(fakeRelay(path=>path.endsWith("/public/latest")?json({round:"21056850"}):undefined)),/latest round is not a round number/);
  await rejectsStop(sample(fakeRelay(),{round:123456n}),/HTTP 404 for .*\/public\/123456/);
});

test("HTTP errors, bad bodies, network failures and timeouts are stops that name the request",async()=>{
  const bad:Array<[string,Answer,RegExp]>=[
    ["an HTTP 500 for the info",path=>path.endsWith("/info")?json({},500):undefined,/HTTP 500 for https:\/\/relay\.example\/drand\/[0-9a-f]{64}\/info/],
    ["an HTTP 404 for the round",path=>path.endsWith(`/public/${ROUND}`)?json({},404):undefined,/HTTP 404 for .*\/public\/21056750/],
    ["a redirect that was not followed",path=>path.endsWith("/info")?new Response(null,{status:302,headers:{location:"https://elsewhere.example/"}}):undefined,/HTTP 302/],
    ["a body that is not JSON",path=>path.endsWith("/info")?new Response("<html>bad gateway</html>"):undefined,/answer for .*\/info is not valid JSON/],
    ["an empty body",path=>path.endsWith("/info")?new Response(null):undefined,/is not valid JSON/],
    ["a JSON array",path=>path.endsWith("/info")?json([]):undefined,/is not a JSON object/],["JSON null",path=>path.endsWith("/info")?json(null):undefined,/is not a JSON object/],
    ["a body past 16,384 bytes",path=>path.endsWith("/info")?new Response(JSON.stringify({...fixture.info,padding:"x".repeat(20_000)})):undefined,/answer exceeds 16384 bytes/],
    ["a body that is not UTF-8",path=>path.endsWith("/info")?new Response(new Uint8Array([0x7b,0xff,0xfe,0x7d])):undefined,/failed: /],
    ["a refused connection",()=>{throw Object.assign(new TypeError("fetch failed"),{cause:{code:"ECONNREFUSED"}});},/request .*\/info failed: ECONNREFUSED/],
    ["a plain failure",()=>{throw new Error("socket hang up");},/failed: socket hang up/],
  ];
  for(const [name,answer,message] of bad)await rejectsStop(sample(fakeRelay(answer)),message);
  // A relay that never answers is abandoned at the timeout, with the time it was given in the message.
  const hung=fakeRelay((_path,init)=>new Promise<Response>((_resolve,reject)=>init!.signal!.addEventListener("abort",()=>reject(init!.signal!.reason))));
  await rejectsStop(sample(hung,{timeoutMs:20}),/failed: no answer within 20 ms/);
  // A body that stops arriving is abandoned too, as the signal covers the whole read.
  const stalled=fakeRelay((path,init)=>path.endsWith("/info")?new Response(new ReadableStream({start(controller){
    controller.enqueue(new TextEncoder().encode("{"));init!.signal!.addEventListener("abort",()=>controller.error(init!.signal!.reason));}})):undefined);
  await rejectsStop(sample(stalled,{timeoutMs:20}),/failed: no answer within 20 ms/);
});

test("a relay URL must be https without credentials, query or fragment",()=>{
  assert.equal(relayBase("https://api.drand.sh"),"https://api.drand.sh");assert.equal(relayBase("https://api.drand.sh/"),"https://api.drand.sh");
  assert.equal(relayBase("https://relay.example/drand//"),"https://relay.example/drand");assert.equal(relayBase("HTTPS://Relay.Example:8443/a/b/"),"https://relay.example:8443/a/b");
  for(const url of ["http://api.drand.sh","ftp://api.drand.sh","https://user:pass@api.drand.sh","https://api.drand.sh?x=1","https://api.drand.sh#top","file:///tmp/relay"])
    assert.throws(()=>relayBase(url),(error:Error)=>error instanceof Stop&&/must be an https URL without credentials, query or fragment/.test(error.message),url);
  assert.throws(()=>relayBase("not a url"),(error:Error)=>error instanceof Stop&&/--relay is not a URL/.test(error.message));
});

const VERIFIER="0xd20da0aabbccddeeff00112233445566778899aa",SIGNATURE=rounds.get(ROUND)!.signature;
const stops=(values:Parameters<typeof parseRegisterBeaconOptions>[0],manifest:Parameters<typeof parseRegisterBeaconOptions>[1],message:RegExp)=>
  assert.throws(()=>parseRegisterBeaconOptions(values,manifest),(error:Error)=>error instanceof Stop&&message.test(error.message),JSON.stringify(values));

test("register-beacon options default to the evmnet preset, the manifest's verifier and drand's relay, and refuse everything ambiguous",()=>{
  const defaults=parseRegisterBeaconOptions({},{beaconVerifier:VERIFIER.toLowerCase()});
  assert.deepEqual(defaults,{preset,verifier:getAddress(VERIFIER),relay:DEFAULT_RELAY,round:undefined,sample:undefined});
  assert.equal(preset,BEACON_PRESETS.evmnet);assert.equal(DEFAULT_RELAY,"https://api.drand.sh");
  // The command line wins over the manifest, and a round alone is fetched from the relay.
  const other="0x2222222222222222222222222222222222222222";
  assert.deepEqual(parseRegisterBeaconOptions({verifier:other,relay:RELAY,"sample-round":"21056714"},{beaconVerifier:VERIFIER}),
    {preset,verifier:other,relay:RELAY,round:21056714n,sample:undefined});
  // A whole sample is normalized, with or without the 0x prefix and in any case.
  for(const signature of [SIGNATURE,`0x${SIGNATURE}`,SIGNATURE.toUpperCase()])
    assert.deepEqual(parseRegisterBeaconOptions({verifier:other,"sample-round":String(ROUND),"sample-signature":signature},{}).sample,{round:BigInt(ROUND),signature:`0x${SIGNATURE}`});
  stops({},{},/Provide --verifier <address>, or record beaconVerifier in the manifest/);
  stops({verifier:"0x1234"},{beaconVerifier:VERIFIER},/Invalid --verifier/);stops({verifier:`0x${"g".repeat(40)}`},{},/Invalid --verifier/);
  for(const bad of ["0x1234",42,null,""])stops({},{beaconVerifier:bad},/Invalid beaconVerifier in the manifest/);
  for(const name of ["drand","mainnet","__proto__","constructor","toString",""])stops({verifier:VERIFIER,beacon:name},{},/Unknown --beacon .*; the only preset is evmnet/);
  assert.equal(parseRegisterBeaconOptions({verifier:VERIFIER,beacon:"evmnet"},{}).preset,preset);
  for(const round of ["0","01","-1","1.5","abc","","1".repeat(20)])stops({verifier:VERIFIER,"sample-round":round},{},/--sample-round must be a round number of at most 19 digits/);
  for(const signature of ["","0x","0x12",`${SIGNATURE}00`,`zz${SIGNATURE.slice(2)}`,SIGNATURE.slice(0,-1)])
    stops({verifier:VERIFIER,"sample-round":"1","sample-signature":signature},{},/--sample-signature must be the round's 64-byte signature in hex/);
  stops({verifier:VERIFIER,"sample-signature":SIGNATURE},{},/--sample-signature needs --sample-round/);
  stops({verifier:VERIFIER,"sample-round":"1","sample-signature":SIGNATURE,relay:RELAY},{},/either as --sample-round with --sample-signature or through --relay, not both/);
  for(const relay of ["http://api.drand.sh","api.drand.sh","https://user:pass@api.drand.sh"])stops({verifier:VERIFIER,relay},{},/--relay/);
});

const registration=(patch:Partial<BeaconRegistration>={}):BeaconRegistration=>({verifier:getAddress(VERIFIER),...DRAND_EVMNET,...patch});
test("a beacon recipe's slot signer is read from the registry, and an explicit signer must be that one",async()=>{
  const service=await loadServiceCatalog(),providers=service.signers,slot=beaconSlotSigner(registration()),lower=slot.toLowerCase();
  const beaconSigners=new Map([[11,slot]]);
  // Signed built-in recipes keep the service profile's behaviour: their provider's Airnode. Beacon slots take their registry signer, wherever they sit.
  assert.deepEqual(resolveSlotSigners([0,1,2,4,5],{beaconSigners:new Map(),providers}),[providers.hyperliquid,providers.drpc,providers.tickerlayer,providers.nodary,providers.drpc]);
  assert.deepEqual(resolveSlotSigners([0,11,2],{beaconSigners,providers}),[providers.hyperliquid,slot,providers.tickerlayer]);
  assert.deepEqual(resolveSlotSigners([11,4],{beaconSigners,providers}),[slot,providers.nodary]);
  assert.deepEqual(resolveSlotSigners([11],{beaconSigners,providers}),[slot]);
  // The rollout's mixed catalog: passthrough recipes 6-10 are not built in, so they need explicit signers, and the beacon's must match.
  const explicit=[providers.hyperliquid,providers.drpc,providers.tickerlayer,providers.nodary,providers.drpc,slot];
  assert.deepEqual(resolveSlotSigners([6,7,8,9,10,11],{explicit,beaconSigners,providers}),explicit);
  assert.deepEqual(resolveSlotSigners([6,7,8,9,10,11],{explicit:[...explicit.slice(0,5),lower],beaconSigners,providers}),explicit,"an address in lower case is the same address");
  assert.throws(()=>resolveSlotSigners([6,7,8,9,10,11],{beaconSigners,providers}),/Recipe 6 is not a built-in recipe, so its signer cannot be derived; list the signers explicitly with --signers/);
  // A beacon slot listing another signer, an Airnode or the zero address is a mistake to stop on, naming the slot and the signer it needs.
  for(const wrong of [providers.nodary,getAddress(toBeHex(1,20)),beaconSlotSigner(registration({period:6n}))])
    assert.throws(()=>resolveSlotSigners([0,11],{explicit:[providers.hyperliquid,wrong],beaconSigners,providers}),
      new RegExp(`Slot 1 lists recipe 11, a beacon: its signer is ${slot}, the identity its registration derives, not ${wrong}`));
  // A recipe that is not a beacon in the registry is a signed one: its signer is its own, whatever its id.
  assert.deepEqual(resolveSlotSigners([11],{explicit:[providers.nodary],beaconSigners:new Map(),providers}),[providers.nodary]);
  assert.throws(()=>resolveSlotSigners([11],{beaconSigners:new Map(),providers}),/Recipe 11 is not a built-in recipe/);
});

test("registering a beacon twice is refused only for an identical registration",()=>{
  const request=beaconCanonicalRequest(DRAND_EVMNET.chainHash),candidate=registration();
  const signed=BUILTIN_EPOCH_RECIPES.map(recipe=>({id:recipe.id,canonicalRequest:recipe.canonicalRequest}));
  const listed=[...signed,{id:6,canonicalRequest:'["passthrough","GET","/feed/latest",[["name","ETH/USD"]],""]'},{id:7,canonicalRequest:request,beacon:registration()}];
  assert.equal(findDuplicateBeacon(candidate,listed),7);
  assert.equal(findDuplicateBeacon(candidate,signed),undefined);assert.equal(findDuplicateBeacon(candidate,[]),undefined);
  // The same chain under another verifier, key or schedule is a different registration.
  const otherKey=`0x${"11".repeat(128)}`;
  for(const [name,patch] of [["verifier",{verifier:"0x2222222222222222222222222222222222222222"}],["key",{publicKey:otherKey}],["genesis",{genesis:DRAND_EVMNET.genesis+1n}],["period",{period:DRAND_EVMNET.period+1n}]] as const)
    assert.equal(findDuplicateBeacon(registration(patch),listed),undefined,`another ${name}`);
  assert.equal(findDuplicateBeacon(candidate,[{id:7,canonicalRequest:request,beacon:registration({verifier:"0x2222222222222222222222222222222222222222"})}]),undefined);
  // A signed recipe that merely names the chain is not a beacon, and a beacon of another chain is not this one.
  assert.equal(findDuplicateBeacon(candidate,[{id:7,canonicalRequest:request}]),undefined);
  const otherChain=beaconCanonicalRequest(id("another network"));
  assert.equal(findDuplicateBeacon(candidate,[{id:7,canonicalRequest:otherChain,beacon:registration()}]),undefined);
  assert.equal(findDuplicateBeacon(registration({chainHash:id("another network")}),listed),undefined);
  // Addresses and keys are compared as values, not as text; the first identical registration is the one reported.
  assert.equal(findDuplicateBeacon(candidate,[{id:3,canonicalRequest:request,beacon:registration({verifier:VERIFIER.toLowerCase(),publicKey:`0x${DRAND_EVMNET.publicKey.slice(2).toUpperCase()}`})},{id:9,canonicalRequest:request,beacon:registration()}]),3);
});

test("registerBeacon's arguments are in the contract's order",()=>{
  const args=registerBeaconArgs(VERIFIER,preset,{round:BigInt(ROUND),signature:`0x${SIGNATURE}`});
  assert.deepEqual(args,[VERIFIER,DRAND_EVMNET.chainHash,DRAND_EVMNET.publicKey,DRAND_EVMNET.genesis,DRAND_EVMNET.period,BigInt(ROUND),`0x${SIGNATURE}`]);
  assert.equal(preset.scheme,"bls-bn254-unchained-on-g1");assert.equal(fixture.info.schemeID,preset.scheme);
  assert.deepEqual([preset.chainHash,preset.publicKey,preset.genesis,preset.period],[`0x${fixture.info.hash}`,`0x${fixture.info.public_key}`,BigInt(fixture.info.genesis_time as number),BigInt(fixture.info.period as number)]);
});

test("register-beacon refuses bad options before any network access",()=>{
  // Every refusal here comes before the first RPC request, so the run needs no network. The manifest is Arc Testnet's without a
  // beaconVerifier, so recording one there later does not turn the first case into a run that reaches the network.
  const manifest=join(mkdtempSync(join(tmpdir(),"d20-beacon-admin-")),"manifest.json");
  writeFileSync(manifest,JSON.stringify({...JSON.parse(readFileSync("deployments/arc-testnet.json","utf8")),beaconVerifier:undefined}));
  const run=(...args:string[])=>spawnSync(process.execPath,["scripts/admin.ts","register-beacon","--manifest",manifest,...args],{encoding:"utf8"});
  const stop=(result:ReturnType<typeof run>,message:RegExp)=>{assert.equal(result.status,1);assert.match(result.stderr,message);};
  stop(run(),/Arc administration stopped: Provide --verifier <address>, or record beaconVerifier in the manifest/);
  stop(run("--verifier",VERIFIER,"--beacon","drand"),/Unknown --beacon "drand"/);
  stop(run("--verifier",VERIFIER,"--relay","http://api.drand.sh"),/--relay must be an https URL/);
  stop(run("--verifier",VERIFIER,"--sample-signature",SIGNATURE),/--sample-signature needs --sample-round/);
  const unknown=spawnSync(process.execPath,["scripts/admin.ts","nonsense","--manifest","deployments/arc-testnet.json"],{encoding:"utf8"});
  assert.match(unknown.stderr,/register-beacon\|schedule-catalog/);
  // The mainnet rule for owner transactions holds for this action too: it is proposed in the Safe, never applied here.
  stop(run("--verifier",VERIFIER,"--apply"),/--apply needs --env with the owner key file/);
  const mainnet=spawnSync(process.execPath,["scripts/admin.ts","register-beacon","--manifest","deployments/arc-mainnet.json","--verifier",VERIFIER,"--apply"],{encoding:"utf8"});
  assert.equal(mainnet.status,1);assert.match(mainnet.stderr,/Mainnet owner transactions are proposed and signed in the DAO Safe/);
});

test("schedule-catalog states its recipes on every network and its first epoch on a production chain, and defaults only the epoch on a testnet",()=>{
  const mainnet={testnet:false,name:"Arc"},testnet={testnet:true,name:"Arc Testnet"};
  const stops=(chain:typeof mainnet,values:{recipes?:string;"from-epoch"?:string},message:RegExp)=>
    assert.throws(()=>requireExplicitSchedule(chain,values),(error:Error)=>error instanceof Stop&&message.test(error.message),JSON.stringify([chain.name,values]));
  // No catalog is ever assumed, on any network: a default could schedule signed recipes, which 0.4.1 keepers do not serve, and a typo or an
  // omission would then leave every epoch unpublished.
  for(const chain of [mainnet,testnet]){
    stops(chain,{},new RegExp(`^On ${chain.name}, schedule-catalog needs an explicit --recipes. No catalog is assumed: a default could schedule signed API recipes, which 0\\.4\\.1 keepers do not serve`));
    stops(chain,{"from-epoch":"900"},/needs an explicit --recipes/);
  }
  // The current epoch + 2 is never assumed for a network that serves real requests.
  stops(mainnet,{recipes:"11"},/needs an explicit --from-epoch. The owner is a Safe: a 2-of-2 Safe needs time to collect both signatures, and the default, the current epoch [+] 2, leaves about a minute/);
  assert.doesNotThrow(()=>requireExplicitSchedule(mainnet,{recipes:"11","from-epoch":"900"}));
  // A testnet defaults the first epoch to the current epoch + 2.
  for(const values of [{recipes:"11"},{recipes:"11","from-epoch":"900"}])assert.doesNotThrow(()=>requireExplicitSchedule(testnet,values));
});

test("admin.ts refuses schedule-catalog without explicit recipes on either manifest, and on the production manifest without a first epoch, before any network access",()=>{
  const run=(manifest:string,...args:string[])=>spawnSync(process.execPath,["scripts/admin.ts","schedule-catalog","--manifest",manifest,...args],{encoding:"utf8"});
  const stop=(result:ReturnType<typeof run>,message:RegExp)=>{assert.equal(result.status,1);assert.match(result.stderr,message);};
  stop(run("deployments/arc-mainnet.json"),/Arc administration stopped: On Arc, schedule-catalog needs an explicit --recipes/);
  stop(run("deployments/arc-mainnet.json","--from-epoch","900"),/needs an explicit --recipes/);
  stop(run("deployments/arc-mainnet.json","--recipes","11"),/needs an explicit --from-epoch. The owner is a Safe: a 2-of-2 Safe needs time/);
  // A testnet no longer falls back to the service profile's signed catalog.
  stop(run("deployments/arc-testnet.json"),/Arc administration stopped: On Arc Testnet, schedule-catalog needs an explicit --recipes. No catalog is assumed/);
  stop(run("deployments/arc-testnet.json","--from-epoch","900","--allow-signed-recipes"),/needs an explicit --recipes/);
});

test("a catalog for keepers 0.4.1 and later lists beacon recipes only, unless the signed-recipe override is given",()=>{
  // Recipes 0-10 are signed and 11 is a beacon, as on both live networks; the registry holds twelve recipes.
  const beacons=new Set([11]);
  const check=(recipes:number[],options:Partial<Parameters<typeof checkBeaconRecipes>[1]>={})=>checkBeaconRecipes(recipes,{registered:12n,beacons,allowSigned:false,...options});
  const refused=(recipes:number[],message:RegExp,options:Partial<Parameters<typeof checkBeaconRecipes>[1]>={})=>
    assert.throws(()=>check(recipes,options),(error:Error)=>error instanceof Stop&&message.test(error.message),JSON.stringify(recipes));
  // A beacon is accepted.
  assert.deepEqual(check([11]),[]);
  // A signed recipe is refused by name, and so is a typo of the beacon's id that lands on one (--recipes 1 for --recipes 11).
  refused([1],/^Recipe 1 has no beacon registration, and 0\.4\.1 keepers do not serve signed API recipes: every epoch that selects it would stay unpublished until another catalog takes effect/);
  refused([0,1,2,4,5],/^Recipes 0, 1, 2, 4, 5 have no beacon registration, and 0\.4\.1 keepers do not serve signed API recipes: every epoch that selects one of them/);
  // Only the signed recipes of a mixed catalog are named, in the catalog's order.
  refused([11,6,0],/^Recipes 6, 0 have no beacon registration/);
  refused([6,11],/^Recipe 6 has no beacon registration/);
  // The refusal says what lifts it.
  refused([1],/--allow-signed-recipes overrides this for an emergency; every keeper must then run 0\.4\.0 before the catalog takes effect\.$/);
  // The override accepts them and returns them, in catalog order, for the plan's warning; a beacon alone has none to warn about.
  assert.deepEqual(check([1],{allowSigned:true}),[1]);
  assert.deepEqual(check([11,6,0],{allowSigned:true}),[6,0]);
  assert.deepEqual(check([11],{allowSigned:true}),[]);
  // A registry without beacons has none to list: every registered recipe is signed.
  refused([11],/^Recipe 11 has no beacon registration/,{beacons:new Set()});
  assert.deepEqual(check([11],{beacons:new Set(),allowSigned:true}),[11]);
  // A recipe the registry does not hold is not a signed one: registration is checked, and reported, on its own.
  assert.deepEqual(check([12,200]),[]);
  assert.deepEqual(check([11,12]),[]);
});

test("admin.ts schedules no default catalog and checks the recipes of every plan for beacon registrations",()=>{
  const source=readFileSync("scripts/admin.ts","utf8");
  // The service profile's signed rollout catalog is not read for scheduling, and the check runs on the plan's recipes with the override flag.
  assert.doesNotMatch(source,/service\.recipes/);
  assert.match(source,/checkBeaconRecipes[(]recipes,[{][^}]*allowSigned:values\["allow-signed-recipes"\]/);
  assert.match(source,/"allow-signed-recipes":\{type:"boolean",default:false\}/);
});

/// A local JSON-RPC server: `reply` answers each request. Returns its URL, the methods it was asked and a way to stop it.
type Reply=(payload:{id:number;method:string},response:ServerResponse)=>void;
async function serve(reply:Reply){
  const requests:string[]=[];
  const server=createServer((request,response)=>{
    let body="";
    request.on("data",chunk=>{body+=chunk;});
    request.on("end",()=>{const payload=body===""?{id:0,method:"none"}:JSON.parse(body);requests.push(payload.method);reply(payload,response);});
  });
  await new Promise<void>(ready=>server.listen(0,"127.0.0.1",ready));
  return {url:`http://127.0.0.1:${(server.address() as AddressInfo).port}`,requests,close:()=>new Promise<void>(done=>{server.closeAllConnections();server.close(()=>done());})};
}
const answer=(response:ServerResponse,payload:{id:number},message:{result?:unknown;error?:unknown},status=200)=>{
  response.writeHead(status,{"content-type":"application/json"});response.end(JSON.stringify({jsonrpc:"2.0",id:payload.id,...message}));
};
const REGISTRY="0xd20Da048C1A68fa3Bc0B5f5Bc454D1530062C82D",ZERO_WORD=`0x${"00".repeat(32)}`;

test("a registry lacks a function only when its call reverts with no data; every other failure of the RPC is a stop",async()=>{
  const network=Network.from(31337);
  const reason=`0x08c379a0${"00".repeat(31)}20${"00".repeat(31)}06${Buffer.from("paused").toString("hex").padEnd(64,"0")}`;
  const cases:Array<[string,Reply,"missing"|"present"|"stop"]>=[
    ["an answer",(p,r)=>answer(r,p,{result:ZERO_WORD}),"present"],
    // Arc's public nodes report an unknown selector as error 3 without a data field, as all six of them were seen to on 2026-09-30.
    ["an empty revert as Arc's nodes report it",(p,r)=>answer(r,p,{error:{code:3,message:"execution reverted"}}),"missing"],
    ["an empty revert with data 0x, as geth and anvil report it",(p,r)=>answer(r,p,{error:{code:3,message:"execution reverted",data:"0x"}}),"missing"],
    ["a custom error, which carries its selector",(p,r)=>answer(r,p,{error:{code:3,message:"execution reverted",data:"0x35be3ac8"}}),"stop"],
    ["a revert with a reason",(p,r)=>answer(r,p,{error:{code:3,message:"execution reverted: paused",data:reason}}),"stop"],
    ["a rate limit in JSON-RPC's own terms",(p,r)=>answer(r,p,{error:{code:-32005,message:"rate limit exceeded"}}),"stop"],
    ["a node that has no such state",(p,r)=>answer(r,p,{error:{code:-32000,message:"header not found"}}),"stop"],
    ["an error 3 that is not a revert",(p,r)=>answer(r,p,{error:{code:3,message:"gas required exceeds allowance"}}),"stop"],
    ["HTTP 429",(_p,r)=>{r.writeHead(429);r.end("slow down");},"stop"],
    ["HTTP 500",(_p,r)=>{r.writeHead(500);r.end("upstream failed");},"stop"],
    ["an answer that is not JSON",(_p,r)=>{r.writeHead(200,{"content-type":"application/json"});r.end("<html>bad gateway</html>");},"stop"],
    ["a dropped connection",(_p,r)=>{r.socket?.destroy();},"stop"],
    ["an answer of no bytes for a view",(p,r)=>answer(r,p,{result:"0x"}),"stop"],
  ];
  for(const [name,reply,expected] of cases){
    const server=await serve(reply);
    try{
      // One attempt, so that a throttled or failing node is reported at once instead of after ethers' back-off.
      const request=new FetchRequest(server.url);request.setThrottleParams({maxAttempts:1,slotInterval:1});
      const provider=new JsonRpcProvider(request,network,{staticNetwork:network,batchMaxCount:1,cacheTimeout:-1});
      const registry=new Contract(REGISTRY,["function beaconOf(uint8) view returns (uint256)"],provider);
      try{
        const result=answerOrMissing(()=>registry.beaconOf(0),"beaconOf()");
        if(expected==="stop")await rejectsStop(result,/Could not tell whether the registry has beaconOf[(][)]: .*nothing was assumed/);
        else assert.equal(await result===undefined,expected==="missing",name);
        assert.deepEqual(server.requests,["eth_call"],`${name}: one call`);
      }finally{provider.destroy();}
    }finally{await server.close();}
  }
});

test("isEmptyRevert takes only a call exception with no revert data",()=>{
  const call=(data:unknown,rpc?:object)=>({code:"CALL_EXCEPTION",data,info:rpc&&{error:rpc}});
  for(const yes of [call("0x"),call(null,{code:3,message:"execution reverted"}),call(undefined,{code:3,message:"Execution Reverted"}),call(null,{code:3,message:"execution reverted",data:null})])
    assert.equal(isEmptyRevert(yes),true,JSON.stringify(yes));
  for(const no of [call("0x35be3ac8"),call(null),call(null,{code:3,message:"execution reverted",data:"0x35be3ac8"}),call(null,{code:-32000,message:"execution reverted"}),
    call(null,{code:3,message:"out of gas"}),{code:"SERVER_ERROR",data:"0x"},{code:"TIMEOUT"},{code:"NETWORK_ERROR",data:null},{code:"BAD_DATA",data:"0x"},new Error("fetch failed"),null,undefined,"execution reverted"])
    assert.equal(isEmptyRevert(no),false,JSON.stringify(no));
});

test("admin.ts asks the registry whether it has recipeCount, beaconOf and isAuthorizedCommitter only through the classifier",()=>{
  const source=readFileSync("scripts/admin.ts","utf8");
  for(const probe of ["recipeCount()","beaconOf()","isAuthorizedCommitter()"])assert.ok(source.includes(`,"${probe}")`),probe);
  assert.equal(source.split("answerOrMissing(").length-1,3);
  // No probe turns every failure into "not there": that is what took an RPC failure for a registry without beacons.
  assert.doesNotMatch(source,/[.]then[(].*,[(][)]=>(false|undefined)[)]/);
});

test("a relay's redirect is refused rather than followed, so relayBase's https-only rule holds for every hop",async()=>{
  // relayBase insists on https, so this reaches local plain-HTTP servers through a fetch that only rewrites the URL and hands the real
  // fetch everything else, redirect option included. The redirector sends every request to the target, which records what it is asked.
  const asked:string[]=[];
  const target=createServer((request,response)=>{asked.push(request.url??"");response.writeHead(200,{"content-type":"application/json"});response.end(JSON.stringify(fixture.info));});
  await new Promise<void>(ready=>target.listen(0,"127.0.0.1",ready));
  const targetUrl=`http://127.0.0.1:${(target.address() as AddressInfo).port}`;
  const redirecting=createServer((request,response)=>{response.writeHead(302,{location:`${targetUrl}${request.url}`});response.end();});
  await new Promise<void>(ready=>redirecting.listen(0,"127.0.0.1",ready));
  const relayUrl=`http://127.0.0.1:${(redirecting.address() as AddressInfo).port}`;
  const seen:Array<RequestInit["redirect"]>=[];
  const viaLocal:typeof fetch=(input,init)=>{seen.push(init?.redirect);return fetch(String(input).replace(RELAY,relayUrl),init);};
  const stop=(server:typeof target)=>new Promise<void>(done=>{server.closeAllConnections();server.close(()=>done());});
  try{
    await rejectsStop(fetchBeaconSample(RELAY,preset,{fetch:viaLocal}),/The relay request .*[/]info failed: .*redirect/);
    assert.deepEqual(seen,["error"],"the first request says not to follow");
    assert.deepEqual(asked,[],"the redirect target was never asked");
    // The control: the same real fetch, left to its default, does follow the redirect to the target, so this test would notice a lost option.
    const followed=await fetch(`${relayUrl}/${HASH}/info`);
    assert.equal(followed.status,200);assert.deepEqual(asked,[`/${HASH}/info`]);
  }finally{await stop(redirecting);await stop(target);}
});

/// BackupCommitterSet logs, and a node that serves them the way the public RPCs do: by block range, refusing more than 10,000 blocks at once.
const BACKUP_TOPIC=id("BackupCommitterSet(address,bool)");
const ACCOUNTS=[getAddress(toBeHex(0xa1,20)),getAddress(toBeHex(0xb2,20)),getAddress(toBeHex(0xc3,20))];
const setLog=(blockNumber:number,index:number,account:string,allowed:boolean)=>({blockNumber,index,topics:[BACKUP_TOPIC,zeroPadValue(account,32)],data:toBeHex(allowed?1:0,32)}) as unknown as Log;
function publicNode(logs:Log[],failures:(fromBlock:number,attempt:number)=>Error|undefined=()=>undefined){
  const asked:Array<[number,number]>=[],attempts=new Map<number,number>();
  const source:LogSource={getLogs:async({address,topics,fromBlock,toBlock})=>{
    asked.push([fromBlock,toBlock]);
    const attempt=(attempts.get(fromBlock)??0)+1;attempts.set(fromBlock,attempt);
    const failure=failures(fromBlock,attempt);if(failure)throw failure;
    if(toBlock-fromBlock+1>LOG_PAGE_BLOCKS)throw new Error("query exceeds max block range 10000");
    assert.equal(address,REGISTRY);assert.deepEqual(topics,[BACKUP_TOPIC]);
    return logs.filter(log=>log.blockNumber>=fromBlock&&log.blockNumber<=toBlock);
  }};
  return {source,asked};
}

test("a registry's backup committers are found from its BackupCommitterSet events, read a page of at most 10,000 blocks at a time",async()=>{
  const [a,b,c]=ACCOUNTS;
  // a is allowed, removed and allowed again, the last two in one block in log order; b is allowed and then removed; c is allowed.
  const logs=[setLog(30_500,2,a,true),setLog(25_000,0,a,true),setLog(25_000,1,b,true),setLog(30_500,0,a,false),setLog(30_500,1,c,true),setLog(99_999,5,b,false)];
  const node=publicNode(logs);
  const progress:Array<[number,number]>=[];
  const history=await readBackupCommitters(node.source,REGISTRY,20_000,100_000,{progress:(page,pages)=>progress.push([page,pages])});
  assert.deepEqual(node.asked,Array.from({length:9},(_,page)=>[20_000+10_000*page,Math.min(100_000,29_999+10_000*page)]),"nine pages of 10,000 blocks, the last of one");
  assert.deepEqual([...history.state],[[a,true],[b,false],[c,true]]);
  assert.deepEqual(history.active,[a,c]);assert.equal(history.events,6);assert.equal(history.pages,9);
  assert.deepEqual(progress.at(-1),[9,9]);
  // The whole range in one page when it fits, and a range of one block.
  assert.equal((await readBackupCommitters(publicNode(logs).source,REGISTRY,25_000,25_000)).events,2);
  assert.deepEqual((await readBackupCommitters(publicNode([]).source,REGISTRY,5,9_999)).active,[]);
  // A registry with no event has no backup committer.
  const none=await readBackupCommitters(publicNode([]).source,REGISTRY,1,50_000);
  assert.deepEqual([none.active,none.events,none.pages],[[],0,5]);
});

test("a history read in two parts is the history of the whole range, and its pages are paced",async()=>{
  const [a,b,c]=ACCOUNTS;
  const logs=[setLog(12_000,0,a,true),setLog(18_000,0,b,true),setLog(22_000,0,a,false),setLog(24_500,3,c,true),setLog(24_500,4,b,false)];
  const whole=await readBackupCommitters(publicNode(logs).source,REGISTRY,10_000,24_999);
  // The chain moves on while the fork starts: the first part is read up to block 19,999 and the rest afterwards, continuing from it.
  const first=await readBackupCommitters(publicNode(logs).source,REGISTRY,10_000,19_999),rest=publicNode(logs);
  const second=await readBackupCommitters(rest.source,REGISTRY,20_000,24_999,{before:first});
  assert.deepEqual(rest.asked,[[20_000,24_999]],"only the blocks since are asked");
  assert.deepEqual([...second.state],[...whole.state]);assert.deepEqual(second.active,whole.active);
  assert.deepEqual([second.events,second.pages],[whole.events,whole.pages]);assert.deepEqual([...first.state],[[a,true],[b,true]],"the first part is not changed by the second");
  assert.deepEqual(second.active,[c]);
  // A pause between pages, and none before the first or after the last.
  const started=Date.now();
  await readBackupCommitters(publicNode(logs).source,REGISTRY,10_000,39_999,{pauseMs:60});
  const paced=Date.now()-started;
  assert.ok(paced>=110,`three pages, two pauses of 60 ms: took ${paced} ms`);
});

test("a page of BackupCommitterSet events that fails is asked again, and a history that cannot be read is a stop, never a shorter one",async()=>{
  const [a,b]=ACCOUNTS;
  const logs=[setLog(15_000,0,a,true),setLog(35_000,0,b,true)];
  // A rate-limited page answers on its third try; the others are asked once. The pause between tries is shortened for the test.
  const flaky=publicNode(logs,(from,attempt)=>from===20_000&&attempt<3?new Error("429 Too Many Requests"):undefined);
  const history=await readBackupCommitters(flaky.source,REGISTRY,10_000,39_999,{retryMs:1});
  assert.deepEqual(history.active,[a,b]);
  assert.deepEqual(flaky.asked.filter(([from])=>from===20_000).length,3);assert.equal(flaky.asked.length,5);
  const down=publicNode(logs,from=>from===20_000?new Error("connection reset"):undefined);
  await rejectsStop(readBackupCommitters(down.source,REGISTRY,10_000,39_999,{attempts:3,retryMs:1}),/blocks 20000 to 29999 failed after 3 attempts: connection reset/);
  assert.equal(down.asked.filter(([from])=>from===20_000).length,3);
  // A log that is not what was asked for, or lies outside its page, is never applied.
  const stray:LogSource={getLogs:async()=>[{...setLog(1,0,a,true),topics:[id("Other(address)"),zeroPadValue(a,32)]} as unknown as Log]};
  await rejectsStop(readBackupCommitters(stray,REGISTRY,0,9),/is not what was asked for/);
  const early:LogSource={getLogs:async()=>[setLog(3,0,a,true)]};
  await rejectsStop(readBackupCommitters(early,REGISTRY,5,9),/is not what was asked for/);
  for(const [from,to] of [[-1,5],[9,5],[0.5,3]])await rejectsStop(readBackupCommitters(publicNode([]).source,REGISTRY,from,to),/Invalid block range/);
});

test("a node's rate limit is told from every other failure, and only a read that hit it is repeated",async()=>{
  for(const limited of [new Error("Max retries exceeded HTTP error 429 with body: rate limit exceeded"),new Error("server response 429 Too Many Requests"),
    {code:"UNKNOWN_ERROR",error:{code:-32005,message:"rate limit exceeded"}},{code:-32005},{message:"failed to get storage: rate limit exceeded"}])
    assert.equal(isRateLimited(limited),true,JSON.stringify(limited));
  for(const other of [new Error("execution reverted"),new Error("header not found"),new Error("insufficient funds"),new Error("connection reset"),new Error("gas limit 4290000"),
    new Error("block 1429 is not final"),{code:"CALL_EXCEPTION",data:"0x919ddbf6"},null,undefined,"429x"])
    assert.equal(isRateLimited(other),false,JSON.stringify(other));

  const network=Network.from(31337);
  const asked:string[]=[],waits:Array<[string,number]>=[];
  // A node that refuses the first three requests for its rate limit, and answers the fourth and the next.
  const server=await serve((p,r)=>{
    asked.push(p.method);
    if(p.method==="evm_mine")return answer(r,p,{error:{code:-32005,message:"rate limit exceeded"}});
    if(p.method==="eth_call")return answer(r,p,{error:{code:3,message:"execution reverted",data:"0x919ddbf6"}});
    if(asked.filter(method=>method===p.method).length<=3)return answer(r,p,{error:{code:-32005,message:"rate limit exceeded"}});
    answer(r,p,{result:"0x2a"});
  });
  try{
    const provider=calmReads(new JsonRpcProvider(server.url,network,{staticNetwork:network,batchMaxCount:1,cacheTimeout:-1}),{pauseMs:1,attempts:5,onWait:(method,attempt)=>waits.push([method,attempt])});
    try{
      assert.equal(await provider.send("eth_getStorageAt",[REGISTRY,"0x0","latest"]),"0x2a");
      assert.deepEqual(waits,[["eth_getStorageAt",1],["eth_getStorageAt",2],["eth_getStorageAt",3]],"three refusals, three waits");
      assert.equal(asked.filter(method=>method==="eth_getStorageAt").length,4);
      // A write is never repeated, even when it was refused for the rate limit: it may have been half done.
      await assert.rejects(provider.send("evm_mine",[]),(error:Error)=>isRateLimited(error));
      assert.equal(asked.filter(method=>method==="evm_mine").length,1);
      // A revert is an answer, not a limit, and is not repeated.
      await assert.rejects(provider.send("eth_call",[{to:REGISTRY,data:"0x"},"latest"]),(error:Error)=>!isRateLimited(error));
      assert.equal(asked.filter(method=>method==="eth_call").length,1);
      assert.equal(waits.length,3);
    }finally{provider.destroy();}
    // A node that never stops refusing is given up on after `attempts` requests, with its own error.
    const never=await serve((p,r)=>answer(r,p,{error:{code:-32005,message:"rate limit exceeded"}}));
    try{
      const stubborn=calmReads(new JsonRpcProvider(never.url,network,{staticNetwork:network,batchMaxCount:1,cacheTimeout:-1}),{pauseMs:1,attempts:4});
      try{
        await assert.rejects(stubborn.send("eth_getCode",[REGISTRY,"latest"]),(error:Error)=>isRateLimited(error));
        assert.equal(never.requests.length,4);
      }finally{stubborn.destroy();}
    }finally{await never.close();}
  }finally{await server.close();}
});
