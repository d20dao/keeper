import {readFileSync} from "node:fs";
import {expect} from "chai";
import {network} from "hardhat";
import {deployProxy} from "./helpers/proxy.ts";
import {DEFAULT_TEST_RECIPES,EPOCH_TEST_SIGNERS,PROVIDER_TEST_WALLETS,providerTestCatalog,signSelection} from "./helpers/epoch.ts";
import {BUILTIN_EPOCH_RECIPES,epochCatalogHash,fallbackOpensAt,replayEpochCommitment,resolveEpochCatalog,selectEpoch} from "../src/index.ts";
import {adjacentSignerSlots,catalogSigners,initialCatalogSigners,loadServiceCatalog} from "../scripts/lib/catalog.ts";

const {ethers,networkHelpers}=await network.create();
const airnode=JSON.parse(readFileSync(new URL("./fixtures/airnode-recipes-2026-09-17.json",import.meta.url),"utf8"));
const sampleSigner=(recipe:number)=>airnode.recipes.find((entry:{builtinRecipe:number|null})=>entry.builtinRecipe===recipe).samples[0].airnode;

async function registryFixture(){
  const [owner,committer,stranger]=await ethers.getSigners();
  const registry=await deployProxy(ethers,"EpochEntropy",[EPOCH_TEST_SIGNERS,owner.address,committer.address]);
  await networkHelpers.mine(Number(await registry.firstEpochStart())-await ethers.provider.getBlockNumber());
  return {registry,owner,committer,stranger};
}
const view=async(registry:any,epochId:bigint)=>{const [hash,recipes,signers]=await registry.catalogAt(epochId);return {hash,recipes:recipes.map(Number),signers:Array.from(signers) as string[]};};

