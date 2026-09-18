import {expect} from "chai";
import {network} from "hardhat";
import {deployReadyEpochFixture} from "./helpers/epoch.ts";
import {makeProof} from "./helpers/proof.ts";
const {ethers,networkHelpers}=await network.create();
const FEE=123n,HALF=61n;
const fixture=()=>deployReadyEpochFixture(ethers,networkHelpers,FEE);
async function request(c:Awaited<ReturnType<typeof fixture>>,refund=c.user.address){
  await c.consumer.request(ethers.id("admin-fee-test"),200000,refund,{value:FEE});
  return await c.consumer.lastRequestId() as bigint;
}
async function proof(c:Awaited<ReturnType<typeof fixture>>,id:bigint){await networkHelpers.mine(2);return makeProof(await c.rng.requestSeed(id));}

describe("Two-step administration and keeper fee accounting",()=>{
  it("restricts roles, validates updates and transfers registry/coordinator ownership in two steps",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const configuration=await c.rng.protocolConfigurationHash(),key=await c.rng.keyHash();
    for(const call of [()=>c.rng.connect(c.user).setFeeRecipient(c.user.address),()=>c.rng.connect(c.user).setKeeperFeeBps(5000),()=>c.registry.connect(c.user).setCommitter(c.user.address)])
      await expect(call()).to.be.revertedWithCustomError(c.rng,"OwnableUnauthorizedAccount").withArgs(c.user.address);
    await expect(c.rng.setFeeRecipient(ethers.ZeroAddress)).to.be.revertedWithCustomError(c.rng,"InvalidConfig");
    await expect(c.rng.setKeeperFeeBps(10001)).to.be.revertedWithCustomError(c.rng,"InvalidConfig");
    await expect(c.registry.setCommitter(ethers.ZeroAddress)).to.be.revertedWithCustomError(c.registry,"InvalidConfig");
    for(const governed of [c.rng,c.registry]){
      await governed.transferOwnership(c.user.address);
      expect(await governed.owner()).to.equal(c.owner.address);expect(await governed.pendingOwner()).to.equal(c.user.address);
      await expect(governed.connect(c.stranger).acceptOwnership()).to.be.revertedWithCustomError(governed,"OwnableUnauthorizedAccount");
      await governed.connect(c.user).acceptOwnership();expect(await governed.owner()).to.equal(c.user.address);
    }
    await expect(c.rng.setKeeperFeeBps(1)).to.be.revertedWithCustomError(c.rng,"OwnableUnauthorizedAccount");
    await expect(c.registry.setCommitter(c.owner.address)).to.be.revertedWithCustomError(c.registry,"OwnableUnauthorizedAccount");
    await c.rng.connect(c.user).setFeeRecipient(c.stranger.address);await c.rng.connect(c.user).setKeeperFeeBps(5000);
    await c.registry.connect(c.user).setCommitter(c.stranger.address);
    expect(await c.rng.initialFeeRecipient()).to.equal(c.owner.address);
    expect(await c.rng.feeRecipient()).to.equal(c.stranger.address);expect(await c.registry.committer()).to.equal(c.stranger.address);
    expect(await c.rng.protocolConfigurationHash()).to.equal(configuration);expect(await c.rng.keyHash()).to.equal(key);
  });
  it("never lets the owner or a stranger renounce registry/coordinator ownership",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    for(const governed of [c.rng,c.registry]){
      await expect(governed.connect(c.stranger).renounceOwnership()).to.be.revertedWithCustomError(governed,"OwnableUnauthorizedAccount").withArgs(c.stranger.address);
      await expect(governed.renounceOwnership()).to.be.revertedWithCustomError(governed,"RenounceDisabled");
      expect(await governed.owner()).to.equal(c.owner.address);
    }
  });
  it("pays the current registry committer 50%, rounds down and never pays an arbitrary submitter or callback retry",async()=>{
    const c=await networkHelpers.loadFixture(fixture);await c.rng.setKeeperFeeBps(5000);
    const id=await request(c),p=await proof(c,id);await c.registry.setCommitter(c.stranger.address);await c.consumer.setMode(1);
    const before=await ethers.provider.getBalance(c.stranger.address);
    await expect(c.rng.connect(c.user).fulfillRandomness(id,p,{gasLimit:2_000_000})).to.emit(c.rng,"KeeperFeePaid").withArgs(id,c.stranger.address,HALF,true);
    expect(await ethers.provider.getBalance(c.stranger.address)).to.equal(before+HALF);
    expect(await c.rng.earnedFees()).to.equal(FEE-HALF);expect(await c.rng.totalKeeperCredits()).to.equal(0n);
    expect((await c.rng.getRequest(id)).delivered).to.equal(false);
    await networkHelpers.time.increase(61);await c.consumer.setMode(0);await c.rng.retryCallback(id,200000,{gasLimit:500000});
    expect(await ethers.provider.getBalance(c.stranger.address)).to.equal(before+HALF);
    expect(await c.rng.earnedFees()).to.equal(FEE-HALF);
  });
  it("backs failed keeper payments with isolated credit without spending refunds or pending escrow",async()=>{
    const c=await networkHelpers.loadFixture(fixture),rejector=await ethers.deployContract("RejectingKeeperRecipient"),refund=await ethers.deployContract("RejectingRefundRecipient");
    await c.rng.setKeeperFeeBps(5000);await c.registry.setCommitter(await rejector.getAddress());
    const id=await request(c),p=await proof(c,id);await request(c);const refundId=await request(c,await refund.getAddress());
    await c.consumer.setMode(1);
    await expect(c.rng.fulfillRandomness(id,p,{gasLimit:2_000_000})).to.emit(c.rng,"KeeperFeePaid").withArgs(id,await rejector.getAddress(),HALF,false);
    expect(await c.rng.keeperCredits(await rejector.getAddress())).to.equal(HALF);expect(await c.rng.totalKeeperCredits()).to.equal(HALF);
    await c.consumer.setMode(0);await c.rng.retryCallback(id,200000,{gasLimit:500000});
    expect(await c.rng.totalKeeperCredits()).to.equal(HALF);
    await networkHelpers.time.increase(61);await c.rng.refundRequest(refundId,{gasLimit:500000});
    expect(await c.rng.totalRefundCredits()).to.equal(FEE);
    await c.rng.withdrawFees(c.owner.address);
    expect(await ethers.provider.getBalance(await c.rng.getAddress())).to.equal(HALF+2n*FEE);
    await expect(c.rng.connect(c.stranger).withdrawKeeperCredit(c.stranger.address)).to.be.revertedWithCustomError(c.rng,"NoKeeperCredit");
    await expect(rejector.withdraw(await c.rng.getAddress(),await rejector.getAddress())).to.be.revertedWithCustomError(c.rng,"TransferFailed");
    expect(await c.rng.totalKeeperCredits()).to.equal(HALF);
    const before=await ethers.provider.getBalance(c.user.address);await rejector.withdraw(await c.rng.getAddress(),c.user.address);
    expect(await ethers.provider.getBalance(c.user.address)).to.equal(before+HALF);
    expect(await c.rng.totalKeeperCredits()).to.equal(0n);expect(await c.rng.totalRefundCredits()).to.equal(FEE);
    expect(await ethers.provider.getBalance(await c.rng.getAddress())).to.equal(2n*FEE);
    await expect(rejector.withdraw(await c.rng.getAddress(),c.user.address)).to.be.revertedWithCustomError(c.rng,"NoKeeperCredit");
  });
  it("stores each request's escrowed fee and settles keeper share, treasury and refund from it",async()=>{
    const c=await networkHelpers.loadFixture(fixture);await c.rng.setKeeperFeeBps(5000);
    const served=await request(c),p=await proof(c,served),expired=await request(c),rng=await c.rng.getAddress();
    for(const id of [served,expired])expect(await c.rng.requestFeePaid(id)).to.equal(FEE);
    await expect(c.rng.requestFeePaid(99)).to.be.revertedWithCustomError(c.rng,"UnknownRequest");
    const identity=async(escrow:bigint)=>expect(await ethers.provider.getBalance(rng)).to.equal(await c.rng.earnedFees()+await c.rng.totalKeeperCredits()+await c.rng.totalRefundCredits()+escrow);
    await identity(2n*FEE);
    await c.rng.fulfillRandomness(served,p,{gasLimit:2_000_000});
    expect(await c.rng.earnedFees()).to.equal(FEE-HALF);await identity(FEE);
    await networkHelpers.time.increase(61);
    await expect(c.rng.refundRequest(expired,{gasLimit:500000})).to.emit(c.rng,"RequestRefundedTo").withArgs(expired,c.user.address,FEE,true);
    expect(await c.rng.earnedFees()).to.equal(FEE-HALF);await identity(0n);
    expect(await c.rng.requestFeePaid(expired)).to.equal(FEE);
  });
  it("refunds each expired request at the ratio it escrowed under and retains the owner-bounded remainder",async()=>{
    const c=await networkHelpers.loadFixture(fixture),rng=await c.rng.getAddress(),rejector=await ethers.deployContract("RejectingRefundRecipient");
    expect(await c.rng.refundBps()).to.equal(10000);
    await expect(c.rng.connect(c.user).setRefundBps(7500)).to.be.revertedWithCustomError(c.rng,"OwnableUnauthorizedAccount").withArgs(c.user.address);
    for(const bps of [4999,10001])await expect(c.rng.setRefundBps(bps)).to.be.revertedWithCustomError(c.rng,"InvalidConfig");
    const full=await request(c);
    await expect(c.rng.setRefundBps(7500)).to.emit(c.rng,"RefundBpsChanged").withArgs(10000,7500);
    const later=await request(c),credited=await request(c,await rejector.getAddress());
    expect(await c.rng.requestRefundBps(full)).to.equal(10000);expect(await c.rng.requestRefundBps(later)).to.equal(7500);
    await expect(c.rng.requestRefundBps(99)).to.be.revertedWithCustomError(c.rng,"UnknownRequest");
    const identity=async(escrow:bigint)=>expect(await ethers.provider.getBalance(rng)).to.equal(await c.rng.earnedFees()+await c.rng.totalKeeperCredits()+await c.rng.totalRefundCredits()+escrow);
    await networkHelpers.time.increase(61);const before=await ethers.provider.getBalance(c.user.address);
    // A request escrowed under 100% refunds in full even though the live ratio is now 75%.
    await expect(c.rng.refundRequest(full,{gasLimit:500000})).to.emit(c.rng,"RequestRefundedTo").withArgs(full,c.user.address,FEE,true);
    expect(await ethers.provider.getBalance(c.user.address)).to.equal(before+FEE);expect(await c.rng.earnedFees()).to.equal(0n);await identity(2n*FEE);
    // Lowering the ratio again does not touch the 75% request either.
    await expect(c.rng.setRefundBps(5000)).to.emit(c.rng,"RefundBpsChanged").withArgs(7500,5000);
    await expect(c.rng.refundRequest(later,{gasLimit:500000})).to.emit(c.rng,"RequestRefundedTo").withArgs(later,c.user.address,92n,true);
    expect(await ethers.provider.getBalance(c.user.address)).to.equal(before+FEE+92n);expect(await c.rng.earnedFees()).to.equal(31n);await identity(FEE);
    await expect(c.rng.refundRequest(credited,{gasLimit:500000})).to.emit(c.rng,"RequestRefundedTo").withArgs(credited,await rejector.getAddress(),92n,false);
    expect(await c.rng.refundCredits(await rejector.getAddress())).to.equal(92n);expect(await c.rng.totalRefundCredits()).to.equal(92n);
    expect(await c.rng.earnedFees()).to.equal(62n);await identity(0n);
    expect(await c.rng.requestRefundBps(await request(c))).to.equal(5000);
    await c.rng.withdrawFees(c.owner.address);expect(await ethers.provider.getBalance(rng)).to.equal(92n+FEE);
    await rejector.withdraw(rng,c.user.address);expect(await ethers.provider.getBalance(rng)).to.equal(FEE);
  });
  it("changes fee withdrawal administration without changing an accepted request's proof or protocol commitment",async()=>{
    const c=await networkHelpers.loadFixture(fixture);const id=await request(c),p=await proof(c,id);
    const hash=await c.rng.protocolConfigurationHash(),input=await c.rng.requestSeed(id);
    await c.rng.setFeeRecipient(c.user.address);await c.rng.setKeeperFeeBps(10000);
    expect(await c.rng.requestSeed(id)).to.equal(input);expect(await c.rng.protocolConfigurationHash()).to.equal(hash);
    await c.rng.fulfillRandomness(id,p,{gasLimit:2_000_000});expect(await c.rng.earnedFees()).to.equal(0n);
    expect(await c.rng.verifyRequestProof(id,p)).to.equal((await c.rng.getRequest(id)).randomness);
    await expect(c.rng.withdrawFees(c.owner.address)).to.be.revertedWithCustomError(c.rng,"OnlyFeeRecipient");
    await c.rng.connect(c.user).withdrawFees(c.user.address);
  });
});
