import {readFileSync} from "node:fs";
import {expect} from "chai";
import {network} from "hardhat";
import {deployProxy} from "./helpers/proxy.ts";
import {EPOCH_TEST_SIGNERS,signSelection} from "./helpers/epoch.ts";
import {LEGACY_TEMPLATES} from "./helpers/template-corpus.ts";
import {BUILTIN_EPOCH_RECIPES,canonicalApiRequest,canonicalRequestOfBody,encodeDataTemplate,readEpochRecipes,replayEpochCommitment,resolveEpochCatalog,selectEpoch,
  validateEpochData,validateEpochRecipe,verifyEpochAttestation,type EpochRecipe} from "../src/index.ts";

const {ethers,networkHelpers}=await network.create();
const json=(path:string)=>JSON.parse(readFileSync(new URL(path,import.meta.url),"utf8"));
const airnode=json("./fixtures/airnode-recipes-2026-09-17.json");
const {cases,templates}=json("./fixtures/epoch-data-cases.json") as {cases:Array<{template:string;valid:boolean;origin:string;note:string;data:string}>;templates:Record<string,{template:string}>};
const utf8=(text:string)=>ethers.hexlify(ethers.toUtf8Bytes(text));
type Sample={airnode:string;requestHash:string;timestamp:string;data:unknown;signature:string};
const signedBytes=(sample:Sample)=>utf8(typeof sample.data==="string"?sample.data:JSON.stringify(sample.data));
/// Change the last decimal digit: the exact shape survives, the provider's signature does not.
function tamper(data:string){const text=ethers.toUtf8String(data),at=text.search(/[0-9](?=[^0-9]*$)/);return utf8(text.slice(0,at)+String((Number(text[at])+1)%10)+text.slice(at+1));}
/// Simulate the commit as the committer, leaving the epoch unpublished.
function simulateCommit(registry:any,epochId:bigint,attempt:number,a:object){
  return attempt===0?registry.commitEpoch.staticCall(epochId,a):registry.commitEpochFallback.staticCall(epochId,attempt,a);
}
const definition=(recipe:EpochRecipe)=>[recipe.canonicalRequest,recipe.template,recipe.body] as const;
async function registryFixture(){
  const [owner,stranger]=await ethers.getSigners();
  const registry=await deployProxy(ethers,"EpochEntropy",[EPOCH_TEST_SIGNERS,owner.address,owner.address]);
  return {registry,owner,stranger};
}

