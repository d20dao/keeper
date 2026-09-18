import {deployProxy} from "./helpers/proxy.ts";
import { EPOCH_TEST_WALLETS as TEST_API_WALLETS,epochFixtureData,deployReadyEpochFixture } from "./helpers/epoch.ts";
import { attestationDigest } from "../src/sources.ts";
async function epochAttestation(registry:any,epochId:bigint,body?:string) {
  const s=await registry.getEpochSelection(epochId);
  const data=ethers.hexlify(ethers.toUtf8Bytes(body??JSON.stringify(epochFixtureData(Number(s.recipe)))));
  const a={timestamp:BigInt((await ethers.provider.getBlock("latest"))!.timestamp),data,signature:"0x"};
  a.signature=await TEST_API_WALLETS[Number(s.source)].signMessage(ethers.getBytes(attestationDigest(s.queryHash,a)));
  return a;
}
import { expect } from "chai";
import { network } from "hardhat";
import { makeProof, proofOutput, publicKey, TEST_SECRET } from "./helpers/proof.ts";

const FEE = 10n ** 15n; // Native 18-decimal units; no production price claim.
const CALLBACK_GAS = 200_000;
const { ethers, networkHelpers } = await network.create();
const seed = ethers.id("locked-mining-proof");

async function fixture() {
  return deployReadyEpochFixture(ethers,networkHelpers,FEE);
}
type Fixture = Awaited<ReturnType<typeof fixture>>;
async function request(ctx: Fixture, gas = CALLBACK_GAS, refund?: string) {
  await ctx.consumer.request(seed, gas, refund ?? ctx.user.address, { value: FEE });
  return await ctx.consumer.lastRequestId();
}
async function ready(ctx: Fixture, id: bigint) {
  await networkHelpers.mine(2);
  return makeProof(await ctx.rng.requestSeed(id));
}

