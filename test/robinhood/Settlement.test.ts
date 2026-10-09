import {expect} from "chai";
import {network} from "hardhat";
import {proofOutput} from "../helpers/proof.ts";
import {ROUND_LEAD,assignedRound,deployRoundCoordinator,eventsOf,nextFee,outcome,proveRequest,request,roundTime,serve,signRound,solvency} from "../helpers/robinhood.ts";

const {ethers,networkHelpers}=await network.create();

async function fixture(){
  const c=await deployRoundCoordinator(ethers,networkHelpers);
  const hostile=await ethers.deployContract("HostileRoundConsumer",[await c.coordinator.getAddress()]);
  const payee=await ethers.deployContract("RoundPayee",[await c.coordinator.getAddress()]);
  return {...c,hostile,payee};
}
type Fixture=Awaited<ReturnType<typeof fixture>>;
const REENTRANT=ethers.id("ReentrancyGuardReentrantCall()").slice(0,10);
/// A raw request from the hostile consumer at its exact fee: its id and fee.
async function hostileRequest(c:Fixture,options:{gas?:number;refundTo?:string;at?:bigint}={}){
  const gas=options.gas??100_000;
  if(options.at!==undefined)await networkHelpers.time.setNextBlockTimestamp(options.at);
  const fee=await nextFee(c,networkHelpers,gas);
  await (await c.hostile.request(ethers.id(`hostile ${await c.coordinator.nextRequestId()}`),gas,options.refundTo??c.extra.address,{value:fee})).wait();
  return {id:await c.hostile.lastRequestId() as bigint,fee};
}
/// The hostile consumer's Reentry events of a receipt: the selector it called and the error it got.
const reentries=(c:Fixture,receipt:any)=>eventsOf(c.hostile,receipt,"Reentry").map((e:any)=>[e.args.called,e.args.success,e.args.error]);
const selector=(c:Fixture,name:string)=>c.coordinator.interface.getFunction(name)!.selector;

