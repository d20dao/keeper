import {expect} from "chai";
import {network} from "hardhat";
import {deployProxy} from "./helpers/proxy.ts";
import {EPOCH_TEST_SIGNERS,epochAttestation} from "./helpers/epoch.ts";
import {makeProof,proofOutput,publicKey} from "./helpers/proof.ts";

const {ethers,networkHelpers}=await network.create();
const FEE=10n**15n;

async function fixture(){
  const [owner,committer,backup,second,stranger,user]=await ethers.getSigners();
  const registry=await deployProxy(ethers,"EpochEntropy",[EPOCH_TEST_SIGNERS,owner.address,committer.address]);
  const rng=await deployProxy(ethers,"D20VRFCoordinator",[publicKey(),owner.address,owner.address,FEE,1,await registry.getAddress(),5000]);
  await rng.setPricing(FEE,0,300000);
  const consumer=await ethers.deployContract("TestConsumer",[await rng.getAddress()]);
  await networkHelpers.mine(Number(await registry.firstEpochStart())-await ethers.provider.getBlockNumber());
  return {registry,rng,consumer,owner,committer,backup,second,stranger,user};
}
const as=(registry:any,signer:any)=>registry.connect(signer) as any;

describe("Backup committers",function(){
  it("are set and removed only by the owner, never zero, the primary committer or a no-op, and stay bounded",async()=>{
    const {registry,committer,backup,second,stranger}=await networkHelpers.loadFixture(fixture);
    expect(await registry.MAX_BACKUP_COMMITTERS()).to.equal(4n);
    await expect(as(registry,stranger).setBackupCommitter(backup.address,true)).to.be.revertedWithCustomError(registry,"OwnableUnauthorizedAccount");
    for(const [account,allowed] of [[ethers.ZeroAddress,true],[committer.address,true],[backup.address,false]] as const)
      await expect(registry.setBackupCommitter(account,allowed)).to.be.revertedWithCustomError(registry,"InvalidConfig");
    await expect(registry.setBackupCommitter(backup.address,true)).to.emit(registry,"BackupCommitterSet").withArgs(backup.address,true);
    await expect(registry.setBackupCommitter(backup.address,true)).to.be.revertedWithCustomError(registry,"InvalidConfig");
    expect([await registry.isBackupCommitter(backup.address),await registry.isBackupCommitter(second.address),await registry.backupCommitterCount()]).to.deep.equal([true,false,1n]);
    // The coordinator reads one view for both roles when it pays the keeper share.
    expect([await registry.isAuthorizedCommitter(committer.address),await registry.isAuthorizedCommitter(backup.address),await registry.isAuthorizedCommitter(second.address)]).to.deep.equal([true,true,false]);
    const extra=[second,stranger,ethers.Wallet.createRandom()].map(signer=>signer.address);
    for(const account of extra)await registry.setBackupCommitter(account,true);
    expect(await registry.backupCommitterCount()).to.equal(4n);
    await expect(registry.setBackupCommitter(ethers.Wallet.createRandom().address,true)).to.be.revertedWithCustomError(registry,"InvalidConfig");
    await expect(registry.setBackupCommitter(second.address,false)).to.emit(registry,"BackupCommitterSet").withArgs(second.address,false);
    expect([await registry.isBackupCommitter(second.address),await registry.backupCommitterCount()]).to.deep.equal([false,3n]);
    // The primary committer and the stored roles are otherwise unchanged.
    expect(await registry.committer()).to.equal(committer.address);
  });

  it("lets a backup publish the selected source and a fallback with the committer's rules, until it is removed",async()=>{
    const {registry,committer,backup,stranger}=await networkHelpers.loadFixture(fixture);
    await expect(as(registry,backup).commitEpoch(1,await epochAttestation(registry,1n,ethers))).to.be.revertedWithCustomError(registry,"OnlyCommitter");
    await registry.setBackupCommitter(backup.address,true);
    await as(registry,backup).commitEpoch(1,await epochAttestation(registry,1n,ethers));
    expect((await registry.getEpoch(1)).epochHash).not.to.equal(ethers.ZeroHash);
    await expect(as(registry,committer).commitEpoch(1,await epochAttestation(registry,1n,ethers))).to.be.revertedWithCustomError(registry,"AlreadyCommitted");
    // Fallback windows, freshness and signatures are identical for both.
    await networkHelpers.mine(Number(await registry.epochStart(2))-await ethers.provider.getBlockNumber());
    const fallback=await epochAttestation(registry,2n,ethers,undefined,1);
    await expect(as(registry,backup).commitEpochFallback(2,1,fallback)).to.be.revertedWithCustomError(registry,"FallbackNotOpen");
    await networkHelpers.mine(20);
    await expect(as(registry,backup).commitEpochFallback(2,1,{...fallback,timestamp:fallback.timestamp-241n})).to.be.revertedWithCustomError(registry,"InvalidTime");
    await expect(as(registry,backup).commitEpochFallback(2,1,{...fallback,signature:(await epochAttestation(registry,2n,ethers)).signature})).to.be.revertedWithCustomError(registry,"InvalidSigner");
    await as(registry,backup).commitEpochFallback(2,1,await epochAttestation(registry,2n,ethers,undefined,1));
    await registry.setBackupCommitter(backup.address,false);
    await networkHelpers.mine(Number(await registry.epochStart(3))-await ethers.provider.getBlockNumber());
    await expect(as(registry,backup).commitEpoch(3,await epochAttestation(registry,3n,ethers))).to.be.revertedWithCustomError(registry,"OnlyCommitter");
    await expect(as(registry,stranger).commitEpoch(3,await epochAttestation(registry,3n,ethers))).to.be.revertedWithCustomError(registry,"OnlyCommitter");
    await as(registry,committer).commitEpoch(3,await epochAttestation(registry,3n,ethers));
  });

  it("keeps primary rotation through setCommitter unchanged",async()=>{
    const {registry,committer,backup,second}=await networkHelpers.loadFixture(fixture);
    await registry.setBackupCommitter(backup.address,true);
    await expect(registry.setCommitter(second.address)).to.emit(registry,"CommitterChanged").withArgs(committer.address,second.address);
    await expect(as(registry,committer).commitEpoch(1,await epochAttestation(registry,1n,ethers))).to.be.revertedWithCustomError(registry,"OnlyCommitter");
    await as(registry,second).commitEpoch(1,await epochAttestation(registry,1n,ethers));
    await networkHelpers.mine(Number(await registry.epochStart(2))-await ethers.provider.getBlockNumber());
    expect(await registry.isBackupCommitter(backup.address)).to.equal(true);
    await as(registry,backup).commitEpoch(2,await epochAttestation(registry,2n,ethers));
    // A backup can be promoted; removing its backup entry afterwards stays possible.
    await registry.setCommitter(backup.address);
    await registry.setBackupCommitter(backup.address,false);
    expect([await registry.committer(),await registry.backupCommitterCount()]).to.deep.equal([backup.address,0n]);
  });

  it("pays the keeper share to the wallet that submitted the proof when the registry authorizes it",async()=>{
    const {registry,rng,consumer,committer,backup,stranger,user}=await networkHelpers.loadFixture(fixture);
    await registry.setBackupCommitter(backup.address,true);
    const ids:bigint[]=[];
    for(const label of ["primary","backup","stranger","removed","contract"]){
      await consumer.request(ethers.id(label),200000,user.address,{value:FEE});
      ids.push(await consumer.lastRequestId() as bigint);
    }
    await as(registry,committer).commitEpoch(1,await epochAttestation(registry,1n,ethers));
    await networkHelpers.mine(2);
    const fulfill=async(id:bigint,submitter:any)=>{
      const proof=makeProof(await rng.requestSeed(id));
      const receipt=(await(await (rng.connect(submitter) as any).fulfillRandomness(id,proof,{gasLimit:2_000_000})).wait())!;
      expect(await consumer.results(id)).to.equal(proofOutput(proof));
      const paid=receipt.logs.map((log:any)=>rng.interface.parseLog(log)).find((event:any)=>event?.name==="KeeperFeePaid")!;
      return [paid.args.keeper,paid.args.amount,paid.args.paid];
    };
    // Each authorized keeper is paid for what it serves; any other submitter's fulfillment still pays committer().
    const committerBalance=await ethers.provider.getBalance(committer.address),backupBalance=await ethers.provider.getBalance(backup.address);
    expect(await fulfill(ids[0],committer)).to.deep.equal([committer.address,FEE/2n,true]);
    const backupReceipt=(await(await (rng.connect(backup) as any).fulfillRandomness(ids[1],makeProof(await rng.requestSeed(ids[1])),{gasLimit:2_000_000})).wait())!;
    const backupPaid=backupReceipt.logs.map((log:any)=>rng.interface.parseLog(log)).find((event:any)=>event?.name==="KeeperFeePaid")!;
    expect([backupPaid.args.keeper,backupPaid.args.amount,backupPaid.args.paid]).to.deep.equal([backup.address,FEE/2n,true]);
    expect(await ethers.provider.getBalance(backup.address)).to.equal(backupBalance+FEE/2n-BigInt(backupReceipt.gasUsed)*BigInt(backupReceipt.gasPrice));
    expect(await fulfill(ids[2],stranger)).to.deep.equal([committer.address,FEE/2n,true]);
    // The committer received the share of its own fulfillment and of the stranger's, and none of the backup's.
    expect(await ethers.provider.getBalance(committer.address)).to.be.greaterThan(committerBalance+FEE/2n);
    // Removing a backup returns its fulfillments to paying committer().
    await registry.setBackupCommitter(backup.address,false);
    expect(await fulfill(ids[3],backup)).to.deep.equal([committer.address,FEE/2n,true]);
    // An authorized submitter that cannot receive keeps its share as keeper credit.
    const forwarder=await ethers.deployContract("FulfillmentForwarder"),forwarderAddress=await forwarder.getAddress();
    await registry.setBackupCommitter(forwarderAddress,true);
    const proof=makeProof(await rng.requestSeed(ids[4]));
    const forwarded=(await(await forwarder.forward(await rng.getAddress(),ids[4],proof)).wait())!;
    const credited=forwarded.logs.filter((log:any)=>log.address===rng.target).map((log:any)=>rng.interface.parseLog(log)).find((event:any)=>event?.name==="KeeperFeePaid")!;
    expect([credited.args.keeper,credited.args.amount,credited.args.paid]).to.deep.equal([forwarderAddress,FEE/2n,false]);
    expect(await rng.keeperCredits(forwarderAddress)).to.equal(FEE/2n);
    expect(await consumer.results(ids[4])).to.equal(proofOutput(proof));
  });

  it("pays the keeper share to committer() when a backup published the epoch and a third party fulfilled the request",async()=>{
    const {registry,rng,consumer,committer,backup,stranger,user}=await networkHelpers.loadFixture(fixture);
    await registry.setBackupCommitter(backup.address,true);
    await consumer.request(ethers.id("backup-published"),200000,user.address,{value:FEE});
    const id=await consumer.lastRequestId();
    await as(registry,backup).commitEpoch(1,await epochAttestation(registry,1n,ethers));
    await networkHelpers.mine(2);
    const proof=makeProof(await rng.requestSeed(id));
    const committerBalance=await ethers.provider.getBalance(committer.address),backupBalance=await ethers.provider.getBalance(backup.address);
    const receipt=(await(await rng.connect(stranger).getFunction("fulfillRandomness")(id,proof,{gasLimit:2_000_000})).wait())!;
    const paid=receipt.logs.map((log:any)=>rng.interface.parseLog(log)).find((event:any)=>event?.name==="KeeperFeePaid");
    expect([paid!.args.keeper,paid!.args.amount,paid!.args.paid]).to.deep.equal([committer.address,FEE/2n,true]);
    expect(await ethers.provider.getBalance(committer.address)).to.equal(committerBalance+FEE/2n);
    expect(await ethers.provider.getBalance(backup.address)).to.equal(backupBalance);
    expect(await consumer.results(id)).to.equal(proofOutput(proof));
  });
});