describe("D20VRFCoordinator", function () {
  it("requires valid immutable signers, publisher, registry, VRF key and confirmation settings",async()=>{
    const [owner]=await ethers.getSigners();
    const signers=TEST_API_WALLETS.map(w=>w.address);
    await expect(deployProxy(ethers,"EpochEntropy",[[ethers.ZeroAddress,signers[1],signers[2],signers[3]],owner.address,owner.address])).to.revert(ethers);
    await expect(deployProxy(ethers,"EpochEntropy",[signers,owner.address,ethers.ZeroAddress])).to.revert(ethers);
    const registry=await deployProxy(ethers,"EpochEntropy",[signers,owner.address,owner.address]);
    for(const args of [
      [[1,1],owner.address,owner.address,FEE,1,await registry.getAddress(),0],
      [publicKey(),ethers.ZeroAddress,owner.address,FEE,1,await registry.getAddress(),0],
      [publicKey(),owner.address,ethers.ZeroAddress,FEE,1,await registry.getAddress(),0],
      [publicKey(),owner.address,owner.address,FEE,0,await registry.getAddress(),0],
      [publicKey(),owner.address,owner.address,FEE,65,await registry.getAddress(),0],
      [publicKey(),owner.address,owner.address,FEE,1,ethers.ZeroAddress,0],
      [publicKey(),owner.address,owner.address,FEE,1,await registry.getAddress(),10001],
      [publicKey(),owner.address,owner.address,10n**19n+1n,1,await registry.getAddress(),0],
    ])await expect(deployProxy(ethers,"D20VRFCoordinator",args)).to.revert(ethers);
  });
  it("rejects underpayment, credits overpayment to the refund address, and requires contract consumers, bounded callback gas and a refund address", async function () {
    const c = await networkHelpers.loadFixture(fixture);
    for (const value of [0n, FEE - 1n]) {
      await expect(c.consumer.request(seed, CALLBACK_GAS, c.user.address, { value }))
        .to.be.revertedWithCustomError(c.rng, "IncorrectFee").withArgs(FEE, value);
    }
    await expect(c.rng.requestRandomness(seed, CALLBACK_GAS, c.user.address, { value: FEE }))
      .to.be.revertedWithCustomError(c.rng, "ContractConsumerRequired");
    for (const gas of [29_999, 1_000_001]) {
      await expect(c.consumer.request(seed, gas, c.user.address, { value: FEE }))
        .to.be.revertedWithCustomError(c.rng, "InvalidCallbackGas");
    }
    await expect(c.consumer.request(seed, CALLBACK_GAS, ethers.ZeroAddress, { value: FEE }))
      .to.be.revertedWithCustomError(c.rng, "InvalidRefundAddress");
    await request(c);
    expect(await c.rng.earnedFees()).to.equal(0);
    expect(await ethers.provider.getBalance(await c.rng.getAddress())).to.equal(FEE);
    await expect(c.consumer.request(seed, CALLBACK_GAS, c.stranger.address, { value: FEE + 1n }))
      .to.emit(c.rng, "FeeOverpaymentCredited").withArgs(2n, c.stranger.address, 1n);
    expect(await c.rng.requestFeePaid(2n)).to.equal(FEE);
    expect(await c.rng.refundCredits(c.stranger.address)).to.equal(1n);
    expect(await ethers.provider.getBalance(await c.rng.getAddress())).to.equal(2n * FEE + 1n);
  });

  it("fulfills a real secp256k1 proof and delivers the callback in the same transaction", async function () {
    const c = await networkHelpers.loadFixture(fixture);
    const id = await request(c);
    await expect(c.rng.requestSeed(id)).to.be.revertedWithCustomError(c.rng, "NotReady");
    const proof = await ready(c, id);
    await expect(c.rng.connect(c.stranger).getFunction("fulfillRandomness")(id, proof, { gasLimit: 2_000_000 }))
      .to.emit(c.rng, "RandomnessFulfilled").withArgs(id, proofOutput(proof), c.stranger.address)
      .and.to.emit(c.rng, "CallbackAttempted").withArgs(id, true, CALLBACK_GAS);
    expect(await c.consumer.results(id)).to.equal(proofOutput(proof));
    expect((await c.rng.getRequest(id)).delivered).to.equal(true);
    expect(await c.rng.earnedFees()).to.equal(FEE);
    await expect(c.rng.fulfillRandomness(id, proof)).to.be.revertedWithCustomError(c.rng, "AlreadyFulfilled");
    await expect(c.rng.retryCallback(id, CALLBACK_GAS)).to.be.revertedWithCustomError(c.rng, "AlreadyDelivered");
  });

  it("different proof nonces give the same unique randomness", async function () {
    const c = await networkHelpers.loadFixture(fixture);
    const id = await request(c);
    const base = await ready(c, id);
    const input = await c.rng.requestSeed(id);
    const a = makeProof(input, TEST_SECRET, 1n);
    const b = makeProof(input, TEST_SECRET, 2n);
    expect(a.c).not.to.equal(b.c);
    expect(proofOutput(a)).to.equal(proofOutput(b));
    await c.rng.fulfillRandomness.staticCall(id, a, { gasLimit: 2_000_000 });
    await c.rng.fulfillRandomness(id, b, { gasLimit: 2_000_000 });
  });

  it("rejects wrong keys, seed, malformed proofs and replay across requests/coordinators", async function () {
    const c = await networkHelpers.loadFixture(fixture);
    const id = await request(c);
    const proof = await ready(c, id);
    await expect(c.rng.fulfillRandomness(id, makeProof(proof.seed, TEST_SECRET + 1n)))
      .to.be.revertedWithCustomError(c.rng, "WrongPublicKey");
    await expect(c.rng.fulfillRandomness(id, { ...proof, seed: proof.seed + 1n }))
      .to.be.revertedWithCustomError(c.rng, "WrongSeed");
    for (const altered of [
      { ...proof, gamma: [1n, 1n] }, { ...proof, c: proof.c + 1n },
      { ...proof, s: proof.s + 1n }, { ...proof, zInv: 0n },
      { ...proof, uWitness: ethers.ZeroAddress }, { ...proof, cGammaWitness: [1n, 1n] },
      { ...proof, sHashWitness: [1n, 1n] },
    ]) await expect(c.rng.fulfillRandomness(id, altered, { gasLimit: 2_000_000 })).to.revert(ethers);
    expect((await c.rng.getRequest(id)).fulfilled).to.equal(false);
    expect(await c.rng.earnedFees()).to.equal(0);
    const otherId = await request(c);
    await ready(c, otherId);
    await expect(c.rng.fulfillRandomness(otherId, proof)).to.be.revertedWithCustomError(c.rng, "WrongSeed");
    const other = await fixture();
    const sameId = await request(other);
    const otherProof = await ready(other, sameId);
    expect(await other.rng.requestSeed(sameId)).not.to.equal(proof.seed);
    await expect(other.rng.fulfillRandomness(sameId, proof)).to.be.revertedWithCustomError(other.rng, "WrongSeed");
  });

  it("authenticates callbacks and rejects unknown requests", async function () {
    const c = await networkHelpers.loadFixture(fixture);
    await expect(c.consumer.rawFulfillRandomness(1, seed)).to.be.revertedWithCustomError(c.consumer, "OnlyCoordinator");
    await expect(c.rng.getRequest(999)).to.be.revertedWithCustomError(c.rng, "UnknownRequest");
    await expect(c.rng.refundRequest(999)).to.be.revertedWithCustomError(c.rng, "UnknownRequest");
    const id = await request(c);
    await expect(c.rng.retryCallback(id, CALLBACK_GAS)).to.be.revertedWithCustomError(c.rng, "NotFulfilled");
  });

  for (const [mode, title] of [[1, "revert"], [2, "out-of-gas"], [3, "return-data bomb"]] as const) {
    it(`keeps proof and fee when callback has ${title}; retries the same result after deadline`, async function () {
      const c = await networkHelpers.loadFixture(fixture);
      const id = await request(c);
      const proof = await ready(c, id);
      await c.consumer.setMode(mode);
      await c.rng.fulfillRandomness(id, proof, { gasLimit: 2_000_000 });
      const r = await c.rng.getRequest(id);
      expect(r.fulfilled).to.equal(true);
      expect(r.delivered).to.equal(false);
      expect(r.randomness).to.equal(proofOutput(proof));
      expect(await c.rng.earnedFees()).to.equal(FEE);
      await networkHelpers.time.increase(61);
      await expect(c.rng.refundRequest(id)).to.be.revertedWithCustomError(c.rng, "RefundNotAvailable");
      await c.consumer.setMode(0);
      await c.rng.connect(c.stranger).getFunction("retryCallback")(id, 300_000, { gasLimit: 500_000 });
      expect(await c.consumer.results(id)).to.equal(proofOutput(proof));
      expect(await c.consumer.callbackCount()).to.equal(1);
      expect(await c.rng.earnedFees()).to.equal(FEE);
    });
  }

  it("blocks callback reentry and does not create a second request", async function () {
    const c = await networkHelpers.loadFixture(fixture);
    const id = await request(c);
    const proof = await ready(c, id);
    await c.consumer.setMode(4);
    await c.rng.fulfillRandomness(id, proof, { gasLimit: 2_000_000 });
    expect(await c.consumer.reentrySucceeded()).to.equal(false);
    expect(await c.rng.nextRequestId()).to.equal(2);
    expect((await c.rng.getRequest(id)).delivered).to.equal(true);
  });

  it("insufficient fulfillment gas cannot falsely record a callback failure and earn a fee", async function () {
    const c = await networkHelpers.loadFixture(fixture);
    const id = await request(c, 1_000_000);
    const proof = await ready(c, id);
    await expect(c.rng.fulfillRandomness(id, proof, { gasLimit: 500_000 })).to.revert(ethers);
    expect((await c.rng.getRequest(id)).fulfilled).to.equal(false);
    expect(await c.rng.earnedFees()).to.equal(0);
  });

  it("refunds only after the deadline, only to the pinned address, and never twice", async function () {
    const c = await networkHelpers.loadFixture(fixture);
    const id = await request(c);
    const proof = await ready(c, id);
    const r = await c.rng.getRequest(id);
    await networkHelpers.time.setNextBlockTimestamp(r.deadline);
    await expect(c.rng.refundRequest(id)).to.be.revertedWithCustomError(c.rng, "RefundNotAvailable");
    await networkHelpers.time.setNextBlockTimestamp(r.deadline + 1n);
    const before = await ethers.provider.getBalance(c.user.address);
    await c.rng.connect(c.stranger).getFunction("refundRequest")(id, { gasLimit: 500000 });
    expect(await ethers.provider.getBalance(c.user.address)).to.equal(before + FEE);
    expect((await c.rng.getRequest(id)).refunded).to.equal(true);
    expect(await c.rng.earnedFees()).to.equal(0);
    await expect(c.rng.refundRequest(id)).to.be.revertedWithCustomError(c.rng, "RefundNotAvailable");
    await expect(c.rng.fulfillRandomness(id, proof)).to.be.revertedWithCustomError(c.rng, "RequestRefunded");
  });

  it("accepts valid fulfillment exactly at 60 seconds", async function () {
    const c = await networkHelpers.loadFixture(fixture);
    const id = await request(c);
    const proof = await ready(c, id);
    await networkHelpers.time.setNextBlockTimestamp((await c.rng.getRequest(id)).deadline);
    await c.rng.fulfillRandomness(id, proof, { gasLimit: 2_000_000 });
    expect((await c.rng.getRequest(id)).fulfilled).to.equal(true);
  });

  it("rejects late fulfillment even before anyone has requested a refund", async function () {
    const c = await networkHelpers.loadFixture(fixture);
    const id = await request(c);
    const proof = await ready(c, id);
    await networkHelpers.time.increaseTo((await c.rng.getRequest(id)).deadline + 1n);
    await expect(c.rng.fulfillRandomness(id, proof)).to.be.revertedWithCustomError(c.rng, "RequestExpired");
    await c.rng.refundRequest(id, { gasLimit: 500000 });
  });

  it("backs a rejected refund with withdrawable credit that nobody else can redirect", async function () {
    const c = await networkHelpers.loadFixture(fixture);
    const rejector = await ethers.deployContract("RejectingRefundRecipient");
    const address = await rejector.getAddress();
    const id = await request(c, CALLBACK_GAS, address);
    await networkHelpers.time.increase(61);
    await c.rng.refundRequest(id, { gasLimit: 500000 });
    expect(await c.rng.refundCredits(address)).to.equal(FEE);
    expect(await c.rng.totalRefundCredits()).to.equal(FEE);
    await expect(c.rng.connect(c.stranger).getFunction("withdrawRefundCredit")(c.stranger.address))
      .to.be.revertedWithCustomError(c.rng, "NoRefundCredit");
    await c.rng.withdrawFees(c.owner.address);
    expect(await ethers.provider.getBalance(await c.rng.getAddress())).to.equal(FEE);
    const before = await ethers.provider.getBalance(c.user.address);
    await rejector.withdraw(await c.rng.getAddress(), c.user.address);
    expect(await ethers.provider.getBalance(c.user.address)).to.equal(before + FEE);
    expect(await c.rng.totalRefundCredits()).to.equal(0);
    expect(await ethers.provider.getBalance(await c.rng.getAddress())).to.equal(0);
  });

  it("withdraws earned fees without spending pending request escrow", async function () {
    const c = await networkHelpers.loadFixture(fixture);
    const id = await request(c);
    const proof = await ready(c, id);
    await request(c);
    await c.rng.fulfillRandomness(id, proof, { gasLimit: 2_000_000 });
    await expect(c.rng.connect(c.stranger).getFunction("withdrawFees")(c.stranger.address))
      .to.be.revertedWithCustomError(c.rng, "OnlyFeeRecipient");
    await c.rng.withdrawFees(c.owner.address);
    expect(await ethers.provider.getBalance(await c.rng.getAddress())).to.equal(FEE);
    expect(await c.rng.earnedFees()).to.equal(0);
  });


});