describe("Round coordinator: refunds",function(){
  it("refunds only after the deadline, to the fixed refund address, at the ratio the request escrowed under",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    for(const bps of [4999,10_001])await expect(c.coordinator.setRefundBps(bps),`${bps}`).to.be.revertedWithCustomError(c.coordinator,"InvalidConfig");
    await expect(c.coordinator.connect(c.stranger).setRefundBps(5000)).to.be.revertedWithCustomError(c.coordinator,"OwnableUnauthorizedAccount");
    expect(await c.coordinator.MIN_REFUND_BPS()).to.equal(5000n);
    // An odd fee, so that a partial refund is rounded down.
    const fee=25_000_000_000_007n;
    await c.coordinator.setPricing(fee,0,360_000);
    const full=await hostileRequest(c);
    await expect(c.coordinator.setRefundBps(5000)).to.emit(c.coordinator,"RefundBpsChanged").withArgs(10_000,5000);
    const half=await hostileRequest(c);
    await c.coordinator.setRefundBps(7777);
    const odd=await hostileRequest(c);
    // The ratio a request escrowed under is its own: a later change does not reach it.
    await c.coordinator.setRefundBps(10_000);
    const ratios=[[full,10_000n],[half,5000n],[odd,7777n]] as const;
    for(const [r,bps] of ratios)expect(await c.coordinator.requestRefundBps(r.id)).to.equal(bps);
    // Not before the deadline, not at it (mined at that very second), and from the second after it by anyone.
    const deadline=(await c.coordinator.getRoundRequest(odd.id)).deadline;
    await expect(c.coordinator.refundRequest(odd.id)).to.be.revertedWithCustomError(c.coordinator,"RefundNotAvailable");
    await networkHelpers.time.setNextBlockTimestamp(deadline);
    await expect(c.coordinator.connect(c.stranger).refundRequest(odd.id,{gasLimit:600_000})).to.be.revertedWithCustomError(c.coordinator,"RefundNotAvailable");
    let earned=0n;
    for(const [r,bps] of ratios){
      const amount=fee*bps/10_000n,before=await ethers.provider.getBalance(c.extra.address);
      await expect(c.coordinator.connect(c.stranger).refundRequest(r.id)).to.emit(c.coordinator,"RequestRefundedTo").withArgs(r.id,c.extra.address,amount,true);
      expect(await ethers.provider.getBalance(c.extra.address)-before).to.equal(amount);
      earned+=fee-amount;
      expect(await c.coordinator.earnedFees(),`bps ${bps}`).to.equal(earned);
      await expect(c.coordinator.refundRequest(r.id)).to.be.revertedWithCustomError(c.coordinator,"RefundNotAvailable");
    }
    await expect(c.coordinator.refundRequest(99)).to.be.revertedWithCustomError(c.coordinator,"UnknownRequest");
    const s=await solvency(c,ethers);
    expect([s.balance,s.owed]).to.deep.equal([earned,earned]);
  });

  it("serves a request at its deadline, refuses it after, and never refunds a fulfilled one",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const atDeadline=await request(c,networkHelpers),late=await request(c,networkHelpers);
    const signature=signRound(atDeadline.request.round);
    const proof=await proveRequest(c,ethers,atDeadline.id,signature);
    await networkHelpers.time.setNextBlockTimestamp(atDeadline.request.deadline);
    await (await c.coordinator.fulfillRandomness(atDeadline.id,proof,signature,{gasLimit:2_000_000})).wait();
    expect((await c.coordinator.getRoundRequest(atDeadline.id)).fulfilled).to.equal(true);
    // The second request's deadline is one block later: one second after it, before anyone asks for a refund, it can no longer be served.
    await networkHelpers.time.setNextBlockTimestamp(late.request.deadline+1n);
    const lateSignature=signRound(late.request.round);
    await expect(c.coordinator.fulfillRandomness(late.id,await proveRequest(c,ethers,late.id,lateSignature),lateSignature,{gasLimit:2_000_000}))
      .to.be.revertedWithCustomError(c.coordinator,"RequestExpired");
    await networkHelpers.time.increase(100);
    await expect(c.coordinator.refundRequest(atDeadline.id)).to.be.revertedWithCustomError(c.coordinator,"RefundNotAvailable");
    await c.coordinator.refundRequest(late.id);
    await expect(c.coordinator.fulfillRandomness(late.id,await proveRequest(c,ethers,late.id,lateSignature),lateSignature))
      .to.be.revertedWithCustomError(c.coordinator,"RequestRefunded");
  });

  it("turns a refund whose transfer fails into credit for the refund address, and a payment hook cannot request in the refund",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    let credit=0n;
    // 1 refuses the payment, 2 spends the whole 30,000-gas payment call, 3 tries to request randomness from its receive hook.
    for(const mode of [1,2,3]){
      await c.payee.setMode(mode);
      const r=await hostileRequest(c,{refundTo:await c.payee.getAddress()});
      await networkHelpers.time.increase(61);
      const receipt=await (await c.coordinator.connect(c.stranger).refundRequest(r.id)).wait();
      await expect(receipt,`mode ${mode}`).to.emit(c.coordinator,"RequestRefundedTo").withArgs(r.id,c.payee.target,r.fee,mode===3);
      if(mode===3){
        expect(eventsOf(c.payee,receipt,"Reentry").map((e:any)=>[e.args.success,e.args.error])).to.deep.equal([[false,REENTRANT]]);
        expect(await c.coordinator.nextRequestId()).to.equal(r.id+1n);
      }else credit+=r.fee;
      expect(await c.coordinator.refundCredits(c.payee.target)).to.equal(credit);
      // The notification goes to the consumer whatever happened to the payment.
      await expect(receipt).to.emit(c.coordinator,"RefundCallbackAttempted").withArgs(r.id,c.hostile.target,true,100_000);
    }
    let s=await solvency(c,ethers);
    expect(s.balance).to.equal(s.owed);
    await c.payee.setMode(0);
    await expect(c.payee.execute(c.coordinator.target,c.coordinator.interface.encodeFunctionData("withdrawRefundCredit",[c.payee.target])))
      .to.emit(c.coordinator,"RefundCreditWithdrawn").withArgs(c.payee.target,c.payee.target,credit);
    // Every refund was whole, so nothing is owed any more.
    s=await solvency(c,ethers);
    expect([s.balance,s.owed]).to.deep.equal([0n,0n]);
  });

  it("refuses a refund without the gas for its notification, and changes nothing",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const r=await hostileRequest(c);
    await networkHelpers.time.increase(61);
    for(const gasLimit of [150_000,250_000,265_000])
      expect(await outcome(c.coordinator,()=>c.coordinator.refundRequest.staticCall(r.id,{gasLimit})),`${gasLimit}`).to.equal("InsufficientCallbackGas");
    await expect(c.coordinator.refundRequest(r.id,{gasLimit:265_000})).to.be.revertedWithCustomError(c.coordinator,"InsufficientCallbackGas");
    expect((await c.coordinator.getRoundRequest(r.id)).refunded).to.equal(false);
    await expect(c.coordinator.refundRequest(r.id)).to.emit(c.coordinator,"RefundCallbackAttempted").withArgs(r.id,c.hostile.target,true,100_000);
  });
});

