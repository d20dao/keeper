import {expect} from "chai";
import {network} from "hardhat";
import {AbiCoder,keccak256} from "ethers";
import {bn254} from "@noble/curves/bn254";
import {beaconPublicKey,otherTestKey,signBeaconRound} from "../helpers/beacon.ts";
import {ROUND_BEACON_DOMAIN,ROUND_LEAD,ROUND_PRICING,deployRoundCoordinator,eventsOf,outcome,registrationOf,request,roundTime,signRound,
  type RoundBeacon,type RoundFixture} from "../helpers/robinhood.ts";

const {ethers,networkHelpers}=await network.create();
const abi=AbiCoder.defaultAbiCoder();

/// The round coordinator with a TestBeaconVerifier that misbehaves on demand, deployed but not registered.
async function fixture(){
  const c=await deployRoundCoordinator(ethers,networkHelpers);
  const testVerifier=await ethers.deployContract("TestBeaconVerifier");
  return {...c,testVerifier};
}
type Fixture=Awaited<ReturnType<typeof fixture>>;
const now=async()=>BigInt(await networkHelpers.time.latest());
const flipBit=(hex:string)=>ethers.toBeHex(BigInt(hex)^1n,ethers.dataLength(hex));
/// A registration's identity as BeaconBook computes it.
const identityOf=(b:{verifier:string;chainHash:string;publicKey:string;genesis:bigint;period:bigint})=>keccak256(abi.encode(
  ["bytes32","address","bytes32","bytes32","uint64","uint64"],[ROUND_BEACON_DOMAIN,b.verifier,b.chainHash,keccak256(b.publicKey),b.genesis,b.period]));
// A second test beacon key, so that a beacon's own key is seen to decide its signatures. Never a production key.
const SECOND_SECRET=BigInt(ethers.id("D20 second round test beacon secret"))%bn254.fields.Fr.ORDER;
const secondKeyBeacon=(c:RoundFixture):RoundBeacon=>({...c.beacon,chainHash:ethers.id("second key network"),publicKey:beaconPublicKey(SECOND_SECRET),
  sampleSignature:signBeaconRound(SECOND_SECRET,1n)});
/// The test beacon on another network with its own genesis and period, so that its rounds differ from beacon 0's.
const otherBeacon=(c:RoundFixture,name:string,period:bigint,genesisShift=0n):RoundBeacon=>({...c.beacon,chainHash:ethers.id(name),period,genesis:c.beacon.genesis+genesisShift});
/// Register a beacon and return its id.
async function register(c:RoundFixture,b:RoundBeacon){
  const receipt=await (await c.coordinator.registerBeacon(registrationOf(b))).wait();
  return eventsOf(c.coordinator,receipt,"BeaconRegistered")[0].args.beaconId as bigint;
}
/// A flat fee, so that a request needs no base-fee arithmetic.
const flatPricing=(c:RoundFixture)=>c.coordinator.setPricing(ROUND_PRICING.minFee,0,ROUND_PRICING.fulfillGasOverhead);
/// A raw request at a block timestamp under flat pricing: its receipt and the beacon and round it was assigned.
async function requestAt(c:RoundFixture,t:bigint,seed=ethers.id(`at ${t}`)){
  await networkHelpers.time.setNextBlockTimestamp(t);
  const receipt=await (await c.consumer.request(seed,100_000,c.owner.address,{value:ROUND_PRICING.minFee})).wait();
  const {beaconId,round}=eventsOf(c.coordinator,receipt,"RoundAssigned")[0].args;
  return {receipt,beaconId:beaconId as bigint,round:round as bigint};
}
/// The receipt of a transaction that reverts: the node mines it, then rejects the send with its hash.
async function failedReceipt(send:()=>Promise<unknown>){
  const error:any=await send().then(()=>undefined,(caught:unknown)=>caught);
  if(!error?.transactionHash)throw new Error("Expected a mined transaction that reverted");
  return (await ethers.provider.getTransactionReceipt(error.transactionHash))!;
}
const TOO_LOW="BeaconGasTooLow";
/// The smallest gas limit above `low` at which `at` stops answering BeaconGasTooLow: it answers that at `low` and something else at `high`.
async function firstPassing(at:(gas:number)=>Promise<string>,low:number,high:number){
  expect(await at(low),`gas limit ${low}`).to.equal(TOO_LOW);
  expect(await at(high),`gas limit ${high}`).not.to.equal(TOO_LOW);
  while(high-low>1){const middle=Math.floor((low+high)/2);if(await at(middle)===TOO_LOW)low=middle;else high=middle;}
  return high;
}
/// Gas limits around a threshold: each one within 12, then steps out to 200,000 below and 500,000 above it.
const OFFSETS=[...Array.from({length:25},(_,i)=>i-12),-200_000,-50_000,-10_000,-1_000,-100,100,1_000,10_000,50_000,500_000];
/// Below the threshold every gas limit answers BeaconGasTooLow; at and above it none does, and `above` holds of every answer.
async function sweep(name:string,at:(gas:number)=>Promise<string>,threshold:number,above:(answer:string)=>boolean){
  for(const offset of OFFSETS){
    const gas=threshold+offset,answer=await at(gas);
    if(offset<0)expect(answer,`${name} at ${gas} (${offset})`).to.equal(TOO_LOW);
    else expect(above(answer),`${name} at ${gas} (+${offset}) answered ${answer}`).to.equal(true);
  }
}

