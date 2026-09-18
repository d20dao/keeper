import {deployProxy} from "./helpers/proxy.ts";
import {readFileSync} from "node:fs";
import {expect} from "chai";
import {network} from "hardhat";
import {selectEpoch,verifyEpochAttestation,canonicalApiRequest,type EpochCatalog} from "../src/index.ts";
import {EPOCH_TEST_SIGNERS,EPOCH_TEST_WALLETS} from "./helpers/epoch.ts";
const {ethers,networkHelpers}=await network.create();
const fixture=JSON.parse(readFileSync(new URL("./fixtures/tickerlayer-2026-09-15.json",import.meta.url),"utf8"));
describe("TickerLayer compact last-trade records",function(){
  it("authenticates both real signed responses and the full canonical BTCUSD/ETHUSD query",async()=>{
    const catalog:EpochCatalog={signers:[EPOCH_TEST_SIGNERS[0],EPOCH_TEST_SIGNERS[1],fixture.signer,fixture.signer],registry:ethers.ZeroAddress,chainId:31337n,firstEpochStart:200n};
    for(const row of fixture.rows){
      const source=row.symbol==="BTCUSD"?2:3;let n=0;let s;
      do{s=selectEpoch(catalog,1n,ethers.id(`ticker-${n++}`));}while(s.source!==source&&n<100);
      expect(s.source).to.equal(source);expect(s.canonicalRequest).to.equal(canonicalApiRequest(row.request));
      const a={timestamp:BigInt(row.envelope.timestamp),data:ethers.hexlify(ethers.toUtf8Bytes(JSON.stringify(row.envelope.data))),signature:row.envelope.signature};
      expect(verifyEpochAttestation(s,a,a.timestamp).signer.toLowerCase()).to.equal(fixture.signer.toLowerCase());
      expect(()=>verifyEpochAttestation(s,{...a,data:ethers.hexlify(ethers.toUtf8Bytes(JSON.stringify({...row.envelope.data,symbol:"WRONG"})))},a.timestamp)).to.throw();
    }
  });
  it("accepts scientific trade sizes onchain and rejects wrong symbols/types/field order",async()=>{
    const [owner]=await ethers.getSigners();const registry=await deployProxy(ethers,"EpochEntropy",[EPOCH_TEST_SIGNERS,owner.address,owner.address]);await networkHelpers.mine(Number(await registry.firstEpochStart())-await ethers.provider.getBlockNumber());
    const seen=new Set<number>();
    for(let epoch=1;epoch<=64&&seen.size<2;epoch++){
      const s=await registry.getEpochSelection(epoch),source=Number(s.source);
      if(source>=2){
        const symbol=source===2?"BTCUSD":"ETHUSD";
        const sign=async(body:string)=>{const a={timestamp:BigInt(await networkHelpers.time.latest()),data:ethers.hexlify(ethers.toUtf8Bytes(body)),signature:"0x"};a.signature=await EPOCH_TEST_WALLETS[source].signMessage(ethers.getBytes(ethers.keccak256(ethers.solidityPacked(["bytes32","uint256","bytes"],[s.queryHash,a.timestamp,a.data]))));return a;};
        for(const body of [`{"symbol":"${symbol}","price":1,"size":-1,"timestamp":1}`,`{"symbol":"${symbol}","price":1,"size":1,"timestamp":1e3}`,`{"symbol":"${symbol}","price":01,"size":1,"timestamp":1}`,`{"symbol":"${symbol}","price":1,"price":2,"size":1,"timestamp":1}`])
          await expect(registry.commitEpoch(epoch,await sign(body))).to.be.revertedWithCustomError(registry,"InvalidData");
        await registry.commitEpoch(epoch,await sign(`{"symbol":"${symbol}","price":76000.12,"size":6e-8,"timestamp":1789490939269}`));seen.add(source);
      }
      await networkHelpers.mine(Number(await registry.epochStart(epoch+1))-await ethers.provider.getBlockNumber());
    }
    expect([...seen].sort()).to.deep.equal([2,3]);
  });
});
