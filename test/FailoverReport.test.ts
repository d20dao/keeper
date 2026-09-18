import {expect} from "chai";
import {network} from "hardhat";
import {deployProxy} from "./helpers/proxy.ts";
import {EPOCH_TEST_SIGNERS,epochAttestation} from "./helpers/epoch.ts";
import {makeProof,publicKey} from "./helpers/proof.ts";
import {failoverReport} from "../scripts/lib/failover-report.ts";

const {ethers,networkHelpers}=await network.create();
const FEE=10n**15n;

describe("Failover drill report",function(){
  it("attributes publications and fulfillments to the primary or the follower and lists refunds, duplicates, reverts and cancellations",async()=>{
    const [owner,primary,follower,user,stranger]=await ethers.getSigners();
    const registry=await deployProxy(ethers,"EpochEntropy",[EPOCH_TEST_SIGNERS,owner.address,primary.address]);
    const rng=await deployProxy(ethers,"D20VRFCoordinator",[publicKey(),owner.address,owner.address,FEE,1,await registry.getAddress(),5000]);
    await rng.setPricing(FEE,0,300000);
    const consumer=await ethers.deployContract("TestConsumer",[await rng.getAddress()]);
    await networkHelpers.mine(Number(await registry.firstEpochStart())-await ethers.provider.getBlockNumber());
    const fromBlock=await ethers.provider.getBlockNumber()+1;
    await registry.setBackupCommitter(follower.address,true);
    const request=async(label:string)=>{await consumer.request(ethers.id(label),200000,user.address,{value:FEE});return await consumer.lastRequestId() as bigint;};
    // Primary down: the follower publishes epoch 1 and serves the first request; a late primary commit reverts.
    const first=await request("takeover");
    await (registry.connect(follower) as any).commitEpoch(1,await epochAttestation(registry,1n,ethers));
    const late=await (registry.connect(primary) as any).commitEpoch(1,await epochAttestation(registry,1n,ethers),{gasLimit:500_000}).catch((error:unknown)=>error);
    expect(late).to.be.instanceOf(Error);
    await networkHelpers.mine(2);
    await (rng.connect(follower) as any).fulfillRandomness(first,makeProof(await rng.requestSeed(first)),{gasLimit:2_000_000});
    // Primary back: it serves the next two in one batch, a follower duplicate is skipped and a pending follower nonce is cancelled.
    const second=await request("primary-back-1"),third=await request("primary-back-2");
    await networkHelpers.mine(2);
    const proofs=[makeProof(await rng.requestSeed(second)),makeProof(await rng.requestSeed(third))];
    await (rng.connect(primary) as any).fulfillRandomnessBatch([second,third],proofs,{gasLimit:3_000_000});
    await (rng.connect(follower) as any).fulfillRandomnessBatch([second,third],proofs,{gasLimit:3_000_000});
    await follower.sendTransaction({to:follower.address,value:0});
    // An unserved request expires and is refunded.
    const expired=await request("expired");
    await networkHelpers.time.increase(61);
    await (rng.connect(stranger) as any).refundRequest(expired);
    const toBlock=await ethers.provider.getBlockNumber();

    const report=await failoverReport(ethers.provider,{registry:await registry.getAddress(),coordinator:await rng.getAddress(),fromBlock,toBlock});
    expect(report.wallets).to.deep.include({address:primary.address,role:"committer"}).and.to.deep.include({address:follower.address,role:"backup committer"});
    expect(report.epochs.map(e=>[e.epochId,e.publisher,e.publisherRole,e.recipe])).to.deep.equal([["1",follower.address,"backup committer",Number((await registry.getEpoch(1)).source)]]);
    const byId=Object.fromEntries(report.requests.map(r=>[r.requestId,r]));
    // An allowed backup committer earns the share of what it serves; the primary earns the rest.
    expect([byId[first.toString()].fulfilled!.submitter,byId[first.toString()].fulfilled!.submitterRole,byId[first.toString()].fulfilled!.keeperShareTo]).to.deep.equal([follower.address,"backup committer",follower.address]);
    for(const id of [second,third]){
      const r=byId[id.toString()];
      expect([r.fulfilled!.submitter,r.fulfilled!.submitterRole]).to.deep.equal([primary.address,"committer"]);
      expect(r.skipped.map(s=>[s.from,s.reason])).to.deep.equal([[follower.address,"already fulfilled"]]);
    }
    expect(byId[expired.toString()].fulfilled).to.equal(undefined);expect(byId[expired.toString()].refunded!.amount).to.equal(FEE.toString());
    expect(report.summary).to.deep.include({epochsPublishedBy:{[follower.address]:1},requestsFulfilledBy:{[follower.address]:1,[primary.address]:2},
      requests:4,unserved:[],refunds:1,skippedDuplicates:2,nonceCancellations:1,duplicateAttempts:2,duplicateAttemptsBy:{[primary.address]:1,[follower.address]:1}});
    // The late epoch commit and the follower's skipped batch are the contention a drill must show.
    expect(report.duplicateAttempts.map(tx=>[tx.from,tx.method,tx.status,tx.work])).to.deep.equal([
      [primary.address,"commitEpoch",0,["1"]],[follower.address,"fulfillRandomnessBatch",1,[second.toString(),third.toString()]]]);
    expect(report.revertedKeeperTransactions.map(tx=>[tx.from,tx.role,tx.method,tx.status])).to.deep.equal([[primary.address,"committer","commitEpoch",0]]);
    expect(report.summary.revertedKeeperTransactions).to.equal(1);
    expect(report.nonceCancellations.map(tx=>tx.from)).to.deep.equal([follower.address]);
    const tooLong=await failoverReport(ethers.provider,{registry:await registry.getAddress(),coordinator:await rng.getAddress(),fromBlock:0,toBlock:60_000}).catch((error:Error)=>error);
    expect(String(tooLong)).to.contain("exceeds 50000 blocks");
  });
});