describe("BeaconBook: registration",function(){
  it("registers beacon 0 at initialization, in force from that block, with its identity",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const [registered]=await c.coordinator.queryFilter(c.coordinator.filters.BeaconRegistered());
    const [scheduled]=await c.coordinator.queryFilter(c.coordinator.filters.BeaconScheduled());
    const b=c.beacon,identity=identityOf(b);
    expect([...registered.args]).to.deep.equal([0n,identity,b.verifier,b.chainHash,b.publicKey,b.genesis,b.period]);
    expect(await c.coordinator.beaconIdentity(0)).to.equal(identity);
    expect(Array.from(await c.coordinator.getBeacon(0))).to.deep.equal([b.verifier,b.genesis,b.period,b.chainHash,b.publicKey]);
    const block=await ethers.provider.getBlock(scheduled.blockNumber);
    expect(scheduled.blockNumber).to.equal(registered.blockNumber);
    expect([...scheduled.args]).to.deep.equal([0n,BigInt(block!.timestamp)]);
    expect(Array.from(await c.coordinator.beaconSchedule())).to.deep.equal([0n,BigInt(block!.timestamp),0n,0n]);
    expect(await c.coordinator.beaconCount()).to.equal(1n);
    expect([await c.coordinator.ROUND_BEACON_DOMAIN(),await c.coordinator.MAX_BEACONS(),await c.coordinator.MAX_BEACON_PERIOD(),await c.coordinator.MIN_SCHEDULE_LEAD(),
      await c.coordinator.ROUND_VERIFY_GAS(),await c.coordinator.ROUND_LEAD()]).to.deep.equal([ROUND_BEACON_DOMAIN,256n,10n,600n,400_000n,3n]);
  });

  it("refuses every invalid registration with InvalidBeacon and registers nothing, and only the owner registers",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const current=(await now()-c.beacon.genesis)/c.beacon.period+1n;
    const good:RoundBeacon={...c.beacon,chainHash:ethers.id("second network")};
    const refused:Array<[string,Partial<RoundBeacon>]>=[
      ["a verifier that is an account",{verifier:c.stranger.address}],["the zero address as verifier",{verifier:ethers.ZeroAddress}],
      ["a zero chain hash",{chainHash:ethers.ZeroHash}],["genesis 0",{genesis:0n}],["period 0",{period:0n}],
      ["period 11, one above MAX_BEACON_PERIOD",{period:11n}],["a period beyond uint16",{period:65_539n}],["sample round 0",{sampleRound:0n}],
      ["a sample round scheduled after the block time",{sampleRound:current+100n,sampleSignature:signRound(current+100n)}],
      ["a 127-byte key",{publicKey:c.beacon.publicKey.slice(0,-2)}],["a 129-byte key",{publicKey:c.beacon.publicKey+"00"}],["an empty key",{publicKey:"0x"}],
      ["an off-curve key",{publicKey:flipBit(c.beacon.publicKey)}],
      ["a sample signature of another round",{sampleSignature:signRound(2n)}],["a tampered sample signature",{sampleSignature:flipBit(signRound(1n))}],
      ["a 63-byte sample signature",{sampleSignature:signRound(1n).slice(0,-2)}],["an empty sample signature",{sampleSignature:"0x"}],
      ["a well-formed key that did not sign the sample",{publicKey:otherTestKey()}],
      ["beacon 0's registration again",{chainHash:c.beacon.chainHash}],
      ["beacon 0 again, vouched for by another sample round",{chainHash:c.beacon.chainHash,sampleRound:2n,sampleSignature:signRound(2n)}],
    ];
    for(const [name,patch] of refused)
      await expect(c.coordinator.registerBeacon(registrationOf({...good,...patch})),name).to.be.revertedWithCustomError(c.coordinator,"InvalidBeacon");
    await expect(c.coordinator.connect(c.stranger).registerBeacon(registrationOf(good))).to.be.revertedWithCustomError(c.coordinator,"OwnableUnauthorizedAccount")
      .withArgs(c.stranger.address);
    expect(await c.coordinator.beaconCount()).to.equal(1n);
    for(const view of ["getBeacon","beaconIdentity"])await expect(c.coordinator[view](1),view).to.be.revertedWithCustomError(c.coordinator,"InvalidBeacon");

    // A sample round may be scheduled at the block's own time, and not one second later.
    const sampleRound=current+2n,time=roundTime(c.beacon,sampleRound),sampled={...good,sampleRound,sampleSignature:signRound(sampleRound)};
    await networkHelpers.time.setNextBlockTimestamp(time-1n);
    await expect(c.coordinator.registerBeacon(registrationOf(sampled)),"one second ahead").to.be.revertedWithCustomError(c.coordinator,"InvalidBeacon");
    await networkHelpers.time.setNextBlockTimestamp(time);
    await expect(c.coordinator.registerBeacon(registrationOf(sampled))).to.emit(c.coordinator,"BeaconRegistered")
      .withArgs(1,identityOf(good),good.verifier,good.chainHash,good.publicKey,good.genesis,good.period);
    expect(Array.from(await c.coordinator.getBeacon(1))).to.deep.equal([good.verifier,good.genesis,good.period,good.chainHash,good.publicKey]);
    expect(await c.coordinator.beaconIdentity(1)).to.equal(identityOf(good));
    // A registration is no schedule: beacon 0 stays in force.
    expect((await c.coordinator.beaconSchedule())[0]).to.equal(0n);
  });

  it("tells registrations apart by verifier, network, key, genesis and period only, and accepts periods 1 and 10",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const secondVerifier=await (await ethers.deployContract("D20BeaconVerifier")).getAddress();
    const variants:Array<[string,RoundBeacon]>=[
      ["another verifier",{...c.beacon,verifier:secondVerifier}],["another key",secondKeyBeacon(c)],
      ["another genesis",{...c.beacon,genesis:c.beacon.genesis-1n}],["period 1",{...c.beacon,period:1n}],["period 10",{...c.beacon,period:10n}],
    ];
    for(const [index,[name,b]] of variants.entries()){
      await expect(c.coordinator.registerBeacon(registrationOf(b)),name).to.emit(c.coordinator,"BeaconRegistered").withArgs(index+1,identityOf(b),b.verifier,
        b.chainHash,b.publicKey,b.genesis,b.period);
      // The same registration again, whatever sample round vouches for it, is a duplicate.
      await expect(c.coordinator.registerBeacon(registrationOf({...b,sampleRound:3n,sampleSignature:b===variants[1][1]?signBeaconRound(SECOND_SECRET,3n):signRound(3n)})),
        `${name} again`).to.be.revertedWithCustomError(c.coordinator,"InvalidBeacon");
    }
    expect(await c.coordinator.beaconCount()).to.equal(6n);
    const identities=await Promise.all([0,1,2,3,4,5].map(id=>c.coordinator.beaconIdentity(id)));
    expect(new Set(identities).size).to.equal(6);
  });

  it("registers 256 beacons, ids 0 to 255, and refuses one more",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const verifier=await c.testVerifier.getAddress();
    const at=(i:number):RoundBeacon=>({...c.beacon,verifier,chainHash:ethers.id(`network ${i}`),publicKey:"0x01",sampleSignature:"0x"});
    for(let i=1;i<256;i++)await c.coordinator.registerBeacon(registrationOf(at(i)));
    expect(await c.coordinator.beaconCount()).to.equal(256n);
    expect((await c.coordinator.getBeacon(255)).chainHash).to.equal(ethers.id("network 255"));
    await expect(c.coordinator.registerBeacon(registrationOf(at(256)))).to.be.revertedWithCustomError(c.coordinator,"InvalidBeacon");
    expect(await c.coordinator.beaconCount()).to.equal(256n);
    // The last id can be put in force.
    const from=await now()+700n;
    await expect(c.coordinator.scheduleBeacon(255,from)).to.emit(c.coordinator,"BeaconScheduled").withArgs(255,from);
  });
});

