import {readFileSync} from "node:fs";
import {expect} from "chai";
import {network} from "hardhat";
import {deployProxy,implementationAddress} from "./helpers/proxy.ts";
import {EPOCH_TEST_SIGNERS,epochAttestation} from "./helpers/epoch.ts";
import {publicKey,makeProof,proofOutput} from "./helpers/proof.ts";
const {ethers,networkHelpers,provider}=await network.create();
const FEE=10n**15n;
// The coordinator implementation live on Arc Mainnet and Arc Testnet before the batch gas guard.
const live=JSON.parse(readFileSync(new URL("./fixtures/coordinator-deployed-de5f82e.json",import.meta.url),"utf8"));
// The contract's up-front budget: 140000 + Σ(limit + limit/63 + 400000) over the members it will serve.
const RESERVE=140_000n,OVERHEAD=400_000n;
const budget=(limits:readonly bigint[])=>limits.reduce((sum,limit)=>sum+limit+limit/63n+OVERHEAD,RESERVE);

/// A registry with a published first epoch and a coordinator proxy running the live implementation code.
async function liveFixture(){
  const [owner,user,stranger]=await ethers.getSigners();
  const registry:any=await deployProxy(ethers,"EpochEntropy",[EPOCH_TEST_SIGNERS,owner.address,owner.address]);
  await networkHelpers.mine(Number(await registry.firstEpochStart())-await ethers.provider.getBlockNumber());
  await registry.commitEpoch(1,await epochAttestation(registry,1n,ethers));
  // Runtime code only, at its live address, where its UUPS self address is valid.
  await provider.request({method:"hardhat_setCode",params:[live.implementation,live.deployedBytecode]});
  expect(ethers.keccak256(await ethers.provider.getCode(live.implementation))).to.equal(live.runtimeCodeHash);
  const coordinatorInterface=(await ethers.getContractFactory("D20VRFCoordinator")).interface;
  const proxy=await ethers.deployContract("D20Proxy",[live.implementation,coordinatorInterface.encodeFunctionData("initialize",[publicKey(),owner.address,owner.address,FEE,1,await registry.getAddress(),5000])]);
  const rng:any=await ethers.getContractAt("D20VRFCoordinator",await proxy.getAddress());
  await rng.setPricing(FEE,0,300000);
  return {registry,rng,owner,user,stranger};
}
async function guardedFixture(){
  const c=await liveFixture(),next=await ethers.deployContract("D20VRFCoordinator");
  await c.rng.upgradeToAndCall(await next.getAddress(),"0x");
  expect(await implementationAddress(ethers,c.rng)).to.equal(await next.getAddress());
  return c;
}
type Fixture=Awaited<ReturnType<typeof liveFixture>>;
async function publishCurrent(c:Fixture){
  const epoch=await c.registry.epochForBlock(await ethers.provider.getBlockNumber()+1);
  if((await c.registry.getEpoch(epoch)).epochHash===ethers.ZeroHash)await c.registry.commitEpoch(epoch,await epochAttestation(c.registry,epoch,ethers));
}
async function proofs(c:Fixture,ids:readonly bigint[]){await networkHelpers.mine(2);return Promise.all(ids.map(async id=>makeProof(await c.rng.requestSeed(id))));}
/// The keeper's batch sizing: eth_estimateGas on the exact payload, then 1.2 × estimate + 50000.
async function keeperPlan(c:Fixture,ids:readonly bigint[],ps:readonly unknown[],from=c.owner){
  const data=c.rng.interface.encodeFunctionData("fulfillRandomnessBatch",[ids,ps]);
  // Arc nodes estimate with tx.gasprice == 0 (checked against Arc's public RPC endpoints); say so explicitly here.
  const estimate=await ethers.provider.estimateGas({from:from.address,to:await c.rng.getAddress(),data,gasPrice:0});
  return {data,estimate,gasLimit:estimate*12n/10n+50_000n,send:async(gasLimit=estimate*12n/10n+50_000n)=>from.sendTransaction({to:await c.rng.getAddress(),data,gasLimit})};
}
/// One attacker round: the draw and its two helpers are opened in one transaction and prepared as one batch.
async function attackRound(c:Fixture,attacker:any){
  await publishCurrent(c);
  await (await attacker.drawAndArm(FEE,FEE,{value:3n*FEE})).wait();
  const draw=await attacker.drawId() as bigint,ids=[draw,draw+1n,draw+2n];
  const ps=await proofs(c,ids);
  return {ids,ps,won:BigInt(proofOutput(ps[0]))%8n===0n};
}
async function consumers(c:Fixture,count:number){
  const address=await c.rng.getAddress();
  return Promise.all(Array.from({length:count},()=>ethers.deployContract("TestConsumer",[address]))) as Promise<any[]>;
}
async function openRequests(c:Fixture,members:readonly any[],limits:readonly bigint[]){
  const ids:bigint[]=[];
  for(const [i,consumer] of members.entries()){await consumer.request(ethers.id(`guard-${i}`),limits[i],c.user.address,{value:FEE});ids.push(await consumer.lastRequestId() as bigint);}
  return ids;
}
const intrinsic=(data:string)=>ethers.getBytes(data).reduce((gas,byte)=>gas+(byte===0?4n:16n),21_000n);