describe("Mining game callback integration", function () {
  it("request + exact payment occur in one game tx, callback reveals three stable candidate seeds", async function () {
    const c = await networkHelpers.loadFixture(fixture);
    const game = await ethers.deployContract("TestMiningGame", [await c.rng.getAddress()]);
    await game.submitVerifiedClaim(0, seed, c.user.address, { value: FEE });
    const id = (await game.claimRandomness(0)).requestId;
    await expect(game.candidateSeed(0, 0)).to.be.revertedWithCustomError(game, "ClaimNotReady");
    await expect(game.submitVerifiedClaim(0, seed, c.user.address, { value: FEE }))
      .to.be.revertedWithCustomError(game, "ClaimAlreadyRequested");
    const proof = await ready(c, id);
    await expect(c.rng.fulfillRandomness(id, proof, { gasLimit: 2_000_000 }))
      .to.emit(game, "ClaimReady").withArgs(0, id, proofOutput(proof));
    expect((await game.claimRandomness(0)).ready).to.equal(true);
    const cards = await Promise.all([0, 1, 2].map(i => game.candidateSeed(0, i)));
    expect(new Set(cards).size).to.equal(3);
    await networkHelpers.mine(3);
    expect(await game.candidateSeed(0, 0)).to.equal(cards[0]);
    await expect(game.candidateSeed(0, 3)).to.be.revertedWithCustomError(game, "InvalidCandidate");
  });
});