describe("BeaconBook: the beacon verifier's answers",function(){
  it("answers BeaconVerifierFailed, never InvalidBeacon or false, when a verifier reverts, runs out of its allowance or answers no boolean word",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const base={...c.beacon,chainHash:ethers.id("watched network"),sampleSignature:"0x"};
    // Each question of a registration, to a fresh verifier: 1 burns its gas, 2 reverts, 4 answers the word 2, 5 nothing, 6 two words,
    // 8 true followed by 200,000 bytes. A false (mode 3) stays a refusal.
    for(const question of ["key","round"] as const){
      for(const mode of [1,2,4,5,6,8,3]){
        const fresh:any=await ethers.deployContract("TestBeaconVerifier");
        await (question==="key"?fresh.setKeyMode(mode):fresh.setMode(mode));
        const call=c.coordinator.registerBeacon(registrationOf({...base,verifier:await fresh.getAddress()}),{gasLimit:3_000_000});
        await expect(call,`${question} mode ${mode}`).to.be.revertedWithCustomError(c.coordinator,mode===3?"InvalidBeacon":"BeaconVerifierFailed");
      }
    }
    expect(await c.coordinator.beaconCount()).to.equal(1n);
    // A registered beacon whose verifier then misbehaves: checkRoundSignature and getProofContext name the failure apart from an invalid signature.
    const broken=c.testVerifier;
    const id=await register(c,{...base,verifier:await broken.getAddress()});
    expect(await c.coordinator.checkRoundSignature(id,5,"0x")).to.equal(true);
    for(const mode of [1,2,4,5,6,8]){
      await broken.setMode(mode);
      await expect(c.coordinator.checkRoundSignature(id,5,"0x",{gasLimit:3_000_000}),`mode ${mode}`).to.be.revertedWithCustomError(c.coordinator,"BeaconVerifierFailed");
    }
    await broken.setMode(3);
    expect(await c.coordinator.checkRoundSignature(id,5,"0x")).to.equal(false);
  });

  it("gives a verifier at most ROUND_VERIFY_GAS: one that loops forever costs a refused registration the same at any gas limit",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    await c.testVerifier.setKeyMode(1);
    const registration=registrationOf({...c.beacon,chainHash:ethers.id("looping"),verifier:await c.testVerifier.getAddress(),sampleSignature:"0x"});
    const used:bigint[]=[];
    for(const gasLimit of [1_000_000,10_000_000]){
      const failed=await failedReceipt(()=>c.coordinator.registerBeacon(registration,{gasLimit}));
      expect(failed.status).to.equal(0);
      used.push(failed.gasUsed);
    }
    expect(used[0]).to.equal(used[1]);
    expect(used[1]).to.be.greaterThan(400_000n).and.to.be.lessThan(600_000n);
  });

  it("answers checkRoundSignature true for the round's signature under the beacon's own key and false for any other",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const second=await register(c,secondKeyBeacon(c));
    const round=(await now()-c.beacon.genesis)/c.beacon.period;
    expect(await c.coordinator.checkRoundSignature(0,round,signRound(round))).to.equal(true);
    expect(await c.coordinator.checkRoundSignature(second,round,signBeaconRound(SECOND_SECRET,round))).to.equal(true);
    const no:Array<[string,bigint,bigint,string]>=[
      ["another round's signature",0n,round,signRound(round+1n)],["a tampered signature",0n,round,flipBit(signRound(round))],
      ["a 63-byte signature",0n,round,signRound(round).slice(0,-2)],["an empty signature",0n,round,"0x"],
      ["another key's signature",0n,round,signBeaconRound(SECOND_SECRET,round)],["beacon 0's signature on beacon 1",second,round,signRound(round)],
    ];
    for(const [name,id,r,signature] of no)expect(await c.coordinator.checkRoundSignature(id,r,signature),name).to.equal(false);
    await expect(c.coordinator.checkRoundSignature(2,round,signRound(round))).to.be.revertedWithCustomError(c.coordinator,"InvalidBeacon");
  });

  it("reverts BeaconGasTooLow, never false or InvalidBeacon, when the call's gas cannot give the verifier its whole allowance",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    // A verifier that answers true only when it was given nearly all of ROUND_VERIFY_GAS: a coordinator that let the 63/64 rule shorten
    // the allowance would get a no from it just below the threshold. The control shows that it does answer no when it is given less.
    const probe=c.testVerifier;
    await probe.setMode(7);await probe.setKeyMode(7);
    expect([await probe.verifyRound.staticCall("0x",1,"0x",{gasLimit:380_000}),await probe.verifyRound.staticCall("0x",1,"0x",{gasLimit:450_000})]).to.deep.equal([false,true]);
    const watched={...c.beacon,chainHash:ethers.id("watched"),verifier:await probe.getAddress(),sampleSignature:"0x"};
    await c.coordinator.registerBeacon(registrationOf(watched),{gasLimit:2_000_000});
    const round=(await now()-c.beacon.genesis)/c.beacon.period,signature=signRound(round);
    // checkRoundSignature: the same threshold for the real verifier and the watching one, and true at and above it. Calldata costs 4 gas for a
    // zero byte and 16 for any other, so the two beacon ids move a threshold by 12 gas: the thresholds are compared without it.
    const calldataGas=(data:string)=>ethers.getBytes(data).reduce((gas,byte)=>gas+(byte===0?4:16),0);
    const thresholds:number[]=[];
    for(const id of [0,1]){
      const at=(gasLimit:number)=>outcome(c.coordinator,()=>c.coordinator.checkRoundSignature.staticCall(id,round,signature,{gasLimit}));
      const threshold=await firstPassing(at,100_000,3_000_000);
      expect(threshold,`beacon ${id}`).to.be.greaterThan(400_000+6_349+21_000).and.to.be.lessThan(500_000);
      await sweep(`checkRoundSignature(${id})`,at,threshold,answer=>answer==="returns true");
      thresholds.push(threshold-calldataGas(c.coordinator.interface.encodeFunctionData("checkRoundSignature",[id,round,signature])));
    }
    expect(thresholds[0]).to.equal(thresholds[1]);
    // registerBeacon: short of the allowance at either of its two questions is the same error, never InvalidBeacon.
    const registration=registrationOf({...watched,chainHash:ethers.id("watched again")});
    const registerAt=(gasLimit:number)=>outcome(c.coordinator,()=>c.coordinator.registerBeacon.staticCall(registration,{gasLimit}));
    const registerThreshold=await firstPassing(registerAt,150_000,3_000_000);
    await sweep("registerBeacon",registerAt,registerThreshold,answer=>answer==="fails"||answer.startsWith("returns"));
    expect(await registerAt(2_000_000)).to.equal("returns 2");
    // getProofContext verifies a signature in a view in the same way.
    const {id}=await request(c,networkHelpers);
    const q=await c.coordinator.getRoundRequest(id),own=signRound(q.round);
    const contextAt=(gasLimit:number)=>outcome(c.coordinator,()=>c.coordinator.getProofContext.staticCall(id,own,{gasLimit}));
    const contextThreshold=await firstPassing(contextAt,100_000,3_000_000);
    await sweep("getProofContext",contextAt,contextThreshold,answer=>answer.startsWith("returns"));
    console.log(`BeaconGasTooLow below these gas limits, calldata excluded for checkRoundSignature: checkRoundSignature ${thresholds[0]}, registerBeacon ${registerThreshold}, getProofContext ${contextThreshold}`);
  });
});

