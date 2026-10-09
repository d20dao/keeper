import {expect} from "chai";
import {network} from "hardhat";
import {proofOutput} from "../helpers/proof.ts";
import {ROUND_PRICING,deployImplementation,deployRoundCoordinator,eventsOf,proveRequest,registrationOf,request,serve,signRound,solvency,
  type RoundFixture} from "../helpers/robinhood.ts";

const {ethers,networkHelpers}=await network.create();

async function fixture(){
  const c=await deployRoundCoordinator(ethers,networkHelpers);
  const payee=await ethers.deployContract("RoundPayee",[await c.coordinator.getAddress()]);
  const hostile=await ethers.deployContract("HostileRoundConsumer",[await c.coordinator.getAddress()]);
  return {...c,payee,hostile};
}
type Fixture=Awaited<ReturnType<typeof fixture>>;
/// floor(fee × bps / 10000), the keeper share.
const share=(fee:bigint,bps:bigint)=>fee*bps/10_000n;
/// Fulfil a request from a contract account through RoundPayee.execute.
async function serveFrom(c:Fixture,account:any,requestId:bigint){
  const q=await c.coordinator.getRoundRequest(requestId),signature=signRound(q.round);
  const proof=await proveRequest(c,ethers,requestId,signature);
  const data=c.coordinator.interface.encodeFunctionData("fulfillRandomness",[requestId,proof,signature]);
  return (await (await account.execute(c.coordinator.target,data,{gasLimit:2_000_000})).wait());
}

