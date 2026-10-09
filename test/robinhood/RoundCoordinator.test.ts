import {expect} from "chai";
import {network} from "hardhat";
import {AbiCoder,keccak256} from "ethers";
import {makeProof,proofOutput,publicKey} from "../helpers/proof.ts";
import {CONFIG_DOMAIN,EVMNET,EVMNET_START,L2_OFFSET,ROUND_LEAD,ROUND_PRICING,TRANSCRIPT_DOMAIN,assignedRound,compiled,deployRoundCoordinator,
  proveRequest,randomnessOf,registrationOf,request,roundSeed,roundTime,signRound,type RoundFixture} from "../helpers/robinhood.ts";

const {ethers,networkHelpers}=await network.create();
const abi=AbiCoder.defaultAbiCoder();

const fixture=()=>deployRoundCoordinator(ethers,networkHelpers);
/// The names of a receipt's coordinator events, in order.
const eventNames=(c:RoundFixture,receipt:any)=>receipt.logs.map((log:any)=>{try{return c.coordinator.interface.parseLog(log)?.name;}catch{return undefined;}}).filter(Boolean);
const events=(c:RoundFixture,receipt:any,name:string)=>receipt.logs.map((log:any)=>{try{return c.coordinator.interface.parseLog(log);}catch{return null;}})
  .filter((event:any)=>event?.name===name);
/// A timestamp from which the next `count` requests, one block each, bind rounds of the test beacon with the given offsets after the round
/// boundary: the first request lands `offset` seconds after a round's scheduled time minus ROUND_LEAD.
async function roundBoundary(c:RoundFixture,ahead=30n){
  const now=BigInt(await networkHelpers.time.latest())+ahead;
  const round=assignedRound(c.beacon,now)+1n;
  return {round,time:roundTime(c.beacon,round)-ROUND_LEAD};
}

describe("Round coordinator: requests bind a future drand round",function(){
  it("binds the first round scheduled at least ROUND_LEAD seconds after the request block's timestamp, for consecutive timestamps",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    expect(await c.coordinator.ROUND_LEAD()).to.equal(3n);
    const start=BigInt(await networkHelpers.time.latest())+10n;
    for(let k=0n;k<7n;k++){
      const at=start+k;
      const {id,receipt,request:q}=await request(c,networkHelpers,{at});
      const expected=assignedRound(c.beacon,at);
      expect(q.beaconId).to.equal(0n);
      expect(q.round,`timestamp ${at}`).to.equal(expected);
      // The round is scheduled at least 3 seconds after the request, and the round before it less than 3 seconds after.
      expect(roundTime(c.beacon,expected)).to.be.at.least(at+ROUND_LEAD);
      expect(roundTime(c.beacon,expected-1n)).to.be.lessThan(at+ROUND_LEAD);
      expect(await c.coordinator.roundTime(0,expected)).to.equal(roundTime(c.beacon,expected));
      expect(Array.from(await c.coordinator.roundAt(at))).to.deep.equal([0n,expected]);
      await expect(receipt).to.emit(c.coordinator,"RoundAssigned").withArgs(id,0,expected);
      // The request block is the L2 number from ArbSys; the deadline is 60 seconds of block time.
      expect(q.requestBlock).to.equal(BigInt(receipt.blockNumber)+L2_OFFSET);
      expect(q.deadline).to.equal(at+60n);
      expect(eventNames(c,receipt)).to.deep.equal(["RandomnessRequested","MappingRequested","RoundAssigned"]);
    }
  });

  it("records every seed input, the escrowed fee and the status in getRoundRequest, and keeps the round's randomness zero until it is verified",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const {id,fee,request:q}=await request(c,networkHelpers,{seed:ethers.id("player action 7"),gas:80_000});
    expect(fee).to.equal(ROUND_PRICING.minFee);
    expect(q.consumer).to.equal(await c.consumer.getAddress());
    expect(q.callbackGasLimit).to.equal(80_000n);
    expect(q.refundAddress).to.equal(c.owner.address);
    expect(q.clientSeed).to.equal(ethers.id("player action 7"));
    expect(q.mappingHash).to.equal(keccak256(abi.encode(["uint8","uint256","uint256","uint32","uint32"],[0,0,0,0,0])));
    expect(q.feePaid).to.equal(fee);
    expect([q.roundRandomness,q.randomness,q.proofHash,q.transcriptHash]).to.deep.equal(Array(4).fill(ethers.ZeroHash));
    expect([q.fulfilled,q.delivered,q.refunded]).to.deep.equal([false,false,false]);
    expect(await c.coordinator.requestFeePaid(id)).to.equal(fee);
    expect(await c.coordinator.requestRefundBps(id)).to.equal(10000n);
    // No seed before the round is verified on chain; a signature that verifies gives it to an eth_call.
    await expect(c.coordinator.requestSeed(id)).to.be.revertedWithCustomError(c.coordinator,"RoundUnavailable");
    await expect(c.coordinator.verifyRequestProof(id,makeProof(1n))).to.be.revertedWithCustomError(c.coordinator,"RoundUnavailable");
    const signature=signRound(q.round);
    const [seed,deadline,fulfilled,refunded]=await c.coordinator.getProofContext(id,signature);
    expect(seed).to.equal(await roundSeed(c,ethers,id,randomnessOf(signature)));
    expect([deadline,fulfilled,refunded]).to.deep.equal([q.deadline,false,false]);
    await expect(c.coordinator.getProofContext(id,signRound(q.round+1n))).to.be.revertedWithCustomError(c.coordinator,"InvalidRoundSignature");
    await expect(c.coordinator.getRoundRequest(999)).to.be.revertedWithCustomError(c.coordinator,"UnknownRequest");
  });
});

