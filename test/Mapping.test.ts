import { deployReadyEpochFixture } from "./helpers/epoch.ts";
import { expect } from "chai";
import { network } from "hardhat";
import { builtins, mapRandomness, hashMapping, deriveRequestSeed, hashProof, verifyVRFProof } from "../src/index.ts";
import { makeProof, publicKey, proofOutput } from "./helpers/proof.ts";

const { ethers, networkHelpers } = await network.create();
const MAX = (1n << 256n) - 1n;
const FEE = 10n ** 15n;
async function fixture() {
  return deployReadyEpochFixture(ethers,networkHelpers,FEE);
}

describe("Built-in mapping", function () {
  const specs = [
    builtins.raw(), builtins.d20(), builtins.d12(), builtins.d10(), builtins.d8(), builtins.d6(), builtins.d4(),
    builtins.dN(100n), builtins.diceRoll(6n, 4), builtins.diceRoll(20n, 128), builtins.coinFlip(),
    builtins.numberRange(7n, 7n), builtins.numberRange(10n, 1000n), builtins.numberRange(0n, MAX),
    builtins.numberRange(MAX - 1n, MAX), builtins.numberRange(0n, 1n << 255n),
    builtins.chooseOne(1), builtins.chooseOne(256), builtins.chooseMany(20, 7),
    builtins.chooseMany(256, 256), builtins.shuffle(1), builtins.shuffle(52),
  ];
  it("recomputes every built-in identically in Solidity and TypeScript, including uint256 boundaries", async function () {
    const { rng } = await networkHelpers.loadFixture(fixture);
    for (const randomness of [ethers.ZeroHash, ethers.id("mapping-fixture-a"), ethers.id("mapping-fixture-b")]) {
      for (const spec of specs) {
        const result = mapRandomness(randomness, spec);
        expect(Array.from(await rng.mapRandomness(randomness, spec))).to.deep.equal(result);
        if (spec.operation === 1) for (const die of result) expect(die >= 1n && die <= spec.upper).to.equal(true);
        if (spec.operation === 2) expect([0n, 1n]).to.include(result[0]);
        if (spec.operation === 3) expect(result[0] >= spec.lower && result[0] <= spec.upper).to.equal(true);
        if (spec.operation >= 4) {
          expect(new Set(result).size).to.equal(result.length);
          expect(result.length).to.equal(spec.count);
          for (const index of result) expect(index >= 0n && index < BigInt(spec.population)).to.equal(true);
        }
      }
    }
  });

  it("rejects empty/oversized pools, duplicate-choice counts and non-canonical unused parameters", async function () {
    const { rng } = await networkHelpers.loadFixture(fixture);
    const badSpecs = [
      { ...builtins.d6(), upper: 1n }, { ...builtins.d6(), count: 0 }, { ...builtins.d6(), count: 129 },
      { ...builtins.coinFlip(), upper: 2n }, { ...builtins.raw(), count: 1 },
      { ...builtins.numberRange(0n, 1n), lower: 2n },
      { ...builtins.chooseOne(1), population: 0 }, { ...builtins.chooseOne(1), population: 257 },
      { ...builtins.chooseMany(2, 2), count: 3 }, { ...builtins.shuffle(3), count: 2 },
    ];
    for (const spec of badSpecs) {
      expect(() => mapRandomness(ethers.ZeroHash, spec)).to.throw();
      await expect(rng.mapRandomness(ethers.ZeroHash, spec)).to.revert(ethers);
    }
    expect(() => builtins.chooseOne(1.5)).to.throw();
    expect(() => builtins.numberRange(-1n, 5n)).to.throw();
  });

  it("exercises rejection sampling instead of biased modulo for a large awkward range", async function () {
    const { rng } = await networkHelpers.loadFixture(fixture);
    const bound = (1n << 255n) + 1n;
    const threshold = (1n << 256n) % bound;
    const abi = ethers.AbiCoder.defaultAbiCoder();
    let randomness = ethers.ZeroHash;
    const word = (cursor: number) => BigInt(ethers.keccak256(abi.encode(
      ["bytes32", "bytes32", "uint256"], [ethers.id("D20_MAP"), randomness, cursor]
    )));
    let counter = 0;
    while (!(word(0) < threshold && word(1) >= threshold)) randomness = ethers.id(`reject-${++counter}`);
    const spec = builtins.numberRange(0n, bound - 1n);
    const result = mapRandomness(randomness, spec);
    expect(result).to.deep.equal([word(1) % bound]);
    expect(result[0]).not.to.equal(word(0) % bound);
    expect(Array.from(await rng.mapRandomness(randomness, spec))).to.deep.equal(result);
  });
});