describe("Round coordinator: the keeper share",function(){
  it("pays floor(fee × keeperFeeBps / 10000) of the escrowed fee at the share in force when the proof lands, and the rest is earned",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    // An odd flat fee, so that the share is rounded.
    const fee=25_000_000_000_007n;
    await c.coordinator.setPricing(fee,0,360_000);
    await expect(c.coordinator.setKeeperFeeBps(10_001)).to.be.revertedWithCustomError(c.coordinator,"InvalidConfig");
    await expect(c.coordinator.connect(c.stranger).setKeeperFeeBps(1)).to.be.revertedWithCustomError(c.coordinator,"OwnableUnauthorizedAccount");
    let earned=0n;
    for(const bps of [3333n,1n,9999n,10_000n,0n]){
      const {id}=await request(c,networkHelpers);
      await expect(c.coordinator.setKeeperFeeBps(bps)).to.emit(c.coordinator,"KeeperFeeBpsChanged");
      expect(await c.coordinator.keeperFeeBps()).to.equal(bps);
      const before=await ethers.provider.getBalance(c.keeper.address);
      const {receipt}=await serve(c,ethers,id,{from:c.stranger});
      const paid=eventsOf(c.coordinator,receipt,"KeeperFeePaid");
      if(bps===0n)expect(paid,"no keeper share, no event").to.have.length(0);
      else expect([...paid[0].args]).to.deep.equal([id,c.keeper.address,share(fee,bps),true]);
      expect(await ethers.provider.getBalance(c.keeper.address)-before,`bps ${bps}`).to.equal(share(fee,bps));
      earned+=fee-share(fee,bps);
      expect(await c.coordinator.earnedFees(),`bps ${bps}`).to.equal(earned);
    }
    const s=await solvency(c,ethers);
    expect(s.balance).to.equal(s.owed);
  });

  it("pays keeper() for any submitter but an allowed backup keeper, follows a keeper rotation, and pays a backup for every member of its batch",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const paidTo=async(submitter:any)=>{
      const {id}=await request(c,networkHelpers);
      const {receipt}=await serve(c,ethers,id,{from:submitter});
      return eventsOf(c.coordinator,receipt,"KeeperFeePaid")[0].args.paidKeeper;
    };
    await c.coordinator.setBackupKeeper(c.backup.address,true);
    // Rotation: the new keeper is paid for a stranger's and the old keeper's submissions; the backup still for its own.
    await expect(c.coordinator.setKeeper(c.extra.address)).to.emit(c.coordinator,"KeeperChanged").withArgs(c.keeper.address,c.extra.address);
    expect([await c.coordinator.isAuthorizedKeeper(c.keeper.address),await c.coordinator.isAuthorizedKeeper(c.extra.address)]).to.deep.equal([false,true]);
    expect(await paidTo(c.stranger)).to.equal(c.extra.address);
    expect(await paidTo(c.keeper)).to.equal(c.extra.address);
    expect(await paidTo(c.extra)).to.equal(c.extra.address);
    expect(await paidTo(c.backup)).to.equal(c.backup.address);
    // A backup made the keeper keeps its backup entry, and is paid as the keeper.
    await c.coordinator.setKeeper(c.backup.address);
    expect([await c.coordinator.isBackupKeeper(c.backup.address),await c.coordinator.backupKeeperCount()]).to.deep.equal([true,1n]);
    expect(await paidTo(c.backup)).to.equal(c.backup.address);
    expect(await paidTo(c.extra)).to.equal(c.backup.address);
    await c.coordinator.setKeeper(c.keeper.address);
    // A batch a backup submits pays it each member's share.
    const fee=await c.coordinator.quoteFeeAt(100_000,20_000_000n);
    await networkHelpers.setNextBlockBaseFeePerGas(20_000_000n);
    await c.consumer.requestMany(3,100_000,c.owner.address,{value:fee*3n});
    const last=await c.consumer.lastRequestId(),ids=[last-2n,last-1n,last];
    const q=await c.coordinator.getRoundRequest(last),signature=signRound(q.round);
    const proofs=await Promise.all(ids.map(id=>proveRequest(c,ethers,id,signature)));
    const receipt=await (await c.coordinator.connect(c.backup).fulfillRandomnessBatch([[0,q.round,signature]],ids,proofs)).wait();
    expect(eventsOf(c.coordinator,receipt,"KeeperFeePaid").map((e:any)=>[e.args.requestId,e.args.paidKeeper,e.args.amount]))
      .to.deep.equal(ids.map(id=>[id,c.backup.address,share(fee,8000n)]));
  });

  it("allows at most four backup keepers, never zero, the keeper or a no-op, and only the owner changes them",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const signers=await ethers.getSigners();
    await expect(c.coordinator.setBackupKeeper(ethers.ZeroAddress,true)).to.be.revertedWithCustomError(c.coordinator,"InvalidConfig");
    await expect(c.coordinator.setBackupKeeper(c.keeper.address,true)).to.be.revertedWithCustomError(c.coordinator,"InvalidConfig");
    await expect(c.coordinator.setBackupKeeper(c.backup.address,false)).to.be.revertedWithCustomError(c.coordinator,"InvalidConfig");
    await expect(c.coordinator.connect(c.stranger).setBackupKeeper(c.backup.address,true)).to.be.revertedWithCustomError(c.coordinator,"OwnableUnauthorizedAccount");
    await expect(c.coordinator.connect(c.stranger).setKeeper(c.stranger.address)).to.be.revertedWithCustomError(c.coordinator,"OwnableUnauthorizedAccount");
    for(const [i,s] of signers.slice(6,10).entries()){
      await expect(c.coordinator.setBackupKeeper(s.address,true)).to.emit(c.coordinator,"BackupKeeperSet").withArgs(s.address,true);
      expect(await c.coordinator.backupKeeperCount()).to.equal(BigInt(i+1));
    }
    await expect(c.coordinator.setBackupKeeper(signers[6].address,true)).to.be.revertedWithCustomError(c.coordinator,"InvalidConfig");
    await expect(c.coordinator.setBackupKeeper(signers[10].address,true)).to.be.revertedWithCustomError(c.coordinator,"InvalidConfig");
    // Removing one frees a place.
    await expect(c.coordinator.setBackupKeeper(signers[7].address,false)).to.emit(c.coordinator,"BackupKeeperSet").withArgs(signers[7].address,false);
    expect([await c.coordinator.backupKeeperCount(),await c.coordinator.isBackupKeeper(signers[7].address)]).to.deep.equal([3n,false]);
    await c.coordinator.setBackupKeeper(signers[10].address,true);
    expect(await c.coordinator.backupKeeperCount()).to.equal(4n);
    expect(await c.coordinator.MAX_BACKUP_KEEPERS()).to.equal(4n);
  });

  it("pays no keeper share for a callback retry",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    await c.hostile.setCallbackMode(1);
    const fee=await c.coordinator.quoteFeeAt(100_000,20_000_000n);
    await networkHelpers.setNextBlockBaseFeePerGas(20_000_000n);
    await c.hostile.request(ethers.id("retry"),100_000,c.owner.address,{value:fee});
    const id=await c.hostile.lastRequestId();
    await serve(c,ethers,id);
    const earned=await c.coordinator.earnedFees();
    await c.hostile.setCallbackMode(0);
    const retry=await (await c.coordinator.connect(c.keeper).retryCallback(id,100_000)).wait();
    expect(eventsOf(c.coordinator,retry).map((e:any)=>e.name)).to.deep.equal(["CallbackAttempted"]);
    expect(await c.coordinator.earnedFees()).to.equal(earned);
  });
});