describe("Round coordinator: beacon schedule",function(){
  it("bounds a beacon's period to 10 seconds and refuses a second registration of the same beacon",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    expect(await c.coordinator.MAX_BEACON_PERIOD()).to.equal(10n);
    const base={...c.beacon,chainHash:ethers.id("period test")};
    await expect(c.coordinator.registerBeacon(registrationOf({...base,period:11n}))).to.be.revertedWithCustomError(c.coordinator,"InvalidBeacon");
    await expect(c.coordinator.registerBeacon(registrationOf({...base,period:10n}))).to.emit(c.coordinator,"BeaconRegistered");
    await expect(c.coordinator.registerBeacon(registrationOf({...base,period:10n}))).to.be.revertedWithCustomError(c.coordinator,"InvalidBeacon");
    await expect(c.coordinator.registerBeacon(registrationOf(c.beacon))).to.be.revertedWithCustomError(c.coordinator,"InvalidBeacon");
    await expect(c.coordinator.connect(c.stranger).registerBeacon(registrationOf({...base,period:9n}))).to.be.revertedWithCustomError(c.coordinator,"OwnableUnauthorizedAccount");
    expect(await c.coordinator.beaconCount()).to.equal(2n);
  });

  it("shows a switch whose time has come as the beacon in force, and answers roundAt for times before a switch that a later one settled",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const other={...c.beacon,chainHash:ethers.id("second network"),period:5n};
    await c.coordinator.registerBeacon(registrationOf(other));
    const [, since0]=await c.coordinator.beaconSchedule();
    const now=BigInt(await networkHelpers.time.latest());
    await expect(c.coordinator.scheduleBeacon(1,now+599n)).to.be.revertedWithCustomError(c.coordinator,"InvalidSchedule");
    const switchAt=now+700n;
    await expect(c.coordinator.scheduleBeacon(1,switchAt)).to.emit(c.coordinator,"BeaconScheduled").withArgs(1,switchAt);
    expect(Array.from(await c.coordinator.beaconSchedule())).to.deep.equal([0n,since0,1n,switchAt]);
    // The switch takes effect at switchAt: requests from then on bind beacon 1, by its own genesis and period.
    await networkHelpers.time.increaseTo(switchAt);
    expect(Array.from(await c.coordinator.beaconSchedule())).to.deep.equal([1n,switchAt,0n,0n]);
    await expect(c.coordinator.cancelBeaconSchedule()).to.be.revertedWithCustomError(c.coordinator,"InvalidSchedule");
    const {request:q}=await request(c,networkHelpers,{at:switchAt+5n});
    expect([q.beaconId,q.round]).to.deep.equal([1n,assignedRound(other,switchAt+5n)]);
    // A later schedule settles the switch; roundAt still answers beacon 0 for times before it.
    const back=switchAt+1000n;
    await c.coordinator.scheduleBeacon(0,back);
    const expectations:Array<[bigint,bigint,{genesis:bigint;period:bigint}]>=[[switchAt-1n,0n,c.beacon],[since0,0n,c.beacon],[switchAt,1n,other],[back-1n,1n,other],[back,0n,c.beacon]];
    for(const [t,beacon,b] of expectations)expect(Array.from(await c.coordinator.roundAt(t)),`roundAt(${t})`).to.deep.equal([beacon,assignedRound(b,t)]);
    expect(Array.from(await c.coordinator.beaconSchedule())).to.deep.equal([1n,switchAt,0n,back]);
    // A pending switch can be cancelled until it takes effect.
    await expect(c.coordinator.cancelBeaconSchedule()).to.emit(c.coordinator,"BeaconScheduleCancelled").withArgs(0,back);
    expect(Array.from(await c.coordinator.beaconSchedule())).to.deep.equal([1n,switchAt,0n,0n]);
    expect(Array.from(await c.coordinator.roundAt(back))).to.deep.equal([1n,assignedRound(other,back)]);
  });
});

