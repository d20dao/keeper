import {test} from "node:test";
import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";
import {getAddress} from "ethers";
import {DRAND_EVMNET,beaconRoundTime,beaconSlotSigner,verifyBeaconRound} from "../../src/beacon.ts";
import {BEACON_PRESETS,checkBeaconSample,fetchBeaconSample} from "../lib/drand-relay.ts";
import {Stop} from "../lib/deployment.ts";
import {loadChain} from "../lib/chains.ts";

const bytes=(path:string)=>readFile(new URL(`../../${path}`,import.meta.url));
const read=async(path:string)=>JSON.parse((await bytes(path)).toString("utf8"));
const preset=BEACON_PRESETS.evmnet;

// drand's published parameters of the evmnet network, written out here so the file cannot drift together with the library's copy.
const EVMNET={
  chainHash:"0x04f1e9062b8a81f848fded9c12306733282b2727ecced50032187751166ec8c3",
  publicKey:"0x07e1d1d335df83fa98462005690372c643340060d205306a9aa8106b6bd0b3820557ec32c2ad488e4d4f6008f89a346f18492092ccc0d594610de2732c8b808f0095685ae3a85ba243747b1b2f426049010f6b73a0cf1d389351d5aaaa1047f6297d3a4f9749b33eb2d904c9d9ebf17224150ddd7abd7567a9bec6c74480ee0b",
  genesis:1727521075,period:3,
};

test("the beacon file is drand evmnet under the verifier Arc already runs, with a sample round",async()=>{
  const file=await read("config/beacons/drand-evmnet.json");
  assert.deepEqual(Object.keys(file),["verifier","chainHash","publicKey","genesis","period","sampleRound","sampleSignature"]);
  assert.deepEqual({chainHash:file.chainHash,publicKey:file.publicKey,genesis:file.genesis,period:file.period},EVMNET);
  assert.deepEqual({chainHash:file.chainHash,publicKey:file.publicKey,genesis:BigInt(file.genesis),period:BigInt(file.period)},
    {chainHash:DRAND_EVMNET.chainHash,publicKey:DRAND_EVMNET.publicKey,genesis:DRAND_EVMNET.genesis,period:DRAND_EVMNET.period});
  assert.match(file.chainHash,/^0x[0-9a-f]{64}$/);
  assert.match(file.publicKey,/^0x[0-9a-f]{256}$/);
  assert.match(file.sampleSignature,/^0x[0-9a-f]{128}$/);
  assert.ok(Number.isSafeInteger(file.genesis)&&file.genesis>0&&Number.isSafeInteger(file.period)&&file.period>0&&file.period<240&&Number.isSafeInteger(file.sampleRound)&&file.sampleRound>0);
  assert.equal(file.sampleRound,21072365);
  // The stateless verifier is the contract Arc deploys at one CREATE2 address, so the same registration works at the same address elsewhere.
  assert.equal(getAddress(file.verifier),file.verifier);
  for(const network of ["arc-testnet","arc-mainnet"])assert.equal((await read(`deployments/${network}.json`)).beaconVerifier,file.verifier,network);
  // The identity the registry derives from this registration is the one Arc Testnet's registry reports for the same beacon.
  assert.equal(beaconSlotSigner({verifier:file.verifier,chainHash:file.chainHash,publicKey:file.publicKey,genesis:BigInt(file.genesis),period:BigInt(file.period)}),
    "0x97339DF8D605cdB1e5262AfF95FCF7560f90A254");
});

test("the sample round's signature verifies offline under the group key, and nothing else does",async()=>{
  const file=await read("config/beacons/drand-evmnet.json"),round=BigInt(file.sampleRound),sample={round,signature:file.sampleSignature};
  const time=beaconRoundTime({genesis:BigInt(file.genesis),period:BigInt(file.period)},round);
  assert.equal(time,1790738167n); // genesis + (21072365 - 1) × 3
  assert.equal(checkBeaconSample(preset,sample,time),time);
  assert.ok(verifyBeaconRound(file.publicKey,round,file.sampleSignature));
  // registerBeacon refuses a sample scheduled after the chain's latest block.
  assert.throws(()=>checkBeaconSample(preset,sample,time-1n),(error:unknown)=>error instanceof Stop&&/after the latest block time/.test(error.message));
  const flipped=file.sampleSignature.slice(0,-1)+(file.sampleSignature.endsWith("a")?"b":"a");
  assert.throws(()=>checkBeaconSample(preset,{round,signature:flipped},time),(error:unknown)=>error instanceof Stop&&/does not verify/.test(error.message));
  assert.throws(()=>checkBeaconSample(preset,{round:round+1n,signature:file.sampleSignature},time+3n),(error:unknown)=>error instanceof Stop&&/does not verify/.test(error.message));
  assert.equal(verifyBeaconRound(file.publicKey.slice(0,-2)+(file.publicKey.endsWith("0b")?"0c":"0b"),round,file.sampleSignature),false);
});

// A relay serves its chain info and one round; fetchBeaconSample is the code that fetched the file's signature from public relays.
const relay=(info:object,signatureOf:(round:string)=>string)=>async(url:string|URL|Request)=>{
  const path=new URL(String(url)).pathname,round=/\/public\/(\d+)$/.exec(path)?.[1];
  assert.ok(path.startsWith(`/${EVMNET.chainHash.slice(2)}/`),"asks about the evmnet chain hash");
  if(path.endsWith("/info"))return new Response(JSON.stringify(info));
  assert.ok(round!==undefined,`unexpected request ${path}`);
  return new Response(JSON.stringify({round:Number(round),signature:signatureOf(round!)}));
};

