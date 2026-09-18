import {readFileSync} from "node:fs";
import {expect} from "chai";
import {network} from "hardhat";
import {deployProxy,implementationAddress,IMPLEMENTATION_SLOT} from "./helpers/proxy.ts";
import {deployReadyEpochFixture,EPOCH_TEST_SIGNERS,epochAttestation,providerTestCatalog,signSelection} from "./helpers/epoch.ts";
import {publicKey,makeProof,proofOutput} from "./helpers/proof.ts";
import {BUILTIN_EPOCH_RECIPES,builtins,deriveRequestSeed,replayCoordinator,epochCatalogHash,epochProtocolConfigurationHash,resolveEpochCatalog} from "../src/index.ts";
import {runtimeCodeAt,compiledRuntimeCodeHash} from "../scripts/lib/deployment.ts";
const {ethers,networkHelpers,provider}=await network.create();
const FEE=123n;
const json=(path:string)=>JSON.parse(readFileSync(new URL(path,import.meta.url),"utf8"));
// Live EpochEntropy implementations: creation and runtime bytecode behind the Arc Mainnet registry (640b60c), runtime
// bytecode behind the Arc Testnet registry (96cc722), and the coordinator implementation both networks keep.
const mainnetEpoch=json("./fixtures/epoch-entropy-deployed-640b60c.json"),testnetEpoch=json("./fixtures/epoch-entropy-deployed-96cc722.json");
const liveCoordinator=json("./fixtures/coordinator-deployed-640b60c.json");
const mainnet=json("../deployments/arc-mainnet.json"),testnet=json("../deployments/arc-testnet.json");
/// Whether a manifest runs this implementation now or records it as one it upgraded away from. An upgrade path stays
/// supported after the network moves on, so the fixture must still match the deployment it was recorded from.
function deployedHere(manifest:any,implementation:string,codeHash:string){
  if(manifest.epochImplementation===implementation&&manifest.epochImplementationCodeHash===codeHash)return true;
  return (manifest.implementationUpgrades??[]).some((entry:any)=>
    (entry.implementation===implementation&&entry.implementationCodeHash===codeHash)||
    (entry.previousImplementation===implementation&&entry.previousImplementationCodeHash===codeHash));
}
const fixture=()=>deployReadyEpochFixture(ethers,networkHelpers,FEE);
const INITIALIZABLE="0xf0c57e16840df040f15088dc2f81fe391c3923bec73e23a9662efc9c229c6a00";
const OWNABLE="0x9016d09d72d40fdae2fd8ceac6b6234c7706214fd39c1cd1e609a0528c199300",OWNABLE_2STEP="0x237e158222e3e6968b72b9db0d8043aacf074ad9f650f0d1606b4d82ee432c00";
const arrayBase=(slot:bigint)=>BigInt(ethers.solidityPackedKeccak256(["uint256"],[slot]));
const mappingBase=(key:bigint,slot:bigint)=>BigInt(ethers.keccak256(ethers.AbiCoder.defaultAbiCoder().encode(["uint64","uint256"],[key,slot])));
async function mineTo(block:bigint){const now=BigInt(await ethers.provider.getBlockNumber());if(block>now)await networkHelpers.mine(Number(block-now));}
/// Every declared slot and gap (0-47), the first entries of the three array slots, the epoch record and anchor of each
/// given epoch, and the OpenZeppelin namespaces.
async function registryStorage(address:string,epochs:readonly bigint[]){
  const slots=Array.from({length:48},(_,i)=>BigInt(i));
  for(const array of [8n,9n,10n])for(let i=0n;i<32n;i++)slots.push(arrayBase(array)+i);
  for(const epoch of epochs){for(let i=0n;i<9n;i++)slots.push(mappingBase(epoch,6n)+i);slots.push(mappingBase(epoch,7n));}
  slots.push(BigInt(OWNABLE),BigInt(OWNABLE_2STEP),BigInt(INITIALIZABLE));
  return Object.fromEntries(await Promise.all(slots.map(async slot=>[ethers.toBeHex(slot),await ethers.provider.getStorage(address,slot)])));
}
type LiveRegistry={label:string;implementation():Promise<string>;selection:string};
const LIVE_REGISTRIES:LiveRegistry[]=[
  {label:"Arc Mainnet 640b60c",selection:"(uint8 source,address airnode,bytes32 selector,bytes32 queryHash,string canonicalRequest)",async implementation(){
    const [deployer]=await ethers.getSigners();
    const legacy=await new ethers.ContractFactory((await ethers.getContractFactory("EpochEntropy")).interface,mainnetEpoch.bytecode,deployer).deploy();await legacy.waitForDeployment();
    const address=await legacy.getAddress();
    expect(await ethers.provider.getCode(address)).to.equal(runtimeCodeAt(mainnetEpoch.deployedBytecode,mainnetEpoch.immutableReferences,address));
    return address;
  }},
  {label:"Arc Testnet 96cc722",selection:"(uint8 source,uint8 recipe,address airnode,bytes32 selector,bytes32 queryHash,string canonicalRequest)",async implementation(){
    // Runtime code only: placed at its live address, where its UUPS self address is valid.
    await provider.request({method:"hardhat_setCode",params:[testnetEpoch.implementation,testnetEpoch.deployedBytecode]});
    expect(ethers.keccak256(await ethers.provider.getCode(testnetEpoch.implementation))).to.equal(testnetEpoch.runtimeCodeHash);
    return testnetEpoch.implementation;
  }},
];
/// A registry proxy on live implementation code, served by a coordinator proxy on the live coordinator code.
async function liveDeployment(live:LiveRegistry,owner:any,committer:any){
  const epochInterface=(await ethers.getContractFactory("EpochEntropy")).interface;
  const proxy=await ethers.deployContract("D20Proxy",[await live.implementation(),epochInterface.encodeFunctionData("initialize",[EPOCH_TEST_SIGNERS,owner.address,committer.address])]);
  const address=await proxy.getAddress(),registry:any=await ethers.getContractAt("EpochEntropy",address);
  await provider.request({method:"hardhat_setCode",params:[liveCoordinator.implementation,liveCoordinator.deployedBytecode]});
  const coordinatorInterface=(await ethers.getContractFactory("D20VRFCoordinator")).interface;
  const coordinatorProxy=await ethers.deployContract("D20Proxy",[liveCoordinator.implementation,coordinatorInterface.encodeFunctionData("initialize",[publicKey(),owner.address,owner.address,FEE,1,address,5000])]);
  const rng:any=await ethers.getContractAt("D20VRFCoordinator",await coordinatorProxy.getAddress());
  await rng.setPricing(FEE,0,300000);
  const consumer:any=await ethers.deployContract("TestConsumer",[await rng.getAddress()]);
  const selection=new ethers.Interface([`function getEpochFallbackSelection(uint64,uint8) view returns(${live.selection})`]);
  const legacySelect=async(epochId:bigint,attempt:number)=>selection.decodeFunctionResult("getEpochFallbackSelection",
    await ethers.provider.call({to:address,data:selection.encodeFunctionData("getEpochFallbackSelection",[epochId,attempt])}))[0];
  return {registry,rng,consumer,address,legacySelect};
}