describe("BeaconBook: round assignment",function(){
  it("binds the first round scheduled at least ROUND_LEAD seconds after the request, for every period from 1 to 10 over 40 consecutive timestamps",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    await flatPricing(c);
    for(let p=1n;p<=10n;p++){
      // Each period's beacon has its own genesis, so its round boundaries fall at other offsets of the clock.
      const b=otherBeacon(c,`period ${p}`,p,p*7n),id=await register(c,b);
      const from=await now()+700n+p;
      await c.coordinator.scheduleBeacon(id,from);
      const seen=new Map<bigint,number>();
      for(let t=from;t<from+40n;t++){
        const {beaconId,round}=await requestAt(c,t);
        expect(beaconId).to.equal(id);
        // The definition, checked at its edges: the round is scheduled at or after t + 3, and the round before it earlier.
        const boundary=roundTime(b,round)>=t+ROUND_LEAD&&(round===1n||roundTime(b,round-1n)<t+ROUND_LEAD);
        expect(boundary,`period ${p}, timestamp ${t}: round ${round}`).to.equal(true);
        expect(Array.from(await c.coordinator.roundAt(t))).to.deep.equal([id,round]);
        seen.set(round,(seen.get(round)??0)+1);
      }
      // Consecutive rounds, and every round strictly inside the window bound by exactly `period` timestamps.
      const rounds=[...seen.keys()];
      expect(rounds).to.deep.equal(rounds.map((_,i)=>rounds[0]+BigInt(i)));
      for(const r of rounds.slice(1,-1))expect(seen.get(r),`period ${p}, round ${r}`).to.equal(Number(p));
      expect(await c.coordinator.roundTime(id,rounds[0])).to.equal(roundTime(b,rounds[0]));
    }
  });

  it("assigns round 1 to every timestamp at most ROUND_LEAD seconds before genesis, and round 2 from the next second",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const g=c.beacon.genesis;
    // Before any change of beacon, roundAt answers beacon 0 for any timestamp, before initialization included.
    for(let t=g-10n;t<=g-3n;t++)expect(Array.from(await c.coordinator.roundAt(t)),`${t}`).to.deep.equal([0n,1n]);
    expect(Array.from(await c.coordinator.roundAt(g-2n))).to.deep.equal([0n,2n]);
    expect(Array.from(await c.coordinator.roundAt(g))).to.deep.equal([0n,2n]);
    expect(Array.from(await c.coordinator.roundAt(g+1n))).to.deep.equal([0n,3n]);
    expect(Array.from(await c.coordinator.roundAt(0))).to.deep.equal([0n,1n]);
    // The largest timestamp does not overflow: the round is computed in 256 bits.
    const max=2n**64n-1n;
    expect(Array.from(await c.coordinator.roundAt(max))).to.deep.equal([0n,(max+ROUND_LEAD-g+c.beacon.period-1n)/c.beacon.period+1n]);
  });

  it("schedules round r of a beacon at genesis + (r - 1) × period, and has no round 0",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const b=otherBeacon(c,"period 7",7n,5n),id=await register(c,b);
    for(const r of [1n,2n,3n,1_000n,2n**32n,2n**64n-1n])
      expect(await c.coordinator.roundTime(id,r),`round ${r}`).to.equal(b.genesis+(r-1n)*7n);
    await expect(c.coordinator.roundTime(id,0)).to.be.revertedWithCustomError(c.coordinator,"InvalidBeacon");
    await expect(c.coordinator.roundTime(2,1)).to.be.revertedWithCustomError(c.coordinator,"InvalidBeacon");
  });
});

