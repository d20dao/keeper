import {expect} from "chai";
import {network} from "hardhat";
import {deployReadyEpochFixture} from "./helpers/epoch.ts";
import {makeProof,proofOutput} from "./helpers/proof.ts";
import {encodeEvidencePacket} from "../src/index.ts";
const {ethers,networkHelpers}=await network.create();
const FEE=10n**15n,GAS=200_000;
const fixture=()=>deployReadyEpochFixture(ethers,networkHelpers,FEE);
type Fixture=Awaited<ReturnType<typeof fixture>>;
async function requests(c:Fixture,n:number,consumer=c.consumer){
  const ids:bigint[]=[];
  for(let i=0;i<n;i++){await consumer.request(ethers.id(`batch-${i}`),GAS,c.user.address,{value:FEE});ids.push(await consumer.lastRequestId() as bigint);}
  return ids;
}
async function proofs(c:Fixture,ids:bigint[]){await networkHelpers.mine(2);return Promise.all(ids.map(async id=>makeProof(await c.rng.requestSeed(id))));}
const events=(receipt:any,rng:any,name:string)=>receipt.logs.filter((l:any)=>l.address.toLowerCase()===(rng.target as string).toLowerCase()).map((l:any)=>rng.interface.parseLog(l)).filter((e:any)=>e?.name===name);

