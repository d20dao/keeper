import {expect} from "chai";
import {network} from "hardhat";
import {makeProof} from "./helpers/proof.ts";
import {deployReadyEpochFixture} from "./helpers/epoch.ts";
import {encodeEvidencePacket,decodeEvidencePacket,hashProof} from "../src/index.ts";
const {ethers,networkHelpers}=await network.create();
const fixture=()=>deployReadyEpochFixture(ethers,networkHelpers,123n);
describe("Public proof evidence",function(){
  it("retains exact proof bytes for wrapper submissions and rejects noncanonical lengths",async()=>{
    const {rng,consumer,user}=await networkHelpers.loadFixture(fixture);
    const forwarder=await ethers.deployContract("FulfillmentForwarder");
    await consumer.request(ethers.ZeroHash,200000,user.address,{value:123});await networkHelpers.mine(2);
    const proof=makeProof(await rng.requestSeed(1));
    const receipt=await(await forwarder.forward(await rng.getAddress(),1,proof,{gasLimit:2000000})).wait();
    const event=receipt!.logs.filter((l:any)=>l.address.toLowerCase()===(rng.target as string).toLowerCase()).map((l:any)=>rng.interface.parseLog(l)).find((e:any)=>e?.name==="FulfillmentEvidence")!;
    expect(event.args.packet).to.equal(encodeEvidencePacket(proof));expect(ethers.getBytes(event.args.packet).length).to.equal(416);
    expect(hashProof(decodeEvidencePacket(event.args.packet).proof)).to.equal((await rng.getRequest(1)).proofHash);
    for(const packet of [event.args.packet+"00",ethers.dataSlice(event.args.packet,0,415),ethers.concat([ethers.ZeroHash,event.args.packet])])expect(()=>decodeEvidencePacket(packet)).to.throw();
  });
});