import { builtins } from "../src/mapping.ts";
import { deriveRequestSeed } from "../src/verification.ts";
import { BUILTIN_EPOCH_RECIPES, resolveEpochCatalog } from "../src/epoch.ts";
import { PROVIDER_TEST_WALLETS, providerTestCatalog, epochAttestation as signedEpochAttestation } from "./helpers/epoch.ts";
import { epochCatalogHash, selectEpoch, verifyEpochAttestation, replayEpochCommitment, epochProtocolConfigurationHash,
  replayEpochCoordinator, encodeEpochEvidencePacket, decodeEpochEvidencePacket, type EpochCatalog, type EpochRequestContext } from "../src/epoch.ts";
async function registryFixture() {
  const [owner,user]=await ethers.getSigners();
  const registry=await deployProxy(ethers,"EpochEntropy",[[TEST_API_WALLETS[0].address,TEST_API_WALLETS[1].address,TEST_API_WALLETS[2].address,TEST_API_WALLETS[3].address],owner.address,owner.address]);
  const rng=await deployProxy(ethers,"D20VRFCoordinator",[publicKey(),owner.address,owner.address,FEE,1,await registry.getAddress(),0]);
  await rng.setPricing(FEE,0,300000);
  const consumer=await ethers.deployContract("TestConsumer",[await rng.getAddress()]);
  return {owner,user,registry,rng,consumer};
}
describe("On-demand epoch publication and replay",function(){
  it("escrows the first request before publication, fixes a future target and never overwrites the epoch",async function(){
    const c=await networkHelpers.loadFixture(registryFixture);
    await expect(c.consumer.request(seed,CALLBACK_GAS,c.user.address,{value:FEE})).to.be.revertedWithCustomError(c.rng,"EpochUnavailable");
    expect(await c.rng.nextRequestId()).to.equal(1n);
    expect(await ethers.provider.getBalance(await c.rng.getAddress())).to.equal(0n);
    const first=await c.registry.firstEpochStart();
    expect(await c.registry.nextEpochToPrepare(first-1n)).to.equal(0n);
    await networkHelpers.mine(Number(first)-await ethers.provider.getBlockNumber());
    const a=await epochAttestation(c.registry,1n);
    const receipt=(await(await c.consumer.request(seed,CALLBACK_GAS,c.user.address,{value:FEE})).wait())!;
    const id=await c.consumer.lastRequestId();
    const pending=await c.rng.getRequest(id);
    expect(pending.requestBlock).to.equal(receipt.blockNumber);
    expect(pending.targetBlock).to.equal(0n);expect(pending.epochHash).to.equal(ethers.ZeroHash);
    expect(await ethers.provider.getBalance(await c.rng.getAddress())).to.equal(FEE);
    expect(await c.rng.earnedFees()).to.equal(0n);
    expect(await c.registry.epochAnchors(1)).to.equal((await ethers.provider.getBlock(Number(first-1n)))!.hash);
    await networkHelpers.mine(2);
    await expect(c.rng.requestSeed(id)).to.be.revertedWithCustomError(c.rng,"NotReady");
    await expect(c.registry.connect(c.user).getFunction("commitEpoch")(1,a)).to.be.revertedWithCustomError(c.registry,"OnlyCommitter");
    const committed=(await(await c.registry.commitEpoch(1,a)).wait())!;
    const resolved=await c.rng.getRequest(id);
    expect(resolved.targetBlock).to.equal(BigInt(committed.blockNumber)+1n);
    expect(resolved.epochHash).to.equal((await c.registry.getEpoch(1)).epochHash);
    expect(resolved.deadline).to.equal(pending.deadline);
    await expect(c.rng.requestSeed(id)).to.be.revertedWithCustomError(c.rng,"NotReady");
    await networkHelpers.mine();
    await expect(c.rng.requestSeed(id)).to.be.revertedWithCustomError(c.rng,"NotReady");
    await networkHelpers.mine();
    await c.rng.fulfillRandomness(id,makeProof(await c.rng.requestSeed(id)),{gasLimit:2_000_000});
    await expect(c.registry.commitEpoch(1,a)).to.be.revertedWithCustomError(c.registry,"AlreadyCommitted");
    expect((await c.rng.getRequest(id)).fulfilled).to.equal(true);
  });
  it("shares one snapshot among an escrowed batch and refunds an unpublished expired request",async()=>{
    const c=await networkHelpers.loadFixture(registryFixture);
    await networkHelpers.mine(Number(await c.registry.firstEpochStart())-await ethers.provider.getBlockNumber());
    const saved=await epochAttestation(c.registry,1n);
    for(let n=0;n<3;n++)await c.consumer.request(ethers.id(`batch-${n}`),CALLBACK_GAS,c.user.address,{value:FEE});
    expect((await c.registry.getEpoch(1)).epochHash).to.equal(ethers.ZeroHash);
    await c.registry.commitEpoch(1,saved);await networkHelpers.mine(2);
    const requests=await Promise.all([1,2,3].map(id=>c.rng.getRequest(id)));
    expect(new Set(requests.map(r=>r.epochHash)).size).to.equal(1);
    expect(new Set(requests.map(r=>r.targetBlock.toString())).size).to.equal(1);
    const seeds=await Promise.all([1,2,3].map(id=>c.rng.requestSeed(id)));
    expect(new Set(seeds).size).to.equal(3);
    for(let id=1;id<=3;id++)await c.rng.fulfillRandomness(id,makeProof(seeds[id-1]),{gasLimit:2_000_000});
    await networkHelpers.mine(Number(await c.registry.epochStart(2))-await ethers.provider.getBlockNumber());
    await c.consumer.request(seed,CALLBACK_GAS,c.user.address,{value:FEE});const pending=await c.rng.getRequest(4);
    await networkHelpers.time.increaseTo(pending.deadline+1n);
    await c.rng.refundRequest(4);expect((await c.rng.getRequest(4)).refunded).to.equal(true);
    expect((await c.registry.getEpoch(2)).epochHash).to.equal(ethers.ZeroHash);
  });
  it("rejects altered data, full bodies, wrong signer/query, stale/future and noncanonical signatures",async function(){
    const c=await networkHelpers.loadFixture(registryFixture);
    await networkHelpers.mine(Number(await c.registry.firstEpochStart())-await ethers.provider.getBlockNumber());
    const a=await epochAttestation(c.registry,1n);
    await expect(c.registry.commitEpoch(1,{...a,data:ethers.hexlify(ethers.toUtf8Bytes('{}'))})).to.be.revertedWithCustomError(c.registry,"InvalidData");
    const s=await c.registry.getEpochSelection(1);
    const wrong=await c.owner.signMessage(ethers.getBytes(attestationDigest(s.queryHash,a)));
    await expect(c.registry.commitEpoch(1,{...a,signature:wrong})).to.be.revertedWithCustomError(c.registry,"InvalidSigner");
    const query=await TEST_API_WALLETS[Number(s.source)].signMessage(ethers.getBytes(attestationDigest(ethers.ZeroHash,a)));
    await expect(c.registry.commitEpoch(1,{...a,signature:query})).to.be.revertedWithCustomError(c.registry,"InvalidSigner");
    await expect(c.registry.commitEpoch(1,{...a,timestamp:a.timestamp+1000n})).to.be.revertedWithCustomError(c.registry,"InvalidTime");
    await expect(c.registry.commitEpoch(1,{...a,timestamp:a.timestamp-241n})).to.be.revertedWithCustomError(c.registry,"InvalidTime");
    await expect(c.registry.commitEpoch(1,{...a,signature:a.signature.slice(0,-2)})).to.revert(ethers);
    const badBodies=s.source===0n?['{"symbol":"ETH","value":"1"}','{"symbol":"BTC","value":"01"}','{"symbol":"BTC","value":"-1"}']
      :['{"id":null,"jsonrpc":"2.0","result":"0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdeF"}'];
    for(const body of badBodies) await expect(c.registry.commitEpoch(1,await epochAttestation(c.registry,1n,body))).to.be.revertedWithCustomError(c.registry,"InvalidData");
  });
  it("accepts an attestation exactly 240 seconds old onchain and in replay, and rejects 241",async function(){
    const c=await networkHelpers.loadFixture(registryFixture);
    await networkHelpers.mine(Number(await c.registry.firstEpochStart())-await ethers.provider.getBlockNumber());
    const s=await c.registry.getEpochSelection(1),selected={source:Number(s.source),recipe:Number(s.recipe),queryHash:s.queryHash,airnode:s.airnode,template:BUILTIN_EPOCH_RECIPES[Number(s.recipe)].template} as ReturnType<typeof selectEpoch>;
    const data=ethers.hexlify(ethers.toUtf8Bytes(JSON.stringify(epochFixtureData(selected.recipe))));
    const sign=async(timestamp:bigint)=>{const a={timestamp,data,signature:"0x"};a.signature=await TEST_API_WALLETS[selected.source].signMessage(ethers.getBytes(attestationDigest(s.queryHash,a)));return a;};
    const at=BigInt(await networkHelpers.time.latest())+1000n,stale=await sign(at-241n),fresh=await sign(at+1n-240n);
    expect(()=>verifyEpochAttestation(selected,stale,at)).to.throw("Invalid epoch attestation time");
    expect(verifyEpochAttestation(selected,fresh,at+1n).signer.toLowerCase()).to.equal(s.airnode.toLowerCase());
    await networkHelpers.time.setNextBlockTimestamp(at);
    await expect(c.registry.commitEpoch(1,stale,{gasLimit:500000})).to.be.revertedWithCustomError(c.registry,"InvalidTime");
    await networkHelpers.time.setNextBlockTimestamp(at+1n);
    await c.registry.commitEpoch(1,fresh,{gasLimit:500000});
    expect((await c.registry.getEpoch(1)).signedAt).to.equal(fresh.timestamp);
    expect(await c.registry.MAX_ATTESTATION_AGE()).to.equal(240n);
  });
  it("archives one epoch packet, fulfills across rotation and replays immutable epoch binding",async function(){
    const c=await networkHelpers.loadFixture(registryFixture);
    const firstStart=await c.registry.firstEpochStart();
    await networkHelpers.mine(Number(firstStart+198n)-await ethers.provider.getBlockNumber());
    const a=await epochAttestation(c.registry,1n),selection=await c.registry.getEpochSelection(1);
    const requestReceipt=(await(await c.consumer.requestMapped(seed,CALLBACK_GAS,c.user.address,builtins.d20(),{value:FEE})).wait())!;
    const id=await c.consumer.lastRequestId();expect((await c.rng.getRequest(id)).epochHash).to.equal(ethers.ZeroHash);
    // A live request checkpoints epoch 1 so its unchanged snapshot can publish after rotation.
    const commitReceipt=(await(await c.registry.commitEpoch(1,a)).wait())!;
    const record=await c.registry.getEpoch(1),first=await c.registry.firstEpochStart();
    const catalog:EpochCatalog={signers:[TEST_API_WALLETS[0].address,TEST_API_WALLETS[1].address,TEST_API_WALLETS[2].address,TEST_API_WALLETS[3].address],registry:await c.registry.getAddress(),chainId:(await ethers.provider.getNetwork()).chainId,firstEpochStart:first};
    expect(epochCatalogHash(catalog.signers)).to.equal(await c.registry.catalogHash());
    expect(selectEpoch(catalog,1n,record.anchorHash).canonicalRequest).to.equal(selection.canonicalRequest);
    const packet=encodeEpochEvidencePacket(selection.canonicalRequest,a);
    expect(decodeEpochEvidencePacket(packet).attestation).to.deep.equal(a);
    expect(()=>decodeEpochEvidencePacket(packet+'00')).to.throw();
    const commitTimestamp=BigInt((await ethers.provider.getBlock(commitReceipt.blockNumber))!.timestamp);
    const epoch={catalog,record,commitTimestamp,packet};
    expect(replayEpochCommitment({...epoch,epochId:1n}).epochHash).to.equal(record.epochHash);
    const r=await c.rng.getRequest(id);
    expect(r.requestBlock).to.equal(first+199n);
    expect(r.targetBlock).to.equal(BigInt(commitReceipt.blockNumber)+1n);
    expect(r.epochId).to.equal(1n);expect(r.epochHash).to.equal(record.epochHash);
    await networkHelpers.mine(2);
    const requestBlock=(await ethers.provider.getBlock(requestReceipt.blockNumber))!;
    const targetBlock=(await ethers.provider.getBlock(Number(r.targetBlock)))!;
    const context:EpochRequestContext={chainId:catalog.chainId,coordinator:await c.rng.getAddress(),keyHash:await c.rng.keyHash(),requestId:id,
      consumer:await c.consumer.getAddress(),clientSeed:seed,mapping:builtins.d20(),requestBlock:r.requestBlock,targetBlock:r.targetBlock,blockHash:targetBlock.hash!,epochId:1n,epochHash:r.epochHash};
    const proof=makeProof(await c.rng.requestSeed(id)); expect(deriveRequestSeed(context)).to.equal(proof.seed);
    const receipt=(await(await c.rng.fulfillRandomness(id,proof,{gasLimit:2_000_000})).wait())!,recorded=await c.rng.getRequest(id);
    const configuration={publicKey:publicKey(),feeRecipient:c.owner.address,initialMinFee:FEE,confirmationBlocks:1,registry:catalog.registry,catalogHash:record.catalogHash,firstEpochStart:first};
    const protocolConfigurationHash=epochProtocolConfigurationHash(configuration); expect(protocolConfigurationHash).to.equal(await c.rng.protocolConfigurationHash());
    const input={context,configuration,protocolConfigurationHash,epoch,requestedAt:BigInt(requestBlock.timestamp),deadline:r.deadline,
      acceptanceTimestamp:BigInt((await ethers.provider.getBlock(receipt.blockNumber))!.timestamp),acceptanceBlock:BigInt(receipt.blockNumber),vrfProof:proof,recorded};
    expect(replayEpochCoordinator(input).map.values).to.deep.equal([...(await c.rng.getMappedResult(id))]);
    await c.rng.setFeeRecipient(c.user.address);await c.rng.setKeeperFeeBps(5000);
    expect(await c.rng.initialFeeRecipient()).to.equal(configuration.feeRecipient);
    expect(await c.rng.protocolConfigurationHash()).to.equal(protocolConfigurationHash);
    expect(replayEpochCoordinator(input).proof.matchesRecordedState).to.equal(true);
    expect(()=>replayEpochCoordinator({...input,configuration:{...configuration,feeRecipient:c.user.address}})).to.throw();
    for(const changed of [{...context,epochId:2n},{...context,epochHash:ethers.ZeroHash},{...context,blockHash:ethers.ZeroHash},{...context,requestBlock:context.requestBlock+1n},{...context,targetBlock:context.targetBlock-1n}]) expect(()=>replayEpochCoordinator({...input,context:changed})).to.throw();
    expect(()=>replayEpochCoordinator({...input,epoch:{...epoch,catalog:{...catalog,signers:[catalog.signers[1],catalog.signers[0],catalog.signers[2],catalog.signers[3]]}}})).to.throw();
    expect(()=>replayEpochCoordinator({...input,epoch:{...epoch,record:{...record,committedBlock:first}}})).to.throw();
    await networkHelpers.mine(300);
    expect(await c.rng.verifyRequestProof(id,proof)).to.equal(recorded.randomness);
    expect(replayEpochCoordinator(input).proof.matchesRecordedState).to.equal(true);
    console.log(`Epoch measured gas: commit=${commitReceipt.gasUsed}, request=${requestReceipt.gasUsed}, fulfill=${receipt.gasUsed}, epochPacket=${ethers.getBytes(packet).length} bytes`);
  });
});