describe("Batched fulfillment",()=>{
  it("fulfills every member with per-request events, evidence, keeper share and delivery; prints gas for 8",async()=>{
    const c=await networkHelpers.loadFixture(fixture);await c.rng.setKeeperFeeBps(5000);await c.registry.setCommitter(c.stranger.address);
    const ids=await requests(c,8),ps=await proofs(c,ids),before=await ethers.provider.getBalance(c.stranger.address);
    const receipt=(await(await c.rng.fulfillRandomnessBatch(ids,ps,{gasLimit:8_000_000})).wait())!;
    for(const name of ["BlockHashStored","RequestServed","ProofVerified","RandomnessFulfilled","FulfillmentEvidence","CallbackAttempted","KeeperFeePaid"])expect(events(receipt,c.rng,name).length,name).to.equal(8);
    expect(events(receipt,c.rng,"FulfillmentSkipped").length).to.equal(0);
    for(const [i,id] of ids.entries()){
      const r=await c.rng.getRequest(id);expect(r.fulfilled).to.equal(true);expect(r.delivered).to.equal(true);
      expect(r.randomness).to.equal(proofOutput(ps[i]));expect(await c.consumer.results(id)).to.equal(proofOutput(ps[i]));
      expect(await c.rng.servedRequestAt(i+1)).to.equal(id);
      expect(events(receipt,c.rng,"FulfillmentEvidence").find((e:any)=>e.args.requestId===id)!.args.packet).to.equal(encodeEvidencePacket(ps[i]));
      expect(await c.rng.verifyRequestProof(id,ps[i])).to.equal(r.randomness);
    }
    expect(await ethers.provider.getBalance(c.stranger.address)).to.equal(before+8n*(FEE/2n));
    expect(await c.rng.earnedFees()).to.equal(8n*(FEE/2n));expect(await c.rng.lastServedIndex()).to.equal(8n);
    console.log(`Batch of 8 fulfillments: gas=${receipt.gasUsed}`);
  });
  for(const [mode,title] of [[1,"revert"],[2,"out-of-gas"],[3,"return-data bomb"]] as const)it(`keeps siblings settled and delivered when one callback has ${title}`,async()=>{
    const c=await networkHelpers.loadFixture(fixture),bad=await ethers.deployContract("TestConsumer",[await c.rng.getAddress()]);
    const [a]=await requests(c,1),[b]=await requests(c,1,bad),[d]=await requests(c,1);const ps=await proofs(c,[a,b,d]);await bad.setMode(mode);
    const receipt=(await(await c.rng.fulfillRandomnessBatch([a,b,d],ps,{gasLimit:4_000_000})).wait())!;
    expect(events(receipt,c.rng,"CallbackAttempted").map((e:any)=>[e.args.requestId,e.args.success])).to.deep.equal([[a,true],[b,false],[d,true]]);
    for(const id of [a,b,d])expect((await c.rng.getRequest(id)).fulfilled).to.equal(true);
    expect((await c.rng.getRequest(b)).delivered).to.equal(false);expect(await c.consumer.results(d)).to.equal(proofOutput(ps[2]));
    expect(await c.rng.earnedFees()).to.equal(3n*FEE);
    await bad.setMode(0);await c.rng.retryCallback(b,GAS,{gasLimit:500000});expect(await bad.results(b)).to.equal(proofOutput(ps[1]));
  });
  it("skips members already fulfilled by a stranger, expired or refunded, and still fulfills the rest",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const [expired]=await requests(c,1);await networkHelpers.time.increase(61);
    const [taken,live]=await requests(c,2),ps=await proofs(c,[expired,taken,live]);
    await c.rng.connect(c.stranger).fulfillRandomness(taken,ps[1],{gasLimit:2_000_000});
    const receipt=(await(await c.rng.fulfillRandomnessBatch([expired,taken,live],ps,{gasLimit:4_000_000})).wait())!;
    expect(events(receipt,c.rng,"FulfillmentSkipped").map((e:any)=>[e.args.requestId,e.args.reason])).to.deep.equal([[expired,3n],[taken,1n]]);
    expect(events(receipt,c.rng,"RandomnessFulfilled").map((e:any)=>e.args.requestId)).to.deep.equal([live]);
    expect((await c.rng.getRequest(expired)).fulfilled).to.equal(false);expect((await c.rng.getRequest(live)).delivered).to.equal(true);
    expect(await c.rng.lastServedIndex()).to.equal(2n);expect(await c.rng.earnedFees()).to.equal(2n*FEE);
    await c.rng.refundRequest(expired,{gasLimit:500000});
    await expect(c.rng.fulfillRandomnessBatch([expired],[ps[0]],{gasLimit:1_000_000})).to.emit(c.rng,"FulfillmentSkipped").withArgs(expired,2);
    expect(await c.rng.earnedFees()).to.equal(2n*FEE);
  });
  it("reverts the whole batch on a wrong-seed or unready member and on empty, oversized or mismatched input",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    await networkHelpers.mine(Number(await c.registry.epochStart(2))-3-await ethers.provider.getBlockNumber());
    const ids=await requests(c,2),ps=await proofs(c,ids);
    const [unpublished]=await requests(c,1);expect((await c.rng.getRequest(unpublished)).epochId).to.equal(2n);
    for(const [members,list,error] of [
      [ids,[ps[0],{...ps[1],seed:ps[1].seed+1n}],"WrongSeed"],[ids,[ps[1],ps[0]],"WrongSeed"],[[ids[0],unpublished],[ps[0],ps[0]],"NotReady"],
    ] as const)await expect(c.rng.fulfillRandomnessBatch(members,list,{gasLimit:4_000_000})).to.be.revertedWithCustomError(c.rng,error);
    for(const id of ids)expect((await c.rng.getRequest(id)).fulfilled).to.equal(false);
    expect(await c.rng.earnedFees()).to.equal(0n);expect(await c.rng.lastServedIndex()).to.equal(0n);
    await expect(c.rng.fulfillRandomnessBatch([],[])).to.be.revertedWithCustomError(c.rng,"InvalidBatch");
    await expect(c.rng.fulfillRandomnessBatch(ids,[ps[0]])).to.be.revertedWithCustomError(c.rng,"InvalidBatch");
    await expect(c.rng.fulfillRandomnessBatch(Array(17).fill(ids[0]),Array(17).fill(ps[0]))).to.be.revertedWithCustomError(c.rng,"InvalidBatch");
    await expect(c.rng.fulfillRandomnessBatch([999n],[ps[0]])).to.be.revertedWithCustomError(c.rng,"UnknownRequest");
    await expect(c.rng.fulfillRandomnessBatch([ids[0],ids[0]],[ps[0],ps[0]],{gasLimit:4_000_000})).to.emit(c.rng,"FulfillmentSkipped").withArgs(ids[0],1);
    expect(await c.consumer.results(ids[0])).to.equal(proofOutput(ps[0]));
  });
});
