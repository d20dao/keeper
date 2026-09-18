import { expect } from "chai";
import { network } from "hardhat";
import { deployProxy } from "./helpers/proxy.ts";
import { EPOCH_TEST_WALLETS, EPOCH_TEST_SIGNERS, epochFixtureData } from "./helpers/epoch.ts";
import { attestationDigest } from "../src/sources.ts";
import { FALLBACK_DELAY_BLOCKS, encodeEpochEvidencePacket, fallbackOpensAt, replayEpochCommitment, selectEpoch, type EpochCatalog } from "../src/epoch.ts";

const { ethers, networkHelpers } = await network.create();

async function registryFixture() {
  const [owner, stranger] = await ethers.getSigners();
  const registry = await deployProxy(ethers, "EpochEntropy", [EPOCH_TEST_SIGNERS, owner.address, owner.address]);
  await networkHelpers.mine(Number(await registry.firstEpochStart()) - await ethers.provider.getBlockNumber());
  return { registry, owner, stranger };
}
async function attest(registry: any, epochId: bigint, attempt: number) {
  const s = await registry.getEpochFallbackSelection(epochId, attempt);
  const data = ethers.hexlify(ethers.toUtf8Bytes(JSON.stringify(epochFixtureData(Number(s.source)))));
  const a = { timestamp: BigInt((await ethers.provider.getBlock("latest"))!.timestamp), data, signature: "0x" };
  a.signature = await EPOCH_TEST_WALLETS[Number(s.source)].signMessage(ethers.getBytes(attestationDigest(s.queryHash, a)));
  return { a, s };
}

describe("Deterministic epoch source fallback", function () {
  it("opens each next source after its delay and replays the committed fallback", async function () {
    const { registry, stranger } = await networkHelpers.loadFixture(registryFixture);
    const start = await registry.epochStart(1);
    const primary = await registry.getEpochSelection(1);
    for (let attempt = 0; attempt <= 3; attempt++) {
      const s = await registry.getEpochFallbackSelection(1, attempt);
      expect(Number(s.source)).to.equal((Number(primary.source) + attempt) % 4);
      expect(s.airnode).to.equal(EPOCH_TEST_SIGNERS[Number(s.source)]);
      expect(await registry.fallbackOpensAt(1, attempt)).to.equal(start + BigInt(attempt) * FALLBACK_DELAY_BLOCKS);
    }
    await expect(registry.getEpochFallbackSelection(1, 4)).to.be.revertedWithCustomError(registry, "InvalidFallback");

    // Attempt 2 before its window: rejected even with a valid signature for that source.
    const early = await attest(registry, 1n, 2);
    await expect(registry.commitEpochFallback(1, 2, early.a)).to.be.revertedWithCustomError(registry, "FallbackNotOpen");
    await expect(registry.commitEpochFallback(1, 0, early.a)).to.be.revertedWithCustomError(registry, "InvalidFallback");
    await expect(registry.connect(stranger).getFunction("commitEpochFallback")(1, 2, early.a)).to.be.revertedWithCustomError(registry, "OnlyCommitter");

    await networkHelpers.mine(Number(start + 2n * FALLBACK_DELAY_BLOCKS) - await ethers.provider.getBlockNumber());
    const { a, s } = await attest(registry, 1n, 2);
    // A fallback cannot be committed with the selected source's data, and attempt 2 is not attempt 1.
    const primaryData = await attest(registry, 1n, 0);
    await expect(registry.commitEpochFallback(1, 2, primaryData.a)).to.revert(ethers);
    await expect(registry.commitEpochFallback(1, 1, a)).to.revert(ethers);
    const receipt = (await (await registry.commitEpochFallback(1, 2, a)).wait())!;
    await expect(registry.commitEpoch(1, primaryData.a)).to.be.revertedWithCustomError(registry, "AlreadyCommitted");

    const record = await registry.getEpoch(1);
    expect(Number(record.source)).to.equal(Number(s.source));
    const catalog: EpochCatalog = { signers: [EPOCH_TEST_SIGNERS[0], EPOCH_TEST_SIGNERS[1], EPOCH_TEST_SIGNERS[2], EPOCH_TEST_SIGNERS[3]], registry: await registry.getAddress(), chainId: (await ethers.provider.getNetwork()).chainId, firstEpochStart: await registry.firstEpochStart() };
    expect(selectEpoch(catalog, 1n, record.anchorHash, 2).canonicalRequest).to.equal(s.canonicalRequest);
    const commitTimestamp = BigInt((await ethers.provider.getBlock(receipt.blockNumber))!.timestamp);
    const epoch = { catalog, record, commitTimestamp, packet: encodeEpochEvidencePacket(s.canonicalRequest, a), epochId: 1n };
    expect(replayEpochCommitment(epoch).epochHash).to.equal(record.epochHash);
    // Replay rejects a fallback record that claims a commit block before its window.
    expect(() => replayEpochCommitment({ ...epoch, record: { ...record.toObject(), committedBlock: fallbackOpensAt(catalog.firstEpochStart, 1n, 2) - 1n } })).to.throw("Invalid epoch commit block");
  });

  it("keeps the selected source as the only option at the epoch start", async function () {
    const { registry } = await networkHelpers.loadFixture(registryFixture);
    const one = await attest(registry, 1n, 1);
    await expect(registry.commitEpochFallback(1, 1, one.a)).to.be.revertedWithCustomError(registry, "FallbackNotOpen");
    const primary = await attest(registry, 1n, 0);
    await registry.commitEpoch(1, primary.a);
    expect(Number((await registry.getEpoch(1)).source)).to.equal(Number(primary.s.source));
  });
});