describe("Scheduled catalog rotation",function(){
  const INITIAL_SIGNERS=TEST_API_WALLETS.map(w=>w.address);
  /// The epoch's selected record, signed by a chosen wallet instead of the Airnode the selection names.
  async function attestAs(registry:any,epochId:bigint,wallet:(typeof TEST_API_WALLETS)[number]){
    const s=await registry.getEpochSelection(epochId);
    const a={timestamp:BigInt((await ethers.provider.getBlock("latest"))!.timestamp),data:ethers.hexlify(ethers.toUtf8Bytes(JSON.stringify(epochFixtureData(Number(s.recipe))))),signature:"0x"};
    a.signature=await wallet.signMessage(ethers.getBytes(attestationDigest(s.queryHash,a)));
    return a;
  }
  it("activates a scheduled five-source catalog two epochs ahead while the current and next epoch, open requests, pins and the initial catalog keep the old signers",async()=>{
    const c=await networkHelpers.loadFixture(registryFixture);
    await networkHelpers.mine(Number(await c.registry.firstEpochStart())-await ethers.provider.getBlockNumber());
    const initialHash=await c.registry.catalogHash(),configuration=await c.rng.protocolConfigurationHash(),next=providerTestCatalog(),nextHash=epochCatalogHash(next.signers,next.recipes);
    expect(await c.registry.epochForBlock(await ethers.provider.getBlockNumber())).to.equal(1n);
    await c.registry.scheduleCatalog(next.recipes,next.signers,3);
    for(const epoch of [1n,2n]){const [hash,recipes,signers]=await c.registry.catalogAt(epoch);expect([hash,recipes.map(Number),Array.from(signers)]).to.deep.equal([initialHash,[0,1,2,3],INITIAL_SIGNERS]);}
    for(const epoch of [3n,99n]){const [hash,recipes,signers]=await c.registry.catalogAt(epoch);expect([hash,recipes.map(Number),Array.from(signers)]).to.deep.equal([nextHash,next.recipes,next.signers]);}
    expect(await c.registry.catalogHash()).to.equal(initialHash);expect(await c.registry.hyperliquidSigner()).to.equal(INITIAL_SIGNERS[0]);expect(await c.registry.ethTradeSigner()).to.equal(INITIAL_SIGNERS[3]);
    expect(await c.rng.protocolConfigurationHash()).to.equal(configuration);
    const providerWallet=async(epochId:bigint)=>PROVIDER_TEST_WALLETS[BUILTIN_EPOCH_RECIPES[Number((await c.registry.getEpochSelection(epochId)).recipe)].provider];
    const initialWallet=async(epochId:bigint)=>TEST_API_WALLETS[Number((await c.registry.getEpochSelection(epochId)).source)];
    await expect(c.registry.commitEpoch(1,await attestAs(c.registry,1n,await providerWallet(1n)))).to.be.revertedWithCustomError(c.registry,"InvalidSigner");
    await c.registry.commitEpoch(1,await attestAs(c.registry,1n,await initialWallet(1n)));
    expect((await c.registry.getEpoch(1)).catalogHash).to.equal(initialHash);
    // An epoch-2 request escrowed just before the boundary is published and served in epoch 3 with the old signer.
    await networkHelpers.mine(Number(await c.registry.epochStart(3))-2-await ethers.provider.getBlockNumber());
    await c.consumer.request(seed,CALLBACK_GAS,c.user.address,{value:FEE});const id2=await c.consumer.lastRequestId();
    expect((await c.rng.getRequest(id2)).epochId).to.equal(2n);
    await networkHelpers.mine(2);expect(await c.registry.epochForBlock(await ethers.provider.getBlockNumber())).to.equal(3n);
    await expect(c.registry.commitEpoch(2,await attestAs(c.registry,2n,await providerWallet(2n)))).to.be.revertedWithCustomError(c.registry,"InvalidSigner");
    const commit2=(await(await c.registry.commitEpoch(2,await attestAs(c.registry,2n,await initialWallet(2n)))).wait())!;
    expect((await c.registry.getEpoch(2)).catalogHash).to.equal(initialHash);
    await networkHelpers.mine(2);await c.rng.fulfillRandomness(id2,makeProof(await c.rng.requestSeed(id2)),{gasLimit:2_000_000});
    expect((await c.rng.getRequest(id2)).delivered).to.equal(true);
    // The activation epoch selects among five sources, commits only with the new provider signer and records the new hash.
    expect(await c.registry.sourceCountAt(3)).to.equal(5n);
    const selected3=await c.registry.getEpochSelection(3);
    expect(Number(selected3.recipe)).to.equal(next.recipes[Number(selected3.source)]);
    await expect(c.registry.commitEpoch(3,await attestAs(c.registry,3n,TEST_API_WALLETS[0]))).to.be.revertedWithCustomError(c.registry,"InvalidSigner");
    const commit3=(await(await c.registry.commitEpoch(3,await signedEpochAttestation(c.registry,3n,ethers))).wait())!;
    const record3=await c.registry.getEpoch(3);expect(record3.catalogHash).to.equal(nextHash);
    expect(await c.registry.catalogHash()).to.equal(initialHash);expect(await c.rng.protocolConfigurationHash()).to.equal(configuration);
    const requestReceipt=(await(await c.consumer.request(seed,CALLBACK_GAS,c.user.address,{value:FEE})).wait())!;const id3=await c.consumer.lastRequestId();
    await networkHelpers.mine(2);const proof3=makeProof(await c.rng.requestSeed(id3));
    const accepted=(await(await c.rng.fulfillRandomness(id3,proof3,{gasLimit:2_000_000})).wait())!;
    // Replay each epoch with the catalog it used, resolved from catalogAt; the other catalog must not verify it.
    const base={registry:await c.registry.getAddress(),chainId:31337n,firstEpochStart:await c.registry.firstEpochStart()};
    const catalogOf=async(epochId:bigint)=>{const [hash,recipes,signers]=await c.registry.catalogAt(epochId);return resolveEpochCatalog(base,{hash,recipes,signers});};
    const epochInput=async(epochId:bigint,receipt:{blockNumber:number})=>({catalog:await catalogOf(epochId),epochId,record:await c.registry.getEpoch(epochId),
      commitTimestamp:BigInt((await ethers.provider.getBlock(receipt.blockNumber))!.timestamp),packet:c.registry.interface.parseLog((await c.registry.queryFilter(c.registry.filters.EpochCommitted(epochId)))[0])!.args.packet});
    const input2=await epochInput(2n,commit2),input3=await epochInput(3n,commit3);
    expect(input2.catalog.recipes).to.equal(undefined);expect(input3.catalog.recipes).to.deep.equal(next.recipes);
    for(const [input,other] of [[input2,input3.catalog],[input3,input2.catalog]] as const){
      expect(replayEpochCommitment(input).epochHash).to.equal(input.record.epochHash);
      expect(()=>replayEpochCommitment({...input,catalog:other})).to.throw();
    }
    // Coordinator replay of the rotated-epoch request keeps the initial catalog in the configuration hash.
    const r3=await c.rng.getRequest(id3);
    const context:EpochRequestContext={chainId:31337n,coordinator:await c.rng.getAddress(),keyHash:await c.rng.keyHash(),requestId:id3,consumer:await c.consumer.getAddress(),clientSeed:seed,mapping:builtins.raw(),
      requestBlock:r3.requestBlock,targetBlock:r3.targetBlock,blockHash:r3.blockHash,epochId:3n,epochHash:r3.epochHash};
    const config={publicKey:publicKey(),feeRecipient:c.owner.address,initialMinFee:FEE,confirmationBlocks:1,registry:base.registry,catalogHash:initialHash,firstEpochStart:base.firstEpochStart};
    const input={context,configuration:config,protocolConfigurationHash:epochProtocolConfigurationHash(config),epoch:{catalog:input3.catalog,record:input3.record,commitTimestamp:input3.commitTimestamp,packet:input3.packet},
      requestedAt:BigInt((await ethers.provider.getBlock(requestReceipt.blockNumber))!.timestamp),deadline:r3.deadline,acceptanceTimestamp:BigInt((await ethers.provider.getBlock(accepted.blockNumber))!.timestamp),acceptanceBlock:BigInt(accepted.blockNumber),vrfProof:proof3,recorded:r3};
    expect(input.protocolConfigurationHash).to.equal(configuration);
    expect(replayEpochCoordinator(input).proof.matchesRecordedState).to.equal(true);
    expect(()=>replayEpochCoordinator({...input,epoch:{...input.epoch,catalog:{...base,signers:INITIAL_SIGNERS}}})).to.throw();
    expect(()=>replayEpochCoordinator({...input,configuration:{...config,catalogHash:nextHash}})).to.throw();
  });
});

