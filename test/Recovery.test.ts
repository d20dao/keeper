import {expect} from "chai";
import {network} from "hardhat";
import {makeProof} from "./helpers/proof.ts";
import {deployReadyEpochFixture} from "./helpers/epoch.ts";
const {ethers,networkHelpers}=await network.create();
const fixture=()=>deployReadyEpochFixture(ethers,networkHelpers,123n);
describe("Request recovery and serve order",function(){
  it("preserves older pending gaps after out-of-order fulfillment and callback retry",async()=>{
    const {rng,consumer,user}=await networkHelpers.loadFixture(fixture);
    for(let i=0;i<3;i++)await consumer.request(ethers.ZeroHash,200000,user.address,{value:123});
    await networkHelpers.mine(2);
    await rng.fulfillRandomness(2,makeProof(await rng.requestSeed(2)),{gasLimit:2000000});
    expect(await rng.servedRequestAt(1)).to.equal(2);
    const page=await rng.getPendingRequestIds(1,256);expect(Array.from(page[0])).to.deep.equal([1n,3n]);
    await consumer.setMode(1);await rng.fulfillRandomness(1,makeProof(await rng.requestSeed(1)),{gasLimit:2000000});
    expect(await rng.servedRequestAt(2)).to.equal(1);expect((await rng.getRequest(1)).delivered).to.equal(false);
    await consumer.setMode(0);await rng.retryCallback(1,200000,{gasLimit:400000});
    expect(await rng.lastServedIndex()).to.equal(2);
  });
  it("excludes expired requests while retaining fixed-recipient refunds and bounded scans",async()=>{
    const {rng,consumer,user}=await networkHelpers.loadFixture(fixture);
    await consumer.request(ethers.ZeroHash,200000,user.address,{value:123});await networkHelpers.time.increase(61);
    const page=await rng.getPendingRequestIds(1,128);expect(Array.from(page[0])).to.deep.equal([]);expect(page[1]).to.equal(2);
    expect((await rng.getRequest(1)).refunded).to.equal(false);await rng.refundRequest(1,{gasLimit:500000});
    expect((await rng.getRequest(1)).refunded).to.equal(true);expect(await rng.lastServedIndex()).to.equal(0);
    await expect(rng.getPendingRequestIds(0,128)).to.be.revertedWithCustomError(rng,"InvalidScan");
    await expect(rng.getPendingRequestIds(1,257)).to.be.revertedWithCustomError(rng,"InvalidScan");
  });
});
