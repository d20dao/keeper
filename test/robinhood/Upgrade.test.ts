import {expect} from "chai";
import {network} from "hardhat";
import {implementationAddress} from "../helpers/proxy.ts";
import {proofOutput} from "../helpers/proof.ts";
import {mapRandomness as mapOffChain} from "../../src/mapping.ts";
import {ROUND_LEAD,deployImplementation,deployRoundCoordinator,eventsOf,nextFee,proveRequest,registrationOf,request,roundTime,serve,signRound,solvency,
  testRoundBeacon} from "../helpers/robinhood.ts";

const {ethers,networkHelpers}=await network.create();
const fixture=()=>deployRoundCoordinator(ethers,networkHelpers);
// keccak256(abi.encode(uint256(keccak256("d20.storage.BeaconBook")) - 1)) & ~bytes32(uint256(0xff))
const BEACON_BOOK=BigInt(ethers.keccak256(ethers.AbiCoder.defaultAbiCoder().encode(["uint256"],[BigInt(ethers.id("d20.storage.BeaconBook"))-1n])))&~0xffn;

describe("Round coordinator: storage and upgrades",function(){
  it("keeps BeaconBook in its ERC-7201 namespace and the coordinator's own state from slot 0",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const proxy=await c.coordinator.getAddress();
    expect(ethers.toBeHex(BEACON_BOOK,32)).to.equal("0xf6225eefaeaf4ae83c5e55b6cf54b47540db50cae2f6ec243c42137121953f00");
    // The namespace's first member is the beacon array, whose length is the beacon count; linear slot 0 is the configuration hash.
    expect(BigInt(await ethers.provider.getStorage(proxy,BEACON_BOOK))).to.equal(await c.coordinator.beaconCount());
    expect(await ethers.provider.getStorage(proxy,0)).to.equal(await c.coordinator.protocolConfigurationHash());
  });

  it("preserves pending and served requests, cached rounds, the schedule, the roles and the balances across an upgrade",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    // A served request, a pending one in its cached round, and a pending one whose round is not verified yet.
    const now=BigInt(await networkHelpers.time.latest())+30n;
    const round=(now-c.beacon.genesis)/c.beacon.period+3n,time=roundTime(c.beacon,round)-3n;
    const served=await request(c,networkHelpers,{at:time-2n,seed:ethers.id("served")});
    const cachedPending=await request(c,networkHelpers,{at:time-1n,seed:ethers.id("cached")});
    const uncachedPending=await request(c,networkHelpers,{at:time+1n,seed:ethers.id("uncached")});
    expect([served.request.round,cachedPending.request.round,uncachedPending.request.round]).to.deep.equal([round,round,round+1n]);
    const s=signRound(round);
    await (await c.coordinator.connect(c.keeper).fulfillRandomness(served.id,await proveRequest(c,ethers,served.id,s),s)).wait();
    // A second beacon, a pending switch to it, a backup keeper and a pending owner.
    const other={...testRoundBeacon(await c.beaconVerifier.getAddress(),BigInt(await networkHelpers.time.latest())),chainHash:ethers.id("another network")};
    await c.coordinator.registerBeacon(registrationOf(other));
    const switchAt=BigInt(await networkHelpers.time.latest())+3600n;
    await c.coordinator.scheduleBeacon(1,switchAt);
    await c.coordinator.setBackupKeeper(c.backup.address,true);
    await c.coordinator.transferOwnership(c.stranger.address);
    const views=["owner","pendingOwner","keeper","backupKeeperCount","feeRecipient","initialFeeRecipient","keeperFeeBps","minFee","initialMinFee","feeMultiplier",
      "fulfillGasOverhead","refundBps","nextRequestId","earnedFees","totalKeeperCredits","totalRefundCredits","lastServedRequestId","lastServedIndex",
      "protocolConfigurationHash","keyHash","publicKeyX","publicKeyY","beaconCount","proofVerifier"];
    const snapshot=async()=>({
      scalars:await Promise.all(views.map(name=>c.coordinator[name]())),
      requests:await Promise.all([served.id,cachedPending.id,uncachedPending.id].map(async id=>Array.from(await c.coordinator.getRoundRequest(id)))),
      schedule:Array.from(await c.coordinator.beaconSchedule()),
      beacons:await Promise.all([0,1].map(async id=>[Array.from(await c.coordinator.getBeacon(id)),await c.coordinator.beaconIdentity(id)])),
      roundAt:Array.from(await c.coordinator.roundAt(switchAt)),
      cached:await c.coordinator.roundRandomness(0,round),
      backup:await c.coordinator.isBackupKeeper(c.backup.address),
      balance:await ethers.provider.getBalance(await c.coordinator.getAddress()),
      served:await c.coordinator.servedRequestAt(1),
    });
    const before=await snapshot();
    expect(before.schedule).to.deep.equal([0n,before.schedule[1],1n,switchAt]);
    // Only the current owner upgrades; then the probe implementation takes over the same proxy.
    const probe=await deployImplementation(ethers,{proofVerifier:c.proofVerifier,mapping:c.mapping},"CoordinatorRobinhoodUpgradeProbe");
    await expect(c.coordinator.connect(c.stranger).upgradeToAndCall(await probe.getAddress(),"0x")).to.be.revertedWithCustomError(c.coordinator,"OwnableUnauthorizedAccount");
    await c.coordinator.upgradeToAndCall(await probe.getAddress(),"0x");
    expect(await implementationAddress(ethers,c.coordinator)).to.equal(await probe.getAddress());
    const upgraded=probe.attach(await c.coordinator.getAddress()) as any;
    expect(await upgraded.upgradeMarker()).to.equal(2n);
    expect(await snapshot()).to.deep.equal(before);
    // Both pending requests are served after the upgrade: one from the cache, one verifying its round now, and paid to the backup keeper.
    const p1=await proveRequest(c,ethers,cachedPending.id,s);
    await (await upgraded.connect(c.keeper).fulfillRandomness(cachedPending.id,p1,"0x")).wait();
    const t=signRound(round+1n),p2=await proveRequest(c,ethers,uncachedPending.id,t);
    await expect(upgraded.connect(c.backup).fulfillRandomness(uncachedPending.id,p2,t)).to.emit(upgraded,"KeeperFeePaid").withArgs(uncachedPending.id,c.backup.address,uncachedPending.fee*8000n/10000n,true);
    expect(await c.consumer.results(cachedPending.id)).to.equal(proofOutput(p1));
    expect(await c.consumer.results(uncachedPending.id)).to.equal(proofOutput(p2));
    // The pending owner can still accept.
    await upgraded.connect(c.stranger).acceptOwnership();
    expect(await upgraded.owner()).to.equal(c.stranger.address);
  });

  it("preserves past eras, pricing, credits, mappings and failed deliveries across an upgrade, and keeps folding the schedule after it",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const proxy=await c.coordinator.getAddress();
    const now=async()=>BigInt(await networkHelpers.time.latest());
    // Beacons 1 and 2 with their own periods and geneses; the schedule switches 0 -> 1 -> 2, each switch folding the era before it, and a
    // switch back to 0 is pending at the upgrade.
    const beacons=[c.beacon,{...c.beacon,chainHash:ethers.id("era one"),period:5n,genesis:c.beacon.genesis+2n},
      {...c.beacon,chainHash:ethers.id("era two"),period:2n,genesis:c.beacon.genesis+1n}];
    for(const b of beacons.slice(1))await c.coordinator.registerBeacon(registrationOf(b));
    const [,since]=await c.coordinator.beaconSchedule();
    const eras:Array<{id:number;from:bigint}>=[{id:0,from:since}];
    for(const id of [1,2,0]){
      const from=await now()+700n;
      await c.coordinator.scheduleBeacon(id,from);
      eras.push({id,from});
      if(id!==0)await networkHelpers.time.increaseTo(from);
    }
    // Pricing, shares, refund ratio and roles away from their initial values.
    await c.coordinator.setPricing(30_000_000_000_000n,5,400_000);
    await c.coordinator.setKeeperFeeBps(6000);
    await c.coordinator.setRefundBps(7000);
    await c.coordinator.setFeeRecipient(c.extra.address);
    await c.coordinator.setBackupKeeper(c.backup.address,true);
    // Settled and open state of every kind: a keeper credit, an overpayment credit, a fulfilled request whose callback failed, a refunded
    // request whose notification failed, and an open mapped request.
    const payee=await ethers.deployContract("RoundPayee",[proxy]),hostile=await ethers.deployContract("HostileRoundConsumer",[proxy]);
    await payee.setMode(1);
    await c.coordinator.setKeeper(payee.target);
    const credited=await request(c,networkHelpers);
    await serve(c,ethers,credited.id);
    await c.coordinator.setKeeper(c.keeper.address);
    await hostile.setCallbackMode(1);await hostile.setRefundMode(1);
    const fee=await nextFee(c,networkHelpers,100_000);
    await hostile.request(ethers.id("undelivered"),100_000,c.stranger.address,{value:fee+777n});
    const undelivered=await hostile.lastRequestId();
    const {proof:undeliveredProof}=await serve(c,ethers,undelivered);
    await nextFee(c,networkHelpers,100_000);
    await hostile.request(ethers.id("unnotified"),100_000,c.stranger.address,{value:fee});
    const unnotified=await hostile.lastRequestId();
    await networkHelpers.time.increase(61);
    await c.coordinator.refundRequest(unnotified);
    const dice=[1,0n,6n,4,0];
    await nextFee(c,networkHelpers,100_000);
    await c.consumer.requestMapped(ethers.id("dice"),100_000,c.owner.address,dice,{value:fee});
    const mapped=await c.consumer.lastRequestId();
    expect((await c.coordinator.getRoundRequest(mapped)).beaconId).to.equal(2n);

    const ids=Array.from({length:Number(await c.coordinator.nextRequestId())-1},(_,i)=>BigInt(i+1));
    const times=[0n,since-1n,since,...eras.flatMap(e=>[e.from-1n,e.from,e.from+5n]),eras.at(-1)!.from+10_000n];
    // The raw words: the coordinator's 24 linear slots, BeaconBook's beacon count, schedule and past-era count, and each past era.
    const words=async()=>{
      const linear=await Promise.all(Array.from({length:24},(_,i)=>ethers.provider.getStorage(proxy,i)));
      const book=await Promise.all(Array.from({length:3},(_,i)=>ethers.provider.getStorage(proxy,BEACON_BOOK+BigInt(i))));
      const pastBase=BigInt(ethers.keccak256(ethers.toBeHex(BEACON_BOOK+2n,32)));
      const past=await Promise.all(Array.from({length:Number(BigInt(book[2]))},(_,i)=>ethers.provider.getStorage(proxy,pastBase+BigInt(i))));
      return {linear,book,past};
    };
    const snapshot=async()=>({
      words:await words(),
      pricing:Array.from(await c.coordinator.pricing()),
      roles:[await c.coordinator.owner(),await c.coordinator.keeper(),await c.coordinator.feeRecipient(),await c.coordinator.isBackupKeeper(c.backup.address),
        await c.coordinator.backupKeeperCount(),await c.coordinator.keeperFeeBps(),await c.coordinator.refundBps()],
      credits:[await c.coordinator.keeperCredits(payee.target),await c.coordinator.totalKeeperCredits(),await c.coordinator.refundCredits(c.stranger.address),
        await c.coordinator.totalRefundCredits(),await c.coordinator.earnedFees()],
      requests:await Promise.all(ids.map(async id=>[Array.from(await c.coordinator.getRoundRequest(id)),await c.coordinator.requestRefundBps(id),
        Array.from(await c.coordinator.getMapping(id)),await c.coordinator.refundCallbackDelivered(id)])),
      pending:Array.from((await c.coordinator.getPendingRequestIds(1,256))[0]),
      schedule:Array.from(await c.coordinator.beaconSchedule()),
      roundAt:await Promise.all(times.map(async t=>Array.from(await c.coordinator.roundAt(t)))),
      beacons:await Promise.all([0,1,2].map(async id=>[Array.from(await c.coordinator.getBeacon(id)),await c.coordinator.beaconIdentity(id)])),
      balance:await ethers.provider.getBalance(proxy),
    });
    const before=await snapshot();
    expect(before.words.past.length).to.equal(2);
    expect(before.schedule).to.deep.equal([2n,eras[2].from,0n,eras[3].from]);
    expect(before.pending).to.deep.equal([mapped]);
    const probe=await deployImplementation(ethers,{proofVerifier:c.proofVerifier,mapping:c.mapping},"CoordinatorRobinhoodUpgradeProbe");
    await c.coordinator.upgradeToAndCall(probe.target,"0x");
    const upgraded=probe.attach(proxy) as any;
    expect(await upgraded.upgradeMarker()).to.equal(2n);
    expect(await snapshot()).to.deep.equal(before);

    // The open mapped request is served by the new implementation and maps as before.
    const {proof:mappedProof}=await serve({...c,coordinator:upgraded},ethers,mapped);
    expect(await upgraded.getMappedResult(mapped)).to.deep.equal(mapOffChain(proofOutput(mappedProof),{operation:1,lower:0n,upper:6n,count:4,population:0}));
    // The pending switch takes effect, and the next schedule folds era 2 behind the two eras folded before the upgrade.
    await networkHelpers.time.increaseTo(eras[3].from);
    const last=await now()+700n;
    await upgraded.scheduleBeacon(1,last);
    eras.push({id:1,from:last});
    expect(BigInt(await ethers.provider.getStorage(proxy,BEACON_BOOK+2n))).to.equal(3n);
    for(const t of [...times,last-1n,last,last+9n]){
      let era=eras[0];
      for(const e of eras)if(t>=e.from)era=e;
      const b=beacons[era.id],round=t+ROUND_LEAD<=b.genesis?1n:(t+ROUND_LEAD-b.genesis+b.period-1n)/b.period+1n;
      expect(Array.from(await upgraded.roundAt(t)),`roundAt(${t})`).to.deep.equal([BigInt(era.id),round]);
    }
    // The failed delivery retries the result stored before the upgrade; the failed notification retries without a second payment.
    await hostile.setCallbackMode(0);await hostile.setRefundMode(0);
    await expect(upgraded.retryCallback(undelivered,100_000)).to.emit(upgraded,"CallbackAttempted").withArgs(undelivered,true,100_000);
    expect(await hostile.results(undelivered)).to.equal(proofOutput(undeliveredProof));
    const retried=await (await upgraded.retryRefundCallback(unnotified,100_000)).wait();
    expect(eventsOf(upgraded,retried).map((e:any)=>e.name)).to.deep.equal(["RefundCallbackAttempted"]);
    // Credits withdraw as they were recorded, and nothing is left in escrow.
    await payee.setMode(0);
    await expect(payee.execute(proxy,upgraded.interface.encodeFunctionData("withdrawKeeperCredit",[payee.target])))
      .to.emit(upgraded,"KeeperCreditWithdrawn").withArgs(payee.target,payee.target,before.credits[0]);
    await expect(upgraded.connect(c.stranger).withdrawRefundCredit(c.stranger.address)).to.emit(upgraded,"RefundCreditWithdrawn")
      .withArgs(c.stranger.address,c.stranger.address,777n);
    const s=await solvency(c,ethers);
    expect([s.balance,s.escrow]).to.deep.equal([s.owed,0n]);
  });

  it("cannot be initialized twice, and its implementation cannot be initialized at all",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const registration=registrationOf(c.beacon);
    await expect(c.coordinator.initialize(c.params,registration)).to.be.revertedWithCustomError(c.coordinator,"InvalidInitialization");
    await expect(c.implementation.initialize(c.params,registration)).to.be.revertedWithCustomError(c.implementation,"InvalidInitialization");
    await expect(c.coordinator.renounceOwnership()).to.be.revertedWithCustomError(c.coordinator,"RenounceDisabled");
  });
});
