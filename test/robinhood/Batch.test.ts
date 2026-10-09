import {expect} from "chai";
import {network} from "hardhat";
import {proofOutput} from "../helpers/proof.ts";
import {ROUND_LEAD,assignedRound,deployRoundCoordinator,eventsOf,nextFee,outcome,proveRequest,randomnessOf,request,roundTime,serve,signRound,
  solvency,type RoundFixture} from "../helpers/robinhood.ts";

const {ethers,networkHelpers}=await network.create();

async function fixture(){
  const c=await deployRoundCoordinator(ethers,networkHelpers);
  const hostile=await ethers.deployContract("HostileRoundConsumer",[await c.coordinator.getAddress()]);
  return {...c,hostile};
}
type Fixture=Awaited<ReturnType<typeof fixture>>;
const MEMBER_OVERHEAD=400_000n,CALLBACK_RESERVE=140_000n;
/// ROUND_VERIFY_GAS + ROUND_VERIFY_GAS / 63 + 5,000: the budget of one round still to verify.
const ROUND_BUDGET=400_000n+400_000n/63n+5_000n;
const memberGas=(limit:bigint)=>limit+limit/63n+MEMBER_OVERHEAD;
/// A round ahead of the chain's clock, and the timestamp ROUND_LEAD seconds before its scheduled time: the last at which a request binds it.
async function aheadRound(c:RoundFixture,ahead=30n){
  const round=assignedRound(c.beacon,BigInt(await networkHelpers.time.latest())+ahead)+1n;
  return {round,time:roundTime(c.beacon,round)-ROUND_LEAD};
}
/// n raw requests of the RoundConsumer in one transaction, so in one block and one round: their ids.
async function requestMany(c:RoundFixture,n:number,gas=100_000,at?:bigint){
  if(at!==undefined)await networkHelpers.time.setNextBlockTimestamp(at);
  const fee=await nextFee(c,networkHelpers,gas);
  await (await c.consumer.requestMany(n,gas,c.owner.address,{value:fee*BigInt(n)})).wait();
  const last=await c.consumer.lastRequestId() as bigint;
  return Array.from({length:n},(_,i)=>last-BigInt(n-1-i));
}
/// A request of the hostile consumer with a callback gas limit, at a timestamp.
async function hostileAt(c:Fixture,gas:number,at:bigint){
  await networkHelpers.time.setNextBlockTimestamp(at);
  const fee=await nextFee(c,networkHelpers,gas);
  await (await c.hostile.request(ethers.id(`hostile ${at}`),gas,c.owner.address,{value:fee})).wait();
  return await c.hostile.lastRequestId() as bigint;
}
const roundOf=async(c:RoundFixture,id:bigint)=>(await c.coordinator.getRoundRequest(id)).round as bigint;
const calldataGas=(data:string)=>BigInt(ethers.getBytes(data).reduce((gas,byte)=>gas+(byte===0?4:16),0));
const TOO_LOW="InsufficientCallbackGas";
/// The smallest gas limit at which a batch stops answering InsufficientCallbackGas.
async function threshold(c:RoundFixture,rounds:unknown[],ids:bigint[],proofs:unknown[]){
  const at=(gasLimit:number)=>outcome(c.coordinator,()=>c.coordinator.fulfillRandomnessBatch.staticCall(rounds,ids,proofs,{gasLimit}));
  let low=200_000,high=16_000_000;
  expect(await at(low)).to.equal(TOO_LOW);
  expect(await at(high)).to.match(/^returns/);
  while(high-low>1){const middle=Math.floor((low+high)/2);if(await at(middle)===TOO_LOW)low=middle;else high=middle;}
  // Above the threshold the batch goes through; it never runs out of gas once the guard lets it start.
  for(const extra of [0,1,1_000,100_000])expect(await at(high+extra),`threshold + ${extra}`).to.match(/^returns/);
  return {gas:BigInt(high),calldata:calldataGas(c.coordinator.interface.encodeFunctionData("fulfillRandomnessBatch",[rounds,ids,proofs]))};
}

