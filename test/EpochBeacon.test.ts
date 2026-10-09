import {readFileSync} from "node:fs";
import {expect} from "chai";
import {network} from "hardhat";
import {deployProxy} from "./helpers/proxy.ts";
import {EPOCH_TEST_SIGNERS,PROVIDER_TEST_WALLETS,replayServedRequest,signSelection} from "./helpers/epoch.ts";
import {beaconAttestation,otherTestKey,signTestRound,testBeacon} from "./helpers/beacon.ts";
import {makeProof,proofOutput,publicKey} from "./helpers/proof.ts";
import {BEACON_TEMPLATE,beaconCanonicalRequest,beaconRoundAt,beaconRoundTime,beaconSlotSigner,decodeEpochEvidencePacket,epochCatalogHash,hashAttestation,
  readEpochRecipes,replayEpochCommitment,resolveEpochCatalog,selectEpoch,verifyEpochAttestation} from "../src/index.ts";

const {ethers,networkHelpers,provider}=await network.create();
// The runtime bytecode of de5f82e, which both public registries ran until their drand upgrades: it has the recipe registry and no beacon views. The registry a rollback returns to.
const liveEpoch=JSON.parse(readFileSync(new URL("./fixtures/epoch-entropy-deployed-de5f82e.json",import.meta.url),"utf8"));
const FEE=10n**15n,CALLBACK_GAS=200_000,CHAIN_ID=31337n,HYPERLIQUID=PROVIDER_TEST_WALLETS.hyperliquid.address;
// The gas cap that scripts/admin.ts register-beacon sends with.
const REGISTRATION_GAS_CAP=950_000n;
const utf8=(text:string)=>ethers.hexlify(ethers.toUtf8Bytes(text));
const flipBit=(hex:string)=>ethers.toBeHex(BigInt(hex)^1n,ethers.dataLength(hex));
const chainTime=async()=>BigInt(await networkHelpers.time.latest());
async function mineTo(block:bigint){const head=BigInt(await ethers.provider.getBlockNumber());if(block>head)await networkHelpers.mine(Number(block-head));}

/// A registry with the coordinator that serves it, and the test beacon with its verifier. Each test registers the beacon itself, as recipe 6.
async function fixture(){
  const [owner,committer,stranger,user]=await ethers.getSigners();
  const registry=await deployProxy(ethers,"EpochEntropy",[EPOCH_TEST_SIGNERS,owner.address,committer.address]);
  const address=await registry.getAddress();
  const rng=await deployProxy(ethers,"D20VRFCoordinator",[publicKey(),owner.address,owner.address,FEE,1,address,0]);
  await rng.setPricing(FEE,0,300000);
  const consumer=await ethers.deployContract("TestConsumer",[await rng.getAddress()]);
  const verifier=await ethers.deployContract("D20BeaconVerifier");
  return {registry,rng,consumer,address,owner,committer,stranger,user,beacon:testBeacon(await verifier.getAddress(),await chainTime())};
}
type Fixture=Awaited<ReturnType<typeof fixture>>;
type Attestation={timestamp:bigint;data:string;signature:string};
const register=(registry:any,b:Fixture["beacon"],sampleRound=1n)=>registry.registerBeacon(b.verifier,b.chainHash,b.publicKey,b.genesis,b.period,sampleRound,signTestRound(sampleRound));
const commit=(c:Fixture,epoch:bigint,a:Attestation,attempt=0)=>attempt===0?c.registry.connect(c.committer).commitEpoch(epoch,a):c.registry.connect(c.committer).commitEpochFallback(epoch,attempt,a);
/// Schedule a catalog two epochs ahead and mine to the start of its first epoch.
async function scheduleAndMine(c:Fixture,recipes:number[],signers:string[]){
  const from=await c.registry.epochForBlock(await ethers.provider.getBlockNumber())+2n;
  await c.registry.scheduleCatalog(recipes,signers,from);
  await mineTo(await c.registry.epochStart(from));
  return from;
}
/// The first epoch from `from` on whose selected recipe satisfies `want`, mined to its start.
async function epochSelecting(c:Fixture,from:bigint,want:(recipe:number)=>boolean){
  for(let epoch=from;epoch<from+60n;epoch++){
    await mineTo(await c.registry.epochStart(epoch));
    if(want(Number((await c.registry.getEpochSelection(epoch)).recipe)))return epoch;
  }
  throw new Error("No epoch selected the wanted recipe");
}
/// The receipt of a transaction that reverts: the node mines it, then rejects the send with its hash.
async function failedReceipt(send:()=>Promise<unknown>){
  const error:any=await send().then(()=>undefined,(caught:unknown)=>caught);
  if(!error?.transactionHash)throw new Error("Expected a mined transaction that reverted");
  return (await ethers.provider.getTransactionReceipt(error.transactionHash))!;
}
const REGISTRY_ERRORS=(await ethers.getContractFactory("EpochEntropy")).interface;
/// What a call answers, in a word: `returns <value>`, the name of the registry's custom error it reverted with, or `fails` when it ran
/// out of gas or reverted without one. The provider's error carries the revert data.
async function outcome(call:()=>Promise<unknown>):Promise<string>{
  try{return `returns ${String(await call())}`;}
  catch(error:any){return typeof error?.data==="string"&&error.data!=="0x"?REGISTRY_ERRORS.parseError(error.data)?.name??"fails":"fails";}
}
const TOO_LOW="BeaconGasTooLow";
/// The smallest gas limit above `low` at which `at` stops answering BeaconGasTooLow: it answers that at `low` and something else at `high`.
async function firstPassing(at:(gas:number)=>Promise<string>,low:number,high:number){
  expect(await at(low),`gas limit ${low}`).to.equal(TOO_LOW);expect(await at(high),`gas limit ${high}`).not.to.equal(TOO_LOW);
  while(high-low>1){const middle=Math.floor((low+high)/2);if(await at(middle)===TOO_LOW)low=middle;else high=middle;}
  return high;
}
/// Gas limits around a threshold: each one within 40, and then steps out to 250,000 below and 500,000 above it.
const OFFSETS=[...Array.from({length:81},(_,i)=>i-40),-250_000,-100_000,-50_000,-20_000,-10_000,-5_000,-2_000,-1_000,-500,-250,-100,-50,50,100,250,500,1_000,2_000,5_000,10_000,50_000,500_000];
/// Below the threshold every gas limit answers BeaconGasTooLow; at and above it none does, and `above` holds of every answer.
async function sweep(name:string,at:(gas:number)=>Promise<string>,threshold:number,above:(answer:string)=>boolean){
  for(const offset of OFFSETS){
    const gas=threshold+offset,answer=await at(gas);
    if(offset<0)expect(answer,`${name} at ${gas} (${offset})`).to.equal(TOO_LOW);
    else expect(above(answer),`${name} at ${gas} (+${offset}) answered ${answer}`).to.equal(true);
  }
}