describe("Round coordinator: keeper credits",function(){
  it("backs a keeper share whose transfer fails with credit that only that keeper withdraws, to any recipient but the zero address",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    await c.coordinator.setKeeper(c.payee.target);
    const amount=share(ROUND_PRICING.minFee,8000n);
    // Refused (mode 1) and burned (mode 2: spends the whole 30,000-gas payment call) payments become credit; an accepted one is paid.
    let credit=0n;
    for(const mode of [1,2,0]){
      await c.payee.setMode(mode);
      const {id}=await request(c,networkHelpers);
      const {receipt}=await serve(c,ethers,id);
      await expect(receipt,`mode ${mode}`).to.emit(c.coordinator,"KeeperFeePaid").withArgs(id,c.payee.target,amount,mode===0);
      if(mode!==0)credit+=amount;
      expect([await c.coordinator.keeperCredits(c.payee.target),await c.coordinator.totalKeeperCredits()]).to.deep.equal([credit,credit]);
      // The result was delivered whatever happened to the keeper's payment.
      expect((await c.coordinator.getRoundRequest(id)).delivered).to.equal(true);
    }
    expect(await ethers.provider.getBalance(c.payee.target)).to.equal(amount);
    let s=await solvency(c,ethers);
    expect(s.balance).to.equal(s.owed);
    const withdraw=(recipient:string)=>c.payee.execute(c.coordinator.target,c.coordinator.interface.encodeFunctionData("withdrawKeeperCredit",[recipient]));
    await expect(c.coordinator.connect(c.keeper).withdrawKeeperCredit(c.keeper.address)).to.be.revertedWithCustomError(c.coordinator,"NoKeeperCredit");
    await expect(withdraw(ethers.ZeroAddress)).to.be.revertedWithCustomError(c.coordinator,"InvalidConfig");
    await c.payee.setMode(1);
    await expect(withdraw(c.payee.target as string)).to.be.revertedWithCustomError(c.coordinator,"TransferFailed");
    expect(await c.coordinator.keeperCredits(c.payee.target)).to.equal(credit);
    const before=await ethers.provider.getBalance(c.stranger.address);
    await expect(withdraw(c.stranger.address)).to.emit(c.coordinator,"KeeperCreditWithdrawn").withArgs(c.payee.target,c.stranger.address,credit);
    expect(await ethers.provider.getBalance(c.stranger.address)-before).to.equal(credit);
    expect([await c.coordinator.keeperCredits(c.payee.target),await c.coordinator.totalKeeperCredits()]).to.deep.equal([0n,0n]);
    await expect(withdraw(c.stranger.address)).to.be.revertedWithCustomError(c.coordinator,"NoKeeperCredit");
    s=await solvency(c,ethers);
    expect(s.balance).to.equal(s.owed);
  });

  it("credits a backup keeper that refuses its payment, apart from keeper()",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    await c.coordinator.setBackupKeeper(c.payee.target,true);
    await c.payee.setMode(1);
    const {id}=await request(c,networkHelpers);
    const receipt=await serveFrom(c,c.payee,id);
    await expect(receipt).to.emit(c.coordinator,"KeeperFeePaid").withArgs(id,c.payee.target,share(ROUND_PRICING.minFee,8000n),false);
    expect([await c.coordinator.keeperCredits(c.payee.target),await c.coordinator.keeperCredits(c.keeper.address)])
      .to.deep.equal([share(ROUND_PRICING.minFee,8000n),0n]);
    expect(await c.consumer.results(id)).to.not.equal(ethers.ZeroHash);
  });
});

