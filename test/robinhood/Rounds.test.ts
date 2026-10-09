import {expect} from "chai";
import {network} from "hardhat";
import {AbiCoder,keccak256} from "ethers";
import {bn254} from "@noble/curves/bn254";
import {hashProof} from "../../src/verification.ts";
import {signBeaconRound} from "../helpers/beacon.ts";
import {makeProof,proofOutput} from "../helpers/proof.ts";
import {L2_OFFSET,ROUND_LEAD,TRANSCRIPT_DOMAIN,assignedRound,deployRoundCoordinator,eventsOf,nextFee,proveRequest,randomnessOf,request,roundSeed,
  roundTime,serve,signRound,type RoundFixture} from "../helpers/robinhood.ts";

const {ethers,networkHelpers}=await network.create();
const abi=AbiCoder.defaultAbiCoder();
const fixture=()=>deployRoundCoordinator(ethers,networkHelpers);
/// A round ahead of the chain's clock, and the timestamp ROUND_LEAD seconds before its scheduled time: the last at which a request binds it.
async function aheadRound(c:RoundFixture,ahead=30n){
  const round=assignedRound(c.beacon,BigInt(await networkHelpers.time.latest())+ahead)+1n;
  return {round,time:roundTime(c.beacon,round)-ROUND_LEAD};
}
/// Two requests of round R, at the last two seconds that bind it, and one of round R + 1, at the first second that binds it.
async function twoRounds(c:RoundFixture){
  const {round,time}=await aheadRound(c);
  const a1=await request(c,networkHelpers,{at:time-1n,seed:ethers.id("a1")});
  const a2=await request(c,networkHelpers,{at:time,seed:ethers.id("a2")});
  const b1=await request(c,networkHelpers,{at:time+1n,seed:ethers.id("b1")});
  expect([a1.request.round,a2.request.round,b1.request.round]).to.deep.equal([round,round,round+1n]);
  return {round,a1,a2,b1,sa:signRound(round),sb:signRound(round+1n)};
}
const names=(c:RoundFixture,receipt:any)=>eventsOf(c.coordinator,receipt).map((e:any)=>e.name);
const SECOND_SECRET=BigInt(ethers.id("D20 second round test beacon secret"))%bn254.fields.Fr.ORDER;