describe("BeaconBook: the schedule",function(){
  it("puts a scheduled beacon in force exactly at fromTime: a request one second before binds the old beacon, one at fromTime the new",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    await flatPricing(c);
    const b=otherBeacon(c,"switch target",5n,1n),id=await register(c,b);
    const [,since]=await c.coordinator.beaconSchedule();
    const from=await now()+900n;
    await expect(c.coordinator.scheduleBeacon(id,from)).to.emit(c.coordinator,"BeaconScheduled").withArgs(id,from);
    expect(Array.from(await c.coordinator.beaconSchedule())).to.deep.equal([0n,since,id,from]);
    const before=await requestAt(c,from-1n);
    expect([before.beaconId,before.round]).to.deep.equal([0n,(from-1n+ROUND_LEAD-c.beacon.genesis+2n)/3n+1n]);
    // Still pending in the block before fromTime.
    expect(Array.from(await c.coordinator.beaconSchedule())).to.deep.equal([0n,since,id,from]);
    const at=await requestAt(c,from);
    expect([at.beaconId,at.round]).to.deep.equal([id,(from+ROUND_LEAD-b.genesis+4n)/5n+1n]);
    await expect(at.receipt).to.emit(c.coordinator,"RoundAssigned").withArgs(await c.consumer.lastRequestId(),id,at.round);
    // From fromTime the view shows the change as the beacon in force, before any transaction settles it.
    expect(Array.from(await c.coordinator.beaconSchedule())).to.deep.equal([id,from,0n,0n]);
    expect(Array.from(await c.coordinator.roundAt(from-1n))).to.deep.equal([0n,before.round]);
    expect(Array.from(await c.coordinator.roundAt(from))).to.deep.equal([id,at.round]);
    // The request bound before the switch keeps its beacon and round.
    const q=await c.coordinator.getRoundRequest(await c.consumer.lastRequestId()-1n);
    expect([q.beaconId,q.round]).to.deep.equal([0n,before.round]);
  });

  it("enforces MIN_SCHEDULE_LEAD from the scheduling block, the uint40 bound, a registered beacon and the owner",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const id=await register(c,otherBeacon(c,"lead",4n));
    // Each attempt is mined at a timestamp of its own, with a fixed gas limit, so that a refusal is judged at that block's time.
    const t=await now()+100n,gas={gasLimit:500_000};
    await networkHelpers.time.setNextBlockTimestamp(t);
    await expect(c.coordinator.scheduleBeacon(id,t+599n,gas)).to.be.revertedWithCustomError(c.coordinator,"InvalidSchedule");
    for(const from of [2n**40n,2n**64n-1n])
      await expect(c.coordinator.scheduleBeacon(id,from),`${from}`).to.be.revertedWithCustomError(c.coordinator,"InvalidSchedule");
    await expect(c.coordinator.scheduleBeacon(2,t+700n)).to.be.revertedWithCustomError(c.coordinator,"InvalidBeacon");
    await expect(c.coordinator.connect(c.stranger).scheduleBeacon(id,t+700n)).to.be.revertedWithCustomError(c.coordinator,"OwnableUnauthorizedAccount");
    await networkHelpers.time.setNextBlockTimestamp(t+10n);
    await expect(c.coordinator.scheduleBeacon(id,t+610n,gas)).to.emit(c.coordinator,"BeaconScheduled").withArgs(id,t+610n);
    // The last uint40 second is a valid, if distant, time.
    await expect(c.coordinator.scheduleBeacon(id,2n**40n-1n)).to.emit(c.coordinator,"BeaconScheduled").withArgs(id,2n**40n-1n);
    expect((await c.coordinator.beaconSchedule())[3]).to.equal(2n**40n-1n);
  });

  it("replaces a pending change with a new schedule, without keeping the replaced one anywhere",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    await flatPricing(c);
    const one=await register(c,otherBeacon(c,"one",4n)),two=await register(c,otherBeacon(c,"two",6n,2n));
    const [,since]=await c.coordinator.beaconSchedule();
    const t=await now(),first=t+700n,second=t+1_000n;
    await c.coordinator.scheduleBeacon(one,first);
    const receipt=await (await c.coordinator.scheduleBeacon(two,second)).wait();
    expect(eventsOf(c.coordinator,receipt).map((e:any)=>e.name)).to.deep.equal(["BeaconScheduled"]);
    expect(Array.from(await c.coordinator.beaconSchedule())).to.deep.equal([0n,since,two,second]);
    // At the replaced change's time nothing switches.
    expect((await requestAt(c,first)).beaconId).to.equal(0n);
    expect((await c.coordinator.roundAt(first))[0]).to.equal(0n);
    expect((await requestAt(c,second)).beaconId).to.equal(two);
    // A later replacement may also move the change earlier.
    const three=await register(c,otherBeacon(c,"three",2n));
    const t2=await now(),late=t2+5_000n,early=t2+700n;
    await c.coordinator.scheduleBeacon(three,late);
    await c.coordinator.scheduleBeacon(0,early);
    expect(Array.from(await c.coordinator.beaconSchedule())).to.deep.equal([two,second,0n,early]);
    expect((await requestAt(c,early)).beaconId).to.equal(0n);
    expect((await requestAt(c,late)).beaconId).to.equal(0n);
  });

  it("folds a change that has taken effect into the past when the next one is scheduled",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const one=otherBeacon(c,"fold one",5n,3n),oneId=await register(c,one);
    const two=otherBeacon(c,"fold two",2n,1n),twoId=await register(c,two);
    const [,since]=await c.coordinator.beaconSchedule();
    const t1=await now()+700n;
    await c.coordinator.scheduleBeacon(oneId,t1);
    await networkHelpers.time.increaseTo(t1+50n);
    const t2=await now()+700n;
    await c.coordinator.scheduleBeacon(twoId,t2);
    expect(Array.from(await c.coordinator.beaconSchedule())).to.deep.equal([oneId,t1,twoId,t2]);
    // The era of beacon 0 still answers for the times before t1.
    const expectations:Array<[bigint,bigint,RoundBeacon]>=[[since-100n,0n,c.beacon],[since,0n,c.beacon],[t1-1n,0n,c.beacon],[t1,oneId,one],[t2-1n,oneId,one],
      [t2,twoId,two],[t2+10_000n,twoId,two]];
    for(const [t,id,b] of expectations)
      expect(Array.from(await c.coordinator.roundAt(t)),`roundAt(${t})`).to.deep.equal([id,(t+ROUND_LEAD-b.genesis+b.period-1n)/b.period+1n]);
    // A beacon may also be scheduled while it is already in force: a new era of the same beacon.
    await networkHelpers.time.increaseTo(t2);
    const t3=await now()+700n;
    await c.coordinator.scheduleBeacon(twoId,t3);
    expect(Array.from(await c.coordinator.beaconSchedule())).to.deep.equal([twoId,t2,twoId,t3]);
    expect((await c.coordinator.roundAt(t1))[0]).to.equal(oneId);
  });

  it("cancels a pending change until the second before it takes effect, and refuses with nothing pending or after the change",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    await flatPricing(c);
    const id=await register(c,otherBeacon(c,"cancel",8n));
    const [,since]=await c.coordinator.beaconSchedule();
    await expect(c.coordinator.cancelBeaconSchedule()).to.be.revertedWithCustomError(c.coordinator,"InvalidSchedule");
    const from=await now()+700n;
    await c.coordinator.scheduleBeacon(id,from);
    await expect(c.coordinator.connect(c.stranger).cancelBeaconSchedule()).to.be.revertedWithCustomError(c.coordinator,"OwnableUnauthorizedAccount");
    await networkHelpers.time.setNextBlockTimestamp(from-1n);
    await expect(c.coordinator.cancelBeaconSchedule()).to.emit(c.coordinator,"BeaconScheduleCancelled").withArgs(id,from);
    expect(Array.from(await c.coordinator.beaconSchedule())).to.deep.equal([0n,since,0n,0n]);
    await expect(c.coordinator.cancelBeaconSchedule()).to.be.revertedWithCustomError(c.coordinator,"InvalidSchedule");
    expect((await requestAt(c,from)).beaconId).to.equal(0n);
    expect((await c.coordinator.roundAt(from+100n))[0]).to.equal(0n);
    // After the change has taken effect, from its very second, there is nothing to cancel.
    const next=await now()+700n;
    await c.coordinator.scheduleBeacon(id,next);
    await networkHelpers.time.setNextBlockTimestamp(next);
    await expect(c.coordinator.cancelBeaconSchedule({gasLimit:500_000})).to.be.revertedWithCustomError(c.coordinator,"InvalidSchedule");
    expect(Array.from(await c.coordinator.beaconSchedule())).to.deep.equal([id,next,0n,0n]);
    // A schedule after the change folds it; that new change can be cancelled, and the folded one stays.
    const later=await now()+700n;
    await c.coordinator.scheduleBeacon(0,later);
    await expect(c.coordinator.cancelBeaconSchedule()).to.emit(c.coordinator,"BeaconScheduleCancelled").withArgs(0,later);
    expect(Array.from(await c.coordinator.beaconSchedule())).to.deep.equal([id,next,0n,0n]);
    expect((await c.coordinator.roundAt(next-1n))[0]).to.equal(0n);
    expect((await c.coordinator.roundAt(later))[0]).to.equal(id);
  });

  it("answers roundAt across several folded switches, before the first switch and before initialization included, as requests bound them",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    await flatPricing(c);
    const beacons:RoundBeacon[]=[c.beacon,otherBeacon(c,"era one",5n,3n),otherBeacon(c,"era two",2n,1n),otherBeacon(c,"era three",9n,4n)];
    for(const b of beacons.slice(1))await register(c,b);
    const [,since]=await c.coordinator.beaconSchedule();
    // Switches to 1, 2, 0, 3 and 1 again; each scheduled once the one before has taken effect, so each folds its predecessor.
    const plan=[1,2,0,3,1];
    const eras:Array<{id:number;from:bigint}>=[{id:0,from:since}];
    const bound:Array<{t:bigint;beaconId:bigint;round:bigint}>=[];
    for(const id of plan){
      const from=await now()+700n+BigInt(eras.length)*37n;
      await c.coordinator.scheduleBeacon(id,from);
      eras.push({id,from});
      // A request in the second before the switch and one at it.
      for(const t of [from-1n,from])bound.push({t,...await requestAt(c,t)});
    }
    const expected=(t:bigint)=>{
      let era=eras[0];
      for(const e of eras)if(t>=e.from)era=e;
      const b=beacons[era.id];
      return [BigInt(era.id),(t+ROUND_LEAD<=b.genesis?1n:(t+ROUND_LEAD-b.genesis+b.period-1n)/b.period+1n)];
    };
    // Every request bound what roundAt now says of its timestamp.
    for(const {t,beaconId,round} of bound){
      expect([beaconId,round],`request at ${t}`).to.deep.equal(expected(t));
      expect(Array.from(await c.coordinator.roundAt(t)),`roundAt(${t})`).to.deep.equal([beaconId,round]);
    }
    // And roundAt answers every era for times inside it, at its edges and far outside the schedule.
    const times=[0n,c.beacon.genesis-10n,since-1n,since,...eras.flatMap(e=>[e.from-1n,e.from,e.from+1n,e.from+300n]),eras.at(-1)!.from+100_000n];
    for(const t of times)expect(Array.from(await c.coordinator.roundAt(t)),`roundAt(${t})`).to.deep.equal(expected(t));
    expect(Array.from(await c.coordinator.beaconSchedule())).to.deep.equal([1n,eras.at(-1)!.from,0n,0n]);
  });
});

describe("BeaconBook: a genesis beyond uint40",function(){
  // Its own chain, whose clock this test moves past 2^40 seconds.
  let chain:Awaited<ReturnType<typeof network.create>>;
  before(async()=>{chain=await network.create();});

  it("refuses a genesis above uint40 even when its sample round is in the past, and accepts the largest uint40 genesis",async()=>{
    const {ethers:e,networkHelpers:h}=chain;
    const c=await deployRoundCoordinator(e,h);
    const max=2n**40n-1n;
    await h.time.increaseTo(max+10_000n);
    const base={...c.beacon,chainHash:e.id("far future")};
    await expect(c.coordinator.registerBeacon(registrationOf({...base,genesis:max+1n}))).to.be.revertedWithCustomError(c.coordinator,"InvalidBeacon");
    await expect(c.coordinator.registerBeacon(registrationOf({...base,genesis:max}))).to.emit(c.coordinator,"BeaconRegistered");
    expect((await c.coordinator.getBeacon(1)).genesis).to.equal(max);
    expect(await c.coordinator.beaconIdentity(1)).to.equal(identityOf({...base,genesis:max}));
  });
});
