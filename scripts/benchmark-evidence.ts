import {network} from "hardhat";
import {makeProof} from "../test/helpers/proof.ts";
import {encodeEvidencePacket} from "../src/evidence.ts";

const {ethers}=await network.create();
if((await ethers.provider.getNetwork()).chainId!==31337n)throw new Error("Local gas benchmark only");
const results=[];
for(const seed of [123n]){
  const packet=encodeEvidencePacket(makeProof(seed));
  const contract=await ethers.deployContract("EvidenceLogBenchmark");
  const baseline=await contract.baseline.estimateGas(packet);
  const logSegment=await contract.measureLogSegment.staticCall(packet);
  const log=await(await contract.logPacket(packet)).wait();
  const storage=await(await contract.storePacket(packet)).wait();
  results.push({packetBytes:ethers.getBytes(packet).length,baselineGas:baseline.toString(),
    logTransactionGas:log!.gasUsed.toString(),receiptDifferenceFromBaseline:(log!.gasUsed-baseline).toString(),
    logSegmentGas:logSegment.toString(),
    firstStorageWriteGas:storage!.gasUsed.toString()});
}
console.log(JSON.stringify({network:"local EVM simulation; excludes VRF/API verification and callback; not Arc mainnet pricing",results},null,2));