describe("Rounds: getRoundRequest",function(){
  it("returns every seed input, the result, the escrowed fee and the status, named and typed, through a request's whole life",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const fragment=c.coordinator.interface.getFunction("getRoundRequest")!;
    expect(fragment.outputs[0].components!.map((p:any)=>`${p.type} ${p.name}`)).to.deep.equal([
      "address consumer","uint32 callbackGasLimit","uint64 requestBlock","uint64 deadline","address refundAddress","bytes32 clientSeed",
      "bytes32 mappingHash","uint8 beaconId","uint64 round","bytes32 roundRandomness","bytes32 randomness","bytes32 proofHash",
      "bytes32 transcriptHash","uint256 feePaid","bool fulfilled","bool delivered","bool refunded"]);
    const {round,a1,a2,sa}=await twoRounds(c);
    const [requested]=eventsOf(c.coordinator,a1.receipt,"RandomnessRequested");
    const block=await ethers.provider.getBlock(a1.receipt.blockNumber);
    const zero=ethers.ZeroHash,consumer=await c.consumer.getAddress();
    // Open, its round not verified yet.
    const open=[consumer,100_000n,BigInt(a1.receipt.blockNumber)+L2_OFFSET,BigInt(block!.timestamp)+60n,c.owner.address,ethers.id("a1"),
      keccak256(abi.encode(["uint8","uint256","uint256","uint32","uint32"],[0,0,0,0,0])),0n,round,zero,zero,zero,zero,a1.fee,false,false,false];
    expect(Array.from(await c.coordinator.getRoundRequest(a1.id))).to.deep.equal(open);
    expect([requested.args.requestBlock,requested.args.deadline,requested.args.feePaid,requested.args.refundAddress,requested.args.clientSeed,
      requested.args.callbackGasLimit]).to.deep.equal([open[2],open[3],open[13],open[4],open[5],open[1]]);
    // Its round verified by another request's fulfilment: the round's randomness shows, the request is still open.
    const {proof:p2}=await serve(c,ethers,a2.id);
    expect(Array.from(await c.coordinator.getRoundRequest(a1.id))).to.deep.equal([...open.slice(0,9),randomnessOf(sa),...open.slice(10)]);
    // Fulfilled and delivered: the result, the hash of its proof and the transcript.
    const {proof}=await serve(c,ethers,a1.id,{signature:"0x"});
    const q=await c.coordinator.getRoundRequest(a1.id),output=proofOutput(proof);
    const transcript=keccak256(abi.encode(["bytes32","uint256","address","uint256","bytes32","uint8","uint64","bytes32","bytes32","bytes32","bytes32"],
      [TRANSCRIPT_DOMAIN,(await ethers.provider.getNetwork()).chainId,c.coordinator.target,a1.id,await c.coordinator.protocolConfigurationHash(),0,round,
        randomnessOf(sa),hashProof(proof),output,open[6]]));
    expect(Array.from(q)).to.deep.equal([...open.slice(0,9),randomnessOf(sa),output,hashProof(proof),transcript,a1.fee,true,true,false]);
    expect((await c.coordinator.getRoundRequest(a2.id)).randomness).to.equal(proofOutput(p2));
    // Refunded: only the flag changes, and the round shows if it is verified.
    const late=await request(c,networkHelpers);
    await networkHelpers.time.increase(61);
    await c.coordinator.refundRequest(late.id);
    const refunded=await c.coordinator.getRoundRequest(late.id);
    expect([refunded.fulfilled,refunded.delivered,refunded.refunded,refunded.randomness,refunded.feePaid]).to.deep.equal([false,false,true,zero,late.fee]);
  });

  it("has no getRequest(uint256): the selector of another coordinator's request view reverts with no data",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const {id}=await request(c,networkHelpers);
    const data=ethers.id("getRequest(uint256)").slice(0,10)+ethers.toBeHex(id,32).slice(2);
    const error:any=await ethers.provider.call({to:c.coordinator.target,data}).then(()=>null,(caught:unknown)=>caught);
    expect(error,"getRequest(uint256) must revert").to.not.equal(null);
    expect(error.data??"0x").to.equal("0x");
  });
});

describe("Rounds: first in round and cached",function(){
  it("verifies a round with its first served request, then serves the round's other requests from the cache with any signature or none",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const {round,a1,a2,b1,sa,sb}=await twoRounds(c);
    const first=await serve(c,ethers,a1.id);
    expect(names(c,first.receipt)[0]).to.equal("RoundVerified");
    await expect(first.receipt).to.emit(c.coordinator,"RoundVerified").withArgs(0,round,randomnessOf(sa),sa);
    // The cached round ignores the signature argument: empty, another round's or garbage.
    const proof=await proveRequest(c,ethers,a2.id,sa);
    for(const ignored of [sb,"0x01"])await c.coordinator.fulfillRandomness.staticCall(a2.id,proof,ignored);
    const cached=await (await c.coordinator.fulfillRandomness(a2.id,proof,"0x")).wait();
    expect(names(c,cached)).not.to.include("RoundVerified");
    expect(cached.gasUsed).to.be.lessThan(first.receipt.gasUsed-150_000n);
    // The next round is not cached: it needs its own signature.
    const pb=await proveRequest(c,ethers,b1.id,sb);
    await expect(c.coordinator.fulfillRandomness(b1.id,pb,"0x")).to.be.revertedWithCustomError(c.coordinator,"InvalidRoundSignature");
    await expect(c.coordinator.fulfillRandomness(b1.id,pb,sa)).to.be.revertedWithCustomError(c.coordinator,"InvalidRoundSignature");
    await (await c.coordinator.fulfillRandomness(b1.id,pb,sb)).wait();
    expect([await c.coordinator.roundRandomness(0,round),await c.coordinator.roundRandomness(0,round+1n)]).to.deep.equal([randomnessOf(sa),randomnessOf(sb)]);
  });

  it("refuses another round's signature, another beacon key's signature of the round, and a malformed one, caching nothing",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const {round,a1,sa}=await twoRounds(c);
    const proof=await proveRequest(c,ethers,a1.id,sa);
    const wrong:Array<[string,string]>=[["the next round's",signRound(round+1n)],["the previous round's",signRound(round-1n)],
      ["round 1's",signRound(1n)],["another key's",signBeaconRound(SECOND_SECRET,round)],["empty","0x"],["63 bytes",sa.slice(0,-2)],
      ["65 bytes",sa+"00"],["x + p",ethers.toBeHex(BigInt(sa.slice(0,66))+bn254.fields.Fp.ORDER,32)+sa.slice(66)]];
    for(const [name,signature] of wrong){
      await expect(c.coordinator.fulfillRandomness(a1.id,proof,signature),name).to.be.revertedWithCustomError(c.coordinator,"InvalidRoundSignature");
      await expect(c.coordinator.getProofContext(a1.id,signature),name).to.be.revertedWithCustomError(c.coordinator,"InvalidRoundSignature");
      await expect(c.coordinator.fulfillRandomnessBatch([[0,round,signature]],[a1.id],[proof]),name).to.be.revertedWithCustomError(c.coordinator,"InvalidRoundSignature");
    }
    expect(await c.coordinator.roundRandomness(0,round)).to.equal(ethers.ZeroHash);
  });
});