describe("Round coordinator: refund notifications",function(){
  it("notifies the consumer once the refund has settled, with 100,000 gas and the request id",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const r=await hostileRequest(c);
    await networkHelpers.time.increase(61);
    const receipt=await (await c.coordinator.refundRequest(r.id)).wait();
    expect(eventsOf(c.coordinator,receipt).map((e:any)=>e.name)).to.deep.equal(["RequestRefundedTo","RefundCallbackAttempted"]);
    await expect(receipt).to.emit(c.coordinator,"RefundCallbackAttempted").withArgs(r.id,c.hostile.target,true,100_000);
    expect([await c.hostile.refundNotices(r.id),await c.hostile.refundedWhenNotified(r.id),await c.coordinator.refundCallbackDelivered(r.id)])
      .to.deep.equal([1n,true,true]);
    await expect(c.coordinator.retryRefundCallback(r.id,100_000)).to.be.revertedWithCustomError(c.coordinator,"RefundCallbackAlreadyDelivered");
  });

  it("isolates a notification that reverts, burns its gas or returns a revert bomb, and retries only the notification",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const open=await hostileRequest(c);
    await expect(c.coordinator.retryRefundCallback(open.id,100_000)).to.be.revertedWithCustomError(c.coordinator,"NotRefunded");
    await expect(c.coordinator.retryRefundCallback(99,100_000)).to.be.revertedWithCustomError(c.coordinator,"UnknownRequest");
    for(const mode of [1,2,3]){
      await c.hostile.setRefundMode(mode);
      const r=await hostileRequest(c);
      await networkHelpers.time.increase(61);
      const before=await ethers.provider.getBalance(c.extra.address);
      const receipt=await (await c.coordinator.refundRequest(r.id)).wait();
      // The refund is paid; only the notification failed.
      await expect(receipt,`mode ${mode}`).to.emit(c.coordinator,"RequestRefundedTo").withArgs(r.id,c.extra.address,r.fee,true);
      await expect(receipt,`mode ${mode}`).to.emit(c.coordinator,"RefundCallbackAttempted").withArgs(r.id,c.hostile.target,false,100_000);
      expect(await ethers.provider.getBalance(c.extra.address)-before).to.equal(r.fee);
      expect([(await c.coordinator.getRoundRequest(r.id)).refunded,await c.coordinator.refundCallbackDelivered(r.id)]).to.deep.equal([true,false]);
      for(const gas of [99_999,1_000_001])
        await expect(c.coordinator.retryRefundCallback(r.id,gas),`${gas}`).to.be.revertedWithCustomError(c.coordinator,"InvalidCallbackGas");
      // Still failing: the retry records it again, and nothing is paid twice.
      const again=await (await c.coordinator.retryRefundCallback(r.id,300_000)).wait();
      await expect(again).to.emit(c.coordinator,"RefundCallbackAttempted").withArgs(r.id,c.hostile.target,false,300_000);
      // Fixed: anyone retries it, and it is delivered once.
      await c.hostile.setRefundMode(0);
      const balance=await ethers.provider.getBalance(c.coordinator.target);
      const retried=await (await c.coordinator.connect(c.stranger).retryRefundCallback(r.id,100_000)).wait();
      expect(eventsOf(c.coordinator,retried).map((e:any)=>[e.name,...e.args])).to.deep.equal([["RefundCallbackAttempted",r.id,c.hostile.target,true,100_000n]]);
      expect(await ethers.provider.getBalance(c.coordinator.target)).to.equal(balance);
      expect([await c.hostile.refundNotices(r.id),await c.coordinator.refundCallbackDelivered(r.id)]).to.deep.equal([1n,true]);
      await expect(c.coordinator.retryRefundCallback(r.id,100_000)).to.be.revertedWithCustomError(c.coordinator,"RefundCallbackAlreadyDelivered");
    }
  });
});