describe("Beacon recipes in the epoch registry",function(){
  this.timeout(120_000);

  it("registers a beacon only for the owner, and only when its verifier, schedule, key and sample signature check out",async()=>{
    const c=await networkHelpers.loadFixture(fixture),{registry,beacon,stranger}=c,current=beaconRoundAt(beacon,await chainTime());
    const good={verifier:beacon.verifier,chainHash:beacon.chainHash,publicKey:beacon.publicKey,genesis:beacon.genesis,period:beacon.period,sampleRound:1n,sampleSignature:signTestRound(1n)};
    const call=(runner:any,args:typeof good)=>runner.registerBeacon(args.verifier,args.chainHash,args.publicKey,args.genesis,args.period,args.sampleRound,args.sampleSignature);
    const refused:Array<[string,Partial<typeof good>]>=[
      ["a verifier that is an account",{verifier:stranger.address}],["the zero address as verifier",{verifier:ethers.ZeroAddress}],
      ["a zero chain hash",{chainHash:ethers.ZeroHash}],["genesis 0",{genesis:0n}],["period 0",{period:0n}],["sample round 0",{sampleRound:0n}],
      ["a sample round scheduled after the block time",{sampleRound:current+100n,sampleSignature:signTestRound(current+100n)}],
      ["a 127-byte key",{publicKey:beacon.publicKey.slice(0,-2)}],["a 129-byte key",{publicKey:beacon.publicKey+"00"}],["an empty key",{publicKey:"0x"}],
      ["an off-curve key",{publicKey:flipBit(beacon.publicKey)}],
      ["a sample signature of another round",{sampleSignature:signTestRound(2n)}],["a tampered sample signature",{sampleSignature:flipBit(signTestRound(1n))}],
      ["a 63-byte sample signature",{sampleSignature:signTestRound(1n).slice(0,-2)}],["an empty sample signature",{sampleSignature:"0x"}],
      ["a well-formed key that did not sign the sample",{publicKey:otherTestKey()}],
    ];
    for(const [name,patch] of refused)await expect(call(registry,{...good,...patch}),name).to.be.revertedWithCustomError(registry,"InvalidConfig");
    await expect(call(registry.connect(stranger),good)).to.be.revertedWithCustomError(registry,"OwnableUnauthorizedAccount").withArgs(stranger.address);
    // Nothing was registered by any refusal.
    expect(await registry.recipeCount()).to.equal(6n);
    await expect(registry.beaconOf(6)).to.be.revertedWithCustomError(registry,"InvalidConfig");

    // A sample round may be scheduled at the block's own time, and not one second later.
    const sampleRound=current+2n,time=beaconRoundTime(beacon,sampleRound),sampled={...good,sampleRound,sampleSignature:signTestRound(sampleRound)};
    await networkHelpers.time.setNextBlockTimestamp(time-1n);
    await expect(call(registry,sampled),"a sample round one second ahead").to.be.revertedWithCustomError(registry,"InvalidConfig");
    await networkHelpers.time.setNextBlockTimestamp(time);
    const request=beaconCanonicalRequest(beacon.chainHash);
    await expect(call(registry,sampled))
      .to.emit(registry,"RecipeRegistered").withArgs(6,ethers.id(request),request,BEACON_TEMPLATE,request)
      .and.to.emit(registry,"BeaconRegistered").withArgs(6,beacon.verifier,beacon.chainHash,beacon.publicKey,beacon.genesis,beacon.period);
    expect(await registry.recipeCount()).to.equal(7n);
    expect(Array.from(await registry.beaconOf(6))).to.deep.equal([beacon.verifier,beacon.genesis,beacon.period,beacon.chainHash,beacon.publicKey]);
    // A signed recipe has no beacon, and an id that nothing registered has no answer.
    expect(Array.from(await registry.beaconOf(0))).to.deep.equal([ethers.ZeroAddress,0n,0n,ethers.ZeroHash,"0x"]);
    for(const recipe of [7,255])await expect(registry.beaconOf(recipe),`recipe ${recipe}`).to.be.revertedWithCustomError(registry,"InvalidConfig");
  });

  it("serves a request from a beacon epoch, records the round it committed and replays the request from public data",async()=>{
    const c=await networkHelpers.loadFixture(fixture),{registry,rng,consumer,committer,user,beacon}=c;
    await register(registry,beacon);
    const slot=await registry.slotSigner(6),request=beaconCanonicalRequest(beacon.chainHash),queryHash=ethers.id(request);
    expect(slot).to.equal(beaconSlotSigner(beacon));
    const epoch=await scheduleAndMine(c,[6],[slot]),start=await registry.epochStart(epoch);
    const [hash,recipes,signers]=await registry.catalogAt(epoch),selection=await registry.getEpochSelection(epoch);
    expect([hash,recipes.map(Number),Array.from(signers)]).to.deep.equal([epochCatalogHash([slot],[6]),[6],[slot]]);
    expect([selection.source,selection.recipe,selection.airnode,selection.queryHash,selection.canonicalRequest]).to.deep.equal([0n,6n,slot,queryHash,request]);

    // The request escrows its fee at the start of the epoch, before anything is published.
    await consumer.request(ethers.id("beacon request"),CALLBACK_GAS,user.address,{value:FEE});
    const id=await consumer.lastRequestId() as bigint,pending=await rng.getRequest(id);
    expect([pending.epochId,pending.targetBlock,pending.epochHash]).to.deep.equal([epoch,0n,ethers.ZeroHash]);
    // The round current at the epoch's start, signed by the beacon.
    const startTime=BigInt((await ethers.provider.getBlock(Number(start)))!.timestamp),a=beaconAttestation(beacon,startTime),round=beaconRoundAt(beacon,startTime);
    const committed=(await(await commit(c,epoch,a)).wait())!,record=await registry.getEpoch(epoch);
    expect(a.timestamp).to.equal(beaconRoundTime(beacon,round)).and.to.be.at.most(startTime);
    expect([record.source,record.queryHash,record.signedAt,record.dataHash,record.attestationHash,record.catalogHash,record.anchorHash,record.committedBlock]).to.deep.equal(
      [0n,queryHash,a.timestamp,ethers.keccak256(utf8(String(round))),hashAttestation(queryHash,a),epochCatalogHash([slot],[6]),(await ethers.provider.getBlock(Number(start)-1))!.hash,BigInt(committed.blockNumber)]);
    // The published packet carries the beacon's request and the signed round exactly.
    const event=registry.interface.parseLog(committed.logs[0])!;
    expect([event.name,event.args.epochHash]).to.deep.equal(["EpochCommitted",record.epochHash]);
    expect(decodeEpochEvidencePacket(event.args.packet)).to.deep.equal({canonicalRequest:request,attestation:a});

    // The pending request resolves to a target strictly after the commit; the deadline never moves.
    const resolved=await rng.getRequest(id);
    expect([resolved.targetBlock,resolved.epochHash,resolved.deadline]).to.deep.equal([BigInt(committed.blockNumber)+1n,record.epochHash,pending.deadline]);
    await networkHelpers.mine(2);
    const proof=makeProof(await rng.requestSeed(id));
    const accepted=(await(await rng.fulfillRandomness(id,proof,{gasLimit:2_000_000})).wait())!;
    expect(await consumer.results(id)).to.equal(proofOutput(proof));
    const replayed=await replayServedRequest(ethers,c,id,proof,committed,accepted,FEE);
    expect(replayed.proof.matchesRecordedState).to.equal(true);
    expect([replayed.request.epochId,replayed.request.epochHash]).to.deep.equal([epoch,record.epochHash]);
  });

  it("refuses a stale round, malformed round data and an outsider, accepts a round exactly 240 seconds old, and commits an epoch only once",async()=>{
    const c=await networkHelpers.loadFixture(fixture),{registry,address,owner,stranger,beacon}=c;
    await register(registry,beacon);
    const slot=await registry.slotSigner(6),epoch=await scheduleAndMine(c,[6],[slot]),start=await registry.epochStart(epoch);
    // The library's view of the same epoch: its catalog, the registry's own recipe definitions and the selected beacon recipe.
    const [hash,recipes,signers]=await registry.catalogAt(epoch);
    const catalog=resolveEpochCatalog({registry:address,chainId:CHAIN_ID,firstEpochStart:await registry.firstEpochStart(),
      recipeBook:await readEpochRecipes(ethers.provider,address,[6])},{hash,recipes,signers});
    const selected=selectEpoch(catalog,epoch,(await ethers.provider.getBlock(Number(start)-1))!.hash!);
    const head=await chainTime(),a=beaconAttestation(beacon,head),round=beaconRoundAt(beacon,head),time=a.timestamp;
    // The commit is simulated in a block with a chosen timestamp; the library gets the same one.
    const refuse=async(name:string,bad:Attestation,error:string,message:string,at:bigint)=>{
      await networkHelpers.time.setNextBlockTimestamp(at);
      await expect(commit(c,epoch,bad),name).to.be.revertedWithCustomError(registry,error);
      expect(()=>verifyEpochAttestation(selected,bad,at),name).to.throw(message);
    };

    for(const outsider of [stranger,owner])await expect(registry.connect(outsider).commitEpoch(epoch,a),outsider.address).to.be.revertedWithCustomError(registry,"OnlyCommitter");
    // Older than 240 seconds when committed: a round from long before, and this round one second past the bound.
    await refuse("a round from 1000 seconds before",beaconAttestation(beacon,time-1000n),"InvalidTime","Invalid epoch attestation time",head+1n);
    await refuse("a round 241 seconds old",a,"InvalidTime","Invalid epoch attestation time",time+241n);
    // The data of a round is its number in decimal: 1 to 19 digits without a leading zero.
    for(const [name,text] of [["the data 0","0"],["a 20-digit number","1".repeat(20)],["empty data",""],["a non-digit",`${round}x`]])
      await refuse(name,{...a,data:utf8(text)},"InvalidData","Invalid exact epoch data",head+1n);

    // Exactly 240 seconds old is still fresh, onchain and in the library.
    await networkHelpers.time.setNextBlockTimestamp(time+240n);
    expect(verifyEpochAttestation(selected,a,time+240n).signer).to.equal(slot);
    const receipt=(await(await commit(c,epoch,a)).wait())!,record=await registry.getEpoch(epoch);
    expect(BigInt((await ethers.provider.getBlock(receipt.blockNumber))!.timestamp)).to.equal(time+240n);
    expect(record.signedAt).to.equal(time);
    const packet=registry.interface.parseLog(receipt.logs[0])!.args.packet;
    expect(replayEpochCommitment({catalog,epochId:epoch,record,commitTimestamp:time+240n,packet}).epochHash).to.equal(record.epochHash);
    // The epoch is published once: the same round and a newer one are both refused.
    for(const again of [a,beaconAttestation(beacon,time+240n)])await expect(commit(c,epoch,again)).to.be.revertedWithCustomError(registry,"AlreadyCommitted");
    expect((await registry.getEpoch(epoch)).epochHash).to.equal(record.epochHash);
  });

  it("commits the selected source of a mixed catalog whichever kind it is, and its fallback source of the other kind",async()=>{
    const c=await networkHelpers.loadFixture(fixture),{registry,address,beacon}=c;
    await register(registry,beacon);
    const slot=await registry.slotSigner(6),firstEpochStart=await registry.firstEpochStart(),[signedQuery]=await registry.getRecipe(0);
    const recipeBook=await readEpochRecipes(ethers.provider,address,[0,6]);
    expect("beacon" in recipeBook[6]!).to.equal(true);expect("beacon" in recipeBook[0]!).to.equal(false);
    const covered=new Set<string>();
    /// Commit one attempt of an epoch with the attestation of its kind, refuse the other kind's, and replay the published epoch.
    const commitAndReplay=async(epoch:bigint,attempt:number)=>{
      const selection=await registry.getEpochFallbackSelection(epoch,attempt),isBeacon=Number(selection.recipe)===6,time=await chainTime();
      const right=isBeacon?beaconAttestation(beacon,time):await signSelection(selection,time);
      // The other kind's attestation is well formed and validly signed for its own recipe, and this slot's recipe fixes another data shape.
      const wrong=isBeacon?await signSelection({recipe:0,airnode:HYPERLIQUID,queryHash:signedQuery},time):beaconAttestation(beacon,time);
      await expect(commit(c,epoch,wrong,attempt)).to.be.revertedWithCustomError(registry,"InvalidData");
      const receipt=(await(await commit(c,epoch,right,attempt)).wait())!,record=await registry.getEpoch(epoch);
      const [hash,recipes,signers]=await registry.catalogAt(epoch);
      const catalog=resolveEpochCatalog({registry:address,chainId:CHAIN_ID,firstEpochStart,recipeBook},{hash,recipes,signers});
      const replayed=replayEpochCommitment({catalog,epochId:epoch,record,commitTimestamp:BigInt((await ethers.provider.getBlock(receipt.blockNumber))!.timestamp),
        packet:registry.interface.parseLog(receipt.logs[0])!.args.packet});
      expect([record.source,replayed.epochHash,replayed.signer,replayed.selected.recipe,replayed.selected.beacon!==undefined]).to.deep.equal(
        [selection.source,record.epochHash,selection.airnode,Number(selection.recipe),isBeacon]);
      covered.add(`attempt ${attempt} ${isBeacon?"beacon":"signed"}`);
    };
    /// The selected source at attempt 0, then the fallback source at attempt 1 once its window opens, each from the epoch's start.
    const exercise=async(epoch:bigint)=>{
      const snapshot=await networkHelpers.takeSnapshot();
      await commitAndReplay(epoch,0);await snapshot.restore();
      await mineTo(await registry.fallbackOpensAt(epoch,1));
      await commitAndReplay(epoch,1);await snapshot.restore();
    };
    // Either order of the two recipes: the beacon's kind travels with the recipe, not with slot 0 or slot 1.
    for(const order of [[6,0],[0,6]]){
      const signers=order.map(recipe=>recipe===6?slot:HYPERLIQUID),head=await ethers.provider.getBlockNumber();
      // Each recipe needs its own kind of signer: the beacon's identity in the beacon's slot, an Airnode in the signed one.
      await expect(registry.scheduleCatalog(order,[...signers].reverse(),await registry.epochForBlock(head)+2n)).to.be.revertedWithCustomError(registry,"InvalidConfig");
      let epoch=await scheduleAndMine(c,order,signers);
      // One epoch that selects the beacon first and one that selects the signed recipe first, so each kind is committed both ways.
      for(const beaconFirst of [true,false]){
        epoch=await epochSelecting(c,epoch,recipe=>(recipe===6)===beaconFirst);
        await exercise(epoch);epoch++;
      }
    }
    expect([...covered].sort()).to.deep.equal(["attempt 0 beacon","attempt 0 signed","attempt 1 beacon","attempt 1 signed"]);
  });

  it("counts a verifier that spends all its gas, reverts or answers anything but a true as an invalid signature, and gives it at most BEACON_VERIFY_GAS",async()=>{
    const c=await networkHelpers.loadFixture(fixture),{registry,committer}=c;
    const hostile=await ethers.deployContract("TestBeaconVerifier"),beacon={...c.beacon,verifier:await hostile.getAddress()};
    // In mode 0 it accepts anything, so the beacon registers whatever sample it is shown.
    await registry.registerBeacon(beacon.verifier,beacon.chainHash,beacon.publicKey,beacon.genesis,beacon.period,1,"0x");
    const slot=await registry.slotSigner(6),epoch=await scheduleAndMine(c,[6],[slot]),a=beaconAttestation(beacon,await chainTime()),round=beaconRoundAt(beacon,a.timestamp);
    expect(await registry.BEACON_VERIFY_GAS()).to.equal(400_000n);
    expect(await registry.verifyBeacon(6,round,a.signature)).to.equal(true);
    // Loops forever, reverts, false, the word 2, no answer, and a true with a second word behind it: only a lone 32-byte true counts.
    const broken=[1,2,3,4,5,6];
    for(const mode of broken){
      await hostile.setMode(mode);
      expect(await registry.verifyBeacon(6,round,a.signature),`mode ${mode}`).to.equal(false);
      await expect(commit(c,epoch,a),`mode ${mode}`).to.be.revertedWithCustomError(registry,"InvalidSigner");
    }
    // Registration asks the same question under the same allowance, so a verifier that answers badly then cannot register either.
    const unregistered=await ethers.deployContract("TestBeaconVerifier");
    for(const mode of broken){
      await unregistered.setMode(mode);
      await expect(registry.registerBeacon(await unregistered.getAddress(),beacon.chainHash,beacon.publicKey,beacon.genesis,beacon.period,1,"0x"),`mode ${mode}`)
        .to.be.revertedWithCustomError(registry,"InvalidConfig");
    }
    expect(await registry.recipeCount()).to.equal(7n);
    // Mode 1 loops forever. Sent with a gas limit far above what the registry forwards, it fails having used the verifier's
    // allowance and the registry's own work only, the same under a limit of a million.
    await hostile.setMode(1);
    const used:bigint[]=[];
    for(const gasLimit of [1_000_000,10_000_000]){
      const failed=await failedReceipt(()=>registry.connect(committer).commitEpoch(epoch,a,{gasLimit}));
      expect(failed.status).to.equal(0);used.push(failed.gasUsed);
    }
    console.log(`Failed commitEpoch with a verifier that loops forever: gas=${used[1]} (limits 1,000,000 and 10,000,000 used the same)`);
    expect(used[0]).to.equal(used[1]);
    expect(used[1]).to.be.greaterThan(400_000n).and.to.be.lessThan(1_000_000n);
    // None of that spoiled the epoch: it commits once the verifier answers.
    await hostile.setMode(0);
    await commit(c,epoch,a);
    expect((await registry.getEpoch(epoch)).signedAt).to.equal(a.timestamp);

    // A broken beacon cannot stall a catalog that has another source: when it is selected, the fallback commits after its window.
    await hostile.setMode(2);
    const from=await scheduleAndMine(c,[6,0],[slot,HYPERLIQUID]),stalled=await epochSelecting(c,from,recipe=>recipe===6);
    await expect(commit(c,stalled,beaconAttestation(beacon,await chainTime()))).to.be.revertedWithCustomError(registry,"InvalidSigner");
    await mineTo(await registry.fallbackOpensAt(stalled,1));
    const fallback=await registry.getEpochFallbackSelection(stalled,1);
    expect(Number(fallback.recipe)).to.equal(0);
    await commit(c,stalled,await signSelection(fallback,await chainTime()),1);
    expect((await registry.getEpoch(stalled)).source).to.equal(fallback.source);
  });

  it("commits a beacon epoch for about one verifier call more than a signed epoch",async()=>{
    const c=await networkHelpers.loadFixture(fixture),{registry,beacon}=c;
    await register(registry,beacon);
    // One source per catalog and no request before either commit, so both commits checkpoint and record their epoch alike.
    const signedEpoch=await scheduleAndMine(c,[0],[HYPERLIQUID]);
    const signed=(await(await commit(c,signedEpoch,await signSelection(await registry.getEpochSelection(signedEpoch),await chainTime()))).wait())!;
    const beaconEpoch=await scheduleAndMine(c,[6],[await registry.slotSigner(6)]);
    const beaconed=(await(await commit(c,beaconEpoch,beaconAttestation(beacon,await chainTime()))).wait())!;
    console.log(`commitEpoch gas: signed recipe=${signed.gasUsed}, beacon=${beaconed.gasUsed} (+${beaconed.gasUsed-signed.gasUsed})`);
    expect((await registry.getEpoch(signedEpoch)).epochHash).not.to.equal(ethers.ZeroHash);expect((await registry.getEpoch(beaconEpoch)).epochHash).not.to.equal(ethers.ZeroHash);
    expect(beaconed.gasUsed).to.be.lessThan(signed.gasUsed+250_000n);
  });

  it("reverts BeaconGasTooLow, never InvalidSigner, InvalidConfig or false, when the sender's gas cannot give the verifier its whole allowance",async()=>{
    const c=await networkHelpers.loadFixture(fixture),{registry,committer,beacon}=c;
    await register(registry,beacon);
    // Recipe 7 is the same network under a verifier that answers true only when it was given nearly all of BEACON_VERIFY_GAS. A registry
    // that let the 63/64 rule shorten the allowance would get a no from it at the gas limits just below the threshold, and would read that
    // no as an invalid signature. The control shows that it does answer no when it is given less.
    const probe:any=await ethers.deployContract("TestBeaconVerifier"),watched={...beacon,verifier:await probe.getAddress()};
    await probe.setMode(7);await probe.setKeyMode(7);
    expect([await probe.verifyRound.staticCall("0x",1,"0x",{gasLimit:380_000}),await probe.verifyRound.staticCall("0x",1,"0x",{gasLimit:450_000})]).to.deep.equal([false,true]);
    expect([await probe.isValidPublicKey.staticCall("0x",{gasLimit:380_000}),await probe.isValidPublicKey.staticCall("0x",{gasLimit:450_000})]).to.deep.equal([false,true]);
    await register(registry,watched);
    const realSlot=await registry.slotSigner(6),watchedSlot=await registry.slotSigner(7);

    // The view: the verifier's answer, or the error, and nothing else at any gas limit.
    const a0=beaconAttestation(beacon,await chainTime()),round=beaconRoundAt(beacon,a0.timestamp),viewThresholds:number[]=[];
    for(const [recipe,name] of [[6,"the reference verifier"],[7,"the watching verifier"]] as const){
      const at=(gasLimit:number)=>outcome(()=>registry.verifyBeacon.staticCall(recipe,round,a0.signature,{gasLimit}));
      const threshold=await firstPassing(at,100_000,3_000_000);
      // The check needs the allowance, its 63/64 share and a margin left, on top of what the view spends before it and the intrinsic cost.
      expect(threshold,name).to.be.greaterThan(400_000+6_349+21_000).and.to.be.lessThan(500_000);
      await sweep(`verifyBeacon, ${name}`,at,threshold,answer=>answer==="returns true");
      viewThresholds.push(threshold);
    }
    expect(viewThresholds[0]).to.equal(viewThresholds[1]);

    // A commit, in the epoch each recipe's catalog opens: published once the gas is enough, and never refused as an invalid signature. The two
    // epochs are some 400 seconds apart, too far for one round to be fresh at both, so the commits carry different rounds and signatures. Calldata
    // costs 4 gas for a zero byte and 16 for any other, so a signature with one zero byte more or less moves a threshold by 12 gas, whichever
    // verifier the recipe has. The thresholds are compared without the cost of their own calldata.
    const calldataGas=(data:string)=>ethers.getBytes(data).reduce((gas,byte)=>gas+(byte===0?4:16),0);
    const thresholds:number[]=[],calldata:number[]=[];
    for(const [recipe,slot,name] of [[6,realSlot,"the reference verifier"],[7,watchedSlot,"the watching verifier"]] as const){
      const epoch=await scheduleAndMine(c,[recipe],[slot]),a=beaconAttestation(beacon,await chainTime());
      const at=(gasLimit:number)=>outcome(()=>registry.connect(committer).commitEpoch.staticCall(epoch,a,{gasLimit}));
      const threshold=await firstPassing(at,250_000,3_000_000);
      await sweep(`commitEpoch, ${name}`,at,threshold,answer=>answer.startsWith("returns")||answer==="fails");
      expect(await at(threshold+60_000),name).to.match(/^returns/);
      thresholds.push(threshold);calldata.push(calldataGas(registry.interface.encodeFunctionData("commitEpoch",[epoch,a])));
    }
    expect(thresholds[0]-calldata[0],"the thresholds, less the gas of their own calldata").to.equal(thresholds[1]-calldata[1]);

    // A registration: gas short of the allowance at either of its two verifier questions is the same error, never InvalidConfig.
    const args=[watched.verifier,watched.chainHash,watched.publicKey,watched.genesis,watched.period,1,signTestRound(1n)] as const;
    const registerAt=(gasLimit:number)=>outcome(()=>registry.registerBeacon.staticCall(...args,{gasLimit}));
    const registration=await firstPassing(registerAt,150_000,3_000_000);
    await sweep("registerBeacon",registerAt,registration,answer=>answer!=="InvalidConfig");
    // The registry needs far more than that to store the registration: at the threshold the call runs out of gas, and well above it registers.
    expect(await registerAt(registration)).to.equal("fails");expect(await registerAt(700_000)).to.match(/^returns 8/);
    console.log(`BeaconGasTooLow below these gas limits: verifyBeacon ${viewThresholds[0]}, commitEpoch ${thresholds[0]}, registerBeacon ${registration}`);
  });

  it("commits a beacon epoch with the gas limit the keeper derives from eth_estimateGas, and registers within the administration script's gas cap",async()=>{
    const c=await networkHelpers.loadFixture(fixture),{registry,committer,beacon}=c;
    const registration=(await(await register(registry,beacon)).wait())!;
    const epoch=await scheduleAndMine(c,[6],[await registry.slotSigner(6)]),a=beaconAttestation(beacon,await chainTime());
    const at=(gasLimit:number)=>outcome(()=>registry.connect(committer).commitEpoch.staticCall(epoch,a,{gasLimit}));
    const estimate=await registry.connect(committer).commitEpoch.estimateGas(epoch,a);
    // The estimate is a limit at which the commit succeeds, and no lower limit does: it is the smallest, up to the node's rounding.
    expect(await at(Number(estimate))).to.match(/^returns/);
    const minimum=await firstPassing(at,250_000,Number(estimate)+1);
    expect(estimate>=BigInt(minimum)&&estimate-BigInt(minimum)<5_000n,`estimate ${estimate}, minimum ${minimum}`).to.equal(true);
    // The keeper sends estimate × 1.2 + 50,000, and pays for what is used, not for the limit.
    const limit=estimate*12n/10n+50_000n,receipt=(await(await registry.connect(committer).commitEpoch(epoch,a,{gasLimit:limit})).wait())!;
    expect(receipt.status).to.equal(1);expect(receipt.gasUsed).to.be.lessThan(estimate);
    expect((await registry.getEpoch(epoch)).signedAt).to.equal(a.timestamp);
    console.log(`Beacon commitEpoch: gas used=${receipt.gasUsed}, eth_estimateGas=${estimate}, smallest gas limit that succeeds=${minimum}, keeper limit=${limit}; registerBeacon gas used=${registration.gasUsed}`);
    expect(registration.gasUsed).to.be.lessThan(REGISTRATION_GAS_CAP);
  });

  it("reads at most 32 bytes of a verifier's answer to either question",async()=>{
    const c=await networkHelpers.loadFixture(fixture),{registry,committer,beacon}=c;
    const hostile:any=await ethers.deployContract("TestBeaconVerifier"),watched={...beacon,verifier:await hostile.getAddress()};
    await registry.registerBeacon(watched.verifier,watched.chainHash,watched.publicKey,watched.genesis,watched.period,1,"0x");
    const epoch=await scheduleAndMine(c,[6],[await registry.slotSigner(6)]),a=beaconAttestation(watched,await chainTime());
    // Mode 8 answers true followed by 200,000 bytes. That costs the verifier the memory for them, 95,071 gas, and would cost a caller that
    // copied them as much again and more for the copy. Mode 5 answers with nothing. The registry refuses both, and what the long answer
    // adds to the cost of the refused call is the verifier's memory and nothing else: the registry does not read the rest.
    const words=(200_032n+31n)/32n,memory=3n*words+words*words/512n;
    expect(memory).to.equal(95_071n);
    const commitGas=async(mode:number)=>{
      await hostile.setMode(mode);
      await expect(commit(c,epoch,a),`mode ${mode}`).to.be.revertedWithCustomError(registry,"InvalidSigner");
      const failed=await failedReceipt(()=>registry.connect(committer).commitEpoch(epoch,a,{gasLimit:5_000_000}));
      expect(failed.status).to.equal(0);return failed.gasUsed;
    };
    const [none,long]=[await commitGas(5),await commitGas(8)];
    const registerGas=async(question:"key"|"round",mode:number)=>{
      const fresh:any=await ethers.deployContract("TestBeaconVerifier");
      if(question==="key")await fresh.setKeyMode(mode);else await fresh.setMode(mode);
      const send=()=>registry.registerBeacon(fresh.target,beacon.chainHash,beacon.publicKey,beacon.genesis,beacon.period,1,"0x",{gasLimit:5_000_000});
      await expect(send(),`${question} mode ${mode}`).to.be.revertedWithCustomError(registry,"InvalidConfig");
      const failed=await failedReceipt(send);
      expect(failed.status).to.equal(0);return failed.gasUsed;
    };
    const [noneKey,longKey,noneRound,longRound]=[await registerGas("key",5),await registerGas("key",8),await registerGas("round",5),await registerGas("round",8)];
    console.log(`Gas of a refused call, no answer against 200,032 bytes: commitEpoch ${none}/${long}, registerBeacon isValidPublicKey ${noneKey}/${longKey}, verifyRound ${noneRound}/${longRound}`);
    for(const [name,x,y] of [["commitEpoch",none,long],["isValidPublicKey",noneKey,longKey],["verifyRound",noneRound,longRound]] as const)
      expect(y-x,`${name}: a 200,032-byte answer against none`).to.be.within(memory,memory+1_000n);
  });

  it("counts an isValidPublicKey that reverts, burns its gas or answers anything but a true as an invalid key, and gives it at most BEACON_VERIFY_GAS",async()=>{
    const c=await networkHelpers.loadFixture(fixture),{registry,beacon}=c;
    const key:any=await ethers.deployContract("TestBeaconVerifier");
    const args=[await key.getAddress(),beacon.chainHash,beacon.publicKey,beacon.genesis,beacon.period,1,"0x"] as const;
    // Only a lone 32-byte true is a valid key: a revert, no answer, false, the word 2, true with a second word and true with 200,000 more bytes are not.
    for(const mode of [1,2,3,4,5,6,8]){
      await key.setKeyMode(mode);
      await expect(registry.registerBeacon(...args),`key mode ${mode}`).to.be.revertedWithCustomError(registry,"InvalidConfig");
    }
    // Mode 1 loops forever. Without a gas limit on the question it would take 63/64 of everything the sender gave; with it, both limits end
    // having spent the allowance and the registry's own work only, however much more the sender gave.
    await key.setKeyMode(1);
    const used:bigint[]=[];
    for(const gasLimit of [1_000_000,10_000_000]){
      const failed=await failedReceipt(()=>registry.registerBeacon(...args,{gasLimit}));
      expect(failed.status).to.equal(0);used.push(failed.gasUsed);
    }
    console.log(`Refused registerBeacon with an isValidPublicKey that loops forever: gas=${used[1]} (limits 1,000,000 and 10,000,000 used the same)`);
    expect(used[0]).to.equal(used[1]);expect(used[1]).to.be.greaterThan(400_000n).and.to.be.lessThan(600_000n);
    expect(await registry.recipeCount()).to.equal(6n);
    // A key the verifier accepts registers, and its round question is still asked.
    await key.setKeyMode(0);await key.setMode(3);
    await expect(registry.registerBeacon(...args)).to.be.revertedWithCustomError(registry,"InvalidConfig");
    await key.setMode(0);await registry.registerBeacon(...args);
    expect(await registry.recipeCount()).to.equal(7n);
  });

  it("replays a beacon epoch after the registry rolled back to an implementation without beaconOf, reading its recipes at a past block",async()=>{
    const c=await networkHelpers.loadFixture(fixture),{registry,address,beacon}=c;
    const registered=(await(await register(registry,beacon)).wait())!;
    const slot=await registry.slotSigner(6),epoch=await scheduleAndMine(c,[6],[slot]),a=beaconAttestation(beacon,await chainTime());
    const committed=(await(await commit(c,epoch,a)).wait())!,record=await registry.getEpoch(epoch);
    const packet=registry.interface.parseLog(committed.logs[0])!.args.packet,commitTimestamp=BigInt((await ethers.provider.getBlock(committed.blockNumber))!.timestamp);
    // The rollback, in the order of the runbook: a catalog without the beacon takes effect first, then the implementation goes back.
    const from=await registry.epochForBlock(await ethers.provider.getBlockNumber())+2n;
    await registry.scheduleCatalog([0],[HYPERLIQUID],from);await mineTo(await registry.epochStart(from));
    await provider.request({method:"hardhat_setCode",params:[liveEpoch.implementation,liveEpoch.deployedBytecode]});
    await registry.upgradeToAndCall(liveEpoch.implementation,"0x");
    expect(ethers.keccak256(await ethers.provider.getCode(liveEpoch.implementation))).to.equal(liveEpoch.runtimeCodeHash);
    for(const view of [()=>registry.beaconOf(6),()=>registry.slotSigner(6),()=>registry.verifyBeacon(6,1,"0x")])await expect(view()).to.revert(ethers);
    // The recipe registry, the catalogs and the epoch records are all still there, so only the registration is missing from the latest block.
    expect(await registry.recipeCount()).to.equal(7n);
    expect((await registry.getEpoch(epoch)).epochHash).to.equal(record.epochHash);
    const [hash,recipes,signers]=await registry.catalogAt(epoch);
    expect([hash,recipes.map(Number),Array.from(signers)]).to.deep.equal([epochCatalogHash([slot],[6]),[6],[slot]]);
    const failure=await readEpochRecipes(ethers.provider,address,[6]).then(()=>undefined,(error:unknown)=>error);
    expect(failure,"the latest block cannot answer beaconOf").to.be.instanceOf(Error);
    expect(Object.keys(await readEpochRecipes(ethers.provider,address,[0,1]))).to.deep.equal(["0","1"]);
    // At the epoch's commit block, or any block of the implementation that had the view, the registration is read as it was.
    for(const blockTag of [committed.blockNumber,ethers.toQuantity(committed.blockNumber),registered.blockNumber]){
      const book=await readEpochRecipes(ethers.provider,address,[6],{blockTag});
      expect(book[6].beacon,String(blockTag)).to.deep.equal(beacon);
    }
    // The tag is honored, not ignored: before the registration the recipe does not exist.
    const before=await readEpochRecipes(ethers.provider,address,[6],{blockTag:registered.blockNumber-1}).then(()=>undefined,(error:unknown)=>error);
    expect(before).to.be.instanceOf(Error);
    // The epoch replays from the catalog in force for it and the recipes read at a past block, as it did before the rollback.
    const recipeBook=await readEpochRecipes(ethers.provider,address,recipes.map(Number),{blockTag:committed.blockNumber});
    const catalog=resolveEpochCatalog({registry:address,chainId:CHAIN_ID,firstEpochStart:await registry.firstEpochStart(),recipeBook},{hash,recipes,signers});
    const replayed=replayEpochCommitment({catalog,epochId:epoch,record,commitTimestamp,packet});
    expect([replayed.epochHash,replayed.signer,replayed.selected.beacon]).to.deep.equal([record.epochHash,slot,beacon]);
    // Without the registration it would be read as a signed record and refused, which is why the block tag matters.
    const {beacon:_,...signed}=recipeBook[6]!;
    expect(()=>replayEpochCommitment({catalog:{...catalog,recipeBook:{6:signed}},epochId:epoch,record,commitTimestamp,packet})).to.throw("Expected canonical 65-byte low-s EIP-191 signature");
  });
});