describe("Batch gas guard (the batch-abort issue)",()=>{
  // Arc runs eth_estimateGas with tx.gasprice == 0, so a callback can look cheap to the keeper's estimate and burn its
  // whole budget on chain (AbortHelper mode 1). Local estimation shows the same zero gas price.
  it("lets only winning draws land on the live implementation, and every losing draw land after the upgrade",async()=>{
    const c=await networkHelpers.loadFixture(liveFixture),attacker:any=await ethers.deployContract("BatchGuardAttacker",[await c.rng.getAddress()]);
    await attacker.setMode(1);
    let aborted=0;
    for(let round=0;round<24&&aborted<2;round++){
      const {ids,ps,won}=await attackRound(c,attacker),plan=await keeperPlan(c,ids,ps);
      if(won){await (await plan.send()).wait();expect(await attacker.won()).to.equal(true);continue;}
      // The draw's callback ran and lost, the helpers burned their budgets and the last member ran out of gas.
      await expect(plan.send()).to.be.revertedWithCustomError(c.rng,"InsufficientCallbackGas");
      for(const id of ids)expect((await c.rng.getRequest(id)).fulfilled).to.equal(false);
      // All three expire and refund in full: the aborted draw cost the attacker gas only.
      await networkHelpers.time.increase(61);
      for(const id of ids)await c.rng.refundRequest(id,{gasLimit:500_000});
      aborted++;
    }
    expect(aborted).to.equal(2);

    const next=await ethers.deployContract("D20VRFCoordinator");
    await c.rng.upgradeToAndCall(await next.getAddress(),"0x");
    let landedLosses=0;
    for(let round=0;round<24&&landedLosses<3;round++){
      const {ids,ps,won}=await attackRound(c,attacker),plan=await keeperPlan(c,ids,ps);
      expect(plan.estimate).to.be.at.least(budget([100_000n,1_000_000n,1_000_000n]));
      const receipt=(await (await plan.send()).wait())!;
      expect(receipt.status).to.equal(1);
      for(const id of ids)expect((await c.rng.getRequest(id)).fulfilled,"every member is served").to.equal(true);
      expect((await c.rng.getRequest(ids[0])).randomness).to.equal(proofOutput(ps[0]));
      expect(await attacker.won()).to.equal(won);
      if(won)continue;
      // Both helpers burned their whole 1,000,000-gas callbacks on chain, and the losing draw stands.
      expect(receipt.gasUsed).to.be.greaterThan(2_000_000n);
      for(const id of ids.slice(1))expect((await c.rng.getRequest(id)).delivered).to.equal(false);
      expect((await c.rng.getRequest(ids[0])).delivered).to.equal(true);
      landedLosses++;
    }
    expect(landedLosses).to.equal(3);
  });

  it("serves every member at the estimated gas limit itself, even when every callback then burns its whole budget",async()=>{
    // Worst-case members: opened before their epoch was published, submitted by an unauthorized sender (the keeper
    // share goes to committer()), and a committer that burns the 30000-gas payment call and leaves a credit.
    const c=await networkHelpers.loadFixture(guardedFixture);
    const epoch=await c.registry.epochForBlock(await ethers.provider.getBlockNumber())+1n;
    await networkHelpers.mine(Number(await c.registry.epochStart(epoch))-await ethers.provider.getBlockNumber());
    const limits=[30_000n,100_000n,200_000n,1_000_000n,50_000n,250_000n,500_000n,30_000n,100_000n,200_000n,1_000_000n,75_000n,150_000n,300_000n,30_000n,120_000n];
    const members=await consumers(c,limits.length),ids=await openRequests(c,members,limits);
    for(const id of ids)expect((await c.rng.getRequest(id)).epochHash).to.equal(ethers.ZeroHash);
    await c.registry.commitEpoch(epoch,await epochAttestation(c.registry,epoch,ethers));
    const burner=await ethers.deployContract("BurningRecipient");await c.registry.setCommitter(await burner.getAddress());
    const ps=await proofs(c,ids),plan=await keeperPlan(c,ids,ps,c.stranger);
    const need=budget(limits);
    expect(plan.estimate).to.be.at.least(need);
    // Nothing is served below the estimate: the guard reverts before the first member.
    await expect(plan.send(need)).to.be.revertedWithCustomError(c.rng,"InsufficientCallbackGas");
    for(const id of ids)expect((await c.rng.getRequest(id)).fulfilled).to.equal(false);
    // The estimate was taken with cheap callbacks; now every callback burns its whole budget, without keeper padding.
    for(const member of members)await member.setMode(2);
    const receipt=(await (await plan.send(plan.estimate)).wait())!;
    expect(receipt.status).to.equal(1);
    for(const id of ids){const r=await c.rng.getRequest(id);expect(r.fulfilled).to.equal(true);expect(r.delivered).to.equal(false);}
    const callbacks=limits.reduce((a,b)=>a+b,0n),perMember=(receipt.gasUsed-intrinsic(plan.data)-callbacks)/BigInt(limits.length);
    expect(await c.rng.keeperCredits(await burner.getAddress())).to.equal(16n*(FEE/2n));
    console.log(`Worst-case batch of 16: estimate=${plan.estimate} budget=${need} gasUsed=${receipt.gasUsed} non-callback gas per member=${perMember}`);
    // The 400000 per-member budget stays well above what a member costs outside its callback.
    expect(perMember).to.be.lessThan(OVERHEAD*3n/4n);
  });

  it("lands honest batches of mixed callback limits sized like the keeper, up to sixteen members or eight 1,000,000-gas callbacks",async()=>{
    const c=await networkHelpers.loadFixture(guardedFixture);
    // TestConsumer stores two fresh words, so its callbacks need about 50000 gas.
    // The local node enforces the EIP-7825 per-transaction cap (16777216), so the largest case here is eight 1,000,000-gas callbacks.
    for(const limits of [[100_000n,60_000n],[60_000n,100_000n,200_000n,1_000_000n,75_000n,250_000n,500_000n,60_000n],
      [60_000n,100_000n,200_000n,300_000n,75_000n,250_000n,150_000n,60_000n,100_000n,120_000n,200_000n,90_000n,60_000n,250_000n,100_000n,80_000n],Array(8).fill(1_000_000n)] as bigint[][]){
      await publishCurrent(c);
      const members=await consumers(c,limits.length),ids=await openRequests(c,members,limits),ps=await proofs(c,ids);
      const plan=await keeperPlan(c,ids,ps),receipt=(await (await plan.send()).wait())!;
      expect(receipt.status).to.equal(1);
      for(const [i,id] of ids.entries()){
        const r=await c.rng.getRequest(id);expect(r.fulfilled).to.equal(true);expect(r.delivered).to.equal(true);
        expect(await members[i].results(id)).to.equal(proofOutput(ps[i]));
      }
      console.log(`Honest batch of ${limits.length}: budget=${budget(limits)} estimate=${plan.estimate} keeper gas limit=${plan.gasLimit} gasUsed=${receipt.gasUsed}`);
    }
  });

  it("budgets only the members it will serve and leaves the single path unchanged",async()=>{
    const c=await networkHelpers.loadFixture(guardedFixture);
    const members=await consumers(c,3);
    const [expired]=await openRequests(c,[members[0]],[1_000_000n]);await networkHelpers.time.increase(61);
    await publishCurrent(c);
    const [taken,served]=await openRequests(c,members.slice(1),[1_000_000n,200_000n]);
    const ps=await proofs(c,[expired,taken,served]);
    await c.rng.connect(c.stranger).fulfillRandomness(taken,ps[1],{gasLimit:2_000_000});
    // Two skipped 1,000,000-gas members add no budget: the batch needs what its one served member needs.
    const plan=await keeperPlan(c,[expired,taken,served],ps);
    expect(plan.estimate).to.be.lessThan(budget([200_000n])+400_000n);
    await expect(plan.send()).to.emit(c.rng,"FulfillmentSkipped");
    expect((await c.rng.getRequest(served)).delivered).to.equal(true);
    // fulfillRandomness keeps its own per-callback check and needs no batch budget.
    const [single]=await openRequests(c,[members[2]],[1_000_000n]),[proof]=await proofs(c,[single]);
    const data=c.rng.interface.encodeFunctionData("fulfillRandomness",[single,proof]);
    expect(await ethers.provider.estimateGas({from:c.owner.address,to:await c.rng.getAddress(),data})).to.be.lessThan(budget([1_000_000n]));
  });
});