describe("Round coordinator: callback retries",function(){
  it("keeps the proof and the fee when the callback fails, and retries the same result with at least the original gas, even after the deadline",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const open=await hostileRequest(c);
    await expect(c.coordinator.retryCallback(open.id,200_000)).to.be.revertedWithCustomError(c.coordinator,"NotFulfilled");
    await expect(c.coordinator.retryCallback(99,200_000)).to.be.revertedWithCustomError(c.coordinator,"UnknownRequest");
    // 1 reverts, 2 burns its whole budget, 3 reverts with a megabyte of data.
    for(const mode of [1,2,3]){
      await c.hostile.setCallbackMode(mode);
      const r=await hostileRequest(c,{gas:200_000});
      const {proof,receipt}=await serve(c,ethers,r.id);
      await expect(receipt,`mode ${mode}`).to.emit(c.coordinator,"CallbackAttempted").withArgs(r.id,false,200_000);
      await expect(receipt).to.emit(c.coordinator,"KeeperFeePaid").withArgs(r.id,c.keeper.address,r.fee*8000n/10_000n,true);
      const stored=await c.coordinator.getRoundRequest(r.id);
      expect([stored.fulfilled,stored.delivered,stored.randomness]).to.deep.equal([true,false,proofOutput(proof)]);
      expect(await c.hostile.deliveries(r.id)).to.equal(0n);
      // A fulfilled request is never refunded, whatever happened to its callback.
      await networkHelpers.time.increase(61);
      await expect(c.coordinator.refundRequest(r.id)).to.be.revertedWithCustomError(c.coordinator,"RefundNotAvailable");
      for(const gas of [29_999,199_999,1_000_001])
        await expect(c.coordinator.retryCallback(r.id,gas),`${gas}`).to.be.revertedWithCustomError(c.coordinator,"InvalidCallbackGas");
      // Too little gas for the callback's whole budget: refused before the call, nothing recorded.
      await expect(c.coordinator.retryCallback(r.id,200_000,{gasLimit:250_000})).to.be.revertedWithCustomError(c.coordinator,"InsufficientCallbackGas");
      // More gas while it still fails: recorded again, the result unchanged.
      await expect(c.coordinator.retryCallback(r.id,1_000_000)).to.emit(c.coordinator,"CallbackAttempted").withArgs(r.id,false,1_000_000);
      await c.hostile.setCallbackMode(0);
      const retried=await (await c.coordinator.connect(c.stranger).retryCallback(r.id,200_000)).wait();
      await expect(retried).to.emit(c.coordinator,"CallbackAttempted").withArgs(r.id,true,200_000);
      expect(await c.hostile.results(r.id)).to.equal(proofOutput(proof));
      expect((await c.coordinator.getRoundRequest(r.id)).randomness).to.equal(stored.randomness);
      await expect(c.coordinator.retryCallback(r.id,200_000)).to.be.revertedWithCustomError(c.coordinator,"AlreadyDelivered");
      expect(await c.hostile.deliveries(r.id)).to.equal(1n);
    }
    const s=await solvency(c,ethers);
    expect(s.balance).to.equal(s.owed);
  });

  it("cannot be made to record a callback failure for want of fulfilment gas: a short gas limit reverts InsufficientCallbackGas",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    // Two requests of one round, two and one seconds before its scheduled time less ROUND_LEAD: the first verifies the round, so the
    // second, whose callback burns its whole 500,000-gas budget, is served from the cache with only the callback guard.
    const round=assignedRound(c.beacon,BigInt(await networkHelpers.time.latest())+30n)+1n,time=roundTime(c.beacon,round)-ROUND_LEAD;
    await c.hostile.setCallbackMode(2);
    const first=await request(c,networkHelpers,{at:time-2n});
    const burner=await hostileRequest(c,{gas:500_000,at:time-1n});
    const q=await c.coordinator.getRoundRequest(burner.id);
    expect([first.request.round,q.round]).to.deep.equal([round,round]);
    await serve(c,ethers,first.id);
    const proof=await proveRequest(c,ethers,burner.id,signRound(q.round));
    const estimate=Number(await c.coordinator.fulfillRandomness.estimateGas(burner.id,proof,"0x"));
    for(const gasLimit of [estimate-1_000,estimate-50_000,estimate-200_000])
      expect(await outcome(c.coordinator,()=>c.coordinator.fulfillRandomness.staticCall(burner.id,proof,"0x",{gasLimit})),`${gasLimit}`).to.equal("InsufficientCallbackGas");
    await (await c.coordinator.fulfillRandomness(burner.id,proof,"0x",{gasLimit:estimate})).wait();
    const served=await c.coordinator.getRoundRequest(burner.id);
    expect([served.fulfilled,served.delivered]).to.deep.equal([true,false]);
  });
});