describe("Round coordinator: batches",function(){
  it("serves a batch of one round, verifying the round once before its first member, with each member's own events",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    // Five requests in one block, and two more in the next block, all of one round.
    const ahead=await aheadRound(c);
    const ids=await requestMany(c,5,100_000,ahead.time-1n),more=await requestMany(c,2,100_000,ahead.time);
    const round=ahead.round,signature=signRound(round);
    for(const id of [...ids,...more])expect(await roundOf(c,id)).to.equal(round);
    const proofs=await Promise.all(ids.map(id=>proveRequest(c,ethers,id,signature)));
    const receipt=await (await c.coordinator.connect(c.keeper).fulfillRandomnessBatch([[0,round,signature]],ids,proofs)).wait();
    const member=["RequestServed","ProofVerified","RandomnessFulfilled","FulfillmentEvidence","CallbackAttempted","KeeperFeePaid"];
    expect(eventsOf(c.coordinator,receipt).map((e:any)=>e.name)).to.deep.equal(["RoundVerified",...ids.flatMap(()=>member)]);
    expect(eventsOf(c.coordinator,receipt,"RequestServed").map((e:any)=>[e.args.requestId,e.args.serveIndex])).to.deep.equal(ids.map((id,i)=>[id,BigInt(i+1)]));
    for(const [i,id] of ids.entries()){
      const q=await c.coordinator.getRoundRequest(id);
      expect([q.fulfilled,q.delivered,q.randomness,q.roundRandomness]).to.deep.equal([true,true,proofOutput(proofs[i]),randomnessOf(signature)]);
      expect(await c.consumer.results(id)).to.equal(proofOutput(proofs[i]));
    }
    // Members of a round verified on chain need no listed signature, and a listed one is not verified again.
    const moreProofs=await Promise.all(more.map(id=>proveRequest(c,ethers,id,signature)));
    const unlisted=await (await c.coordinator.fulfillRandomnessBatch([],more.slice(0,1),moreProofs.slice(0,1))).wait();
    const listed=await (await c.coordinator.fulfillRandomnessBatch([[0,round,signature]],more.slice(1),moreProofs.slice(1))).wait();
    for(const r of [unlisted,listed]){
      expect(eventsOf(c.coordinator,r,"RoundVerified")).to.have.length(0);
      expect(eventsOf(c.coordinator,r,"RandomnessFulfilled")).to.have.length(1);
    }
    const s=await solvency(c,ethers);
    expect(s.balance).to.equal(s.owed);
  });

  it("serves sixteen members, and refuses an empty, oversized or mismatched batch and an unknown member",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    expect(await c.coordinator.MAX_FULFILL_BATCH()).to.equal(16n);
    const ids=await requestMany(c,16);
    const round=await roundOf(c,ids[0]),signature=signRound(round),listed=[[0,round,signature]];
    const proofs=await Promise.all(ids.map(id=>proveRequest(c,ethers,id,signature)));
    const shapes:Array<[string,unknown[],bigint[],unknown[]]>=[
      ["no member",listed,[],[]],["no member and no round",[],[],[]],["seventeen members",listed,[...ids,ids[0]],[...proofs,proofs[0]]],
      ["fewer proofs than members",listed,ids,proofs.slice(1)],["more proofs than members",listed,ids.slice(1),proofs],
      ["more rounds than members",[...listed,...listed],ids.slice(0,1),proofs.slice(0,1)],
    ];
    for(const [name,rounds,members,memberProofs] of shapes)
      await expect(c.coordinator.fulfillRandomnessBatch(rounds,members,memberProofs),name).to.be.revertedWithCustomError(c.coordinator,"InvalidBatch");
    await expect(c.coordinator.fulfillRandomnessBatch(listed,[ids[0],99n],[proofs[0],proofs[1]])).to.be.revertedWithCustomError(c.coordinator,"UnknownRequest");
    const receipt=await (await c.coordinator.connect(c.keeper).fulfillRandomnessBatch(listed,ids,proofs)).wait();
    expect(eventsOf(c.coordinator,receipt,"RandomnessFulfilled")).to.have.length(16);
    expect(eventsOf(c.coordinator,receipt,"RoundVerified")).to.have.length(1);
    console.log(`Batch of 16 members in one new round (RoundConsumer storing each result): gas ${receipt.gasUsed}`);
  });

  it("skips members already fulfilled (1), refunded (2) or past their deadline (3), and serves the rest",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const fulfilled=await request(c,networkHelpers),refunded=await request(c,networkHelpers),expired=await request(c,networkHelpers);
    const first=await serve(c,ethers,fulfilled.id);
    await networkHelpers.time.increase(70);
    await c.coordinator.refundRequest(refunded.id);
    const open=await request(c,networkHelpers);
    const signature=signRound(open.request.round),proof=await proveRequest(c,ethers,open.id,signature);
    const ids=[fulfilled.id,refunded.id,expired.id,open.id];
    // Only the served member's round is listed: a skipped member needs neither a listed round nor a valid proof.
    const receipt=await (await c.coordinator.fulfillRandomnessBatch([[0,open.request.round,signature]],ids,[proof,proof,proof,proof])).wait();
    expect(eventsOf(c.coordinator,receipt,"FulfillmentSkipped").map((e:any)=>[e.args.requestId,e.args.reason])).to.deep.equal([
      [fulfilled.id,1n],[refunded.id,2n],[expired.id,3n]]);
    expect(eventsOf(c.coordinator,receipt,"RandomnessFulfilled").map((e:any)=>e.args.requestId)).to.deep.equal([open.id]);
    // The skipped members are as they were: the fulfilled one keeps its result, the expired one is still refundable.
    expect((await c.coordinator.getRoundRequest(fulfilled.id)).randomness).to.equal(proofOutput(first.proof));
    expect((await c.coordinator.getRoundRequest(expired.id)).fulfilled).to.equal(false);
    await expect(c.coordinator.refundRequest(expired.id)).to.emit(c.coordinator,"RequestRefundedTo");
    // A batch whose every member is skipped changes nothing but its events.
    const all=await (await c.coordinator.fulfillRandomnessBatch([],[fulfilled.id,open.id],[proof,proof])).wait();
    expect(eventsOf(c.coordinator,all).map((e:any)=>[e.name,e.args.reason])).to.deep.equal([["FulfillmentSkipped",1n],["FulfillmentSkipped",1n]]);
  });

  it("reverts the whole batch on one bad member, revealing nothing",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const ids=await requestMany(c,3);
    const round=await roundOf(c,ids[0]),signature=signRound(round);
    const proofs=await Promise.all(ids.map(id=>proveRequest(c,ethers,id,signature)));
    for(const [name,bad,error] of [["a proof of another member",proofs[0],"WrongSeed"],["another key's proof",{...proofs[2],pk:[1n,2n]},"WrongPublicKey"]] as const)
      await expect(c.coordinator.fulfillRandomnessBatch([[0,round,signature]],ids,[proofs[0],proofs[1],bad]),name).to.be.revertedWithCustomError(c.coordinator,error);
    for(const id of ids)expect((await c.coordinator.getRoundRequest(id)).fulfilled).to.equal(false);
    expect(await c.coordinator.roundRandomness(0,round)).to.equal(ethers.ZeroHash);
  });
});

