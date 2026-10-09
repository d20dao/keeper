import {deployProxy} from "../test/helpers/proxy.ts";
// Real Rust VRF on an isolated chain. Epoch 1 uses the initial catalog, signed API recipes, and is published from a signed CI fixture, which
// replay still verifies; the epochs after it use a catalog of drand test networks whose rounds D20BeaconVerifier checks onchain.
import assert from "node:assert/strict";
import {mkdir,mkdtemp,writeFile} from "node:fs/promises";
import {execFileSync} from "node:child_process";
import {join,resolve} from "node:path";
import {network} from "hardhat";
import {TEST_SECRET,publicKey} from "../test/helpers/proof.ts";
import {EPOCH_TEST_SIGNERS,signSelection} from "../test/helpers/epoch.ts";
import {beaconAttestation,registerTestBeacon,testBeacon} from "../test/helpers/beacon.ts";
import {readEpochRecipes,resolveEpochCatalog,selectEpoch,replayEpochCommitment,replayCoordinator,builtins,deriveRequestSeed,hashProof,decodeEvidencePacket,
  verifyEpochAttestation,type BeaconRegistration,type EpochCatalog,type EpochRecord,type RequestContext,type VRFProof} from "../src/index.ts";
const connection=await network.create("loadSim");const {ethers,provider,networkHelpers}=connection;
assert.equal((await ethers.provider.getNetwork()).chainId,31337n);
await mkdir(".research",{recursive:true});const dir=await mkdtemp(resolve(".research/epoch-demo-"));
const key=join(dir,"public-test.key");await writeFile(key,ethers.toBeHex(TEST_SECRET,32),{flag:"wx",mode:0o600});
const binary=resolve("keeper/target/debug/d20dao-keeper"+(process.platform==="win32"?".exe":""));
const [operator,player]=await ethers.getSigners(),fee=123n;
// Epoch 1 uses the initial four-recipe catalog; from epoch 2 a catalog of three drand test networks takes over. They share the test beacon's key
// and differ in their chain hash, and each is registered with the real BLS verifier (recipes 6 to 8, after the registry's own six).
const registry=await deployProxy(ethers,"EpochEntropy",[EPOCH_TEST_SIGNERS,operator.address,operator.address]);
const verifier=await ethers.deployContract("D20BeaconVerifier");
const networks:BeaconRegistration[]=[];
for(let n=1;n<=3;n++){
  const beacon={...testBeacon(await verifier.getAddress(),BigInt(await networkHelpers.time.latest())),chainHash:ethers.id(`D20 test beacon network ${n}`)};
  await registerTestBeacon(registry,beacon);networks.push(beacon);
}
const scheduled={recipes:networks.map((_,n)=>6+n),signers:await Promise.all(networks.map((_,n)=>registry.slotSigner(6+n))) as string[]};
await registry.scheduleCatalog(scheduled.recipes,scheduled.signers,2);
const rng=await deployProxy(ethers,"D20VRFCoordinator",[publicKey(),operator.address,operator.address,fee,1,await registry.getAddress(),5000]);
await rng.setPricing(fee,0,300000); // Flat pricing keeps the settlement arithmetic below exact; base-fee quoting is covered by test/Pricing.test.ts.
const game=await ethers.deployContract("TestMiningGame",[await rng.getAddress()]);
const base={registry:await registry.getAddress() as string,chainId:31337n,firstEpochStart:await registry.firstEpochStart() as bigint};
/// The catalog an epoch uses, with its recipe definitions read from the registry.
async function catalogOf(epochId:bigint):Promise<EpochCatalog>{
  const [hash,recipes,signers]=await registry.catalogAt(epochId);
  return resolveEpochCatalog({...base,recipeBook:await readEpochRecipes(ethers.provider,base.registry,recipes.map(Number))},{hash,recipes,signers});
}
const epochs:Array<{epochId:bigint;catalog:EpochCatalog;recipe:number;record:EpochRecord;commitTimestamp:bigint;packet:string;selection:unknown;txHash:string}>=[];
const requests:unknown[]=[];
const prepared=new Map<bigint,{api:{timestamp:bigint;data:string;signature:string};catalog:EpochCatalog;selected:ReturnType<typeof selectEpoch>}>();
async function mineTo(target:bigint){
  let block=BigInt(await provider.request({method:"eth_blockNumber",params:[]}));
  const timestamp=(await ethers.provider.getBlock("latest"))!.timestamp;
  while(block<target){await provider.request({method:"evm_mine",params:[timestamp]});block++;}
}
async function prepare(epochId:bigint){
  if(prepared.has(epochId))return;
  const start=await registry.epochStart(epochId),anchor=await ethers.provider.getBlock(Number(start-1n));
  const catalog=await catalogOf(epochId),selected=selectEpoch(catalog,epochId,anchor!.hash!);
  const onchain=await registry.getEpochSelection(epochId);
  assert.equal(selected.queryHash,onchain.queryHash);assert.equal(selected.recipe,Number(onchain.recipe));
  // Epoch 1's source is a signed fixture record; every later epoch's is the round of its drand test network current at the chain's time.
  const now=BigInt(await networkHelpers.time.latest());
  const api=selected.beacon===undefined?await signSelection(selected,now):beaconAttestation(selected.beacon,now);
  verifyEpochAttestation(selected,api,now);
  const snapshot={api,catalog,selected};
  prepared.set(epochId,snapshot);
  await writeFile(join(dir,`epoch-${epochId}-local.json`),JSON.stringify(snapshot,(_,v)=>typeof v==="bigint"?v.toString():v,2)+"\n");
  assert.equal((await registry.getEpoch(epochId)).epochHash,ethers.ZeroHash,"Local preparation published idle data");
}
async function publish(epochId:bigint,requestId:bigint){
  if((await registry.getEpoch(epochId)).epochHash!==ethers.ZeroHash)return;
  const demand=await rng.getRequest(requestId);assert.equal(demand.epochId,epochId);assert.equal(demand.fulfilled,false);
  assert(BigInt(await networkHelpers.time.latest())<=demand.deadline,"Expired demand must not publish");
  const snapshot=prepared.get(epochId);assert(snapshot,"Current epoch must be prepared once before publication");
  const {api,catalog,selected}=snapshot;
  const receipt=await(await registry.commitEpoch(epochId,api)).wait();
  const parsed=receipt!.logs.map((l:any)=>registry.interface.parseLog(l)).find((l:any)=>l?.name==="EpochCommitted")!;
  const e=await registry.getEpoch(epochId);
  const record:EpochRecord={epochHash:e.epochHash,catalogHash:e.catalogHash,anchorHash:e.anchorHash,source:e.source,queryHash:e.queryHash,dataHash:e.dataHash,attestationHash:e.attestationHash,signedAt:e.signedAt,committedBlock:e.committedBlock};
  const commitTimestamp=BigInt((await ethers.provider.getBlock(receipt!.blockNumber))!.timestamp);
  replayEpochCommitment({catalog,epochId,record,commitTimestamp,packet:parsed.args.packet});
  epochs.push({epochId,catalog,recipe:selected.recipe,record,commitTimestamp,packet:parsed.args.packet,selection:selected,txHash:receipt!.hash});
  console.log(JSON.stringify({epoch:epochId.toString(),source:selected.source,recipe:selected.recipe,epochHash:e.epochHash,dataBytes:ethers.getBytes(api.data).length,commitBlock:receipt!.blockNumber,startBlock:(await registry.epochStart(epochId)).toString()}));
}
async function request(claim:number){await game.submitVerifiedClaim(claim,ethers.id(`locked-claim-${claim}`),player.address,{value:fee});return (await game.claimRandomness(claim)).requestId;}
async function fulfill(id:bigint){
  const pending=await rng.getRequest(id);await publish(pending.epochId,id);
  const r=await rng.getRequest(id);await mineTo(r.targetBlock+await rng.confirmationBlocks());
  const anchor=await ethers.provider.getBlock(Number(r.targetBlock));
  const context:RequestContext={chainId:31337n,coordinator:await rng.getAddress(),keyHash:await rng.keyHash(),requestId:id,consumer:await game.getAddress(),clientSeed:r.clientSeed,mapping:builtins.raw(),requestBlock:r.requestBlock,targetBlock:r.targetBlock,blockHash:anchor!.hash!,epochId:r.epochId,epochHash:r.epochHash};
  const seed=await rng.requestSeed(id);assert.equal(seed,deriveRequestSeed(context));
  const p=JSON.parse(execFileSync(binary,["prove",ethers.toBeHex(seed,32),key],{encoding:"utf8",windowsHide:true,timeout:15000}));
  const proof:VRFProof={...p,pk:p.pk.map(BigInt),gamma:p.gamma.map(BigInt),c:BigInt(p.c),s:BigInt(p.s),seed:BigInt(p.seed),cGammaWitness:p.cGammaWitness.map(BigInt),sHashWitness:p.sHashWitness.map(BigInt),zInv:BigInt(p.zInv)};
  const calldata=rng.interface.encodeFunctionData("fulfillRandomness",[id,proof]);
  const receipt=await(await operator.sendTransaction({to:await rng.getAddress(),data:calldata,gasLimit:2000000})).wait();
  const rr=await rng.getRequest(id),block=await ethers.provider.getBlock(receipt!.blockNumber);
  const epoch=epochs.find(e=>e.epochId===r.epochId)!;
  const recorded={fulfilled:rr.fulfilled,randomness:rr.randomness,proofHash:rr.proofHash,transcriptHash:rr.transcriptHash};
  const replayInput={context,configuration:{publicKey:publicKey(),feeRecipient:await rng.initialFeeRecipient(),initialMinFee:fee,confirmationBlocks:1,registry:base.registry,catalogHash:await registry.catalogHash(),firstEpochStart:base.firstEpochStart},protocolConfigurationHash:await rng.protocolConfigurationHash(),epoch:{catalog:epoch.catalog,record:epoch.record,commitTimestamp:epoch.commitTimestamp,packet:epoch.packet},requestedAt:r.deadline-60n,deadline:r.deadline,acceptanceTimestamp:BigInt(block!.timestamp),acceptanceBlock:BigInt(block!.number),vrfProof:proof,recorded};
  const replay=replayCoordinator(replayInput);assert.equal(rr.delivered,true);
  const event=receipt!.logs.filter(l=>l.address.toLowerCase()===(rng.target as string).toLowerCase()).map(l=>rng.interface.parseLog(l)).find(l=>l?.name==="FulfillmentEvidence")!;
  assert.equal(hashProof(decodeEvidencePacket(event.args.packet).proof),rr.proofHash);
  requests.push({requestId:id,epochId:r.epochId,randomness:rr.randomness,tx:receipt!.hash,gas:receipt!.gasUsed,calldata,calldataBytes:ethers.getBytes(calldata).length,evidencePacket:event.args.packet,replayInput,replay});
  console.log(JSON.stringify({request:id.toString(),epoch:r.epochId.toString(),randomness:rr.randomness,gas:receipt!.gasUsed.toString(),calldataBytes:ethers.getBytes(calldata).length,replay:true}));
}
await mineTo(base.firstEpochStart);await prepare(1n);
assert.equal((await registry.queryFilter(registry.filters.EpochCommitted())).length,0,"Idle preparation must use no publication transaction");
const first=await request(1),batch=await request(2);
assert.equal((await rng.getRequest(first)).epochHash,ethers.ZeroHash);
await fulfill(first);await fulfill(batch);
assert.equal((await rng.getRequest(first)).targetBlock,(await rng.getRequest(batch)).targetBlock);
assert.equal(epochs.length,1,"Batch requests must share one snapshot");
await mineTo(await registry.epochStart(2)-2n);
const old=await request(3);await mineTo(await registry.epochStart(2));await prepare(2n);
const current=await request(4);await fulfill(old);await fulfill(current);
assert.equal(epochs[0].catalog.recipes,undefined,"Epoch 1 must use the initial catalog");
assert.deepEqual(epochs.at(-1)!.catalog.recipes,scheduled.recipes,"Epoch 2 must use the scheduled catalog");
assert.ok(epochs.slice(1).every(epoch=>"beacon" in epoch.catalog.recipeBook![epoch.recipe]!),"A scheduled epoch was not served by a drand network");
if(process.env.DEMO_ALL_SOURCES==="true") {
  let claim=5;
  for(let epoch=3n;epoch<=96n && new Set(epochs.slice(1).map(e=>e.recipe)).size<scheduled.recipes.length;epoch++) {
    await mineTo(await registry.epochStart(epoch));await prepare(epoch);
    const id=await request(claim++);await fulfill(id);
  }
  assert.equal(new Set(epochs.slice(1).map(e=>e.recipe)).size,scheduled.recipes.length,"Demo did not encounter every scheduled recipe within 96 epochs");
}
assert.equal(await rng.earnedFees(),BigInt(requests.length)*(fee-fee*5000n/10000n));
assert.equal(await rng.totalKeeperCredits(),0n);
await writeFile(join(dir,"trace.json"),JSON.stringify({network:"isolated EDR 31337",sourceMode:"signed CI fixture for epoch 1, drand test networks after it",scheduledCatalog:scheduled,epochs,requests,gameRequests:requests.length},(_,v)=>typeof v==="bigint"?v.toString():v,2)+"\n");
console.log(`TRACE=${join(dir,"trace.json")}`);await connection.close();