test("fetching the sample from relays that agree returns the file's sample; a relay that does not agree is refused",async()=>{
  const file=await read("config/beacons/drand-evmnet.json");
  const info={hash:file.chainHash.slice(2),public_key:file.publicKey.slice(2),genesis_time:file.genesis,period:file.period,schemeID:"bls-bn254-unchained-on-g1"};
  const sample={round:BigInt(file.sampleRound),signature:file.sampleSignature};
  for(const url of ["https://relay-one.example","https://relay-two.example"]){
    const fetch=relay(info,()=>file.sampleSignature.slice(2)) as typeof globalThis.fetch;
    assert.deepEqual(await fetchBeaconSample(url,preset,{fetch,round:sample.round}),sample,url);
  }
  const other=relay(info,()=>"00".repeat(64)) as typeof globalThis.fetch;
  await assert.rejects(fetchBeaconSample("https://relay-three.example",preset,{fetch:other,round:sample.round}),/does not verify|not 64 bytes|Non-canonical/);
  const wrongNetwork=relay({...info,hash:"00".repeat(32)},()=>file.sampleSignature.slice(2)) as typeof globalThis.fetch;
  await assert.rejects(fetchBeaconSample("https://relay-four.example",preset,{fetch:wrongNetwork,round:sample.round}),/another network or another relay/);
});

// The bounds of the round coordinator (contracts/robinhood/D20VRFCoordinatorRobinhood.sol): MAX_MIN_FEE, MAX_FEE_MULTIPLIER,
// MIN_FULFILL_GAS_OVERHEAD and MAX_FULFILL_GAS_OVERHEAD, under which initialize and setPricing accept a price, and the 10,000 bps a share is out of.
const ROUND_BOUNDS={maxMinFee:10n**16n,maxMultiplier:20,minOverhead:100_000,maxOverhead:2_000_000,bps:10_000};

test("the Robinhood service profiles are the reviewed round pair, the same on both networks, and Arc's profile is not theirs",async()=>{
  const testnet=await bytes("config/service.robinhood-testnet.json"),mainnet=await bytes("config/service.robinhood-mainnet.json");
  assert.deepEqual(testnet,mainnet);
  const profile=JSON.parse(testnet.toString("utf8"));
  // What a deployment takes from it: the coordinator, the proof verifier its constructor is given, the library it links, the beacon that
  // initialize registers, and the initial fee, keeper share and pricing that initialize sets. The overhead, 405,000, keeps the keeper's share
  // 20% above its worst cost wherever the dynamic fee binds; the smallest that does is 404,550 (docs/robinhood.md, "Pricing").
  assert.deepEqual(Object.keys(profile),["coordinator","proofVerifier","mappingLibrary","beacon","minFeeWei","keeperFeeBps","pricing"]);
  assert.deepEqual(profile,{coordinator:"D20VRFCoordinatorRobinhood",proofVerifier:"D20VRFProofVerifier",mappingLibrary:"LinkedRandomnessMapping",
    beacon:"config/beacons/drand-evmnet.json",minFeeWei:"25000000000000",keeperFeeBps:8000,pricing:{multiplier:2,overhead:405000}});
  assert.deepEqual(Object.keys(profile.pricing),["multiplier","overhead"]);
  // Within the coordinator's bounds, so initialize accepts them.
  assert.match(profile.minFeeWei,/^[1-9]\d*$/);
  assert.ok(BigInt(profile.minFeeWei)<=ROUND_BOUNDS.maxMinFee,"at most MAX_MIN_FEE");
  assert.ok(Number.isSafeInteger(profile.keeperFeeBps)&&profile.keeperFeeBps>0&&profile.keeperFeeBps<=ROUND_BOUNDS.bps);
  assert.ok(Number.isSafeInteger(profile.pricing.multiplier)&&profile.pricing.multiplier>=0&&profile.pricing.multiplier<=ROUND_BOUNDS.maxMultiplier);
  assert.ok(Number.isSafeInteger(profile.pricing.overhead)&&profile.pricing.overhead>=ROUND_BOUNDS.minOverhead&&profile.pricing.overhead<=ROUND_BOUNDS.maxOverhead);
  // The keeper's share and its fee coverage answer each other: with 80% of each fee it sends only when the fees cover 1.25 times the cost.
  for(const key of ["robinhood-testnet","robinhood-mainnet"])
    assert.equal((await loadChain(key)).keeper?.feeCoverageBps,ROUND_BOUNDS.bps*ROUND_BOUNDS.bps/profile.keeperFeeBps,key);
  assert.deepEqual(await read(profile.beacon),await read("config/beacons/drand-evmnet.json"));
  // Arc keeps its own profile and its initialization defaults (AGENTS.md), and neither profile carries the other's fields.
  const arc=await read("config/service.json");
  assert.equal(arc.minFeeWei,"80000000000000000");assert.equal(arc.keeperFeeBps,5000);
  for(const field of ["coordinator","proofVerifier","mappingLibrary","beacon","pricing"])assert.equal(field in arc,false,field);
  for(const field of Object.keys(arc).filter(field=>field!=="minFeeWei"&&field!=="keeperFeeBps"))assert.equal(field in profile,false,field);
});