describe("Epoch source catalogs",function(){
  it("keeps the rollout catalog interleaved by signer and resolves its signers from the service profile",async()=>{
    const service=await loadServiceCatalog();
    expect(service.recipes).to.deep.equal(DEFAULT_TEST_RECIPES).and.to.deep.equal([0,1,2,4,5]);
    for(const [provider,{signer}] of Object.entries(airnode.providers) as Array<[keyof typeof service.signers,{signer:string}]>)expect(service.signers[provider]).to.equal(signer);
    const signers=catalogSigners(service.recipes,service.signers);
    expect(signers).to.deep.equal(service.recipes.map(sampleSigner));
    expect(adjacentSignerSlots(signers)).to.deep.equal([]);
    expect(initialCatalogSigners(service.signers)).to.deep.equal([0,1,2,3].map(sampleSigner));
    expect(adjacentSignerSlots(catalogSigners([0,2,3,1],service.signers))).to.deep.equal([1]);
    expect(adjacentSignerSlots(catalogSigners([1,0,5],service.signers))).to.deep.equal([2]);
    expect(adjacentSignerSlots([service.signers.nodary])).to.deep.equal([]);
    expect(()=>catalogSigners([6],service.signers)).to.throw("Recipe 6 is not a built-in recipe");
  });

  it("schedules one to ten distinct registered recipes two epochs ahead, replaces a pending version and rejects invalid catalogs",async()=>{
    const {registry,owner,stranger}=await networkHelpers.loadFixture(registryFixture);
    const initial=await view(registry,1n);
    expect(initial).to.deep.equal({hash:await registry.catalogHash(),recipes:[0,1,2,3],signers:EPOCH_TEST_SIGNERS});
    expect(resolveEpochCatalog({registry:ethers.ZeroAddress,chainId:1n,firstEpochStart:1n},initial).recipes).to.equal(undefined);
    const full=providerTestCatalog(),signer=owner.address;
    await expect(registry.connect(stranger).getFunction("scheduleCatalog")(full.recipes,full.signers,3)).to.be.revertedWithCustomError(registry,"OwnableUnauthorizedAccount");
    // Unregistered recipe 6 is refused until it is registered.
    for(const [recipes,signers] of [[[],[]],[[0,1],[signer]],[[0,0],[signer,signer]],[[6],[signer]],[[1,2],[signer,ethers.ZeroAddress]],
      [[0,1,2,3,4,5,0,1,2,3,4],Array(11).fill(signer)]] as Array<[number[],string[]]>)
      await expect(registry.scheduleCatalog(recipes,signers,3)).to.be.revertedWithCustomError(registry,"InvalidConfig");
    expect(await registry.MAX_SOURCES()).to.equal(10n);
    for(const tooSoon of [0,1,2])await expect(registry.scheduleCatalog(full.recipes,full.signers,tooSoon)).to.be.revertedWithCustomError(registry,"InvalidEpoch");
    const single={recipes:[5],signers:[PROVIDER_TEST_WALLETS.drpc.address]};
    await expect(registry.scheduleCatalog(single.recipes,single.signers,3)).to.emit(registry,"CatalogScheduled").withArgs(3,epochCatalogHash(single.signers,single.recipes),single.recipes,single.signers);
    // Scheduling again while that version is pending replaces it rather than stacking.
    await expect(registry.scheduleCatalog(full.recipes,full.signers,4)).to.emit(registry,"CatalogScheduled").withArgs(4,epochCatalogHash(full.signers,full.recipes),full.recipes,full.signers);
    expect(await view(registry,3n)).to.deep.equal(initial);
    const scheduled=await view(registry,4n);
    expect(scheduled).to.deep.equal({hash:epochCatalogHash(full.signers,full.recipes),recipes:full.recipes,signers:full.signers});
    expect(resolveEpochCatalog({registry:ethers.ZeroAddress,chainId:1n,firstEpochStart:1n},scheduled).recipes).to.deep.equal(full.recipes);
    expect(()=>resolveEpochCatalog({registry:ethers.ZeroAddress,chainId:1n,firstEpochStart:1n},{...scheduled,recipes:[...full.recipes].reverse()})).to.throw("Catalog hash mismatch");
    for(const [epoch,count] of [[1n,4n],[3n,4n],[4n,5n],[99n,5n]] as const)expect(await registry.sourceCountAt(epoch)).to.equal(count);
    // An active version is never replaced; a later schedule stacks after it.
    await networkHelpers.mine(Number(await registry.epochStart(4))-await ethers.provider.getBlockNumber());
    await registry.scheduleCatalog(single.recipes,single.signers,6);
    expect((await view(registry,5n)).hash).to.equal(scheduled.hash);expect(await registry.sourceCountAt(6)).to.equal(1n);
    // Ten sources: the six built-ins and four newly registered block-hash recipes.
    const blockHash=BUILTIN_EPOCH_RECIPES[1];
    for(const chain of ["arbitrum","optimism","polygon","bsc"])
      await registry.registerRecipe(blockHash.canonicalRequest.replace("ethereum",chain),blockHash.template,blockHash.body.replace("ethereum",chain));
    const ten=Array.from({length:10},(_,i)=>i);
    await registry.scheduleCatalog(ten,Array(10).fill(signer),8);
    expect(await registry.sourceCountAt(8)).to.equal(10n);
    await expect(registry.scheduleCatalog([10],[signer],9)).to.be.revertedWithCustomError(registry,"InvalidConfig");
    // The replay library applies the same rules: any order of distinct recipe ids below 256, never more than ten sources.
    expect(epochCatalogHash(Array(8).fill(signer),[7,6,5,4,3,2,1,255])).to.match(/^0x[0-9a-f]{64}$/);
    expect(()=>epochCatalogHash([signer,signer],[1,1])).to.throw("Invalid epoch catalog");
    expect(()=>epochCatalogHash([signer],[256])).to.throw("Invalid epoch catalog");
    expect(()=>epochCatalogHash(Array(11).fill(signer),[0,1,2,3,4,5,6,7,8,9,10])).to.throw("Invalid epoch catalog");
  });

  it("selects and falls back across five interleaved providers, then replays the committed fallback",async()=>{
    const {registry,committer}=await networkHelpers.loadFixture(registryFixture);
    const full=providerTestCatalog(),count=full.recipes.length;
    await registry.scheduleCatalog(full.recipes,full.signers,3);
    await networkHelpers.mine(Number(await registry.epochStart(3))-await ethers.provider.getBlockNumber());
    const start=await registry.epochStart(3),base={registry:await registry.getAddress(),chainId:31337n,firstEpochStart:await registry.firstEpochStart()};
    const catalog=resolveEpochCatalog(base,await view(registry,3n)),anchor=(await ethers.provider.getBlock(Number(start)-1))!.hash!;
    const primary=Number((await registry.getEpochSelection(3)).source);
    const providers:string[]=[];
    for(let attempt=0;attempt<count;attempt++){
      const s=await registry.getEpochFallbackSelection(3,attempt),slot=(primary+attempt)%count,recipe=full.recipes[slot];
      expect([Number(s.source),Number(s.recipe),s.airnode]).to.deep.equal([slot,recipe,full.signers[slot]]);
      expect(s.canonicalRequest).to.equal(await registry.recipeRequest(recipe));
      expect(await registry.fallbackOpensAt(3,attempt)).to.equal(start+20n*BigInt(attempt)).and.to.equal(fallbackOpensAt(base.firstEpochStart,3n,attempt));
      const local=selectEpoch(catalog,3n,anchor,attempt);
      expect([local.source,local.recipe,local.airnode,local.queryHash]).to.deep.equal([slot,recipe,full.signers[slot],s.queryHash]);
      providers.push(BUILTIN_EPOCH_RECIPES[recipe].provider);
    }
    // Each fallback moves to another provider, and all four providers take part.
    expect(providers.every((provider,n)=>provider!==providers[(n+1)%count])).to.equal(true);
    expect(new Set(providers).size).to.equal(4);
    await expect(registry.getEpochFallbackSelection(3,count)).to.be.revertedWithCustomError(registry,"InvalidFallback");
    await expect(registry.fallbackOpensAt(3,count)).to.be.revertedWithCustomError(registry,"InvalidFallback");
    expect(()=>selectEpoch(catalog,3n,anchor,count)).to.throw("Invalid fallback attempt");

    const lastAttempt=count-1,last=await registry.getEpochFallbackSelection(3,lastAttempt);
    const early=await signSelection(last,BigInt(await networkHelpers.time.latest()));
    await expect(registry.connect(committer).getFunction("commitEpochFallback")(3,lastAttempt,early)).to.be.revertedWithCustomError(registry,"FallbackNotOpen");
    await networkHelpers.mine(Number(start)+20*lastAttempt-await ethers.provider.getBlockNumber());
    // Neither the previous slot's record nor the right record signed by the previous slot's Airnode stands in for the last attempt.
    const previous=await registry.getEpochFallbackSelection(3,lastAttempt-1),now=BigInt(await networkHelpers.time.latest());
    const fallback=(a:object)=>registry.connect(committer).getFunction("commitEpochFallback")(3,lastAttempt,a);
    await expect(fallback(await signSelection({recipe:previous.recipe,airnode:previous.airnode,queryHash:last.queryHash},now))).to.be.revertedWithCustomError(registry,"InvalidData");
    await expect(fallback(await signSelection({recipe:last.recipe,airnode:previous.airnode,queryHash:last.queryHash},now))).to.be.revertedWithCustomError(registry,"InvalidSigner");
    const a=await signSelection(last,BigInt(await networkHelpers.time.latest()));
    const receipt=(await(await fallback(a)).wait())!;
    const record=await registry.getEpoch(3);
    expect(Number(record.source)).to.equal((primary+lastAttempt)%count);expect(record.catalogHash).to.equal(epochCatalogHash(full.signers,full.recipes));
    const packet=registry.interface.parseLog(receipt.logs[0])!.args.packet;
    const commitTimestamp=BigInt((await ethers.provider.getBlock(receipt.blockNumber))!.timestamp);
    expect(replayEpochCommitment({catalog,epochId:3n,record,commitTimestamp,packet}).epochHash).to.equal(record.epochHash);
    expect(()=>replayEpochCommitment({catalog,epochId:3n,record:{...record.toObject(),committedBlock:start+20n*BigInt(lastAttempt)-1n},commitTimestamp,packet})).to.throw("Invalid epoch commit block");
    expect(()=>replayEpochCommitment({catalog:{...catalog,recipes:DEFAULT_TEST_RECIPES.slice().reverse()},epochId:3n,record,commitTimestamp,packet})).to.throw();
    // Epochs before the version still select among the initial four.
    expect(await registry.sourceCountAt(2)).to.equal(4n);
  });
});