describe("Round coordinator: fulfillment",function(){
  it("verifies the round once, caches drand's randomness, delivers the callback, and serves a second request of the round from the cache",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const {round,time}=await roundBoundary(c);
    // Two requests in consecutive blocks inside the same round.
    const first=await request(c,networkHelpers,{at:time-2n,seed:ethers.id("a")});
    const second=await request(c,networkHelpers,{at:time-1n,seed:ethers.id("b")});
    expect([first.request.round,second.request.round]).to.deep.equal([round,round]);
    const signature=signRound(round),randomness=randomnessOf(signature);
    const proof=await proveRequest(c,ethers,first.id,signature);
    const receipt=await (await c.coordinator.connect(c.keeper).fulfillRandomness(first.id,proof,signature)).wait();
    await expect(receipt).to.emit(c.coordinator,"RoundVerified").withArgs(0,round,randomness,signature);
    expect(eventNames(c,receipt)).to.deep.equal(["RoundVerified","RequestServed","ProofVerified","RandomnessFulfilled","FulfillmentEvidence","CallbackAttempted","KeeperFeePaid"]);
    expect(await c.coordinator.roundRandomness(0,round)).to.equal(randomness);
    // The result is the VRF output; the consumer got it; the transcript binds the round.
    const output=proofOutput(proof);
    const q=await c.coordinator.getRoundRequest(first.id);
    expect([q.fulfilled,q.delivered,q.refunded]).to.deep.equal([true,true,false]);
    expect(q.randomness).to.equal(output);
    expect(q.roundRandomness).to.equal(randomness);
    expect(await c.consumer.results(first.id)).to.equal(output);
    await expect(receipt).to.emit(c.coordinator,"CallbackAttempted").withArgs(first.id,true,100_000);
    expect(q.proofHash).to.equal(keccak256(events(c,receipt,"FulfillmentEvidence")[0].args.packet));
    const chainId=(await ethers.provider.getNetwork()).chainId;
    expect(q.transcriptHash).to.equal(keccak256(abi.encode(
      ["bytes32","uint256","address","uint256","bytes32","uint8","uint64","bytes32","bytes32","bytes32","bytes32"],
      [TRANSCRIPT_DOMAIN,chainId,await c.coordinator.getAddress(),first.id,await c.coordinator.protocolConfigurationHash(),0,round,randomness,q.proofHash,output,q.mappingHash])));
    expect(await c.coordinator.requestSeed(first.id)).to.equal(proof.seed);
    expect(await c.coordinator.verifyRequestProof(first.id,proof)).to.equal(output);
    // The second request of the round: no signature needed and no second verification.
    const cachedProof=await proveRequest(c,ethers,second.id,signature);
    expect(await c.coordinator.requestSeed(second.id)).to.equal(cachedProof.seed);
    const cached=await (await c.coordinator.connect(c.keeper).fulfillRandomness(second.id,cachedProof,"0x")).wait();
    expect(eventNames(c,cached)).not.to.include("RoundVerified");
    expect(await c.consumer.results(second.id)).to.equal(proofOutput(cachedProof));
    expect(cached.gasUsed).to.be.lessThan(receipt.gasUsed-150_000n);
    await expect(c.coordinator.fulfillRandomness(first.id,proof,signature)).to.be.revertedWithCustomError(c.coordinator,"AlreadyFulfilled");
  });

  it("refuses another round's signature and an empty one for a round not verified yet, and names a verifier failure apart",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const {id,request:q}=await request(c,networkHelpers);
    const signature=signRound(q.round),proof=await proveRequest(c,ethers,id,signature);
    for(const wrong of [signRound(q.round+1n),signRound(q.round-1n),"0x",signature.slice(0,-2)])
      await expect(c.coordinator.fulfillRandomness(id,proof,wrong)).to.be.revertedWithCustomError(c.coordinator,"InvalidRoundSignature");
    // Too little gas to give the verifier its whole allowance: BeaconGasTooLow, never a false "invalid".
    await expect(c.coordinator.checkRoundSignature(0,q.round,signature,{gasLimit:300_000})).to.be.revertedWithCustomError(c.coordinator,"BeaconGasTooLow");
    expect(await c.coordinator.checkRoundSignature(0,q.round,signature)).to.equal(true);
    expect(await c.coordinator.checkRoundSignature(0,q.round,signRound(q.round+1n))).to.equal(false);
    // A verifier that reverts, runs out of its allowance or answers no boolean: BeaconVerifierFailed, never InvalidRoundSignature.
    const verifierAddress=await c.beaconVerifier.getAddress();
    await networkHelpers.setCode(verifierAddress,compiled("test/TestBeaconVerifiers.sol","TestBeaconVerifier").deployedBytecode);
    const broken=await ethers.getContractAt("TestBeaconVerifier",verifierAddress);
    for(const mode of [1,2,4,5,6,8]){
      await broken.setMode(mode);
      await expect(c.coordinator.fulfillRandomness(id,proof,signature,{gasLimit:3_000_000}),`mode ${mode}`).to.be.revertedWithCustomError(c.coordinator,"BeaconVerifierFailed");
      await expect(c.coordinator.checkRoundSignature(0,q.round,signature),`mode ${mode}`).to.be.revertedWithCustomError(c.coordinator,"BeaconVerifierFailed");
    }
    // An answer of false stays a refusal of the signature.
    await broken.setMode(3);
    await expect(c.coordinator.fulfillRandomness(id,proof,signature)).to.be.revertedWithCustomError(c.coordinator,"InvalidRoundSignature");
    expect(await c.coordinator.roundRandomness(0,q.round)).to.equal(ethers.ZeroHash);
  });

  it("budgets the round's verification and the callback before verifying, so an uncached round reverts early for want of gas",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const {id,request:q}=await request(c,networkHelpers);
    const signature=signRound(q.round),proof=await proveRequest(c,ethers,id,signature);
    const needed=await c.coordinator.fulfillRandomness.estimateGas(id,proof,signature);
    await expect(c.coordinator.fulfillRandomness(id,proof,signature,{gasLimit:needed-50_000n})).to.be.revertedWithCustomError(c.coordinator,"InsufficientCallbackGas");
    await (await c.coordinator.fulfillRandomness(id,proof,signature,{gasLimit:needed})).wait();
    expect((await c.coordinator.getRoundRequest(id)).delivered).to.equal(true);
  });

  it("serves a batch over two rounds, verifying each listed round when its first member is served",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const {round,time}=await roundBoundary(c);
    const a1=await request(c,networkHelpers,{at:time-2n,seed:ethers.id("a1")});
    const a2=await request(c,networkHelpers,{at:time-1n,seed:ethers.id("a2")});
    const b1=await request(c,networkHelpers,{at:time+1n,seed:ethers.id("b1")});
    expect([a1.request.round,a2.request.round,b1.request.round]).to.deep.equal([round,round,round+1n]);
    const [sa,sb]=[signRound(round),signRound(round+1n)];
    const ids=[a1.id,b1.id,a2.id];
    const proofs=[await proveRequest(c,ethers,a1.id,sa),await proveRequest(c,ethers,b1.id,sb),await proveRequest(c,ethers,a2.id,sa)];
    // The rounds may be listed in any order.
    const receipt=await (await c.coordinator.connect(c.keeper).fulfillRandomnessBatch([[0,round+1n,sb],[0,round,sa]],ids,proofs)).wait();
    const names=eventNames(c,receipt);
    // Round A is verified just before its first member, round B just before its own.
    expect(names.filter((n:string)=>n==="RoundVerified"||n==="RequestServed")).to.deep.equal(["RoundVerified","RequestServed","RoundVerified","RequestServed","RequestServed"]);
    expect(events(c,receipt,"RoundVerified").map((e:any)=>e.args.round)).to.deep.equal([round,round+1n]);
    for(const [i,requestId] of ids.entries()){
      const q=await c.coordinator.getRoundRequest(requestId);
      expect(q.fulfilled&&q.delivered).to.equal(true);
      expect(q.randomness).to.equal(proofOutput(proofs[i]));
    }
    expect(await c.coordinator.roundRandomness(0,round+1n)).to.equal(randomnessOf(sb));
  });

  it("verifies only the rounds of members it serves, and refuses a served member whose round is neither verified nor listed",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const {round,time}=await roundBoundary(c);
    const a=await request(c,networkHelpers,{at:time-1n});
    const b=await request(c,networkHelpers,{at:time+1n});
    const [sa,sb]=[signRound(round),signRound(round+1n)];
    const [pa,pb]=[await proveRequest(c,ethers,a.id,sa),await proveRequest(c,ethers,b.id,sb)];
    // A served member of an unlisted, unverified round reverts the batch before anything is verified.
    await expect(c.coordinator.fulfillRandomnessBatch([[0,round,sa]],[a.id,b.id],[pa,pb])).to.be.revertedWithCustomError(c.coordinator,"RoundUnavailable");
    await (await c.coordinator.fulfillRandomness(a.id,pa,sa)).wait();
    // A batch whose only member is already fulfilled lists round B: the member is skipped and round B stays unverified.
    const skipped=await (await c.coordinator.fulfillRandomnessBatch([[0,round+1n,sb]],[a.id],[pa])).wait();
    expect(eventNames(c,skipped)).to.deep.equal(["FulfillmentSkipped"]);
    expect(await c.coordinator.roundRandomness(0,round+1n)).to.equal(ethers.ZeroHash);
    // A wrong listed signature reverts the batch that serves its round.
    await expect(c.coordinator.fulfillRandomnessBatch([[0,round+1n,sa]],[b.id],[pb])).to.be.revertedWithCustomError(c.coordinator,"InvalidRoundSignature");
    // Batch shape.
    await expect(c.coordinator.fulfillRandomnessBatch([],[],[])).to.be.revertedWithCustomError(c.coordinator,"InvalidBatch");
    await expect(c.coordinator.fulfillRandomnessBatch([[0,round+1n,sb],[0,round+1n,sb]],[b.id],[pb])).to.be.revertedWithCustomError(c.coordinator,"InvalidBatch");
    await expect(c.coordinator.fulfillRandomnessBatch([[0,round+1n,sb]],[b.id],[pb,pb])).to.be.revertedWithCustomError(c.coordinator,"InvalidBatch");
    await (await c.coordinator.fulfillRandomnessBatch([[0,round+1n,sb]],[b.id],[pb])).wait();
    expect(await c.coordinator.roundRandomness(0,round+1n)).to.equal(randomnessOf(sb));
  });
});