describe("Round coordinator: the batch gas guard",function(){
  it("budgets every served member's callback and every round still to verify before anything runs, and nothing for skipped members",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const {round,time}=await aheadRound(c,120n);
    // x: a 1,000,000-gas request of an earlier round that has expired by the time of the batch. y and a: requests of round R with
    // 1,000,000 and 100,000 gas callbacks. b: a request of round R + 1.
    const x=await hostileAt(c,1_000_000,time-100n);
    const y=await hostileAt(c,1_000_000,time-1n);
    const a=await hostileAt(c,100_000,time);
    const b=await hostileAt(c,100_000,time+1n);
    expect([await roundOf(c,y),await roundOf(c,a),await roundOf(c,b)]).to.deep.equal([round,round,round+1n]);
    const [sR,sR1]=[signRound(round),signRound(round+1n)];
    const [pa,py,pb]=[await proveRequest(c,ethers,a,sR),await proveRequest(c,ethers,y,sR),await proveRequest(c,ethers,b,sR1)];
    const R=[0,round,sR],R1=[0,round+1n,sR1];
    const only=await threshold(c,[R],[a],[pa]);
    const withY=await threshold(c,[R],[a,y],[pa,py]);
    const withX=await threshold(c,[R],[a,x],[pa,pa]);
    const withB=await threshold(c,[R,R1],[a,b],[pa,pb]);
    // Each threshold less the gas of its own calldata, against the guard's arithmetic. The guard reads gasleft() in the implementation, which
    // the proxy's DELEGATECALL reaches with at most 63/64 of its own gas (EIP-150): every unit the guard budgets costs 64/63 of a unit of
    // the transaction's gas limit. What remains is what the implementation spends before its check on the longer calldata.
    const viaProxy=(gas:bigint)=>gas*64n/63n;
    const delta=(t:{gas:bigint;calldata:bigint})=>(t.gas-t.calldata)-(only.gas-only.calldata);
    const beyond=[delta(withY)-viaProxy(memberGas(1_000_000n)),delta(withB)-viaProxy(memberGas(100_000n)+ROUND_BUDGET),delta(withX)];
    console.log(`Batch guard thresholds less calldata: a alone ${only.gas-only.calldata}; added beyond 64/63 of the guard's budget: `+
      `a member of the same round ${beyond[0]}, of the next round ${beyond[1]}, a skipped member ${beyond[2]}`);
    for(const extra of beyond)expect(extra).to.be.within(0n,1_000n);
    // The same batch once round R is verified on chain: its budget is gone from the threshold, and nothing else.
    await serve(c,ethers,y);
    const cachedOnly=await threshold(c,[R],[a],[pa]);
    expect(cachedOnly.calldata).to.equal(only.calldata);
    expect(only.gas-cachedOnly.gas).to.be.within(viaProxy(ROUND_BUDGET)-500n,viaProxy(ROUND_BUDGET)+500n);
    // The guard binds: a lone member of a verified round needs CALLBACK_RESERVE and its member budget in the implementation, and the
    // threshold leaves the implementation little more than that.
    const inImplementation=(cachedOnly.gas-cachedOnly.calldata-21_000n)*63n/64n;
    expect(inImplementation-CALLBACK_RESERVE-memberGas(100_000n)).to.be.within(0n,20_000n);
  });

  it("lets no callback, however much of its budget it burns, starve a later member: served at the smallest gas limit, or refused whole",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const {round,time}=await aheadRound(c);
    await c.hostile.setCallbackMode(2);
    const burner=await hostileAt(c,1_000_000,time-1n);
    await networkHelpers.time.setNextBlockTimestamp(time);
    const fee=await nextFee(c,networkHelpers,100_000);
    await (await c.consumer.request(ethers.id("honest"),100_000,c.owner.address,{value:fee})).wait();
    const honest=await c.consumer.lastRequestId();
    expect([await roundOf(c,burner),await roundOf(c,honest)]).to.deep.equal([round,round]);
    const signature=signRound(round),ids=[burner,honest];
    const proofs=[await proveRequest(c,ethers,burner,signature),await proveRequest(c,ethers,honest,signature)];
    const rounds=[[0,round,signature]];
    const {gas}=await threshold(c,rounds,ids,proofs);
    expect(await c.coordinator.fulfillRandomnessBatch.estimateGas(rounds,ids,proofs)).to.be.at.least(gas);
    await expect(c.coordinator.fulfillRandomnessBatch(rounds,ids,proofs,{gasLimit:gas-1n})).to.be.revertedWithCustomError(c.coordinator,TOO_LOW);
    for(const id of ids)expect((await c.coordinator.getRoundRequest(id)).fulfilled).to.equal(false);
    const receipt=await (await c.coordinator.fulfillRandomnessBatch(rounds,ids,proofs,{gasLimit:gas})).wait();
    await expect(receipt).to.emit(c.coordinator,"CallbackAttempted").withArgs(burner,false,1_000_000);
    await expect(receipt).to.emit(c.coordinator,"CallbackAttempted").withArgs(honest,true,100_000);
    expect(await c.consumer.results(honest)).to.equal(proofOutput(proofs[1]));
    console.log(`Batch of a burning 1,000,000-gas callback and an honest one: smallest gas limit ${gas}, used ${receipt.gasUsed}`);
  });
});