describe("Epoch recipe registry",function(){
  it("registers the six built-in recipes at initialization, derived from structured bodies and matching the gateways' signed request hashes",async()=>{
    const {registry}=await networkHelpers.loadFixture(registryFixture);
    const initialized=(await registry.queryFilter(registry.filters.RecipeRegistered()));
    expect(await registry.recipeCount()).to.equal(6n);
    expect(initialized.map((log:any)=>registry.interface.parseLog(log)!.args.toArray().map(String))).to.deep.equal(
      BUILTIN_EPOCH_RECIPES.map(r=>[String(r.id),ethers.id(r.canonicalRequest),r.canonicalRequest,r.template,r.body]));
    for(const recipe of BUILTIN_EPOCH_RECIPES){
      const [queryHash,canonicalRequest,template,body]=await registry.getRecipe(recipe.id);
      expect([queryHash,canonicalRequest,template,body]).to.deep.equal([ethers.id(recipe.canonicalRequest),...definition(recipe)]);
      expect(await registry.recipeRequest(recipe.id)).to.equal(recipe.canonicalRequest);
      expect(canonicalApiRequest(recipe.request)).to.equal(recipe.canonicalRequest).and.to.equal(canonicalRequestOfBody(recipe.body));
      expect(recipe.body).to.equal(JSON.stringify(recipe.request));
      const sampled=airnode.recipes.find((entry:{builtinRecipe:number|null})=>entry.builtinRecipe===recipe.id);
      expect(recipe.request).to.deep.equal(sampled.request);expect(recipe.provider).to.equal(sampled.provider);
      expect(recipe.canonicalRequest).to.equal(sampled.canonicalRequest);expect(ethers.id(sampled.canonicalRequest)).to.equal(sampled.requestHash);
      for(const sample of sampled.samples as Sample[])expect(sample.airnode).to.equal(airnode.providers[recipe.provider].signer);
    }
    // The keeper's fixture of the built-in recipes is exactly what the contract registers.
    const shared=json("./fixtures/builtin-recipes.json").recipes as Array<{id:number;queryHash:string;canonicalRequest:string;template:string;body:string}>;
    expect(await Promise.all(shared.map(async r=>Array.from(await registry.getRecipe(r.id))))).to.deep.equal(shared.map(r=>[r.queryHash,r.canonicalRequest,r.template,r.body]));
    expect(shared.map(r=>r.id)).to.deep.equal([0,1,2,3,4,5]);
    expect(await readEpochRecipes(ethers.provider,await registry.getAddress(),[0,1,2,3,4,5])).to.deep.equal(
      Object.fromEntries(BUILTIN_EPOCH_RECIPES.map(r=>[r.id,{canonicalRequest:r.canonicalRequest,template:r.template,body:r.body}])));
    // Ids 0-3 keep the canonical requests of the implementation they replace (96cc722), so earlier epochs replay unchanged.
    expect(BUILTIN_EPOCH_RECIPES.slice(0,4).map(r=>r.canonicalRequest)).to.deep.equal([
      '["metaAndAssetCtxs",[["dex",""]],[["symbol","/0/universe/0/name"],["value","/1/0/dayNtlVlm"]]]',
      '["jsonRpc",[["method","eth_call"],["network","ethereum"],["params",[[["data","0x27e86d6e"],["to","0xcA11bde05977b3631167028862bE2a173976CA11"]],"latest"]]]]',
      '["lastTrade",[["assetClass","crypto"],["symbol","BTCUSD"]]]','["lastTrade",[["assetClass","crypto"],["symbol","ETHUSD"]]]']);
    await expect(registry.recipeRequest(6)).to.be.revertedWithCustomError(registry,"InvalidConfig");
    await expect(registry.getRecipe(6)).to.be.revertedWithCustomError(registry,"InvalidConfig");
    // Objects are sorted by key at any depth, including inside arrays; arrays keep their order; a projection is a third element.
    expect(canonicalApiRequest({operation:"op",parameters:{b:[{z:1,a:{d:2,c:null}},"x"],a:""},responseProjection:{y:"/1",x:"/0"}}))
      .to.equal('["op",[["a",""],["b",[[["a",[["c",null],["d",2]]],["z",1]],"x"]]],[["x","/0"],["y","/1"]]]');
    expect(()=>{(BUILTIN_EPOCH_RECIPES[1].request.parameters as any).network="sepolia";}).to.throw(TypeError);
    for(const bad of ['[]','{"parameters":{}}','{"operation":"op","parameters":[]}','{"operation":"op","parameters":{},"responseProjection":"x"}'])expect(()=>canonicalRequestOfBody(bad)).to.throw();
  });

  it("appends immutable owner-registered recipes with bounded sizes and a well-formed template",async()=>{
    const {registry,stranger}=await networkHelpers.loadFixture(registryFixture);
    const nodaryBtc:EpochRecipe={canonicalRequest:'["latestFeeds",[["name","BTC/USD"]]]',template:LEGACY_TEMPLATES["nodary-btc-usd"].template,body:'{"operation":"latestFeeds","parameters":{"name":"BTC/USD"}}'};
    await expect(registry.connect(stranger).getFunction("registerRecipe")(...definition(nodaryBtc))).to.be.revertedWithCustomError(registry,"OwnableUnauthorizedAccount");
    expect(await registry.registerRecipe.staticCall(...definition(nodaryBtc))).to.equal(6n);
    await expect(registry.registerRecipe(...definition(nodaryBtc))).to.emit(registry,"RecipeRegistered").withArgs(6,ethers.id(nodaryBtc.canonicalRequest),...definition(nodaryBtc));
    // A changed listing is a new id; an identical definition may be registered again and never overwrites the first.
    await registry.registerRecipe(...definition(nodaryBtc));
    expect(await registry.recipeCount()).to.equal(8n);
    expect((await readEpochRecipes(ethers.provider,await registry.getAddress(),[6,7]))).to.deep.equal({6:nodaryBtc,7:nodaryBtc});
    // No function edits or removes a recipe: every nonpayable recipe function only appends.
    const writes=(registry.interface.fragments as any[]).filter(f=>f.type==="function"&&!["view","pure"].includes(f.stateMutability)).map(f=>f.name as string);
    expect(writes.filter(name=>/recipe/i.test(name)).sort()).to.deep.equal(["initializeRecipeRegistry","registerRecipe"]);
    const template=nodaryBtc.template,body=nodaryBtc.body,request=nodaryBtc.canonicalRequest;
    for(const [args,error] of [
      [["",template,body],"InvalidRecipe"],[["x".repeat(1025),template,body],"InvalidRecipe"],[[request,template,""],"InvalidRecipe"],[[request,template,"x".repeat(2049)],"InvalidRecipe"],
      [[request,"0x",body],"InvalidTemplate"],[[request,"0x01017b",body],"InvalidTemplate"],[[request,"0x0305",body],"InvalidTemplate"],[[request,"0x0280"+"0301",body],"InvalidTemplate"],
      [[request,"0x"+"0300".repeat(129),body],"InvalidTemplate"],[[request,"0x040503",body],"InvalidTemplate"],[[request,"0x01027b",body],"InvalidTemplate"],
    ] as const)await expect(registry.registerRecipe(...args),`${error} ${String(args[0]).slice(0,10)} ${args[1].slice(0,12)}`).to.be.revertedWithCustomError(registry,error);
    // Upper bounds are inclusive.
    await registry.registerRecipe("x".repeat(1024),"0x"+"0300".repeat(128),"y".repeat(2048));
    for(const bad of [{...nodaryBtc,canonicalRequest:"x".repeat(1025)},{...nodaryBtc,body:""},{...nodaryBtc,template:"0x0305"}])expect(()=>validateEpochRecipe(bad)).to.throw();
    validateEpochRecipe({canonicalRequest:"x".repeat(1024),template:"0x"+"0300".repeat(128),body:"y".repeat(2048)});
    // The registry holds at most 256 recipes, ids 0-255.
    const count=Number(await registry.recipeCount());
    for(let id=count;id<256;id++)await registry.registerRecipe(`["op",[["id",${id}]]]`,"0x0300","{}");
    expect(await registry.recipeCount()).to.equal(256n);
    expect(await registry.recipeRequest(255)).to.equal('["op",[["id",255]]]');
    await expect(registry.registerRecipe(...definition(nodaryBtc))).to.be.revertedWithCustomError(registry,"InvalidRecipe");
  });

  it("refuses the registry upgrade step on a registry that already has its recipes",async()=>{
    const {registry,stranger}=await networkHelpers.loadFixture(registryFixture);
    await expect(registry.connect(stranger).getFunction("initializeRecipeRegistry")()).to.be.revertedWithCustomError(registry,"OwnableUnauthorizedAccount");
    await expect(registry.initializeRecipeRegistry()).to.be.revertedWithCustomError(registry,"InvalidConfig");
    expect(await registry.recipeCount()).to.equal(6n);
    const implementation=await ethers.deployContract("EpochEntropy");
    await expect(implementation.initializeRecipeRegistry()).to.be.revertedWithCustomError(implementation,"InvalidInitialization");
  });

  it("gives every hand-written and sampled record the same verdict in a simulated commit and in replay",async()=>{
    const {registry}=await networkHelpers.loadFixture(registryFixture);
    const every=[0,1,2,3,4,5],signers=every.map(recipe=>EPOCH_TEST_SIGNERS[recipe%4]);
    await registry.scheduleCatalog(every,signers,2);
    await networkHelpers.mine(Number(await registry.epochStart(2))+100-await ethers.provider.getBlockNumber());
    const attempts=new Map<number,{attempt:number;selection:any}>();
    for(let attempt=0;attempt<every.length;attempt++){const selection=await registry.getEpochFallbackSelection(2,attempt);attempts.set(Number(selection.recipe),{attempt,selection});}
    const recipeFor=(name:string)=>BUILTIN_EPOCH_RECIPES.find(recipe=>recipe.template===templates[name].template)?.id;
    const checked=cases.filter(c=>(c.origin==="hand"||c.origin==="sample")&&recipeFor(c.template)!==undefined);
    let accepted=0;
    for(const c of checked){
      const recipe=recipeFor(c.template)!,{attempt,selection}=attempts.get(recipe)!;
      const a=await signSelection({...selection.toObject(),airnode:EPOCH_TEST_SIGNERS[recipe%4]},BigInt(await networkHelpers.time.latest()),c.data);
      const name=`recipe ${recipe}: ${c.note}`;
      if(c.valid){await simulateCommit(registry,2n,attempt,a).catch(()=>{throw new Error(`${name} rejected onchain`);});accepted++;}
      else await expect(simulateCommit(registry,2n,attempt,a),name).to.be.revertedWithCustomError(registry,"InvalidData");
      let replayValid=true;try{validateEpochData(BUILTIN_EPOCH_RECIPES[recipe].template,a.data);}catch{replayValid=false;}
      expect(replayValid,name).to.equal(c.valid);
    }
    expect(checked.length).to.be.greaterThan(120);expect(accepted).to.be.greaterThan(30);
  });

  // Built-in listings commit from the registry as initialized; the two sampled listings outside the built-in set are
  // registered first, as the owner would later.
  const extra:Record<string,{template:string}>={"hyperliquid-sol-mid":LEGACY_TEMPLATES["hyperliquid-sol-mid"],"nodary-btc-usd":LEGACY_TEMPLATES["nodary-btc-usd"]};
  for(const listing of airnode.recipes){
    it(`verifies both real signed ${listing.name} responses onchain and in replay`,async()=>{
      for(const sample of listing.samples as Sample[]){
        // Start the local chain before the signing time so the 240-second freshness bound holds.
        const signedAt=BigInt(sample.timestamp);
        const local=await network.create({override:{initialDate:new Date(Number(signedAt-3600n)*1000)}});
        try {
          const [owner]=await local.ethers.getSigners();
          const registry=await deployProxy(local.ethers,"EpochEntropy",[EPOCH_TEST_SIGNERS,owner.address,owner.address]);
          const address=await registry.getAddress();
          let recipe:number=listing.builtinRecipe;
          if(recipe===null){
            const added:EpochRecipe={canonicalRequest:listing.canonicalRequest,template:extra[listing.name].template,body:JSON.stringify(listing.request)};
            expect(canonicalRequestOfBody(added.body)).to.equal(added.canonicalRequest);
            await registry.registerRecipe(...definition(added));recipe=6;
          }
          // A one-source catalog with the provider's real Airnode.
          await registry.scheduleCatalog([recipe],[sample.airnode],2);
          await local.networkHelpers.mine(Number(await registry.epochStart(2))-await local.ethers.provider.getBlockNumber());
          expect(await registry.sourceCountAt(2)).to.equal(1n);
          const s=await registry.getEpochSelection(2);
          expect(Number(s.recipe)).to.equal(recipe);expect(s.airnode).to.equal(sample.airnode);expect(s.queryHash).to.equal(sample.requestHash);
          const a={timestamp:signedAt,data:signedBytes(sample),signature:sample.signature},tampered={...a,data:tamper(a.data)};
          const base={registry:address,chainId:31337n,firstEpochStart:await registry.firstEpochStart(),recipeBook:await readEpochRecipes(local.ethers.provider,address,[recipe])};
          const [hash,recipes,signers]=await registry.catalogAt(2);
          const catalog=resolveEpochCatalog(base,{hash,recipes,signers});
          validateEpochData(catalog.recipeBook![recipe]!.template,tampered.data);
          const anchor=(await local.ethers.provider.getBlock(Number(await registry.epochStart(2))-1))!.hash!;
          const selected=selectEpoch(catalog,2n,anchor);
          expect(verifyEpochAttestation(selected,a,signedAt).signer).to.equal(sample.airnode);
          expect(()=>verifyEpochAttestation(selected,tampered,signedAt)).to.throw("Wrong epoch signer/query");
          await local.networkHelpers.time.setNextBlockTimestamp(signedAt+1n);
          await expect(registry.commitEpoch(2,tampered)).to.be.revertedWithCustomError(registry,"InvalidSigner");
          await local.networkHelpers.time.setNextBlockTimestamp(signedAt+2n);
          const receipt=(await(await registry.commitEpoch(2,a)).wait())!;
          const record=await registry.getEpoch(2);
          expect(record.catalogHash).to.equal(hash);expect(record.signedAt).to.equal(signedAt);
          const packet=registry.interface.parseLog(receipt.logs[0])!.args.packet;
          expect(replayEpochCommitment({catalog,epochId:2n,record,commitTimestamp:signedAt+2n,packet}).epochHash).to.equal(record.epochHash);
          // Without the registered definition, replay names the missing recipe instead of guessing.
          if(recipe>=6)expect(()=>replayEpochCommitment({catalog:{...catalog,recipeBook:undefined},epochId:2n,record,commitTimestamp:signedAt+2n,packet})).to.throw("Unknown epoch recipe 6");
        } finally {await local.close();}
      }
    });
  }

  it("expresses an ANU-style array of fixed-length hex values and commits it as a registered recipe",async()=>{
    const {registry,owner}=await networkHelpers.loadFixture(registryFixture);
    const anu:EpochRecipe={canonicalRequest:'["randomNumbers",[["length",4],["size",8],["type","hex8"]]]',
      template:encodeDataTemplate([{literal:'{"success":true,"type":"hex8","length":"4","data":["'},{hex:16},{literal:'","'},{hex:16},{literal:'","'},{hex:16},{literal:'","'},{hex:16},{literal:'"]}'}]),
      body:'{"operation":"randomNumbers","parameters":{"type":"hex8","length":4,"size":8}}'};
    expect(anu.template).to.equal(LEGACY_TEMPLATES["anu-hex8-array"].template);expect(canonicalRequestOfBody(anu.body)).to.equal(anu.canonicalRequest);
    await registry.registerRecipe(...definition(anu));
    const wallet=new ethers.Wallet(ethers.toBeHex(31n,32));
    await registry.scheduleCatalog([6],[wallet.address],2);
    await networkHelpers.mine(Number(await registry.epochStart(2))-await ethers.provider.getBlockNumber());
    const s=await registry.getEpochSelection(2),timestamp=BigInt(await networkHelpers.time.latest());
    const sign=async(text:string)=>{const a={timestamp,data:utf8(text),signature:"0x"};a.signature=await wallet.signMessage(ethers.getBytes(ethers.solidityPackedKeccak256(["bytes32","uint256","bytes"],[s.queryHash,timestamp,a.data])));return a;};
    const record=(values:string[])=>`{"success":true,"type":"hex8","length":"4","data":[${values.map(v=>`"${v}"`).join(",")}]}`;
    await expect(registry.commitEpoch(2,await sign(record(["0123456789ABCDEF","0".repeat(16),"f".repeat(16),"a".repeat(16)])))).to.be.revertedWithCustomError(registry,"InvalidData");
    await expect(registry.commitEpoch(2,await sign(record(["0123456789abcdef","0".repeat(16),"f".repeat(16)])))).to.be.revertedWithCustomError(registry,"InvalidData");
    await registry.connect(owner).commitEpoch(2,await sign(record(["0123456789abcdef","0".repeat(16),"f".repeat(16),"a".repeat(16)])));
    expect((await registry.getEpoch(2)).queryHash).to.equal(ethers.id(anu.canonicalRequest));
  });
});