describe("Round coordinator: refunds, keeper share and pricing",function(){
  it("refunds a request after its 60-second deadline, never before, and never serves it afterwards",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const {id,fee,request:q}=await request(c,networkHelpers);
    await expect(c.coordinator.refundRequest(id)).to.be.revertedWithCustomError(c.coordinator,"RefundNotAvailable");
    // At the deadline itself the request is still open.
    await networkHelpers.time.setNextBlockTimestamp(q.deadline);
    await expect(c.coordinator.refundRequest(id)).to.be.revertedWithCustomError(c.coordinator,"RefundNotAvailable");
    await networkHelpers.time.increaseTo(q.deadline+1n);
    const signature=signRound(q.round),proof=await proveRequest(c,ethers,id,signature);
    await expect(c.coordinator.fulfillRandomness(id,proof,signature)).to.be.revertedWithCustomError(c.coordinator,"RequestExpired");
    const before=await ethers.provider.getBalance(c.owner.address);
    const receipt=await (await c.coordinator.connect(c.stranger).refundRequest(id)).wait();
    await expect(receipt).to.emit(c.coordinator,"RequestRefundedTo").withArgs(id,c.owner.address,fee,true);
    await expect(receipt).to.emit(c.coordinator,"RefundCallbackAttempted").withArgs(id,await c.consumer.getAddress(),true,100_000);
    expect(await ethers.provider.getBalance(c.owner.address)-before).to.equal(fee);
    expect(await c.consumer.refundNotices(id)).to.equal(1n);
    expect((await c.coordinator.getRoundRequest(id)).refunded).to.equal(true);
    await expect(c.coordinator.fulfillRandomness(id,proof,signature)).to.be.revertedWithCustomError(c.coordinator,"RequestRefunded");
    await expect(c.coordinator.refundRequest(id)).to.be.revertedWithCustomError(c.coordinator,"RefundNotAvailable");
    // Its round was never verified: a refund verifies nothing.
    expect(await c.coordinator.roundRandomness(0,q.round)).to.equal(ethers.ZeroHash);
  });

  it("pays the keeper share to keeper() unless the submitter is an allowed backup keeper",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    expect(await c.coordinator.keeper()).to.equal(c.keeper.address);
    const share=(fee:bigint)=>fee*8000n/10000n;
    async function serve(submitter:any){
      const {id,fee,request:q}=await request(c,networkHelpers);
      const signature=signRound(q.round),proof=await proveRequest(c,ethers,id,signature);
      const receipt=await (await c.coordinator.connect(submitter).fulfillRandomness(id,proof,signature)).wait();
      const paid=events(c,receipt,"KeeperFeePaid")[0].args;
      expect(paid.amount).to.equal(share(fee));
      expect(paid.paid).to.equal(true);
      return paid.paidKeeper;
    }
    expect(await serve(c.keeper)).to.equal(c.keeper.address);
    expect(await serve(c.stranger)).to.equal(c.keeper.address);
    await expect(c.coordinator.connect(c.stranger).setBackupKeeper(c.backup.address,true)).to.be.revertedWithCustomError(c.coordinator,"OwnableUnauthorizedAccount");
    await expect(c.coordinator.setBackupKeeper(c.backup.address,true)).to.emit(c.coordinator,"BackupKeeperSet").withArgs(c.backup.address,true);
    expect([await c.coordinator.isBackupKeeper(c.backup.address),await c.coordinator.isAuthorizedKeeper(c.backup.address),await c.coordinator.backupKeeperCount()]).to.deep.equal([true,true,1n]);
    expect(await serve(c.backup)).to.equal(c.backup.address);
    expect(await serve(c.stranger)).to.equal(c.keeper.address);
    await c.coordinator.setBackupKeeper(c.backup.address,false);
    expect(await serve(c.backup)).to.equal(c.keeper.address);
    // The DAO keeps the rest: 20% of each of the five fees.
    expect(await c.coordinator.earnedFees()).to.equal(5n*(ROUND_PRICING.minFee-share(ROUND_PRICING.minFee)));
    // Role rules: no zero keeper, no backup that is the keeper or already set, at most four backups.
    await expect(c.coordinator.setKeeper(ethers.ZeroAddress)).to.be.revertedWithCustomError(c.coordinator,"InvalidConfig");
    await expect(c.coordinator.setKeeper(c.extra.address)).to.emit(c.coordinator,"KeeperChanged").withArgs(c.keeper.address,c.extra.address);
    await expect(c.coordinator.setBackupKeeper(c.extra.address,true)).to.be.revertedWithCustomError(c.coordinator,"InvalidConfig");
    await expect(c.coordinator.setBackupKeeper(c.backup.address,false)).to.be.revertedWithCustomError(c.coordinator,"InvalidConfig");
    const signers=await ethers.getSigners();
    for(const s of signers.slice(6,10))await c.coordinator.setBackupKeeper(s.address,true);
    expect(await c.coordinator.MAX_BACKUP_KEEPERS()).to.equal(4n);
    await expect(c.coordinator.setBackupKeeper(signers[10].address,true)).to.be.revertedWithCustomError(c.coordinator,"InvalidConfig");
  });

  it("starts with the deployment pricing and bounds setPricing",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    expect(Array.from(await c.coordinator.pricing())).to.deep.equal([ROUND_PRICING.minFee,2n,360_000n]);
    expect([await c.coordinator.keeperFeeBps(),await c.coordinator.refundBps(),await c.coordinator.initialMinFee()]).to.deep.equal([8000n,10000n,ROUND_PRICING.minFee]);
    expect(await c.coordinator.MAX_MIN_FEE()).to.equal(10n**16n);
    // fee = max(minFee, 2 × base fee × (360,000 + callback gas)).
    expect(await c.coordinator.quoteFeeAt(100_000,20_000_000n)).to.equal(ROUND_PRICING.minFee);
    expect(await c.coordinator.quoteFeeAt(100_000,10n**9n)).to.equal(2n*10n**9n*460_000n);
    await expect(c.coordinator.connect(c.stranger).setPricing(1,1,100_000)).to.be.revertedWithCustomError(c.coordinator,"OwnableUnauthorizedAccount");
    for(const [fee,multiplier,overhead] of [[10n**16n+1n,2,360_000],[1n,21,360_000],[1n,2,99_999],[1n,2,2_000_001],[0n,0,360_000]] as const)
      await expect(c.coordinator.setPricing(fee,multiplier,overhead),`${fee} ${multiplier} ${overhead}`).to.be.revertedWithCustomError(c.coordinator,"InvalidConfig");
    for(const [fee,multiplier,overhead] of [[10n**16n,20,2_000_000],[0n,1,100_000],[1n,0,360_000]] as const)
      await expect(c.coordinator.setPricing(fee,multiplier,overhead)).to.emit(c.coordinator,"PricingChanged").withArgs(fee,multiplier,overhead);
    // The configuration hash binds the key, the initial fee recipient and minimum fee, and beacon 0's identity.
    expect(await c.coordinator.protocolConfigurationHash()).to.equal(keccak256(abi.encode(["bytes32","uint256[2]","address","uint256","bytes32"],
      [CONFIG_DOMAIN,publicKey(),c.feeRecipient.address,ROUND_PRICING.minFee,await c.coordinator.beaconIdentity(0)])));
  });
});