describe("Round coordinator: reentrancy",function(){
  it("blocks a consumer callback from entering fulfillRandomness, refundRequest, retryCallback or a new request",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    await c.hostile.setCallbackMode(4);
    const r=await hostileRequest(c,{gas:500_000});
    const {proof,receipt}=await serve(c,ethers,r.id);
    expect(reentries(c,receipt)).to.deep.equal([
      [selector(c,"fulfillRandomness"),false,REENTRANT],[selector(c,"refundRequest"),false,REENTRANT],
      [selector(c,"retryCallback"),false,REENTRANT],[selector(c,"requestRandomness"),false,REENTRANT]]);
    await expect(receipt).to.emit(c.coordinator,"CallbackAttempted").withArgs(r.id,true,500_000);
    expect(await c.hostile.results(r.id)).to.equal(proofOutput(proof));
    expect(await c.coordinator.nextRequestId()).to.equal(r.id+1n);
    // The same from a callback retry.
    const failing=await hostileRequest(c,{gas:500_000});
    await c.hostile.setCallbackMode(1);
    await serve(c,ethers,failing.id);
    await c.hostile.setCallbackMode(4);
    const retried=await (await c.coordinator.retryCallback(failing.id,500_000)).wait();
    expect(reentries(c,retried).map(([,success,error])=>[success,error])).to.deep.equal(Array(4).fill([false,REENTRANT]));
    expect((await c.coordinator.getRoundRequest(failing.id)).delivered).to.equal(true);
  });

  it("blocks a refund notification from entering refundRequest, withdrawRefundCredit, retryRefundCallback or a new request",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    await c.hostile.setRefundMode(4);
    const r=await hostileRequest(c);
    await networkHelpers.time.increase(61);
    const receipt=await (await c.coordinator.refundRequest(r.id)).wait();
    const notified=eventsOf(c.coordinator,receipt,"RefundCallbackAttempted")[0].args;
    // The notification's 100,000 gas may not cover four attempts and their events; a retry with more gas makes them all.
    const attempts=notified.success?receipt:await (await c.coordinator.retryRefundCallback(r.id,500_000)).wait();
    expect(reentries(c,attempts)).to.deep.equal([
      [selector(c,"refundRequest"),false,REENTRANT],[selector(c,"withdrawRefundCredit"),false,REENTRANT],
      [selector(c,"retryRefundCallback"),false,REENTRANT],[selector(c,"requestRandomness"),false,REENTRANT]]);
    expect([await c.hostile.refundNotices(r.id),await c.hostile.refundedWhenNotified(r.id),await c.coordinator.nextRequestId()]).to.deep.equal([1n,true,r.id+1n]);
    console.log(`Refund notification with four reentry attempts: ${notified.success?"fits":"does not fit"} in REFUND_CALLBACK_GAS`);
  });
});

describe("Round coordinator: the pending-request scan",function(){
  it("lists open requests in id order, never skipping an older one, and pages by the number of ids scanned",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    for(const [from,limit] of [[0,10],[1,0],[1,257]])
      await expect(c.coordinator.getPendingRequestIds(from,limit),`${from} ${limit}`).to.be.revertedWithCustomError(c.coordinator,"InvalidScan");
    const scan=async(from:number,limit:number)=>{
      const [ids,cursor]=await c.coordinator.getPendingRequestIds(from,limit,{blockTag:"latest"});
      return [Array.from(ids as bigint[]),cursor as bigint];
    };
    const made=[];
    for(let i=0;i<5;i++)made.push(await request(c,networkHelpers));
    // Served out of order: 2 and 4.
    await serve(c,ethers,made[3].id);await serve(c,ethers,made[1].id);
    expect(await scan(1,256)).to.deep.equal([[1n,3n,5n],6n]);
    expect(await scan(1,2)).to.deep.equal([[1n],3n]);
    expect(await scan(3,2)).to.deep.equal([[3n],5n]);
    expect(await scan(5,2)).to.deep.equal([[5n],6n]);
    expect(await scan(6,10)).to.deep.equal([[],6n]);
    expect(await scan(100,10)).to.deep.equal([[],6n]);
    // A request is open up to its deadline itself, and leaves the list the second after.
    const deadline=made[0].request.deadline;
    await networkHelpers.time.increaseTo(deadline);
    expect((await scan(1,256))[0]).to.include(1n);
    await networkHelpers.time.increaseTo(deadline+1n);
    expect((await scan(1,256))[0]).not.to.include(1n);
    // Expired and refunded requests are out; a new one is in.
    await networkHelpers.time.increase(120);
    await c.coordinator.refundRequest(made[2].id);
    expect(await scan(1,256)).to.deep.equal([[],6n]);
    const fresh=await request(c,networkHelpers);
    expect(await scan(1,256)).to.deep.equal([[fresh.id],7n]);
  });
});