describe("Request -> Reveal -> Map -> Proof", function () {
  it("locks a d20 mapping, verifies the real proof offline/onchain, and audits the accepted proof hash", async function () {
    const { rng, consumer, user } = await networkHelpers.loadFixture(fixture);
    await consumer.rollD20(user.address, { value: FEE });
    const id = await consumer.lastRequestId();
    await expect(rng.getMappedResult(id)).to.be.revertedWithCustomError(rng, "NotFulfilled");
    await networkHelpers.mine(2);
    const r = await rng.getRequest(id);
    const spec = builtins.d20();
    expect(r.mappingHash).to.equal(hashMapping(spec));
    const target = await ethers.provider.getBlock(Number(r.targetBlock));
    const context = {
      chainId: (await ethers.provider.getNetwork()).chainId, coordinator: await rng.getAddress(),
      keyHash: await rng.keyHash(), requestId: id, consumer: await consumer.getAddress(),
      clientSeed: r.clientSeed, mapping: spec, requestBlock:r.requestBlock, targetBlock: r.targetBlock, blockHash: target!.hash!,
      epochId:r.epochId,epochHash:r.epochHash,
    };
    const expectedSeed = deriveRequestSeed(context);
    expect(expectedSeed).to.equal(await rng.requestSeed(id));
    expect(deriveRequestSeed({ ...context, mapping: builtins.d6() })).not.to.equal(expectedSeed);
    const proof = makeProof(expectedSeed);
    const checked = verifyVRFProof(proof, publicKey(), expectedSeed);
    expect(checked).to.deep.equal({ valid: true, randomness: proofOutput(proof) });
    expect(await rng.verifyRequestProof(id, proof)).to.equal(proofOutput(proof));
    // A valid proof before submission is not a timely fulfilled request yet.
    expect((await rng.getRequest(id)).fulfilled).to.equal(false);
    await expect(rng.fulfillRandomness(id, proof, { gasLimit: 2_000_000 }))
      .to.emit(rng, "ProofVerified").withArgs(id, await rng.keyHash(), expectedSeed, hashProof(proof));
    const accepted = await rng.getRequest(id);
    expect(accepted.proofHash).to.equal(hashProof(proof));
    expect(accepted.randomness).to.equal(proofOutput(proof));
    expect(Array.from(await rng.getMappedResult(id))).to.deep.equal(mapRandomness(proofOutput(proof), spec));
    expect(accepted.delivered).to.equal(true);
    for (const changed of [
      { ...proof, seed: expectedSeed + 1n }, { ...proof, c: proof.c + 1n },
      { ...proof, zInv: 0n }, { ...proof, pk: [1n, 1n] as [bigint, bigint] },
    ]) expect(verifyVRFProof(changed, publicKey(), expectedSeed).valid).to.equal(false);
  });

  it("fixes each mapped request's parameters and supports results despite consumer callback failure", async function () {
    const { rng, consumer, user } = await networkHelpers.loadFixture(fixture);
    const spec = builtins.shuffle(10);
    await consumer.requestMapped(ethers.id("fixed-deck-hash"), 200000, user.address, spec, { value: FEE });
    const id = await consumer.lastRequestId();
    await networkHelpers.mine(2);
    const proof = makeProof(await rng.requestSeed(id));
    await consumer.setMode(1);
    await rng.fulfillRandomness(id, proof, { gasLimit: 2_000_000 });
    expect((await rng.getRequest(id)).delivered).to.equal(false);
    expect(Array.from(await rng.getMappedResult(id))).to.deep.equal(mapRandomness(proofOutput(proof), spec));
    await networkHelpers.time.increase(61);
    await expect(rng.refundRequest(id)).to.be.revertedWithCustomError(rng, "RefundNotAvailable");
  });
});