describe("Round coordinator: real drand evmnet rounds",function(){
  // Its own chain, created when these tests start, whose clock begins ten minutes before the fixture's first recent round.
  let chain:Awaited<ReturnType<typeof network.create>>;
  let evmnetFixture:()=>Promise<RoundFixture>;
  before(async()=>{
    chain=await network.create({override:{initialDate:EVMNET_START}});
    evmnetFixture=()=>deployRoundCoordinator(chain.ethers,chain.networkHelpers,{evmnet:true});
  });

  it("verifies a real evmnet signature, whose sha256 is drand's published randomness, and fulfils with it",async()=>{
    const {ethers:e,networkHelpers:h}=chain;
    const c=await h.loadFixture(evmnetFixture);
    const real=EVMNET.rounds[1];
    expect(randomnessOf(real.signature)).to.equal(real.randomness);
    // Two requests 5 and 4 seconds before the round's scheduled time both bind it.
    const at=roundTime(EVMNET,real.round);
    const first=await request(c,h,{at:at-5n,seed:e.id("first")});
    const second=await request(c,h,{at:at-4n,seed:e.id("second")});
    expect([first.request.round,second.request.round]).to.deep.equal([real.round,real.round]);
    const proof=await proveRequest(c,e,first.id,real.signature);
    const receipt=await (await c.coordinator.connect(c.keeper).fulfillRandomness(first.id,proof,real.signature)).wait();
    await expect(receipt).to.emit(c.coordinator,"RoundVerified").withArgs(0,real.round,real.randomness,real.signature);
    expect(await c.consumer.results(first.id)).to.equal(proofOutput(proof));
    const cachedProof=await proveRequest(c,e,second.id,real.signature);
    const cached=await (await c.coordinator.connect(c.keeper).fulfillRandomness(second.id,cachedProof,"0x")).wait();
    expect(await c.consumer.results(second.id)).to.equal(proofOutput(cachedProof));
    expect(await c.coordinator.checkRoundSignature(0,EVMNET.rounds[2].round,EVMNET.rounds[2].signature)).to.equal(true);
    console.log(`Round coordinator gas with real evmnet round ${real.round}: first in round ${receipt.gasUsed}, cached ${cached.gasUsed} (RoundConsumer storing the result)`);
  });

  it("refuses the request's round when it is signed by another real round",async()=>{
    const {ethers:e,networkHelpers:h}=chain;
    const c=await h.loadFixture(evmnetFixture);
    const real=EVMNET.rounds[1],other=EVMNET.rounds[2];
    const {id}=await request(c,h,{at:roundTime(EVMNET,real.round)-3n});
    const proof=await proveRequest(c,e,id,real.signature);
    await expect(c.coordinator.fulfillRandomness(id,proof,other.signature)).to.be.revertedWithCustomError(c.coordinator,"InvalidRoundSignature");
    await expect(c.coordinator.fulfillRandomness(id,proof,EVMNET.rounds[0].signature)).to.be.revertedWithCustomError(c.coordinator,"InvalidRoundSignature");
  });
});