describe("Rounds: batches",function(){
  it("verifies each listed round once, just before its first served member, in member order whatever the list order",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const {round,a1,a2,b1,sa,sb}=await twoRounds(c);
    const ids=[b1.id,a1.id,a2.id];
    const proofs=[await proveRequest(c,ethers,b1.id,sb),await proveRequest(c,ethers,a1.id,sa),await proveRequest(c,ethers,a2.id,sa)];
    // Round R listed twice: the first entry is used and the round is verified once.
    const receipt=await (await c.coordinator.fulfillRandomnessBatch([[0,round,sa],[0,round+1n,sb],[0,round,sa]],ids,proofs)).wait();
    const order=names(c,receipt).filter((n:string)=>n==="RoundVerified"||n==="RequestServed");
    expect(order).to.deep.equal(["RoundVerified","RequestServed","RoundVerified","RequestServed","RequestServed"]);
    expect(eventsOf(c.coordinator,receipt,"RoundVerified").map((e:any)=>e.args.round)).to.deep.equal([round+1n,round]);
  });

  it("refuses a served member whose round is neither verified nor listed, also when the list names another beacon or round",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const {round,a1,a2,b1,sa,sb}=await twoRounds(c);
    const [pa,pb]=[await proveRequest(c,ethers,a1.id,sa),await proveRequest(c,ethers,b1.id,sb)];
    for(const [name,rounds] of [["nothing listed",[]],["another round listed",[[0,round+1n,sb]]],["another beacon's round listed",[[1,round,sa]]]] as const)
      await expect(c.coordinator.fulfillRandomnessBatch(rounds,[a1.id],[pa]),name).to.be.revertedWithCustomError(c.coordinator,"RoundUnavailable");
    // One unlisted member reverts the batch before any round is verified, wherever it stands.
    await expect(c.coordinator.fulfillRandomnessBatch([[0,round,sa]],[a1.id,b1.id],[pa,pb])).to.be.revertedWithCustomError(c.coordinator,"RoundUnavailable");
    expect(await c.coordinator.roundRandomness(0,round)).to.equal(ethers.ZeroHash);
    // Once the round is verified on chain its members need no entry, and an entry with a wrong signature is ignored.
    await serve(c,ethers,a1.id);
    const receipt=await (await c.coordinator.fulfillRandomnessBatch([[0,round,sb]],[a2.id],[await proveRequest(c,ethers,a2.id,sa)])).wait();
    expect(names(c,receipt)).not.to.include("RoundVerified");
    await (await c.coordinator.fulfillRandomnessBatch([[0,round+1n,sb]],[b1.id],[pb])).wait();
  });

  it("verifies no round of a skipped member, whatever it was skipped for (L1)",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    // Three requests of three rounds: one fulfilled through its own round, then one refunded and one expired, of rounds never verified.
    const {round,time}=await aheadRound(c);
    const fulfilled=await request(c,networkHelpers,{at:time});
    const refunded=await request(c,networkHelpers,{at:time+3n});
    const expired=await request(c,networkHelpers,{at:time+6n});
    expect([fulfilled.request.round,refunded.request.round,expired.request.round]).to.deep.equal([round,round+1n,round+2n]);
    await serve(c,ethers,fulfilled.id);
    await networkHelpers.time.increase(70);
    await c.coordinator.refundRequest(refunded.id);
    const proof=await proveRequest(c,ethers,fulfilled.id,signRound(round));
    const listed=[[0,round+1n,signRound(round+1n)],[0,round+2n,signRound(round+2n)],[0,round+3n,signRound(round+3n)]];
    const receipt=await (await c.coordinator.fulfillRandomnessBatch(listed,[fulfilled.id,refunded.id,expired.id],[proof,proof,proof])).wait();
    expect(names(c,receipt)).to.deep.equal(["FulfillmentSkipped","FulfillmentSkipped","FulfillmentSkipped"]);
    for(const r of [round+1n,round+2n,round+3n])expect(await c.coordinator.roundRandomness(0,r),`round ${r}`).to.equal(ethers.ZeroHash);
  });
});

