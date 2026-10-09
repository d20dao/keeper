import {expect} from "chai";
import {network} from "hardhat";
import {mapRandomness as mapOffChain} from "../../src/mapping.ts";
import {proofOutput} from "../helpers/proof.ts";
import {compiled,deployRoundCoordinator,nextFee,proveRequest,request,signRound,type RoundFixture} from "../helpers/robinhood.ts";

const {ethers,networkHelpers}=await network.create();

async function fixture(){
  const c=await deployRoundCoordinator(ethers,networkHelpers);
  // Arc's test consumer, compiled against ID20VRF, D20VRFConsumer and D20VRFRequests and used here unchanged.
  const unchanged=await ethers.deployContract("TestConsumer",[await c.coordinator.getAddress()]);
  const vrf=await ethers.getContractAt("ID20VRF",await c.coordinator.getAddress());
  return {...c,unchanged,vrf};
}
type Fixture=Awaited<ReturnType<typeof fixture>>;
/// Fulfil a request as the keeper, with its round's signature.
async function serve(c:RoundFixture,requestId:bigint){
  const q=await c.coordinator.getRoundRequest(requestId),signature=signRound(q.round);
  const proof=await proveRequest(c,ethers,requestId,signature);
  await (await c.coordinator.connect(c.keeper).fulfillRandomness(requestId,proof,signature)).wait();
  return proofOutput(proof);
}
const signatureOf=(fragment:any)=>`${fragment.format("sighash")}->${fragment.outputs.map((o:any)=>o.format("sighash")).join(",")}`;

describe("Round coordinator: the chain-neutral consumer interface",function(){
  it("implements every ID20VRF function with its exact signature and return types",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    for(const fragment of c.vrf.interface.fragments.filter((f:any)=>f.type==="function")){
      const own=c.coordinator.interface.getFunction((fragment as any).format("sighash"));
      expect(own,(fragment as any).format("sighash")).to.not.equal(null);
      expect(signatureOf(own)).to.equal(signatureOf(fragment));
    }
    // A D20VRFConsumer's callbacks are the selectors the coordinator calls.
    const consumerAbi=compiled("D20VRFConsumer.sol").abi.filter((f:any)=>f.type==="function").map((f:any)=>f.name);
    expect(consumerAbi).to.include.members(["rawFulfillRandomness","onRefund"]);
  });

  it("serves an unchanged ID20VRF consumer: raw, mapped and helper requests, callbacks and mapped results",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const consumer=await c.unchanged.getAddress();
    // Raw request and callback.
    let fee=await nextFee(c,networkHelpers,100_000);
    expect(fee).to.equal(await c.vrf.quoteFeeAt(100_000,20_000_000n));
    await (await c.unchanged.request(ethers.id("raw"),100_000,c.owner.address,{value:fee})).wait();
    const raw=await c.unchanged.lastRequestId();
    expect((await c.coordinator.getRoundRequest(raw)).consumer).to.equal(consumer);
    const rawOutput=await serve(c,raw);
    expect(await c.unchanged.results(raw)).to.equal(rawOutput);
    expect(await c.vrf.getMappedResult(raw)).to.deep.equal([BigInt(rawOutput)]);
    // A mapped request: three six-sided dice, mapped on chain by the linked library exactly as the off-chain reference maps them.
    const spec={operation:1,lower:0n,upper:6n,count:3,population:0};
    fee=await nextFee(c,networkHelpers,150_000);
    await (await c.unchanged.requestMapped(ethers.id("dice"),150_000,c.owner.address,spec,{value:fee})).wait();
    const dice=await c.unchanged.lastRequestId();
    await expect(c.vrf.getMappedResult(dice)).to.be.revertedWithCustomError(c.coordinator,"NotFulfilled");
    const diceOutput=await serve(c,dice);
    const mapped=await c.vrf.getMappedResult(dice);
    expect(mapped).to.deep.equal(mapOffChain(diceOutput,spec));
    expect(mapped.every((v:bigint)=>v>=1n&&v<=6n)).to.equal(true);
    expect(Array.from(await c.coordinator.getMapping(dice)).map(String)).to.deep.equal(["1","0","6","3","0"]);
    expect(await c.coordinator.mapRandomness(diceOutput,spec)).to.deep.equal(mapped);
    // D20VRFRequests.d20 pays the same-transaction quote, which the base fee of that block sets: above the minimum fee here. The gas limit
    // is explicit because an estimate runs at another base fee.
    const base=30_000_000n,quote=await c.vrf.quoteFeeAt(200_000,base);
    expect(quote).to.be.greaterThan(await c.coordinator.minFee());
    await networkHelpers.setNextBlockBaseFeePerGas(base);
    await (await c.unchanged.rollD20(c.owner.address,{value:quote,gasLimit:1_000_000})).wait();
    const d20=await c.unchanged.lastRequestId();
    const d20Output=await serve(c,d20);
    expect(await c.vrf.getMappedResult(d20)).to.deep.equal(mapOffChain(d20Output,{operation:1,lower:0n,upper:20n,count:1,population:0}));
    expect(await c.unchanged.callbackCount()).to.equal(3n);
  });

  it("refunds an unchanged consumer's expired request and notifies it",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const fee=await nextFee(c,networkHelpers,100_000);
    await (await c.unchanged.request(ethers.id("late"),100_000,c.stranger.address,{value:fee})).wait();
    const requestId=await c.unchanged.lastRequestId();
    await networkHelpers.time.increase(61);
    const before=await ethers.provider.getBalance(c.stranger.address);
    const receipt=await (await c.coordinator.connect(c.owner).refundRequest(requestId)).wait();
    await expect(receipt).to.emit(c.coordinator,"RequestRefundedTo").withArgs(requestId,c.stranger.address,fee,true);
    await expect(receipt).to.emit(c.coordinator,"RefundCallbackAttempted").withArgs(requestId,await c.unchanged.getAddress(),true,100_000);
    expect(await ethers.provider.getBalance(c.stranger.address)-before).to.equal(fee);
    expect(await c.coordinator.refundCallbackDelivered(requestId)).to.equal(true);
  });

  it("has no getRequest: a caller on another chain's request ABI gets a clean revert, never data to mis-decode",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    expect(c.coordinator.interface.getFunction("getRequest")).to.equal(null);
    const {id:requestId}=await request(c,networkHelpers);
    const address=await c.coordinator.getAddress();
    // The epoch coordinator's getRequest(uint256) and one-argument getProofContext(uint256), on a request that exists here.
    for(const signature of ["getRequest(uint256)","getProofContext(uint256)"]){
      const data=ethers.id(signature).slice(0,10)+ethers.toBeHex(requestId,32).slice(2);
      await expect(ethers.provider.call({to:address,data}),signature).to.be.revert(ethers);
    }
  });
});
