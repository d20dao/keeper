import {expect} from "chai";
import {network} from "hardhat";
import {deployProxy,implementationAddress} from "./helpers/proxy.ts";
import {EPOCH_TEST_SIGNERS,epochAttestation} from "./helpers/epoch.ts";
import {publicKey,makeProof} from "./helpers/proof.ts";

const {ethers,networkHelpers}=await network.create();
const FEE=123n;
async function fixture(){
  const [owner,tester,stranger]=await ethers.getSigners();
  const registry=await deployProxy(ethers,"EpochEntropy",[EPOCH_TEST_SIGNERS,owner.address,owner.address]);
  await networkHelpers.mine(Number(await registry.firstEpochStart())-await ethers.provider.getBlockNumber());
  await registry.commitEpoch(1,await epochAttestation(registry,1n,ethers));
  const rng=await deployProxy(ethers,"D20VRFCoordinator",[publicKey(),owner.address,owner.address,FEE,1,await registry.getAddress(),5000]);
  await rng.setPricing(FEE,0,300000);
  const client=await deployProxy(ethers,"D20CostClient",[await rng.getAddress(),owner.address,tester.address]);
  return {registry,rng,client,owner,tester,stranger};
}
async function request(c:Awaited<ReturnType<typeof fixture>>,fail=false){
  await c.client.connect(c.tester).request(ethers.id("cost-client"),200000,fail,{value:FEE});
  return await c.client.lastRequestId() as bigint;
}
async function fulfill(c:Awaited<ReturnType<typeof fixture>>,id:bigint){
  await networkHelpers.mine(2);
  const proof=makeProof(await c.rng.requestSeed(id));
  await c.rng.connect(c.stranger).fulfillRandomness(id,proof,{gasLimit:2000000});
  return await c.rng.getRequest(id);
}