describe("Rounds: proof inputs",function(){
  it("gives requestSeed and verifyRequestProof only once the round is verified, and getProofContext from a signature before",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const {a1,a2,sa}=await twoRounds(c);
    const seed=await roundSeed(c,ethers,a2.id,randomnessOf(sa)),proof=makeProof(seed);
    // RoundUnavailable comes before any check of the proof itself.
    await expect(c.coordinator.requestSeed(a2.id)).to.be.revertedWithCustomError(c.coordinator,"RoundUnavailable");
    for(const p of [proof,makeProof(1n),{...proof,pk:[1n,2n]}])
      await expect(c.coordinator.verifyRequestProof(a2.id,p)).to.be.revertedWithCustomError(c.coordinator,"RoundUnavailable");
    expect(Array.from(await c.coordinator.getProofContext(a2.id,sa))).to.deep.equal([seed,a2.request.deadline,false,false]);
    for(const view of ["requestSeed","getProofContext","verifyRequestProof"]){
      const args=view==="requestSeed"?[99]:view==="getProofContext"?[99,sa]:[99,proof];
      await expect(c.coordinator[view](...args),view).to.be.revertedWithCustomError(c.coordinator,"UnknownRequest");
    }
    // Once a1's fulfilment verifies the round, a2's seed and proof check work while a2 is still open, and any signature is ignored.
    await serve(c,ethers,a1.id);
    expect(await c.coordinator.requestSeed(a2.id)).to.equal(seed);
    expect(await c.coordinator.verifyRequestProof(a2.id,proof)).to.equal(proofOutput(proof));
    for(const ignored of ["0x",signRound(1n)])expect((await c.coordinator.getProofContext(a2.id,ignored))[0]).to.equal(seed);
    // A proof of another request, under another key, or tampered, is refused by the view as by fulfilment.
    await expect(c.coordinator.verifyRequestProof(a2.id,await proveRequest(c,ethers,a1.id,sa))).to.be.revertedWithCustomError(c.coordinator,"WrongSeed");
    await expect(c.coordinator.verifyRequestProof(a2.id,makeProof(seed,987654321n))).to.be.revertedWithCustomError(c.coordinator,"WrongPublicKey");
    await expect(c.coordinator.verifyRequestProof(a2.id,{...proof,s:proof.s+1n})).to.be.revert(ethers);
    // A valid proof is no evidence of service: the view answers for a fulfilled request as for an open one, and the status says which.
    await (await c.coordinator.fulfillRandomness(a2.id,proof,"0x")).wait();
    expect(await c.coordinator.verifyRequestProof(a2.id,proof)).to.equal(proofOutput(proof));
    expect(Array.from(await c.coordinator.getProofContext(a2.id,"0x"))).to.deep.equal([seed,a2.request.deadline,true,false]);
    const late=await request(c,networkHelpers);
    await networkHelpers.time.increase(61);
    await c.coordinator.refundRequest(late.id);
    expect(Array.from(await c.coordinator.getProofContext(late.id,signRound(late.request.round))).slice(2)).to.deep.equal([false,true]);
  });

  it("indexes RoundAssigned by request, beacon and round, and RoundVerified by beacon and round with the signature in its data",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const {id,receipt,request:q}=await request(c,networkHelpers);
    const assigned=receipt.logs.find((log:any)=>log.topics[0]===c.coordinator.interface.getEvent("RoundAssigned")!.topicHash);
    expect(assigned.topics.slice(1)).to.deep.equal([id,0n,q.round].map(v=>ethers.toBeHex(v,32)));
    expect(assigned.data).to.equal("0x");
    const {receipt:served,signature}=await serve(c,ethers,id);
    const verified=served.logs.find((log:any)=>log.topics[0]===c.coordinator.interface.getEvent("RoundVerified")!.topicHash);
    expect(verified.topics.slice(1)).to.deep.equal([0n,q.round].map(v=>ethers.toBeHex(v,32)));
    expect(Array.from(abi.decode(["bytes32","bytes"],verified.data))).to.deep.equal([randomnessOf(signature),signature]);
  });
});