describe("Round coordinator: gas",function(){
  it("measures a request, a first-in-round and a cached fulfillment, and a two-member batch, with an empty callback",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    await c.consumer.setSilent(true);
    // Warm the counters a deployment's first request and fulfillment set from zero.
    const warm=await request(c,networkHelpers);
    const ws=signRound(warm.request.round);
    await c.coordinator.fulfillRandomness(warm.id,await proveRequest(c,ethers,warm.id,ws),ws);
    const {round,time}=await roundBoundary(c);
    const a=await request(c,networkHelpers,{at:time-2n,seed:ethers.id("a")});
    const b=await request(c,networkHelpers,{at:time-1n,seed:ethers.id("b")});
    const s=signRound(round);
    const first=await (await c.coordinator.connect(c.keeper).fulfillRandomness(a.id,await proveRequest(c,ethers,a.id,s),s)).wait();
    const cached=await (await c.coordinator.connect(c.keeper).fulfillRandomness(b.id,await proveRequest(c,ethers,b.id,s),s)).wait();
    const next=await roundBoundary(c);
    const x=await request(c,networkHelpers,{at:next.time-2n,seed:ethers.id("x")});
    const y=await request(c,networkHelpers,{at:next.time-1n,seed:ethers.id("y")});
    const t=signRound(next.round);
    const batch=await (await c.coordinator.connect(c.keeper).fulfillRandomnessBatch([[0,next.round,t]],[x.id,y.id],
      [await proveRequest(c,ethers,x.id,t),await proveRequest(c,ethers,y.id,t)])).wait();
    const gas={request:a.receipt.gasUsed,firstInRound:first.gasUsed,cached:cached.gasUsed,batchOfTwo:batch.gasUsed};
    console.log(`Round coordinator gas (empty callback, exact fee, test beacon): request ${gas.request}, first in round ${gas.firstInRound}, cached round ${gas.cached}, batch of two in one new round ${gas.batchOfTwo}`);
    expect(gas.cached).to.be.lessThan(gas.firstInRound);
    expect(gas.batchOfTwo).to.be.lessThan(gas.firstInRound+gas.cached);
  });
});