const erc7201=(id:string)=>BigInt(ethers.keccak256(ethers.AbiCoder.defaultAbiCoder().encode(["uint256"],[BigInt(ethers.id(id))-1n])))&~0xffn;
const NAMESPACES=["openzeppelin.storage.Initializable","openzeppelin.storage.Ownable","openzeppelin.storage.Ownable2Step","openzeppelin.storage.ReentrancyGuard"].map(erc7201);
/// Declared slots 0-21 and the 38-slot gap, the OpenZeppelin namespaces, and every word of the given requests.
async function coordinatorStorage(rng:any,ids:readonly bigint[]){
  const slots=[...Array.from({length:60},(_,i)=>BigInt(i)),...NAMESPACES];
  for(const id of ids){const base=BigInt(ethers.keccak256(ethers.AbiCoder.defaultAbiCoder().encode(["uint256","uint256"],[id,18n])));for(let i=0n;i<11n;i++)slots.push(base+i);}
  const address=await rng.getAddress();
  return Object.fromEntries(await Promise.all(slots.map(async slot=>[ethers.toBeHex(slot),await ethers.provider.getStorage(address,slot)])));
}

describe("In-place upgrade from the live implementation",()=>{
  it("pins the fixture to the coordinator implementation both deployment manifests record",async()=>{
    for(const name of ["arc-mainnet","arc-testnet"]){
      const manifest=JSON.parse(readFileSync(new URL(`../deployments/${name}.json`,import.meta.url),"utf8"));
      // Live now, or recorded as the implementation an upgrade replaced.
      const recorded=(manifest.coordinatorImplementation===live.implementation&&manifest.coordinatorImplementationCodeHash===live.runtimeCodeHash)||
        (manifest.implementationUpgrades??[]).some((entry:any)=>entry.contract==="coordinator"&&entry.previousImplementation===live.implementation&&entry.previousImplementationCodeHash===live.runtimeCodeHash);
      expect(recorded,name).to.equal(true);
    }
    expect(ethers.keccak256(live.deployedBytecode)).to.equal(live.runtimeCodeHash);
  });
  it("keeps every storage word, balance, role and open request, and serves pre-upgrade requests under the guard",async()=>{
    const c=await networkHelpers.loadFixture(liveFixture),rng=c.rng,address=await rng.getAddress();
    await rng.setKeeperFeeBps(6000);
    const members=await consumers(c,6),refundTo=await ethers.deployContract("RejectingRefundRecipient");
    // Served, refunded to a rejecting recipient (credit), overpaid (credit), and three left open across the upgrade.
    const [served]=await openRequests(c,[members[0]],[200_000n]);
    await members[1].request(ethers.id("refund"),200_000,await refundTo.getAddress(),{value:FEE});const refunded=await members[1].lastRequestId() as bigint;
    await members[2].request(ethers.id("overpaid"),200_000,c.user.address,{value:FEE+7n});const overpaid=await members[2].lastRequestId() as bigint;
    const [servedProof]=await proofs(c,[served]);
    await (await rng.fulfillRandomness(served,servedProof,{gasLimit:2_000_000})).wait();
    await networkHelpers.time.increase(61);await rng.refundRequest(refunded,{gasLimit:500_000});
    await publishCurrent(c);
    const open=await openRequests(c,members.slice(3),[1_000_000n,100_000n,60_000n]),openProofs=await proofs(c,open);
    await rng.transferOwnership(c.user.address);
    const ids=[served,refunded,overpaid,...open];
    const before=await coordinatorStorage(rng,ids),balance=await ethers.provider.getBalance(address);
    const views=async()=>({pricing:Array.from(await rng.pricing()),keeperFeeBps:await rng.keeperFeeBps(),refundBps:await rng.refundBps(),earnedFees:await rng.earnedFees(),
      totalKeeperCredits:await rng.totalKeeperCredits(),totalRefundCredits:await rng.totalRefundCredits(),refundCredit:await rng.refundCredits(await refundTo.getAddress()),
      overpayCredit:await rng.refundCredits(c.user.address),owner:await rng.owner(),pendingOwner:await rng.pendingOwner(),feeRecipient:await rng.feeRecipient(),
      nextRequestId:await rng.nextRequestId(),lastServedIndex:await rng.lastServedIndex(),config:await rng.protocolConfigurationHash(),keyHash:await rng.keyHash(),
      requests:await Promise.all(ids.map(async id=>(await rng.getRequest(id)).toArray().map(String)))});
    const viewsBefore=await views();
    expect(viewsBefore.pendingOwner).to.equal(c.user.address);
    expect(viewsBefore.earnedFees).to.be.greaterThan(0n);expect(viewsBefore.refundCredit).to.equal(FEE);expect(viewsBefore.overpayCredit).to.equal(7n);

    const next=await ethers.deployContract("D20VRFCoordinator");
    await expect(rng.connect(c.user).upgradeToAndCall(await next.getAddress(),"0x")).to.be.revertedWithCustomError(rng,"OwnableUnauthorizedAccount");
    await expect(rng.upgradeToAndCall(await next.getAddress(),"0x")).to.emit(rng,"Upgraded").withArgs(await next.getAddress());
    expect(await coordinatorStorage(rng,ids)).to.deep.equal(before);
    expect(await ethers.provider.getBalance(address)).to.equal(balance);
    expect(await views()).to.deep.equal(viewsBefore);

    // Proofs prepared before the upgrade still verify; the open requests are batched under the guard.
    const plan=await keeperPlan(c,open,openProofs);
    expect(plan.estimate).to.be.at.least(budget([1_000_000n,100_000n,60_000n]));
    await (await plan.send()).wait();
    for(const [i,id] of open.entries()){const r=await rng.getRequest(id);expect(r.fulfilled).to.equal(true);expect(r.delivered).to.equal(true);expect(r.randomness).to.equal(proofOutput(openProofs[i]));}
    expect(await rng.verifyRequestProof(served,servedProof)).to.equal(proofOutput(servedProof));
    // Credits, fees, the overpaid request's expiry refund and the two-step ownership transfer all still work.
    await expect(refundTo.withdraw(address,c.stranger.address)).to.emit(rng,"RefundCreditWithdrawn").withArgs(await refundTo.getAddress(),c.stranger.address,FEE);
    await expect(rng.connect(c.user).withdrawRefundCredit(c.user.address)).to.emit(rng,"RefundCreditWithdrawn").withArgs(c.user.address,c.user.address,7n);
    await networkHelpers.time.increase(61);
    await expect(rng.refundRequest(overpaid,{gasLimit:500_000})).to.emit(rng,"RequestRefundedTo").withArgs(overpaid,c.user.address,FEE,true);
    await expect(rng.withdrawFees(c.owner.address)).to.emit(rng,"FeesWithdrawn");
    await rng.connect(c.user).acceptOwnership();expect(await rng.owner()).to.equal(c.user.address);
    expect(await ethers.provider.getBalance(address)).to.equal(0n);
  });
});