describe("Rounds: a reorganised request",function(){
  it("gives the same id with other fields another seed, so a proof made before the reorganisation reverts WrongSeed",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const {round,time}=await aheadRound(c,60n);
    const snapshot=await networkHelpers.takeSnapshot();
    const original=await request(c,networkHelpers,{at:time-1n,seed:ethers.id("player move")});
    const signature=signRound(round),[oldSeed]=await c.coordinator.getProofContext(original.id,signature),oldProof=makeProof(oldSeed);
    // The block replaced, its request re-included with one field changed: a block later, another client seed, or a second later into the next round.
    const variants:Array<[string,()=>Promise<any>]>=[
      ["one block later",async()=>{await networkHelpers.mine(1);return request(c,networkHelpers,{at:time-1n,seed:ethers.id("player move")});}],
      ["another client seed",()=>request(c,networkHelpers,{at:time-1n,seed:ethers.id("another move")})],
      ["the next round",()=>request(c,networkHelpers,{at:time+1n,seed:ethers.id("player move")})],
    ];
    for(const [name,replay] of variants){
      await snapshot.restore();
      const again=await replay();
      expect(again.id,name).to.equal(original.id);
      const own=signRound(again.request.round),[seed]=await c.coordinator.getProofContext(again.id,own);
      expect(seed,name).to.not.equal(oldSeed);
      await expect(c.coordinator.fulfillRandomness(again.id,oldProof,own),name).to.be.revertedWithCustomError(c.coordinator,"WrongSeed");
      await (await c.coordinator.fulfillRandomness(again.id,makeProof(seed),own)).wait();
    }
    // Re-included unchanged, the request keeps its seed and the old proof; but a reorganisation that removed the round's verification
    // removed its cache entry, so the signature is needed again.
    await snapshot.restore();
    const served=await request(c,networkHelpers,{at:time-1n,seed:ethers.id("player move")});
    await (await c.coordinator.fulfillRandomness(served.id,oldProof,signature)).wait();
    await snapshot.restore();
    const same=await request(c,networkHelpers,{at:time-1n,seed:ethers.id("player move")});
    expect(Array.from(same.request)).to.deep.equal(Array.from(original.request));
    expect(await c.coordinator.roundRandomness(0,round)).to.equal(ethers.ZeroHash);
    await expect(c.coordinator.fulfillRandomness(same.id,oldProof,"0x")).to.be.revertedWithCustomError(c.coordinator,"InvalidRoundSignature");
    await (await c.coordinator.fulfillRandomness(same.id,oldProof,signature)).wait();
    expect((await c.coordinator.getRoundRequest(same.id)).randomness).to.equal(proofOutput(oldProof));
  });
});

describe("Rounds: requests of one block",function(){
  it("share a round but never a seed",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const fee=await nextFee(c,networkHelpers,100_000);
    await (await c.consumer.requestMany(4,100_000,c.owner.address,{value:fee*4n})).wait();
    const last=await c.consumer.lastRequestId(),ids=[last-3n,last-2n,last-1n,last];
    const rounds=new Set<bigint>(),seeds=new Set<bigint>();
    for(const id of ids){
      const q=await c.coordinator.getRoundRequest(id);
      rounds.add(q.round);
      seeds.add(await roundSeed(c,ethers,id,randomnessOf(signRound(q.round))));
    }
    expect([rounds.size,seeds.size]).to.deep.equal([1,4]);
  });
});
