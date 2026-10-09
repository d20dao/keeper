import {expect} from "chai";
import {network} from "hardhat";
import {ARB_SYS,L2_OFFSET,arbitrumMocks} from "../helpers/robinhood.ts";

const {ethers,networkHelpers}=await network.create();

async function fixture(){
  const mocks=await arbitrumMocks(ethers,networkHelpers);
  const harness=await ethers.deployContract("ArbitrumBlocksHarness");
  return {...mocks,harness};
}
/// The L2 number of the block the next transaction lands in.
const nextL2=async()=>BigInt(await ethers.provider.getBlockNumber())+1n+L2_OFFSET;

describe("ArbitrumBlocks: L2 block numbers",function(){
  it("reads the L2 number from ArbSys, which the evaluation order of its Yul call must not break",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    // A read of block.number would be a million blocks short; a returndatasize() read before the call would revert every time.
    for(const blocks of [0,1,7]){
      await networkHelpers.mine(blocks);
      const predicted=await nextL2();
      await c.harness.sample();
      expect(await c.harness.sampledNumber()).to.equal(predicted);
      expect(predicted).to.be.greaterThan(L2_OFFSET);
    }
    // A call sees the head or the block after it, always with the offset.
    const head=BigInt(await ethers.provider.getBlockNumber()),viewed=await c.harness.number()-L2_OFFSET;
    expect([head,head+1n]).to.include(viewed);
  });

  it("reverts, and never answers, unless ArbSys answers exactly one word",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    // Reverts, 64 bytes, 31 bytes, nothing.
    for(const mode of [1,2,3,4]){
      await c.arbSys.setMode(mode);
      await expect(c.harness.number(),`mode ${mode}`).to.be.revertedWithoutReason(ethers);
      await expect(c.harness.sample(),`mode ${mode}`).to.be.revertedWithoutReason(ethers);
    }
    await c.arbSys.setMode(0);
    expect(await c.harness.number()).to.be.greaterThan(L2_OFFSET);
    // No precompile at all: a call to an empty address succeeds with no data, which is not an answer either.
    await networkHelpers.setCode(ARB_SYS,"0x");
    await expect(c.harness.number()).to.be.revertedWithoutReason(ethers);
  });

  it("has test doubles that answer as the real contracts do",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    expect(await c.arbSys.OFFSET()).to.equal(L2_OFFSET);
    // ArbSys answers arbBlockNumber() only.
    await expect(ethers.provider.call({to:ARB_SYS,data:"0x2b407a82"+"00".repeat(32)})).to.be.revert(ethers);
    // NodeInterface answers what it is told, for a keeper harness.
    expect(Array.from(await c.nodeInterface.gasEstimateL1Component.staticCall(ethers.ZeroAddress,false,"0x"))).to.deep.equal([0n,0n,0n]);
    await c.nodeInterface.setL1Component(732,23_700_000n,5n);
    expect(Array.from(await c.nodeInterface.gasEstimateL1Component.staticCall(ethers.ZeroAddress,false,"0x1234"))).to.deep.equal([732n,23_700_000n,5n]);
  });
});