describe("Restricted upgradeable cost client",()=>{
  it("locks implementation/proxy initialization and validates configured roles",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const implementation=await ethers.deployContract("D20CostClient");
    const args=[await c.rng.getAddress(),c.owner.address,c.tester.address];
    await expect(implementation.initialize(...args)).to.be.revertedWithCustomError(implementation,"InvalidInitialization");
    await expect(c.client.initialize(...args)).to.be.revertedWithCustomError(c.client,"InvalidInitialization");
    for(const coordinator of [ethers.ZeroAddress,c.stranger.address])
      await expect(deployProxy(ethers,"D20CostClient",[coordinator,c.owner.address,c.tester.address])).to.be.revertedWithCustomError(implementation,"InvalidConfig");
    await expect(deployProxy(ethers,"D20CostClient",[await c.rng.getAddress(),ethers.ZeroAddress,c.tester.address])).to.be.revertedWithCustomError(implementation,"OwnableInvalidOwner");
    await expect(deployProxy(ethers,"D20CostClient",[await c.rng.getAddress(),c.owner.address,ethers.ZeroAddress])).to.be.revertedWithCustomError(implementation,"InvalidConfig");
    expect(await c.client.coordinator()).to.equal(await c.rng.getAddress());
    expect(await c.client.owner()).to.equal(c.owner.address);
    expect(await c.client.tester()).to.equal(c.tester.address);
  });
  it("restricts requests/repairs, forwards the full payment with excess credited to the tester, and fixes refunds to the tester",async()=>{
    const c=await networkHelpers.loadFixture(fixture),next=await c.rng.nextRequestId();
    for(const caller of [c.owner,c.stranger])
      await expect(c.client.connect(caller).request(ethers.ZeroHash,200000,false,{value:FEE})).to.be.revertedWithCustomError(c.client,"OnlyTester");
    await expect(c.client.connect(c.tester).request(ethers.ZeroHash,200000,false,{value:FEE-1n})).to.be.revertedWithCustomError(c.client,"IncorrectFee").withArgs(FEE,FEE-1n);
    expect(await c.rng.nextRequestId()).to.equal(next);
    const id=await request(c),r=await c.rng.getRequest(id);
    expect(r.consumer).to.equal(await c.client.getAddress());
    expect(r.refundAddress).to.equal(c.tester.address);
    expect(await ethers.provider.getBalance(await c.client.getAddress())).to.equal(0n);
    expect(await ethers.provider.getBalance(await c.rng.getAddress())).to.equal(FEE);
    await expect(c.client.connect(c.tester).request(ethers.ZeroHash,200000,false,{value:FEE+1n})).to.emit(c.rng,"FeeOverpaymentCredited").withArgs(id+1n,c.tester.address,1n);
    expect(await c.rng.requestFeePaid(id+1n)).to.equal(FEE);expect(await c.rng.refundCredits(c.tester.address)).to.equal(1n);
    expect(await ethers.provider.getBalance(await c.client.getAddress())).to.equal(0n);
    await expect(c.client.connect(c.stranger).setCallbackFailure(id,true)).to.be.revertedWithCustomError(c.client,"OnlyTester");
    await expect(c.client.connect(c.tester).setCallbackFailure(id+2n,true)).to.be.revertedWithCustomError(c.client,"UnknownRequest");
  });
  it("authenticates callbacks and never overwrites a completed result",async()=>{
    const c=await networkHelpers.loadFixture(fixture),id=await request(c);
    await expect(c.client.connect(c.tester).rawFulfillRandomness(id,ethers.ZeroHash)).to.be.revertedWithCustomError(c.client,"OnlyCoordinator");
    // eth_call simulates the authorized sender without creating any forged transaction.
    const coordinatorAddress=await c.rng.getAddress(),clientAddress=await c.client.getAddress();
    const call=(id:bigint)=>ethers.provider.call({from:coordinatorAddress,to:clientAddress,data:c.client.interface.encodeFunctionData("rawFulfillRandomness",[id,ethers.id("forged")])});
    await expect(call(id+1n)).to.be.revertedWithCustomError(c.client,"UnknownRequest");
    const accepted=await fulfill(c,id);
    expect(await c.client.completed(id)).to.equal(true);
    expect(await c.client.results(id)).to.equal(accepted.randomness);
    await expect(call(id)).to.be.revertedWithCustomError(c.client,"AlreadyCompleted");
    await expect(c.client.connect(c.tester).setCallbackFailure(id,true)).to.be.revertedWithCustomError(c.client,"AlreadyCompleted");
    expect(await c.client.results(id)).to.equal(accepted.randomness);
  });
  it("keeps failed callbacks paid and repairs by replaying only the same accepted word",async()=>{
    const c=await networkHelpers.loadFixture(fixture),id=await request(c,true),accepted=await fulfill(c,id);
    expect(accepted.fulfilled).to.equal(true);expect(accepted.delivered).to.equal(false);
    expect(await c.client.completed(id)).to.equal(false);expect(await c.client.results(id)).to.equal(ethers.ZeroHash);
    const earned=await c.rng.earnedFees();expect(earned).to.equal(62n);
    await networkHelpers.time.increase(61);
    await expect(c.rng.refundRequest(id)).to.be.revertedWithCustomError(c.rng,"RefundNotAvailable");
    await c.client.connect(c.tester).setCallbackFailure(id,false);
    await c.rng.retryCallback(id,200000,{gasLimit:500000});
    const repaired=await c.rng.getRequest(id);
    expect(repaired.randomness).to.equal(accepted.randomness);expect(repaired.proofHash).to.equal(accepted.proofHash);
    expect(repaired.delivered).to.equal(true);expect(await c.client.results(id)).to.equal(accepted.randomness);
    expect(await c.client.completed(id)).to.equal(true);expect(await c.rng.earnedFees()).to.equal(earned);
    await expect(c.rng.retryCallback(id,200000)).to.be.revertedWithCustomError(c.rng,"AlreadyDelivered");
  });
  it("allows only the two-step owner to upgrade while retaining outstanding and completed records",async()=>{
    const c=await networkHelpers.loadFixture(fixture),completed=await request(c),accepted=await fulfill(c,completed),pending=await request(c,true);
    const next=await ethers.deployContract("D20CostClient"),address=await c.client.getAddress();
    await expect(c.client.connect(c.tester).upgradeToAndCall(await next.getAddress(),"0x")).to.be.revertedWithCustomError(c.client,"OwnableUnauthorizedAccount");
    await c.client.transferOwnership(c.stranger.address);
    await expect(c.client.connect(c.stranger).upgradeToAndCall(await next.getAddress(),"0x")).to.be.revertedWithCustomError(c.client,"OwnableUnauthorizedAccount");
    await c.client.connect(c.stranger).acceptOwnership();
    await c.client.connect(c.stranger).upgradeToAndCall(await next.getAddress(),"0x");
    expect(await c.client.getAddress()).to.equal(address);expect(await implementationAddress(ethers,c.client)).to.equal(await next.getAddress());
    expect(await c.client.coordinator()).to.equal(await c.rng.getAddress());expect(await c.client.tester()).to.equal(c.tester.address);
    expect(await c.client.lastRequestId()).to.equal(pending);expect(await c.client.requested(pending)).to.equal(true);
    expect(await c.client.callbackFailure(pending)).to.equal(true);expect(await c.client.completed(pending)).to.equal(false);
    expect(await c.client.completed(completed)).to.equal(true);expect(await c.client.results(completed)).to.equal(accepted.randomness);
  });
});