describe("Epoch catalog continuity",function(){
  it("allows repeated raw API data while binding a different commitment to each epoch",async()=>{
    const c=await registryFixture();const seen=new Map<string,string>();let duplicate=false;
    for(let id=1n;id<=5n;id++){
      const anchor=Number(await c.registry.epochStart(id))-1;
      const block=await ethers.provider.getBlockNumber();if(block<=anchor)await networkHelpers.mine(anchor+1-block);
      const a=await epochAttestation(c.registry,id);await c.registry.commitEpoch(id,a);
      const e=await c.registry.getEpoch(id),prior=seen.get(e.dataHash);
      if(prior){expect(e.epochHash).not.to.equal(prior);duplicate=true;}else seen.set(e.dataHash,e.epochHash);
    }
    expect(duplicate).to.equal(true); // Four fixed data bodies across five epochs must repeat one body.
  });
  it("validates all four pinned bodies and publishes explicit current epochs on one registry",async function(){
    const c=await networkHelpers.loadFixture(registryFixture),seen=new Set<number>();
    for(let id=1n;id<=64n;++id){
      const start=await c.registry.epochStart(id);
      if(BigInt(await ethers.provider.getBlockNumber())<start)await networkHelpers.mine(Number(start)-await ethers.provider.getBlockNumber());
      const s=await c.registry.getEpochSelection(id); seen.add(Number(s.source));
      if(s.source===1n){
        const bad='{"id":null,"jsonrpc":"2.0","result":"0x0123456789abcdeF0123456789abcdef0123456789abcdef0123456789abcdef"}';
        await expect(c.registry.commitEpoch(id,await epochAttestation(c.registry,id,bad))).to.be.revertedWithCustomError(c.registry,"InvalidData");
      }
      await c.registry.commitEpoch(id,await epochAttestation(c.registry,id));
      expect((await c.registry.getEpoch(id)).epochHash).not.to.equal(ethers.ZeroHash);
      expect(await c.registry.nextEpochToPrepare(await ethers.provider.getBlockNumber())).to.equal(id);
      if(seen.size===4&&id>=5n) break;
    }
    expect([...seen].sort()).to.deep.equal([0,1,2,3]);
    expect((await c.registry.getEpoch(1)).epochHash).not.to.equal(ethers.ZeroHash);
  });
});