async function request(c:Awaited<ReturnType<typeof fixture>>,label:string){
  await c.consumer.request(ethers.id(label),200000,c.user.address,{value:FEE});
  return await c.consumer.lastRequestId() as bigint;
}
async function prepareProof(c:Awaited<ReturnType<typeof fixture>>,id:bigint){
  await networkHelpers.mine(2);return makeProof(await c.rng.requestSeed(id));
}
async function upgradePair(c:Awaited<ReturnType<typeof fixture>>){
  const coordinator=await ethers.deployContract("CoordinatorUpgradeProbe"),registry=await ethers.deployContract("EpochUpgradeProbe");
  await c.registry.upgradeToAndCall(await registry.getAddress(),"0x");
  await c.rng.upgradeToAndCall(await coordinator.getAddress(),"0x");
  return {coordinator,registry};
}

describe("Atomic UUPS proxy initialization and upgrades",()=>{
  it("locks implementation initializers and requires atomic, one-time proxy initialization",async()=>{
    const [owner,user]=await ethers.getSigners();
    const epochImplementation=await ethers.deployContract("EpochEntropy");
    const epochArgs=[EPOCH_TEST_SIGNERS,owner.address,owner.address];
    await expect(epochImplementation.connect(user).getFunction("initialize")(...epochArgs)).to.be.revertedWithCustomError(epochImplementation,"InvalidInitialization");
    expect(await epochImplementation.owner()).to.equal(ethers.ZeroAddress);
    await expect(ethers.deployContract("D20Proxy",[await epochImplementation.getAddress(),"0x"])).to.be.revertedWithCustomError(await ethers.getContractFactory("D20Proxy"),"ERC1967ProxyUninitialized");
    const registry=await deployProxy(ethers,"EpochEntropy",epochArgs);
    const coordinatorImplementation=await ethers.deployContract("D20VRFCoordinator");
    const coordinatorArgs=[publicKey(),owner.address,owner.address,FEE,1,await registry.getAddress(),5000];
    await expect(coordinatorImplementation.connect(user).getFunction("initialize")(...coordinatorArgs)).to.be.revertedWithCustomError(coordinatorImplementation,"InvalidInitialization");
    expect(await coordinatorImplementation.owner()).to.equal(ethers.ZeroAddress);
    const coordinator=await deployProxy(ethers,"D20VRFCoordinator",coordinatorArgs);
    for(const [target,args] of [[registry,epochArgs],[coordinator,coordinatorArgs]] as const){
      expect(await target.owner()).to.equal(owner.address);
      await expect(target.connect(user).initialize(...args)).to.be.revertedWithCustomError(target,"InvalidInitialization");
      await expect(target.initialize(...args)).to.be.revertedWithCustomError(target,"InvalidInitialization");
      await expect(target.proxiableUUID()).to.be.revertedWithCustomError(target,"UUPSUnauthorizedCallContext");
    }
    expect(await coordinator.epochRegistry()).to.equal(await registry.getAddress());
    expect(await coordinatorImplementation.proxiableUUID()).to.equal(IMPLEMENTATION_SLOT);
    await expect(coordinatorImplementation.upgradeToAndCall(await coordinatorImplementation.getAddress(),"0x")).to.be.revertedWithCustomError(coordinatorImplementation,"UUPSUnauthorizedCallContext");
  });
  it("permits only the current owner to upgrade and rejects non-UUPS implementations",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const coordinator=await ethers.deployContract("CoordinatorUpgradeProbe"),registry=await ethers.deployContract("EpochUpgradeProbe");
    for(const [proxy,next] of [[c.rng,coordinator],[c.registry,registry]]){
      const before=await implementationAddress(ethers,proxy);
      await expect(proxy.connect(c.user).upgradeToAndCall(await next.getAddress(),"0x")).to.be.revertedWithCustomError(proxy,"OwnableUnauthorizedAccount").withArgs(c.user.address);
      await expect(proxy.upgradeToAndCall(await c.consumer.getAddress(),"0x")).to.be.revertedWithCustomError(proxy,"ERC1967InvalidImplementation");
      expect(await implementationAddress(ethers,proxy)).to.equal(before);
      await proxy.transferOwnership(c.user.address);
      await expect(proxy.connect(c.user).upgradeToAndCall(await next.getAddress(),"0x")).to.be.revertedWithCustomError(proxy,"OwnableUnauthorizedAccount");
      await proxy.connect(c.user).acceptOwnership();
      await expect(proxy.upgradeToAndCall(await next.getAddress(),"0x")).to.be.revertedWithCustomError(proxy,"OwnableUnauthorizedAccount");
      await proxy.connect(c.user).upgradeToAndCall(await next.getAddress(),"0x");
      expect(await implementationAddress(ethers,proxy)).to.equal(await next.getAddress());
    }
  });
  it("preserves balances, fee credits, ownership, requests, epochs and accepted public replay across compatible upgrades",async()=>{
    const c=await networkHelpers.loadFixture(fixture),rejector=await ethers.deployContract("RejectingKeeperRecipient");
    await c.rng.setKeeperFeeBps(5000);await c.registry.setCommitter(await rejector.getAddress());
    const served=await request(c,"history-before-upgrade"),proof=await prepareProof(c,served);
    await c.consumer.setMode(1);const accepted=(await(await c.rng.fulfillRandomness(served,proof,{gasLimit:2_000_000})).wait())!;
    const pending=await request(c,"pending-before-upgrade"),pendingProof=await prepareProof(c,pending);
    await c.rng.setFeeRecipient(c.user.address);await c.rng.transferOwnership(c.stranger.address);await c.registry.transferOwnership(c.stranger.address);
    const oldAddress=await c.rng.getAddress(),registryAddress=await c.registry.getAddress();
    const oldRequest=await c.rng.getRequest(served),oldPending=await c.rng.getRequest(pending),epoch=await c.registry.getEpoch(oldRequest.epochId);
    const scalarNames=["owner","pendingOwner","feeRecipient","initialFeeRecipient","keeperFeeBps","minFee","initialMinFee","feeMultiplier","fulfillGasOverhead","earnedFees","totalKeeperCredits","totalRefundCredits","nextRequestId","lastServedIndex","protocolConfigurationHash","keyHash"];
    const scalarState=await Promise.all(scalarNames.map(name=>c.rng[name]()));
    const balance=await ethers.provider.getBalance(oldAddress),credit=await c.rng.keeperCredits(await rejector.getAddress());
    const requestBlock=(await ethers.provider.getBlock(Number(oldRequest.requestBlock)))!,targetBlock=(await ethers.provider.getBlock(Number(oldRequest.targetBlock)))!;
    const commitment=(await c.registry.queryFilter(c.registry.filters.EpochCommitted(oldRequest.epochId)))[0];
    const commitmentEvent=c.registry.interface.parseLog(commitment)!;
    const catalog={signers:EPOCH_TEST_SIGNERS as [string,string,string,string],registry:registryAddress,chainId:31337n,firstEpochStart:await c.registry.firstEpochStart()};
    const context={chainId:31337n,coordinator:oldAddress,keyHash:await c.rng.keyHash(),requestId:served,consumer:await c.consumer.getAddress(),clientSeed:oldRequest.clientSeed,mapping:builtins.raw(),requestBlock:oldRequest.requestBlock,targetBlock:oldRequest.targetBlock,blockHash:targetBlock.hash!,epochId:oldRequest.epochId,epochHash:oldRequest.epochHash};
    expect(deriveRequestSeed(context)).to.equal(proof.seed);
    expect(deriveRequestSeed({...context,coordinator:await implementationAddress(ethers,c.rng)})).not.to.equal(proof.seed);
    const replayInput={context,configuration:{publicKey:publicKey(),feeRecipient:await c.rng.initialFeeRecipient(),initialMinFee:FEE,confirmationBlocks:1,registry:registryAddress,catalogHash:await c.registry.catalogHash(),firstEpochStart:catalog.firstEpochStart},protocolConfigurationHash:await c.rng.protocolConfigurationHash(),epoch:{catalog,record:epoch,commitTimestamp:BigInt((await ethers.provider.getBlock(commitment.blockNumber))!.timestamp),packet:commitmentEvent.args.packet},requestedAt:BigInt(requestBlock.timestamp),deadline:oldRequest.deadline,acceptanceTimestamp:BigInt((await ethers.provider.getBlock(accepted.blockNumber))!.timestamp),acceptanceBlock:BigInt(accepted.blockNumber),vrfProof:proof,recorded:oldRequest};
    const replayBefore=replayCoordinator(replayInput);
    const next=await upgradePair(c);
    expect(await c.rng.getAddress()).to.equal(oldAddress);expect(await c.registry.getAddress()).to.equal(registryAddress);
    expect(await implementationAddress(ethers,c.rng)).to.equal(await next.coordinator.getAddress());expect(await implementationAddress(ethers,c.registry)).to.equal(await next.registry.getAddress());
    expect(await Promise.all(scalarNames.map(name=>c.rng[name]()))).to.deep.equal(scalarState);
    expect(await ethers.provider.getBalance(oldAddress)).to.equal(balance);expect(await c.rng.keeperCredits(await rejector.getAddress())).to.equal(credit);
    expect(Array.from(await c.rng.getRequest(served))).to.deep.equal(Array.from(oldRequest));expect(Array.from(await c.rng.getRequest(pending))).to.deep.equal(Array.from(oldPending));
    expect(Array.from(await c.registry.getEpoch(oldRequest.epochId))).to.deep.equal(Array.from(epoch));
    expect(await c.registry.owner()).to.equal(c.owner.address);expect(await c.registry.pendingOwner()).to.equal(c.stranger.address);expect(await c.registry.committer()).to.equal(await rejector.getAddress());
    expect(await c.rng.servedRequestAt(1)).to.equal(served);expect(replayCoordinator(replayInput)).to.deep.equal(replayBefore);
    await c.consumer.setMode(0);await c.rng.retryCallback(served,200000,{gasLimit:500000});expect(await c.rng.keeperCredits(await rejector.getAddress())).to.equal(credit);
    expect(await c.rng.requestSeed(pending)).to.equal(pendingProof.seed);await c.rng.fulfillRandomness(pending,pendingProof,{gasLimit:2_000_000});
    expect(await c.consumer.results(pending)).to.equal(proofOutput(pendingProof));expect(replayCoordinator(replayInput)).to.deep.equal(replayBefore);
    expect(await next.coordinator.attach(oldAddress).getFunction("upgradeMarker")()).to.equal(ethers.id("coordinator-storage-probe"));
  });
  it("pins the fixtures to the implementations live behind both public registries and coordinators",async()=>{
    // The 640b60c creation bytecode, deployed at the live address, hashes to the Arc Mainnet pin.
    expect(deployedHere(mainnet,mainnetEpoch.implementation,mainnetEpoch.runtimeCodeHash),"640b60c is Arc Mainnet's implementation or one it upgraded from").to.equal(true);
    expect(ethers.keccak256(runtimeCodeAt(mainnetEpoch.deployedBytecode,mainnetEpoch.immutableReferences,mainnetEpoch.implementation))).to.equal(mainnetEpoch.runtimeCodeHash);
    // Arc Testnet runs 96cc722, recorded with eth_getCode; its previous implementation was the mainnet one.
    expect(deployedHere(testnet,testnetEpoch.implementation,testnetEpoch.runtimeCodeHash),"96cc722 is Arc Testnet's implementation or one it upgraded from").to.equal(true);
    expect(ethers.keccak256(testnetEpoch.deployedBytecode)).to.equal(testnetEpoch.runtimeCodeHash);
    // Arc Testnet reached 96cc722 from the mainnet implementation, so both fixtures sit on one recorded chain.
    expect(testnet.implementationUpgrades.some((entry:any)=>
      entry.implementation===testnetEpoch.implementation&&entry.previousImplementationCodeHash===mainnetEpoch.runtimeCodeHash)).to.equal(true);
    for(const manifest of [mainnet,testnet]){
      const current=manifest.coordinatorImplementation===liveCoordinator.implementation&&manifest.coordinatorImplementationCodeHash===liveCoordinator.runtimeCodeHash;
      const upgraded=(manifest.implementationUpgrades??[]).some((entry:any)=>entry.contract==="coordinator"&&entry.previousImplementation===liveCoordinator.implementation&&entry.previousImplementationCodeHash===liveCoordinator.runtimeCodeHash);
      expect(current||upgraded,"the live coordinator fixture is this network's coordinator or the one it upgraded from").to.equal(true);
    }
    expect(ethers.keccak256(liveCoordinator.deployedBytecode)).to.equal(liveCoordinator.runtimeCodeHash);
    const next=await ethers.deployContract("EpochEntropy");
    // The owner tooling accepts exactly this deployment: its runtime code is the compiled artifact at its own address.
    expect(ethers.keccak256(await ethers.provider.getCode(await next.getAddress()))).to.equal(await compiledRuntimeCodeHash("EpochEntropy",await next.getAddress()));
    for(const live of [mainnetEpoch,testnetEpoch])expect(await compiledRuntimeCodeHash("EpochEntropy",live.implementation)).not.to.equal(live.runtimeCodeHash);
  });

  for(const live of LIVE_REGISTRIES){
    it(`upgrades a registry running the ${live.label} implementation atomically, preserving storage, history and coordinator service`,async()=>{
      const [owner,committer,nextOwner,user,backup]=await ethers.getSigners();
      const {registry,rng,consumer,address,legacySelect}=await liveDeployment(live,owner,committer);
      const commit=(epochId:bigint,attempt:number,a:object)=>attempt===0?registry.connect(committer).commitEpoch(epochId,a):registry.connect(committer).commitEpochFallback(epochId,attempt,a);
      const firstEpochStart=await registry.firstEpochStart();

      // Deployed code: a request served from epoch 1, published from a source other than slot 1 (the retired ANU
      // recipe on 640b60c; no live epoch was committed from it), then an escrowed epoch-2 request left unpublished.
      await mineTo(firstEpochStart);
      await consumer.request(ethers.id(`served-before-upgrade-${live.label}`),200000,user.address,{value:FEE});
      const served=await consumer.lastRequestId() as bigint;
      let attempt1=0,selection1=await legacySelect(1n,0);
      if(Number(selection1.source)===1){attempt1=1;selection1=await legacySelect(1n,1);await mineTo(await registry.epochStart(1n)+20n);}
      const commit1=(await(await commit(1n,attempt1,await signSelection({recipe:Number(selection1.source),airnode:selection1.airnode,queryHash:selection1.queryHash},BigInt(await networkHelpers.time.latest())))).wait())!;
      await networkHelpers.mine(2);
      const proof1=makeProof(await rng.requestSeed(served));
      const accepted1=(await(await rng.fulfillRandomness(served,proof1,{gasLimit:2_000_000})).wait())!;
      await mineTo(await registry.epochStart(2n));
      await consumer.request(ethers.id(`pending-across-upgrade-${live.label}`),200000,user.address,{value:FEE});
      const pending=await consumer.lastRequestId() as bigint,pendingBefore=await rng.getRequest(pending);
      expect(pendingBefore.epochHash).to.equal(ethers.ZeroHash);
      await registry.transferOwnership(nextOwner.address);
      const storageBefore=await registryStorage(address,[1n,2n]);
      const views=async()=>({owner:await registry.owner(),pendingOwner:await registry.pendingOwner(),committer:await registry.committer(),firstEpochStart:await registry.firstEpochStart(),
        catalogHash:await registry.catalogHash(),signers:[await registry.hyperliquidSigner(),await registry.btcTradeSigner(),await registry.ethTradeSigner()],
        epoch1:Array.from(await registry.getEpoch(1)),anchors:[await registry.epochAnchors(1),await registry.epochAnchors(2)],epochForHead:await registry.epochForBlock(await ethers.provider.getBlockNumber())});
      const viewsBefore=await views();
      // Both live layouts leave the retired catalog slot, the catalog array and the new recipe array empty.
      expect([8,9,10].map(slot=>storageBefore[ethers.toBeHex(slot)])).to.deep.equal([ethers.ZeroHash,ethers.ZeroHash,ethers.ZeroHash]);
      expect(storageBefore[INITIALIZABLE]).to.equal(ethers.toBeHex(1,32));

      const next=await ethers.deployContract("EpochEntropy");
      const upgrade=(await(await registry.upgradeToAndCall(await next.getAddress(),registry.interface.encodeFunctionData("initializeRecipeRegistry"))).wait())!;
      expect(await implementationAddress(ethers,registry)).to.equal(await next.getAddress());
      // Raw storage: every existing slot is unchanged; only the recipe array (slot 10 and its data) and the initializer version were written.
      const storageAfter=await registryStorage(address,[1n,2n]);
      const recipeData=new Set(Array.from({length:32},(_,i)=>ethers.toBeHex(arrayBase(10n)+BigInt(i))));
      for(const [slot,value] of Object.entries(storageBefore))
        if(slot!==ethers.toBeHex(10)&&slot!==INITIALIZABLE&&!recipeData.has(slot))expect(storageAfter[slot],`slot ${slot}`).to.equal(value);
      expect(storageAfter[ethers.toBeHex(10)]).to.equal(ethers.toBeHex(6,32));expect(storageAfter[INITIALIZABLE]).to.equal(ethers.toBeHex(2,32));
      expect(await views()).to.deep.equal(viewsBefore);
      // Slot 1 keeps the initial signer; 640b60c named its getter anuSigner.
      expect(await registry.ethereumBlockSigner()).to.equal(EPOCH_TEST_SIGNERS[1]);
      // The built-in recipes are registered in the upgrade transaction with exactly their ids.
      expect(upgrade.logs.filter((log:any)=>log.address===address).map((log:any)=>registry.interface.parseLog(log)).filter((e:any)=>e?.name==="RecipeRegistered").map((e:any)=>[Number(e.args.recipe),e.args.canonicalRequest,e.args.template,e.args.body]))
        .to.deep.equal(BUILTIN_EPOCH_RECIPES.map(r=>[r.id,r.canonicalRequest,r.template,r.body]));
      expect(await registry.recipeCount()).to.equal(6n);
      for(const recipe of BUILTIN_EPOCH_RECIPES)expect(Array.from(await registry.getRecipe(recipe.id))).to.deep.equal([ethers.id(recipe.canonicalRequest),recipe.canonicalRequest,recipe.template,recipe.body]);
      // The upgrade step cannot run again, as the owner or anyone else.
      await expect(registry.initializeRecipeRegistry()).to.be.revertedWithCustomError(registry,"InvalidInitialization");
      await expect(registry.connect(user).getFunction("initializeRecipeRegistry")()).to.be.revertedWithCustomError(registry,"InvalidInitialization");

      // History published by the deployed code replays with the current library, including the coordinator transcript.
      const base={registry:address,chainId:31337n,firstEpochStart};
      const configuration={publicKey:publicKey(),feeRecipient:await rng.initialFeeRecipient(),initialMinFee:FEE,confirmationBlocks:1,registry:address,catalogHash:await registry.catalogHash(),firstEpochStart};
      expect(epochProtocolConfigurationHash(configuration)).to.equal(await rng.protocolConfigurationHash());
      const replayRequest=async(id:bigint,proof:ReturnType<typeof makeProof>,commitReceipt:{blockNumber:number;logs:readonly any[]},acceptance:{blockNumber:number})=>{
        const r=await rng.getRequest(id),[hash,recipes,signers]=await registry.catalogAt(r.epochId);
        const packet=registry.interface.parseLog(commitReceipt.logs.find(log=>log.address===address))!.args.packet;
        const context={chainId:31337n,coordinator:await rng.getAddress(),keyHash:await rng.keyHash(),requestId:id,consumer:await consumer.getAddress(),clientSeed:r.clientSeed,mapping:builtins.raw(),
          requestBlock:r.requestBlock,targetBlock:r.targetBlock,blockHash:(await ethers.provider.getBlock(Number(r.targetBlock)))!.hash!,epochId:r.epochId,epochHash:r.epochHash};
        expect(deriveRequestSeed(context)).to.equal(proof.seed);
        return replayCoordinator({context,configuration,protocolConfigurationHash:await rng.protocolConfigurationHash(),
          epoch:{catalog:resolveEpochCatalog(base,{hash,recipes,signers}),record:await registry.getEpoch(r.epochId),commitTimestamp:BigInt((await ethers.provider.getBlock(commitReceipt.blockNumber))!.timestamp),packet},
          requestedAt:r.deadline-60n,deadline:r.deadline,acceptanceTimestamp:BigInt((await ethers.provider.getBlock(acceptance.blockNumber))!.timestamp),acceptanceBlock:BigInt(acceptance.blockNumber),vrfProof:proof,recorded:r});
      };
      expect((await replayRequest(served,proof1,commit1,accepted1)).proof.matchesRecordedState).to.equal(true);
      expect((await registry.getEpoch(1)).epochHash).to.equal(viewsBefore.epoch1[0]);

      // The deployed coordinator serves the request escrowed before the upgrade from an epoch the new code publishes,
      // here through a backup committer; before its own upgrade the keeper share goes to committer() whoever submits.
      expect([await registry.backupCommitterCount(),await registry.isBackupCommitter(backup.address)]).to.deep.equal([0n,false]);
      await registry.setBackupCommitter(backup.address,true);
      const selection2=await registry.getEpochSelection(2);
      const commit2=(await(await registry.connect(backup).commitEpoch(2n,await signSelection(selection2,BigInt(await networkHelpers.time.latest())))).wait())!;
      expect((await rng.getRequest(pending)).targetBlock).to.equal(BigInt(commit2.blockNumber)+1n);
      await networkHelpers.mine(2);
      const proof2=makeProof(await rng.requestSeed(pending));
      const accepted2=(await(await rng.fulfillRandomness(pending,proof2,{gasLimit:2_000_000})).wait())!;
      expect((await rng.getRequest(pending)).delivered).to.equal(true);
      const keeperPaid=accepted2.logs.map((log:any)=>rng.interface.parseLog(log)).find((event:any)=>event?.name==="KeeperFeePaid");
      expect(keeperPaid!.args.keeper).to.equal(committer.address);expect(await consumer.results(pending)).to.equal(proofOutput(proof2));
      expect((await replayRequest(pending,proof2,commit2,accepted2)).proof.matchesRecordedState).to.equal(true);

      // The coordinator upgrade follows the registry in the same batch: the registry must already answer
      // isAuthorizedCommitter when the new payment path reads it.
      const nextCoordinator=await ethers.deployContract("D20VRFCoordinator");
      const coordinatorViews=async()=>({owner:await rng.owner(),feeRecipient:await rng.feeRecipient(),initialFeeRecipient:await rng.initialFeeRecipient(),
        keeperFeeBps:await rng.keeperFeeBps(),earnedFees:await rng.earnedFees(),keyHash:await rng.keyHash(),epochRegistry:await rng.epochRegistry(),
        protocolConfigurationHash:await rng.protocolConfigurationHash(),lastServedRequestId:await rng.lastServedRequestId(),served:Array.from(await rng.getRequest(served))});
      const coordinatorBefore=await coordinatorViews();
      await rng.upgradeToAndCall(await nextCoordinator.getAddress(),"0x");
      expect(await implementationAddress(ethers,rng)).to.equal(await nextCoordinator.getAddress());
      expect(await coordinatorViews()).to.deep.equal(coordinatorBefore);
      // Each authorized keeper now earns the share of what it serves; an outside submitter still pays committer().
      const keeperShare=async(id:bigint,submitter:any)=>{
        await networkHelpers.mine(2);
        const proof=makeProof(await rng.requestSeed(id));
        const receipt=(await(await (rng.connect(submitter) as any).fulfillRandomness(id,proof,{gasLimit:2_000_000})).wait())!;
        expect((await rng.getRequest(id)).delivered).to.equal(true);
        return receipt.logs.map((log:any)=>rng.interface.parseLog(log)).find((event:any)=>event?.name==="KeeperFeePaid")!.args.keeper;
      };
      const paidRequests=[];
      for(const label of ["paid-backup","paid-committer","paid-stranger"]){
        await consumer.request(ethers.id(`${label}-${live.label}`),200000,user.address,{value:FEE});
        paidRequests.push(await consumer.lastRequestId() as bigint);
      }
      expect(await keeperShare(paidRequests[0],backup)).to.equal(backup.address);
      expect(await keeperShare(paidRequests[1],committer)).to.equal(committer.address);
      expect(await keeperShare(paidRequests[2],user)).to.equal(committer.address);

      // The rollout catalog is scheduled after the upgrade and serves a request from its first epoch.
      const rollout=providerTestCatalog(),from=await registry.epochForBlock(await ethers.provider.getBlockNumber())+2n;
      await registry.scheduleCatalog(rollout.recipes,rollout.signers,from);
      await mineTo(await registry.epochStart(from));
      await consumer.request(ethers.id(`rollout-${live.label}`),200000,user.address,{value:FEE});
      const rolloutRequest=await consumer.lastRequestId() as bigint;
      const commit3=(await(await commit(from,0,await signSelection(await registry.getEpochSelection(from),BigInt(await networkHelpers.time.latest())))).wait())!;
      expect((await registry.getEpoch(from)).catalogHash).to.equal(epochCatalogHash(rollout.signers,rollout.recipes));
      await networkHelpers.mine(2);
      const proof3=makeProof(await rng.requestSeed(rolloutRequest));
      const accepted3=(await(await rng.fulfillRandomness(rolloutRequest,proof3,{gasLimit:2_000_000})).wait())!;
      expect((await replayRequest(rolloutRequest,proof3,commit3,accepted3)).proof.matchesRecordedState).to.equal(true);
      expect(await registry.catalogHash()).to.equal(viewsBefore.catalogHash);
      expect((await replayRequest(served,proof1,commit1,accepted1)).proof.matchesRecordedState).to.equal(true);
    });

    it(`pays committer() through the catch path while the registry still runs the ${live.label} implementation`,async()=>{
      const [owner,committer,,user,stranger]=await ethers.getSigners();
      const {registry,rng,consumer,legacySelect}=await liveDeployment(live,owner,committer);
      // Only the coordinator is upgraded: the registry cannot answer isAuthorizedCommitter at all, which is the
      // state between the two calls of the upgrade batch, and the state of a coordinator upgraded on its own.
      await rng.upgradeToAndCall(await (await ethers.deployContract("D20VRFCoordinator")).getAddress(),"0x");
      await expect(registry.isAuthorizedCommitter(committer.address)).to.revert(ethers);

      await mineTo(await registry.firstEpochStart());
      await consumer.request(ethers.id(`catch-path-${live.label}`),200000,user.address,{value:FEE});
      const pending=await consumer.lastRequestId() as bigint;
      let attempt=0,selection=await legacySelect(1n,0);
      if(Number(selection.source)===1){attempt=1;selection=await legacySelect(1n,1);await mineTo(await registry.epochStart(1n)+20n);}
      const attestation=await signSelection({recipe:Number(selection.source),airnode:selection.airnode,queryHash:selection.queryHash},BigInt(await networkHelpers.time.latest()));
      await (attempt===0?registry.connect(committer).commitEpoch(1n,attestation):registry.connect(committer).commitEpochFallback(1n,attempt,attestation));
      await networkHelpers.mine(2);

      // A submitter the registry cannot vouch for is served exactly as before, and the share is never skipped.
      const proof=makeProof(await rng.requestSeed(pending));
      const receipt=(await(await (rng.connect(stranger) as any).fulfillRandomness(pending,proof,{gasLimit:2_000_000})).wait())!;
      const request=await rng.getRequest(pending);
      expect([request.fulfilled,request.delivered]).to.deep.equal([true,true]);
      expect(await consumer.results(pending)).to.equal(proofOutput(proof));
      const paid=receipt.logs.map((log:any)=>rng.interface.parseLog(log)).find((event:any)=>event?.name==="KeeperFeePaid")!;
      expect([paid.args.keeper,paid.args.paid]).to.deep.equal([committer.address,true]);
    });
  }

  it("recovers from an upgrade sent without the registry step only through the owner, and refuses catalogs under the old recipe ids",async()=>{
    const [owner,committer,,user]=await ethers.getSigners();
    const testnetLive=LIVE_REGISTRIES.find(live=>live.label.includes("96cc722"))!,mainnetLive=LIVE_REGISTRIES.find(live=>live.label.includes("640b60c"))!;
    const next=await ethers.deployContract("EpochEntropy"),init=next.interface.encodeFunctionData("initializeRecipeRegistry");
    {
      const {registry}=await liveDeployment(testnetLive,owner,committer);
      await mineTo(await registry.firstEpochStart());
      await registry.upgradeToAndCall(await next.getAddress(),"0x");
      // Without its recipes the initial catalog cannot select a source, so nothing can be published; requests still escrow.
      await expect(registry.getEpochSelection(1)).to.be.revertedWithCustomError(registry,"InvalidConfig");
      await expect(registry.connect(user).getFunction("initializeRecipeRegistry")()).to.be.revertedWithCustomError(registry,"OwnableUnauthorizedAccount");
      await registry.initializeRecipeRegistry();
      await expect(registry.initializeRecipeRegistry()).to.be.revertedWithCustomError(registry,"InvalidInitialization");
      await registry.connect(committer).commitEpoch(1,await epochAttestation(registry,1n,ethers));
      expect(await registry.recipeCount()).to.equal(6n);
    }
    {
      // A 96cc722 catalog scheduled under the hardcoded ids (5 was SOL mid, 6 and 7 other listings) would change meaning.
      const {registry,address}=await liveDeployment(testnetLive,owner,committer);
      const legacy=new ethers.Contract(address,["function scheduleCatalog(uint8[],address[],uint64)"],owner);
      await legacy.scheduleCatalog([0,1,2,4,5,6,3,7],Array(8).fill(committer.address),2);
      const before=await implementationAddress(ethers,registry);
      await expect(registry.upgradeToAndCall(await next.getAddress(),init)).to.be.revertedWithCustomError(next,"InvalidConfig");
      expect(await implementationAddress(ethers,registry)).to.equal(before);
    }
    {
      // A 640b60c four-signer catalog version occupies the retired slot 8.
      const {registry,address}=await liveDeployment(mainnetLive,owner,committer);
      const legacy=new ethers.Contract(address,["function scheduleCatalog(address[4],uint64)"],owner);
      await legacy.scheduleCatalog(EPOCH_TEST_SIGNERS,2);
      expect(await ethers.provider.getStorage(address,8)).to.equal(ethers.toBeHex(1,32));
      await expect(registry.upgradeToAndCall(await next.getAddress(),init)).to.be.revertedWithCustomError(next,"InvalidConfig");
    }
  });

  it("keeps an unpublished escrowed request and its source checkpoint usable after both upgrades",async()=>{
    const [owner,user]=await ethers.getSigners();
    const registry=await deployProxy(ethers,"EpochEntropy",[EPOCH_TEST_SIGNERS,owner.address,owner.address]);
    const rng=await deployProxy(ethers,"D20VRFCoordinator",[publicKey(),owner.address,owner.address,FEE,1,await registry.getAddress(),0]);
    await rng.setPricing(FEE,0,300000);
    const consumer=await ethers.deployContract("TestConsumer",[await rng.getAddress()]);
    await networkHelpers.mine(Number(await registry.firstEpochStart())-await ethers.provider.getBlockNumber());
    await consumer.request(ethers.id("unpublished-before-upgrade"),200000,user.address,{value:FEE});
    const before=await rng.getRequest(1),anchor=await registry.epochAnchors(before.epochId);
    expect(before.targetBlock).to.equal(0n);expect(before.epochHash).to.equal(ethers.ZeroHash);
    const coordinatorNext=await ethers.deployContract("CoordinatorUpgradeProbe"),registryNext=await ethers.deployContract("EpochUpgradeProbe");
    await rng.upgradeToAndCall(await coordinatorNext.getAddress(),"0x");await registry.upgradeToAndCall(await registryNext.getAddress(),"0x");
    expect(Array.from(await rng.getRequest(1))).to.deep.equal(Array.from(before));expect(await registry.epochAnchors(before.epochId)).to.equal(anchor);
    expect(await ethers.provider.getBalance(await rng.getAddress())).to.equal(FEE);
    const committed=(await(await registry.commitEpoch(before.epochId,await epochAttestation(registry,before.epochId,ethers))).wait())!;
    expect((await rng.getRequest(1)).targetBlock).to.equal(BigInt(committed.blockNumber)+1n);await networkHelpers.mine(2);
    const proof=makeProof(await rng.requestSeed(1));await rng.fulfillRandomness(1,proof,{gasLimit:2_000_000});
    expect((await rng.getRequest(1)).deadline).to.equal(before.deadline);expect(await consumer.results(1)).to.equal(proofOutput(proof));
  });
});