describe("Round coordinator: fee withdrawal",function(){
  it("pays the fee recipient only the fees it earned, never an open request's escrow or a credit",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    // Two served requests, one open, a keeper credit and an overpayment credit.
    const served=[await request(c,networkHelpers),await request(c,networkHelpers)];
    for(const r of served)await serve(c,ethers,r.id);
    const open=await request(c,networkHelpers);
    await c.coordinator.setKeeper(c.payee.target);await c.payee.setMode(1);
    const credited=await request(c,networkHelpers);
    await serve(c,ethers,credited.id);
    const fee=await c.coordinator.quoteFeeAt(100_000,20_000_000n);
    await networkHelpers.setNextBlockBaseFeePerGas(20_000_000n);
    await c.consumer.request(ethers.id("over"),100_000,c.stranger.address,{value:fee+999n});
    const earned=3n*(ROUND_PRICING.minFee-share(ROUND_PRICING.minFee,8000n));
    expect(await c.coordinator.earnedFees()).to.equal(earned);
    for(const caller of [c.owner,c.keeper,c.stranger])
      await expect(c.coordinator.connect(caller).withdrawFees(caller.address)).to.be.revertedWithCustomError(c.coordinator,"OnlyFeeRecipient");
    await expect(c.coordinator.connect(c.feeRecipient).withdrawFees(ethers.ZeroAddress)).to.be.revertedWithCustomError(c.coordinator,"InvalidConfig");
    await expect(c.coordinator.connect(c.feeRecipient).withdrawFees(c.payee.target)).to.be.revertedWithCustomError(c.coordinator,"TransferFailed");
    expect(await c.coordinator.earnedFees()).to.equal(earned);
    const balance=await ethers.provider.getBalance(c.coordinator.target);
    const before=await ethers.provider.getBalance(c.stranger.address);
    await expect(c.coordinator.connect(c.feeRecipient).withdrawFees(c.stranger.address)).to.emit(c.coordinator,"FeesWithdrawn").withArgs(c.stranger.address,earned);
    expect(await ethers.provider.getBalance(c.stranger.address)-before).to.equal(earned);
    expect(await ethers.provider.getBalance(c.coordinator.target)).to.equal(balance-earned);
    const s=await solvency(c,ethers);
    expect(s.balance).to.equal(s.owed);
    expect(s.escrow).to.equal(open.fee+fee);
    expect([await c.coordinator.totalKeeperCredits(),await c.coordinator.totalRefundCredits()]).to.deep.equal([share(ROUND_PRICING.minFee,8000n),999n]);
    // Nothing more is earned until another request is served.
    await expect(c.coordinator.connect(c.feeRecipient).withdrawFees(c.stranger.address)).to.emit(c.coordinator,"FeesWithdrawn").withArgs(c.stranger.address,0n);
  });

  it("moves fee withdrawal with setFeeRecipient, without changing the configuration hash or the initial recipient",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const {id}=await request(c,networkHelpers);
    await serve(c,ethers,id);
    const hash=await c.coordinator.protocolConfigurationHash();
    await expect(c.coordinator.setFeeRecipient(ethers.ZeroAddress)).to.be.revertedWithCustomError(c.coordinator,"InvalidConfig");
    await expect(c.coordinator.connect(c.feeRecipient).setFeeRecipient(c.stranger.address)).to.be.revertedWithCustomError(c.coordinator,"OwnableUnauthorizedAccount");
    await expect(c.coordinator.setFeeRecipient(c.stranger.address)).to.emit(c.coordinator,"FeeRecipientChanged").withArgs(c.feeRecipient.address,c.stranger.address);
    await expect(c.coordinator.connect(c.feeRecipient).withdrawFees(c.feeRecipient.address)).to.be.revertedWithCustomError(c.coordinator,"OnlyFeeRecipient");
    await expect(c.coordinator.connect(c.stranger).withdrawFees(c.stranger.address)).to.emit(c.coordinator,"FeesWithdrawn");
    expect([await c.coordinator.feeRecipient(),await c.coordinator.initialFeeRecipient(),await c.coordinator.protocolConfigurationHash()])
      .to.deep.equal([c.stranger.address,c.feeRecipient.address,hash]);
  });
});

