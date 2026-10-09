import {expect} from "chai";
import {network} from "hardhat";
import {AbiCoder,keccak256} from "ethers";
import {hashMapping,mapRandomness as mapOffChain,type MappingSpec} from "../../src/mapping.ts";
import {makeProof,proofOutput} from "../helpers/proof.ts";
import {SEED_DOMAIN,deployRoundCoordinator,eventsOf,nextFee,randomnessOf,serve,signRound,type RoundFixture} from "../helpers/robinhood.ts";

const {ethers,networkHelpers}=await network.create();
const abi=AbiCoder.defaultAbiCoder();

async function fixture(){
  const c=await deployRoundCoordinator(ethers,networkHelpers);
  const hostile=await ethers.deployContract("HostileRoundConsumer",[await c.coordinator.getAddress()]);
  return {...c,hostile};
}
type Fixture=Awaited<ReturnType<typeof fixture>>;
const spec=(operation:number,lower:bigint,upper:bigint,count:number,population:number):MappingSpec=>({operation,lower,upper,count,population});
const asTuple=(s:MappingSpec)=>[s.operation,s.lower,s.upper,s.count,s.population];
/// One request of every operation, with edge parameters: 128 dice, the whole uint256 range, a 256-item shuffle.
const SPECS:Array<[string,MappingSpec]>=[
  ["three six-sided dice",spec(1,0n,6n,3,0)],["128 two-sided dice",spec(1,0n,2n,128,0)],["a d20",spec(1,0n,20n,1,0)],["a coin flip",spec(2,0n,0n,1,0)],
  ["a number from 10 to 1000",spec(3,10n,1000n,1,0)],["any uint256",spec(3,0n,2n**256n-1n,1,0)],["a single-value range",spec(3,7n,7n,1,0)],
  ["one of 52",spec(4,0n,0n,1,52)],["five of 52",spec(5,0n,0n,5,52)],["all of 3",spec(5,0n,0n,3,3)],["a shuffle of 10",spec(6,0n,0n,10,10)],
  ["a shuffle of 256",spec(6,0n,0n,256,256)],
];
/// A mapped request of the RoundConsumer at its exact fee: its id and receipt.
async function requestMapped(c:RoundFixture,s:MappingSpec,consumer:any=c.consumer){
  const fee=await nextFee(c,networkHelpers,100_000);
  const receipt=await (await consumer.requestMapped(ethers.id(`mapped ${await c.coordinator.nextRequestId()}`),100_000,c.owner.address,asTuple(s),{value:fee})).wait();
  return {id:await consumer.lastRequestId() as bigint,receipt};
}

describe("Round coordinator: mapped requests",function(){
  it("fixes each mapped request's operation and maps its result on chain exactly as the off-chain mapper does",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    for(const [name,s] of SPECS){
      const {id,receipt}=await requestMapped(c,s);
      const [requested]=eventsOf(c.coordinator,receipt,"MappingRequested");
      expect([requested.args.requestId,requested.args.mappingHash,Array.from(requested.args.spec)],name).to.deep.equal([id,hashMapping(s),asTuple(s).map(BigInt)]);
      expect((await c.coordinator.getRoundRequest(id)).mappingHash,name).to.equal(hashMapping(s));
      expect(Array.from(await c.coordinator.getMapping(id)),name).to.deep.equal(asTuple(s).map(BigInt));
      await expect(c.coordinator.getMappedResult(id),name).to.be.revertedWithCustomError(c.coordinator,"NotFulfilled");
      const {proof}=await serve(c,ethers,id);
      const randomness=proofOutput(proof),expected=mapOffChain(randomness,s);
      expect(await c.coordinator.getMappedResult(id),name).to.deep.equal(expected);
      expect(await c.coordinator.mapRandomness(randomness,asTuple(s)),name).to.deep.equal(expected);
      expect(await c.consumer.mappedResult(id),name).to.deep.equal(expected);
      expect(expected.length,name).to.equal(s.operation===1||s.operation>=5?s.count:1);
    }
  });

  it("maps a raw request to its randomness, and stores no mapping for it",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const fee=await nextFee(c,networkHelpers,100_000);
    const receipt=await (await c.consumer.request(ethers.id("raw"),100_000,c.owner.address,{value:fee})).wait();
    const id=await c.consumer.lastRequestId();
    const raw=spec(0,0n,0n,0,0);
    await expect(receipt).to.emit(c.coordinator,"MappingRequested").withArgs(id,hashMapping(raw),asTuple(raw));
    expect(Array.from(await c.coordinator.getMapping(id))).to.deep.equal([0n,0n,0n,0n,0n]);
    const {proof}=await serve(c,ethers,id);
    expect(await c.coordinator.getMappedResult(id)).to.deep.equal([BigInt(proofOutput(proof))]);
    for(const view of ["getMapping","getMappedResult"])await expect(c.coordinator[view](99),view).to.be.revertedWithCustomError(c.coordinator,"UnknownRequest");
  });

  it("binds the mapping into the seed: a proof of the same request under another mapping is refused",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const dice=spec(1,0n,6n,1,0),{id}=await requestMapped(c,dice);
    const q=await c.coordinator.getRoundRequest(id),signature=signRound(q.round);
    const seedWith=async(mappingHash:string)=>BigInt(keccak256(abi.encode(
      ["bytes32","uint256","address","bytes32","uint256","address","bytes32","bytes32","uint64","uint8","uint64","bytes32"],
      [SEED_DOMAIN,(await ethers.provider.getNetwork()).chainId,c.coordinator.target,await c.coordinator.keyHash(),id,q.consumer,q.clientSeed,mappingHash,
        q.requestBlock,q.beaconId,q.round,randomnessOf(signature)])));
    const other=makeProof(await seedWith(hashMapping(spec(1,0n,20n,1,0))));
    await expect(c.coordinator.fulfillRandomness(id,other,signature)).to.be.revertedWithCustomError(c.coordinator,"WrongSeed");
    const own=makeProof(await seedWith(hashMapping(dice)));
    await (await c.coordinator.fulfillRandomness(id,own,signature)).wait();
    expect(await c.coordinator.getMappedResult(id)).to.deep.equal(mapOffChain(proofOutput(own),dice));
  });

  it("keeps a mapped result readable when the consumer's callback failed",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    await c.hostile.setCallbackMode(1);
    const shuffle=spec(6,0n,0n,8,8),{id}=await requestMapped(c,shuffle,c.hostile);
    const {proof,receipt}=await serve(c,ethers,id);
    await expect(receipt).to.emit(c.coordinator,"CallbackAttempted").withArgs(id,false,100_000);
    const mapped=await c.coordinator.getMappedResult(id);
    expect(mapped).to.deep.equal(mapOffChain(proofOutput(proof),shuffle));
    expect([...mapped].sort((x,y)=>Number(x-y))).to.deep.equal([0n,1n,2n,3n,4n,5n,6n,7n]);
  });
});