describe("Round coordinator: ownership",function(){
  it("cannot be renounced, and moves only in two steps: the pending owner accepts, and only then the owner changes",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    await expect(c.coordinator.renounceOwnership()).to.be.revertedWithCustomError(c.coordinator,"RenounceDisabled");
    await expect(c.coordinator.connect(c.stranger).renounceOwnership()).to.be.revertedWithCustomError(c.coordinator,"OwnableUnauthorizedAccount");
    await expect(c.coordinator.connect(c.stranger).transferOwnership(c.stranger.address)).to.be.revertedWithCustomError(c.coordinator,"OwnableUnauthorizedAccount");
    await expect(c.coordinator.transferOwnership(c.extra.address)).to.emit(c.coordinator,"OwnershipTransferStarted").withArgs(c.owner.address,c.extra.address);
    expect([await c.coordinator.owner(),await c.coordinator.pendingOwner()]).to.deep.equal([c.owner.address,c.extra.address]);
    // Until it is accepted, the old owner still administers and the pending one does not.
    await c.coordinator.setKeeperFeeBps(7000);
    await expect(c.coordinator.connect(c.extra).setKeeperFeeBps(6000)).to.be.revertedWithCustomError(c.coordinator,"OwnableUnauthorizedAccount");
    await expect(c.coordinator.connect(c.stranger).acceptOwnership()).to.be.revertedWithCustomError(c.coordinator,"OwnableUnauthorizedAccount");
    await expect(c.coordinator.connect(c.extra).acceptOwnership()).to.emit(c.coordinator,"OwnershipTransferred").withArgs(c.owner.address,c.extra.address);
    expect([await c.coordinator.owner(),await c.coordinator.pendingOwner()]).to.deep.equal([c.extra.address,ethers.ZeroAddress]);
    await c.coordinator.connect(c.extra).setKeeperFeeBps(6000);
    await expect(c.coordinator.connect(c.extra).renounceOwnership()).to.be.revertedWithCustomError(c.coordinator,"RenounceDisabled");
  });

  it("refuses every owner operation, upgrades included, to anyone but the owner",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const probe=await deployImplementation(ethers,{proofVerifier:c.proofVerifier,mapping:c.mapping},"CoordinatorRobinhoodUpgradeProbe");
    const later=BigInt(await networkHelpers.time.latest())+10_000n;
    const calls:Array<[string,unknown[]]>=[
      ["registerBeacon",[registrationOf({...c.beacon,chainHash:ethers.id("x")})]],["scheduleBeacon",[0,later]],["cancelBeaconSchedule",[]],
      ["setKeeper",[c.stranger.address]],["setBackupKeeper",[c.stranger.address,true]],["setFeeRecipient",[c.stranger.address]],
      ["setKeeperFeeBps",[1]],["setPricing",[1,1,100_000]],["setRefundBps",[5000]],["transferOwnership",[c.stranger.address]],
      ["renounceOwnership",[]],["upgradeToAndCall",[probe.target,"0x"]],
    ];
    for(const caller of [c.stranger,c.keeper,c.feeRecipient])for(const [name,args] of calls)
      await expect(c.coordinator.connect(caller)[name](...args),`${name} from ${caller.address}`).to.be.revertedWithCustomError(c.coordinator,"OwnableUnauthorizedAccount")
        .withArgs(caller.address);
  });
});

describe("Round coordinator: served order",function(){
  it("numbers served requests in the order they are served, which need not be the order of their ids",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const a=await request(c,networkHelpers),b=await request(c,networkHelpers);
    const second=await serve(c,ethers,b.id);
    await expect(second.receipt).to.emit(c.coordinator,"RequestServed").withArgs(b.id,1);
    const first=await serve(c,ethers,a.id);
    await expect(first.receipt).to.emit(c.coordinator,"RequestServed").withArgs(a.id,2);
    expect([await c.coordinator.lastServedIndex(),await c.coordinator.lastServedRequestId(),await c.coordinator.servedRequestAt(1),
      await c.coordinator.servedRequestAt(2),await c.coordinator.servedRequestAt(3)]).to.deep.equal([2n,a.id,b.id,a.id,0n]);
    expect(await c.consumer.results(a.id)).to.equal(proofOutput(first.proof));
  });
});
