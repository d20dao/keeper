import {test} from "node:test";
import assert from "node:assert/strict";
import {mkdtemp,readFile,writeFile} from "node:fs/promises";
import {tmpdir} from "node:os";
import {join} from "node:path";
import {pathToFileURL,fileURLToPath} from "node:url";
import {AbiCoder,Contract,Interface,concat,getAddress,keccak256,type Wallet} from "ethers";
import {signTestRound,testBeacon} from "../../test/helpers/beacon.ts";
import {Stop} from "../lib/deployment.ts";
import {compiled,coordinatorInterface,link,type RoundPlan} from "../lib/robinhood-deploy.ts";
import {parseAdminRequest,parseRoundManifest,planAdminAction,sendAdminAction,type AdminPlan} from "../lib/robinhood-admin.ts";
import {SAFE_SINGLETONS,accountKind,checkProfileSafe,readSafe,requireProfileSafe} from "../lib/safe.ts";
import {BACKUP,DEPLOYER,KEEPER,OWNER,SAFE_ABI,SAFE_SIGNERS,SAFE_STAND_IN_CODE,STRANGER,deployLocal,envFile,execSafe,localChain,localSafe,operatorDirectory,root,
  rpcEndpoint,run,safeProfile,type LocalChain} from "./round-local.ts";

const NO_NETWORK=pathToFileURL(fileURLToPath(new URL("./fixtures/no-network.mjs",import.meta.url))).href;
const STAND_IN_PRELOAD=pathToFileURL(fileURLToPath(new URL("./fixtures/stand-in-owner.mjs",import.meta.url))).href;
const UNDECIDED_PRELOAD=pathToFileURL(fileURLToPath(new URL("./fixtures/undecided-owner.mjs",import.meta.url))).href;
const UNREVIEWED_PRELOAD=pathToFileURL(fileURLToPath(new URL("./fixtures/unreviewed-source.mjs",import.meta.url))).href;
const sorted=(addresses:string[])=>[...addresses].sort((a,b)=>BigInt(a)<BigInt(b)?-1:1);
const refusal=(pattern:RegExp)=>(error:unknown)=>error instanceof Stop&&pattern.test(error.message);
const tmp=(name:string)=>mkdtemp(join(tmpdir(),`d20-round-admin-${name}-`));
const abi=AbiCoder.defaultAbiCoder();

/** A local deployment with its manifest, and helpers to plan and send an action against it. */
async function deployed(local:LocalChain,name:string){
  const dir=await tmp(name),{plan,manifestPath}=await deployLocal(local,dir);
  let manifest=parseRoundManifest(JSON.parse(await readFile(manifestPath,"utf8")),manifestPath);
  const face=await coordinatorInterface(),coordinator=new Contract(plan.step.coordinator.address,face,local.provider);
  const request=(action:string,args:string[]=[],options={})=>parseAdminRequest(action,args,options);
  const planned=async(action:string,args:string[]=[],options={},send=false)=>planAdminAction({provider:local.provider,chain:local.chain,manifest,request:await request(action,args,options),send});
  const send=async(action:string,args:string[],signer:Wallet=OWNER,options={})=>
    sendAdminAction({provider:local.provider,chain:local.chain,plan:await planned(action,args,options,true),signer,journalDirectory:dir});
  return {dir,plan,manifestPath,face,coordinator,planned,send,request,get manifest(){return manifest;},set manifest(next){manifest=next;}};
}
/** A coordinator implementation deployed with CREATE from the deployer: the compiled contract (or the test probe) linked to the set's helpers. */
async function anotherImplementation(local:LocalChain,plan:RoundPlan,probe=false){
  const artifact=probe?JSON.parse(await readFile(join(root,"artifacts/contracts/robinhood/test/CoordinatorRobinhoodUpgradeProbe.sol/CoordinatorRobinhoodUpgradeProbe.json"),"utf8"))
    :(await compiled("D20VRFCoordinatorRobinhood")).artifact;
  const data=concat([link(artifact.bytecode,artifact.linkReferences,plan.step.mappingLibrary.address),abi.encode(["address"],[plan.step.proofVerifier.address])]);
  const receipt=await (await DEPLOYER.connect(local.provider).sendTransaction({data})).wait();
  return receipt!.contractAddress!;
}

test("every action's arguments are checked before any network access",async()=>{
  const cases:Array<[string|undefined,string[],object,RegExp]>=[
    [undefined,[],{},/Unknown action ""/],["set-owner",[],{},/Unknown action "set-owner"; the actions are status, set-keeper/],
    ["set-keeper",[],{},/set-keeper takes <address>/],["set-keeper",["0x1234"],{},/must be an address/],["set-keeper",["0x"+"00".repeat(20)],{},/must not be the zero address/],
    ["set-backup-keeper",[BACKUP.address,"yes"],{},/takes true to allow the wallet or false/],["status",["extra"],{},/status takes no arguments/],
    ["set-pricing",["10000000000000001","2","360000"],{},/minFeeWei must be a whole number from 0 to 10000000000000000/],
    ["set-pricing",["1","21","360000"],{},/multiplier must be a whole number from 0 to 20/],["set-pricing",["1","2","99999"],{},/overhead must be a whole number from 100000/],
    ["set-pricing",["0","0","360000"],{},/zero minimum fee needs a multiplier/],["set-pricing",["-1","2","360000"],{},/minFeeWei must be/],
    ["set-keeper-bps",["10001"],{},/keeper share must be a whole number from 0 to 10000/],["set-refund-bps",["4999"],{},/refund share must be a whole number from 5000 to 10000/],
    ["schedule-beacon",["256","1"],{},/beacon id must be a whole number from 0 to 255/],["schedule-beacon",["1",String(2n**40n)],{},/fromTime must be a whole number/],
    ["register-beacon",["does-not-exist.json"],{},/could not be read/],["upgrade",[STRANGER.address],{},/needs --approve-code-hash/],
    ["upgrade",[STRANGER.address],{"approve-code-hash":"0x12"},/--approve-code-hash must be a 32-byte hash/],
    ["set-keeper",[STRANGER.address],{"approve-code-hash":"0x"+"11".repeat(32)},/apply to upgrade only/],["set-keeper",[STRANGER.address],{address:STRANGER.address},/applies to status only/],
  ];
  for(const [action,args,options,pattern] of cases)await assert.rejects(parseAdminRequest(action,args,options),refusal(pattern),`${action} ${args.join(" ")}`);
  assert.deepEqual(await parseAdminRequest("set-backup-keeper",[BACKUP.address.toLowerCase(),"false"]),{action:"set-backup-keeper",address:BACKUP.address,allowed:false});
  assert.deepEqual(await parseAdminRequest("set-pricing",["25000000000000","2","360000"]),{action:"set-pricing",minFee:25_000_000_000_000n,multiplier:2,overhead:360000});
  const dir=await tmp("parse"),file=join(dir,"beacon.json");
  await writeFile(file,JSON.stringify({verifier:STRANGER.address,chainHash:"0x"+"00".repeat(32),publicKey:"0x"+"11".repeat(128),genesis:1,period:3,sampleRound:1,sampleSignature:"0x"+"22".repeat(64)}));
  await assert.rejects(parseAdminRequest("register-beacon",[file]),refusal(/chainHash must not be zero/));
  assert.throws(()=>parseRoundManifest({network:"robinhood-testnet",chainId:46630},"m.json"),refusal(/not a round coordinator's manifest/));
});

test("every owner action plans, prints and sends its exact call against a local deployment, and each refusal stops it first",async()=>{
  const local=await localChain("robinhood-testnet",OWNER.address);
  try{
    const d=await deployed(local,"actions"),c=d.coordinator,proxy=d.plan.step.coordinator.address;
    // status: every view, and the proxy and implementation the manifest records.
    const status=(await d.planned("status")).status as any;
    assert.deepEqual(status.matchesManifest,{proxyCodeHash:true,implementation:true,implementationCodeHash:true});
    assert.deepEqual(status.owner,{address:OWNER.address,kind:"EOA"});assert.equal(status.keeper,KEEPER.address);
    assert.equal(status.keeperFeeBps,"8000");assert.equal(status.roundLead,"3");assert.equal(status.minScheduleLead,"600");assert.equal(status.maxBeaconPeriod,"10");
    assert.equal(status.beacons.length,1);assert.equal(status.beacons[0].identity,d.plan.beaconIdentity);assert.equal(status.schedule.pending,null);
    assert.deepEqual(status.warnings,[]);assert.equal(status.requests.nextRequestId,"1");
    assert.deepEqual(((await d.planned("status",[],{address:BACKUP.address})).status as any).backupKeeper,{address:BACKUP.address,allowed:false});

    // A plan says exactly what it calls, from whom and how, and sends nothing.
    const keeperPlan=await d.planned("set-keeper",[STRANGER.address]);
    assert.deepEqual(keeperPlan.call,{from:OWNER.address,to:proxy,value:"0",function:"setKeeper(address)",args:[STRANGER.address],data:d.face.encodeFunctionData("setKeeper",[STRANGER.address])});
    assert.deepEqual(keeperPlan.sender,{role:"owner",address:OWNER.address,kind:"EOA"});
    assert.equal(keeperPlan.execution,"owner key");assert.equal(keeperPlan.safeTransaction,undefined);
    assert.equal(await c.keeper(),KEEPER.address);
    await assert.rejects(d.planned("set-keeper",[KEEPER.address]),refusal(/is already the keeper/));

    // Backup keepers: allowed, refused twice, the keeper refused, and removed.
    await d.send("set-backup-keeper",[BACKUP.address,"true"]);
    assert.equal(await c.isBackupKeeper(BACKUP.address),true);
    await assert.rejects(d.planned("set-backup-keeper",[BACKUP.address,"true"]),refusal(/already a backup keeper/));
    await assert.rejects(d.planned("set-backup-keeper",[KEEPER.address,"true"]),refusal(/is the keeper; a backup keeper needs its own wallet/));
    await assert.rejects(d.planned("set-keeper",[BACKUP.address]),refusal(/is a backup keeper; remove it/));
    await d.send("set-backup-keeper",[BACKUP.address,"false"]);
    assert.equal(await c.isBackupKeeper(BACKUP.address),false);
    await assert.rejects(d.planned("set-backup-keeper",[BACKUP.address,"false"]),refusal(/is not a backup keeper/));

    await d.send("set-keeper",[STRANGER.address]);
    assert.equal(await c.keeper(),STRANGER.address);
    const pricing=await d.planned("set-pricing",["30000000000000","3","400000"]);
    assert.equal(pricing.call?.function,"setPricing(uint256,uint16,uint32)");assert.deepEqual((pricing.details as any).current,{minFeeWei:"25000000000000",feeMultiplier:"2",fulfillGasOverhead:"405000"});
    await d.send("set-pricing",["30000000000000","3","400000"]);
    assert.deepEqual(Array.from(await c.pricing()),[30_000_000_000_000n,3n,400_000n]);
    await assert.rejects(d.planned("set-pricing",["30000000000000","3","400000"]),refusal(/already uses that pricing/));
    await d.send("set-fee-recipient",[BACKUP.address]);
    assert.equal(await c.feeRecipient(),BACKUP.address);assert.equal(await c.initialFeeRecipient(),OWNER.address);
    await assert.rejects(d.planned("set-fee-recipient",[BACKUP.address]),refusal(/already the fee recipient/));
    await d.send("set-keeper-bps",["7000"]);assert.equal(await c.keeperFeeBps(),7000n);
    await assert.rejects(d.planned("set-keeper-bps",["7000"]),refusal(/already 7000 bps/));
    await d.send("set-refund-bps",["9000"]);assert.equal(await c.refundBps(),9000n);
    await assert.rejects(d.planned("set-refund-bps",["9000"]),refusal(/already 9000 bps/));

    // A beacon: registered from a file, refused twice, scheduled no sooner than allowed, cancelled.
    const head=BigInt((await local.provider.getBlock("latest"))!.timestamp),beaconFile=join(d.dir,"test-beacon.json");
    const test=testBeacon(d.plan.step.beaconVerifier.address,head);
    await writeFile(beaconFile,JSON.stringify({verifier:test.verifier,chainHash:test.chainHash,publicKey:test.publicKey,genesis:Number(test.genesis),period:Number(test.period),
      sampleRound:1,sampleSignature:signTestRound(1n)},null,2));
    const register=await d.planned("register-beacon",[beaconFile]);
    assert.equal(register.call?.function,"registerBeacon((address,bytes32,bytes,uint64,uint64,uint64,bytes))");assert.equal((register.details as any).beaconId,"1");
    await d.send("register-beacon",[beaconFile]);
    assert.equal(await c.beaconCount(),2n);
    await assert.rejects(d.planned("register-beacon",[beaconFile]),refusal(/already registered as beacon 1/));
    await assert.rejects(d.planned("register-beacon",[join(root,"config/beacons/drand-evmnet.json")]),refusal(/already registered as beacon 0/));
    const future=join(d.dir,"future-beacon.json");
    await writeFile(future,JSON.stringify({...JSON.parse(await readFile(beaconFile,"utf8")),chainHash:"0x"+"77".repeat(32),sampleRound:5000}));
    await assert.rejects(d.planned("register-beacon",[future]),refusal(/sample round 5000 is scheduled at \d+, after the latest block time/));
    const otherVerifier=join(d.dir,"other-verifier.json");
    await writeFile(otherVerifier,JSON.stringify({...JSON.parse(await readFile(beaconFile,"utf8")),verifier:d.plan.step.proofVerifier.address}));
    await assert.rejects(d.planned("register-beacon",[otherVerifier]),refusal(/is not the compiled D20BeaconVerifier's/));
    const now=BigInt((await local.provider.getBlock("latest"))!.timestamp);
    await assert.rejects(d.planned("schedule-beacon",["1",String(now+599n)]),refusal(/must be at least 600/));
    await assert.rejects(d.planned("schedule-beacon",["1",String(now+620n)],{},true),refusal(/must be at least 660 \(600 seconds of MIN_SCHEDULE_LEAD and 60 for the transaction to be mined\)/));
    await assert.rejects(d.planned("schedule-beacon",["0",String(now+3600n)]),refusal(/Beacon 0 is already in force/));
    await assert.rejects(d.planned("schedule-beacon",["2",String(now+3600n)]),refusal(/Beacon 2 is not registered; the coordinator holds beacons 0 to 1/));
    const schedule=await d.planned("schedule-beacon",["1",String(now+600n)]);
    assert.equal((schedule.details as any).executeBefore,String(now));
    await assert.rejects(d.planned("cancel-beacon-schedule"),refusal(/No beacon change is pending/));
    await d.send("schedule-beacon",["1",String(now+3600n)]);
    assert.deepEqual(Array.from(await c.beaconSchedule()).slice(2),[1n,now+3600n]);
    await assert.rejects(d.planned("schedule-beacon",["1",String(now+3600n)]),refusal(/already scheduled/));
    assert.deepEqual(((await d.planned("cancel-beacon-schedule")).details as any).cancels.beaconId,"1");
    await d.send("cancel-beacon-schedule",[]);
    assert.deepEqual(Array.from(await c.beaconSchedule()).slice(2),[0n,0n]);

    // Ownership: proposed, accepted only by the pending owner, and the sender must be the planned one.
    await assert.rejects(d.planned("accept-ownership"),refusal(/No ownership transfer is pending/));
    await assert.rejects(d.planned("transfer-ownership",[OWNER.address]),refusal(/already the owner/));
    await d.send("transfer-ownership",[BACKUP.address]);
    const accept=await d.planned("accept-ownership");
    assert.deepEqual(accept.sender,{role:"pending owner",address:BACKUP.address,kind:"EOA"});assert.equal(accept.call?.from,BACKUP.address);
    await assert.rejects(d.send("accept-ownership",[],OWNER),refusal(/The signing wallet 0x\w+ is not the pending owner/));
    await d.send("accept-ownership",[],BACKUP);
    assert.equal(await c.owner(),BACKUP.address);
    await assert.rejects(d.send("set-keeper-bps",["6000"],OWNER),refusal(/is not the owner/));

    // Upgrade: only to the compiled implementation whose code hash is approved, with the reviewed storage layout.
    const next=await anotherImplementation(local,d.plan),nextHash=keccak256(await local.provider.getCode(next));
    await assert.rejects(d.planned("upgrade",[next],{"approve-code-hash":"0x"+"ab".repeat(32)}),refusal(/runtime code hash 0x\w+ is not the approved 0xabab/));
    const probe=await anotherImplementation(local,d.plan,true);
    await assert.rejects(d.planned("upgrade",[probe],{"approve-code-hash":keccak256(await local.provider.getCode(probe))}),refusal(/is not the compiled D20VRFCoordinatorRobinhood/));
    await assert.rejects(d.planned("upgrade",[d.plan.step.coordinatorImplementation.address],{"approve-code-hash":nextHash}),refusal(/already runs that implementation/));
    await assert.rejects(d.planned("upgrade",[STRANGER.address],{"approve-code-hash":nextHash}),refusal(/has no code/));
    const upgrade=await d.planned("upgrade",[next],{"approve-code-hash":nextHash});
    assert.equal(upgrade.call?.function,"upgradeToAndCall(address,bytes)");assert.deepEqual(upgrade.call?.args,[next,"0x"]);
    assert.equal((upgrade.details as any).runtimeCodeHash,nextHash);assert.equal((upgrade.details as any).layoutChanged,false);
    assert.equal((upgrade.details as any).proofVerifier,d.plan.step.proofVerifier.address);assert.equal((upgrade.details as any).mappingLibrary,d.plan.step.mappingLibrary.address);
    // A manifest whose recorded layout differs needs the reviewed layout's hash approved.
    const recorded=d.manifest;
    d.manifest={...recorded,storageLayoutHash:"0x"+"cd".repeat(32)};
    await assert.rejects(d.planned("upgrade",[next],{"approve-code-hash":nextHash}),refusal(/differs from the one the manifest records .* pass --approve-layout-hash 0x\w{64}$/));
    assert.equal(((await d.planned("upgrade",[next],{"approve-code-hash":nextHash,"approve-layout-hash":recorded.storageLayoutHash})).details as any).layoutChanged,true);
    d.manifest=recorded;
    await d.send("upgrade",[next],BACKUP,{"approve-code-hash":nextHash});
    assert.equal("0x"+(await local.provider.getStorage(proxy,"0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc")).slice(-40),next.toLowerCase());
    // The manifest now records the previous implementation: actions stop until it is updated, and status says why.
    await assert.rejects(d.planned("set-keeper-bps",["6000"]),refusal(/update the manifest after a reviewed upgrade/));
    assert.deepEqual(((await d.planned("status")).status as any).warnings,["implementation differs from the manifest","implementationCodeHash differs from the manifest"]);
    d.manifest={...recorded,coordinatorImplementation:next,coordinatorImplementationCodeHash:nextHash};
    assert.equal((await d.planned("set-keeper-bps",["6000"])).call?.function,"setKeeperFeeBps(uint16)");
    await assert.rejects(sendAdminAction({provider:local.provider,chain:local.chain,plan:await d.planned("status"),signer:BACKUP,journalDirectory:d.dir}),refusal(/status sends nothing/));
  }finally{await local.close();}
});

test("the Safe check reads a real Safe 1.5.0, and refuses the one-word stand-in, a foreign singleton, other owners, a threshold below 2 and a module",async()=>{
  const local=await localChain("robinhood-mainnet",OWNER.address);
  try{
    const safe=await localSafe(local),signers=SAFE_SIGNERS.map(signer=>signer.address),singleton=SAFE_SINGLETONS.find(entry=>entry.version==="1.5.0"&&entry.kind==="SafeL2")!;
    const reading=await readSafe(local.provider,safe);
    assert.deepEqual({...reading,owners:sorted(reading.owners!)},{address:safe,problems:[],proxyVersion:"1.5.0",singleton:singleton.address,singletonKind:"SafeL2",
      version:"1.5.0",owners:sorted(signers),threshold:2,modules:[]});
    assert.equal(await accountKind(local.provider,safe),"Safe");assert.equal(await accountKind(local.provider,STRANGER.address),"EOA");
    const chain={...local.chain,...safeProfile(safe),owner:safe},profileWith=(address:string)=>({...chain,safe:{...chain.safe!,address}});
    assert.deepEqual((await checkProfileSafe(local.provider,chain,safe)).problems,[]);
    // The one-word stand-in answers getThreshold() with 1, which was all the old check asked: it is no Safe.
    const standIn=getAddress("0x00000000000000000000000000000000005afe01");
    await local.request("hardhat_setCode",[standIn,SAFE_STAND_IN_CODE]);
    assert.equal(await accountKind(local.provider,standIn),"contract");
    assert.deepEqual((await checkProfileSafe(local.provider,profileWith(standIn),standIn)).problems,[
      `its runtime code hash ${keccak256(SAFE_STAND_IN_CODE)} is not a canonical SafeProxy's (1.3.0, 1.4.1 or 1.5.0)`,
      "its singleton (storage slot 0, 0x0000000000000000000000000000000000000000) is not a canonical Safe or SafeL2 singleton",
      "it does not answer VERSION()","it does not answer getOwners()","its threshold 1 is not 1 to its 0 owners","it does not answer getModulesPaginated()",
      "its threshold is 1: a production owner needs at least 2 signatures","its threshold is 1, not the profile's 2"]);
    // A canonical SafeProxy whose singleton is the SafeL2 code at another address is no Safe either: the singleton's address is pinned.
    const fixture=JSON.parse(await readFile(join(root,"scripts/tests/fixtures/safe-1.5.0.json"),"utf8")),foreign=getAddress("0x00000000000000000000000000000000005afe99");
    await local.request("hardhat_setCode",[foreign,fixture.singleton.code]);
    const factory=new Contract(fixture.factory.address,["function createProxyWithNonce(address,bytes,uint256) returns(address)"],STRANGER.connect(local.provider));
    const proxy=getAddress(await factory.createProxyWithNonce.staticCall(foreign,"0x",9n));
    await (await factory.createProxyWithNonce(foreign,"0x",9n)).wait();
    const foreignReading=await readSafe(local.provider,proxy);
    assert.equal(foreignReading.proxyVersion,"1.5.0");
    assert.ok(foreignReading.problems.includes(`its singleton (storage slot 0, ${foreign}) is not a canonical Safe or SafeL2 singleton`),foreignReading.problems.join("; "));
    // A threshold of 1, other owners, a module, an address that is not the profile's Safe, a profile without owners and one without a Safe.
    const single=await localSafe(local,{threshold:1,saltNonce:1n});
    assert.equal(await accountKind(local.provider,single),"Safe");
    assert.deepEqual((await checkProfileSafe(local.provider,profileWith(single),single)).problems,["its threshold is 1: a production owner needs at least 2 signatures","its threshold is 1, not the profile's 2"]);
    const others=await localSafe(local,{owners:[OWNER.address,BACKUP.address],saltNonce:2n});
    const otherReading=await checkProfileSafe(local.provider,profileWith(others),others);
    assert.deepEqual(otherReading.problems,[`its owners are ${otherReading.owners!.join(", ")}, not the profile's ${signers.join(", ")}`]);
    const moduled=await localSafe(local,{saltNonce:3n});
    await execSafe(local,moduled,{to:moduled,data:new Interface(SAFE_ABI).encodeFunctionData("enableModule",[STRANGER.address])});
    assert.deepEqual((await checkProfileSafe(local.provider,profileWith(moduled),moduled)).problems,[`it has modules enabled (${STRANGER.address}), which act without the owners' signatures`]);
    assert.deepEqual((await checkProfileSafe(local.provider,chain,moduled)).problems,[`it is not the robinhood-mainnet profile's Safe ${safe}`,`it has modules enabled (${STRANGER.address}), which act without the owners' signatures`]);
    assert.deepEqual((await checkProfileSafe(local.provider,{...chain,safe:{...chain.safe!,owners:null}},safe)).problems,["the robinhood-mainnet profile does not record the Safe's owners yet (safe.owners in chains.json is null)"]);
    assert.deepEqual((await checkProfileSafe(local.provider,{...local.chain,safe:undefined},safe)).problems,["the robinhood-mainnet profile names no Safe (safe in chains.json)"]);
    // The profile as chains.json holds it names no Safe: its owner is the account that owns the set on chain. A Safe comes from an operator's
    // private owner overlay (chain-profiles.test.ts).
    assert.deepEqual((await checkProfileSafe(local.provider,local.chain,safe)).problems,["the robinhood-mainnet profile names no Safe (safe in chains.json)"]);
    await assert.rejects(requireProfileSafe(local.provider,chain,single,"The proposed owner"),
      refusal(new RegExp(`^The proposed owner ${single} is not the Safe a production owner must be: it is not the robinhood-mainnet profile's Safe ${safe}; its threshold is 1: a production owner needs at least 2 signatures; its threshold is 1, not the profile's 2$`)));
    // The singleton must have its canonical code on this chain.
    await local.request("hardhat_setCode",[singleton.address,"0x6000"]);
    assert.match((await readSafe(local.provider,safe)).problems.join("; "),new RegExp(`its singleton ${singleton.address} has runtime code hash 0x\\w+ on this chain, not the SafeL2 1\\.5\\.0 code ${singleton.codeHash}`));
  }finally{await local.close();}
});

test("a production chain's owner must be its profile's Safe: its actions print the Safe transaction that its signers execute, and sending, another owner and a transfer to anything else are refused",async()=>{
  const local=await localChain("robinhood-mainnet",OWNER.address);
  try{
    const safe=await localSafe(local);
    local.chain={...local.chain,...safeProfile(safe),owner:safe};
    const d=await deployed(local,"safe"),proxy=d.plan.step.coordinator.address;
    const plan=await d.planned("set-keeper-bps",["7000"]);
    assert.equal(plan.execution,"Safe");assert.deepEqual(plan.sender,{role:"owner",address:safe,kind:"Safe"});
    const batch=plan.safeTransaction!;
    assert.equal(batch.version,"1.0");assert.equal(batch.chainId,"4663");assert.equal(batch.meta.createdFromSafeAddress,safe);
    assert.equal(batch.meta.name,"D20 round coordinator: set-keeper-bps");assert.match(batch.meta.description,/^setKeeperFeeBps\(uint16\) with \(7000\) on the round coordinator 0x\w+ on Robinhood Chain \(chain 4663\), checked and simulated from the Safe/);
    assert.deepEqual(batch.transactions,[{to:proxy,value:"0",data:d.face.encodeFunctionData("setKeeperFeeBps",[7000]),contractMethod:null,contractInputsValues:null}]);
    // The file's transaction, signed by the Safe's two signers, is what changes the coordinator.
    await execSafe(local,safe,batch.transactions[0]);
    assert.equal(await d.coordinator.keeperFeeBps(),7000n);
    // Every action that sends prints a Safe transaction there; status sends nothing.
    for(const [action,args] of [["set-keeper",[STRANGER.address]],["set-backup-keeper",[BACKUP.address,"true"]],["set-pricing",["30000000000000","2","360000"]],
      ["set-refund-bps",["9000"]],["set-keeper-bps",["6000"]]] as Array<[string,string[]]>){
      const p=await d.planned(action,args);
      assert.equal(p.safeTransaction?.transactions[0].data,p.call?.data,action);
    }
    await assert.rejects(d.planned("set-keeper-bps",["6000"],{},true),refusal(/production chain, owned by a Safe: run without --send/));
    await assert.rejects(sendAdminAction({provider:local.provider,chain:local.chain,plan,signer:OWNER,journalDirectory:d.dir}),refusal(/production chain: only its interim owner sends/));
    // A transfer goes only to the profile's Safe, strictly checked, and the fees only to it.
    await assert.rejects(d.planned("transfer-ownership",[STRANGER.address]),
      refusal(new RegExp(`^The proposed owner ${STRANGER.address} is not the Safe a production owner must be: it has no code; it is not the robinhood-mainnet profile's Safe ${safe}$`)));
    const standIn=getAddress("0x00000000000000000000000000000000005afe02");
    await local.request("hardhat_setCode",[standIn,SAFE_STAND_IN_CODE]);
    await assert.rejects(d.planned("transfer-ownership",[standIn]),refusal(/is not the Safe a production owner must be: its runtime code hash 0x\w+ is not a canonical SafeProxy's/));
    const other=await localSafe(local,{owners:[OWNER.address,BACKUP.address],saltNonce:5n});
    await assert.rejects(d.planned("transfer-ownership",[other]),refusal(/is not the Safe a production owner must be: it is not the robinhood-mainnet profile's Safe 0x\w+; its owners are/));
    await assert.rejects(d.planned("transfer-ownership",[safe]),refusal(/is already the owner/));
    await assert.rejects(d.planned("set-fee-recipient",[STRANGER.address]),refusal(new RegExp(`^The proposed fee recipient ${STRANGER.address} is not the Safe a production owner must be`)));
    // An owner that is not the profile's Safe is refused for every action on a production chain, and status warns of it.
    await local.request("hardhat_setCode",[safe,"0x"]);
    await assert.rejects(d.planned("set-keeper-bps",["5000"]),refusal(new RegExp(`^The owner of the coordinator on robinhood-mainnet ${safe} is not the Safe a production owner must be: it has no code$`)));
    assert.deepEqual(((await d.planned("status")).status as any).warnings,[`the owner ${safe} is not the Safe a production owner must be: it has no code`]);
  }finally{await local.close();}
  // On a test network a Safe owner is proposed the Safe way too, and never sent; a contract that is not a Safe is refused.
  const testnet=await localChain("robinhood-testnet",OWNER.address);
  try{
    const safe=await localSafe(testnet);
    testnet.chain={...testnet.chain,owner:safe};
    const d=await deployed(testnet,"testnet-safe");
    assert.equal((await d.planned("set-keeper-bps",["7000"])).execution,"Safe");
    await assert.rejects(d.planned("set-keeper-bps",["7000"],{},true),refusal(/is a contract: run without --send and propose the printed Safe transaction/));
    await testnet.request("hardhat_setCode",[safe,SAFE_STAND_IN_CODE]);
    await assert.rejects(d.planned("set-keeper-bps",["7000"]),refusal(/^The owner 0x\w+ is a contract that is not a Safe \(scripts\/lib\/safe\.ts\): neither its key nor a Safe transaction can act for it$/));
  }finally{await testnet.close();}
});

test("a production set moves from its interim owner to its Safe in three steps that status follows: transfer-ownership from the key, accept-ownership and set-fee-recipient from the Safe",async()=>{
  const local=await localChain("robinhood-mainnet",OWNER.address),dir=await tmp("transfer");
  try{
    const safe=await localSafe(local);
    local.chain={...local.chain,...safeProfile(safe,{interimOwner:DEPLOYER.address}),owner:safe};
    const {plan,manifestPath}=await deployLocal(local,dir,{owner:DEPLOYER.address,feeRecipient:DEPLOYER.address,backupKeeper:BACKUP.address},{finalOwner:safe});
    const manifest=parseRoundManifest(JSON.parse(await readFile(manifestPath,"utf8")),manifestPath);
    assert.equal("ownerTransferPending" in manifest,false);assert.equal("finalOwner" in manifest,false);
    const face=await coordinatorInterface(),proxy=plan.step.coordinator.address,c=new Contract(proxy,face,local.provider);
    const planned=async(action:string,args:string[]=[],send=false,o:{chain?:LocalChain["chain"];manifest?:typeof manifest}={})=>
      planAdminAction({provider:local.provider,chain:o.chain??local.chain,manifest:o.manifest??manifest,request:await parseAdminRequest(action,args),send});
    const sendAs=async(signer:typeof DEPLOYER,action:string,args:string[]=[])=>sendAdminAction({provider:local.provider,chain:local.chain,plan:await planned(action,args,true),signer,journalDirectory:dir});
    const status=async(o:{manifest?:typeof manifest}={})=>(await planned("status",[],false,o)).status as any;
    let s=await status();
    assert.deepEqual(s.owner,{address:DEPLOYER.address,kind:"EOA",interimOwner:true});assert.equal(s.feeRecipient,DEPLOYER.address);
    assert.deepEqual(s.ownership.steps.map((step:any)=>[step.step,step.action,step.done]),[[1,`transfer-ownership ${safe}`,false],[2,"accept-ownership",false],[3,`set-fee-recipient ${safe}`,false]]);
    assert.equal(s.ownership.pending,true);assert.equal(s.ownership.next,`transfer-ownership ${safe}`);assert.equal("manifestOwnerTransferPending" in s.ownership,false);
    assert.equal(s.ownership.safeCheck.passes,true);assert.equal(s.ownership.safeCheck.version,"1.5.0");assert.equal(s.ownership.safeCheck.threshold,2);
    assert.deepEqual(s.warnings,[`the owner ${DEPLOYER.address} is the interim owner: the transfer to the Safe is pending (ownership.next)`]);
    // Until the transfer, the interim owner's key sends every owner action, and only that key.
    const bps=await planned("set-keeper-bps",["7000"],true);
    assert.equal(bps.execution,"owner key");assert.equal(bps.interimOwner,true);assert.equal(bps.safeTransaction,undefined);
    await sendAs(DEPLOYER,"set-keeper-bps",["7000"]);
    assert.equal(await c.keeperFeeBps(),7000n);
    await assert.rejects(sendAs(STRANGER,"set-keeper-bps",["6000"]),refusal(/The signing wallet 0x\w+ is not the owner/));
    assert.equal(await c.isBackupKeeper(BACKUP.address),true);

    // Step 1: transfer-ownership, sent by the interim owner's key, only to the profile's Safe once it passes the strict check.
    await assert.rejects(planned("transfer-ownership",[STRANGER.address],true),refusal(/^The proposed owner 0x\w+ is not the Safe a production owner must be: it has no code; it is not the robinhood-mainnet profile's Safe/));
    await assert.rejects(planned("transfer-ownership",[safe],true,{chain:{...local.chain,safe:{...local.chain.safe!,owners:null}}}),refusal(/does not record the Safe's owners yet/));
    await assert.rejects(planned("transfer-ownership",[safe],true,{chain:{...local.chain,safe:{...local.chain.safe!,threshold:3}}}),refusal(/its threshold is 2, not the profile's 3/));
    const transfer=await planned("transfer-ownership",[safe],true);
    assert.equal(transfer.execution,"owner key");assert.match(transfer.checks!.join("; "),/the proposed owner is the profile's Safe: a canonical SafeProxy and singleton/);
    assert.match(transfer.note!,/set-fee-recipient to the Safe follows, as a Safe transaction/);
    await sendAs(DEPLOYER,"transfer-ownership",[safe]);
    assert.equal(await c.pendingOwner(),safe);assert.equal(await c.owner(),DEPLOYER.address);
    s=await status();
    assert.deepEqual(s.ownership.steps.map((step:any)=>step.done),[true,false,false]);assert.equal(s.ownership.next,"accept-ownership");

    // Step 2: accept-ownership, a Safe transaction that the Safe's signers execute; never sent with a key.
    const accept=await planned("accept-ownership");
    assert.deepEqual(accept.sender,{role:"pending owner",address:safe,kind:"Safe"});assert.equal(accept.execution,"Safe");
    assert.deepEqual(accept.safeTransaction!.transactions,[{to:proxy,value:"0",data:face.encodeFunctionData("acceptOwnership"),contractMethod:null,contractInputsValues:null}]);
    await assert.rejects(planned("accept-ownership",[],true),refusal(/is a contract: run without --send and propose the printed Safe transaction/));
    await execSafe(local,safe,accept.safeTransaction!.transactions[0]);
    assert.equal(await c.owner(),safe);
    s=await status();
    assert.deepEqual(s.owner,{address:safe,kind:"Safe",interimOwner:false});
    assert.deepEqual(s.ownership.steps.map((step:any)=>step.done),[true,true,false]);assert.equal(s.ownership.next,`set-fee-recipient ${safe}`);
    // The interim owner's key no longer acts for the set.
    await assert.rejects(planned("set-keeper-bps",["6000"],true),refusal(/is a contract: run without --send/));

    // Step 3: set-fee-recipient to the Safe, a Safe transaction; any other recipient is refused.
    await assert.rejects(planned("set-fee-recipient",[STRANGER.address]),refusal(/^The proposed fee recipient 0x\w+ is not the Safe a production owner must be/));
    const fee=await planned("set-fee-recipient",[safe]);
    assert.equal(fee.execution,"Safe");assert.match(fee.checks!.join("; "),/the proposed fee recipient is the profile's Safe, strictly checked/);
    await execSafe(local,safe,fee.safeTransaction!.transactions[0]);
    assert.equal(await c.feeRecipient(),safe);assert.equal(await c.initialFeeRecipient(),DEPLOYER.address);
    s=await status();
    // The transfer is read from the chain alone: once it is done there is nothing left to record.
    assert.equal(s.ownership.pending,false);assert.equal(s.ownership.next,null);
    assert.deepEqual(s.warnings,[]);assert.equal("manifestUpdate" in s.ownership,false);
  }finally{await local.close();}
});

test("the mainnet runbook on a local chain through both command lines: the dry run, the deployment with the interim owner, and the move to the Safe",async()=>{
  const local=await localChain("robinhood-mainnet",OWNER.address),endpoint=await rpcEndpoint(local),dir=await tmp("runbook");
  try{
    const safe=await localSafe(local);
    // The production profile with the local Safe and the deployer as its interim owner, as the owner fills chains.json.
    const env={D20_STAND_IN_OWNER:safe,D20_STAND_IN_PROFILE:JSON.stringify(safeProfile(safe,{interimOwner:DEPLOYER.address}))};
    const operator=await operatorDirectory(join(dir,"operator"),DEPLOYER.address),deployerEnv=await envFile(join(dir,"deployer.env"),DEPLOYER);
    const manifestPath=join(dir,"robinhood-mainnet.json");
    const deploy=(extra:string[]=[])=>run("scripts/deploy-robinhood.ts",["--chain","robinhood-mainnet","--mainnet","--interim-owner","--env",deployerEnv,"--operator-directory",operator,
      "--keeper",STRANGER.address,"--backup-keeper",BACKUP.address,"--manifest",manifestPath,"--private-directory",join(dir,"private"),"--rpc-url",endpoint.url,...extra],
      [STAND_IN_PRELOAD,UNREVIEWED_PRELOAD],env);
    const parse=(result:{status:number|null;stdout:string;stderr:string})=>{assert.equal(result.status,0,result.stderr);return JSON.parse(result.stdout);};
    // 1. The dry run: the interim owner, the keeper wallet named on the command line, the backup keeper's call and the Safe's check.
    const dryRun=await deploy(),dry=parse(dryRun);
    assert.match(dryRun.stderr,new RegExp(`^WARNING: robinhood-mainnet's set starts with the interim owner ${DEPLOYER.address}, an account without code, as its owner and fee recipient, not with its Safe ${safe}\\.`));
    assert.match(dryRun.stderr,/admin-robinhood\.ts status reads the transfer from the chain/);
    assert.equal(dry.mode,"dry run");assert.deepEqual(dry.owner,{address:DEPLOYER.address,kind:"EOA"});assert.equal(dry.feeRecipient,DEPLOYER.address);
    assert.equal(dry.keeper,STRANGER.address);assert.equal(dry.keeperSource,"--keeper");assert.equal(dry.backupKeeper,BACKUP.address);
    assert.deepEqual(dry.ownership,{ownerTransferPending:true,finalOwner:{address:safe,kind:"Safe",safeProblems:[]}});
    assert.deepEqual(dry.calls.map((call:any)=>[call.step,call.status]),[["backupKeeper","planned"]]);
    // 2. The deployment with --interim-owner.
    const sent=parse(await deploy(["--send"]));
    assert.deepEqual([...sent.transactions.map((t:any)=>t.status),sent.calls[0].status],[...Array(5).fill("deployed"),"sent"]);
    const manifest=JSON.parse(await readFile(manifestPath,"utf8"));
    assert.equal("ownerTransferPending" in manifest,false);assert.equal("finalOwner" in manifest,false);assert.equal(manifest.owner,DEPLOYER.address);
    assert.equal(manifest.keeper,STRANGER.address);assert.deepEqual(manifest.backupKeepers,[BACKUP.address]);
    // 3. The check: a run that finds everything in place.
    const verified=parse(await deploy());
    assert.equal(verified.verification.owner,DEPLOYER.address);assert.equal(verified.verification.backupKeeper,"true");assert.match(verified.next,/the manifest records it/);
    const admin=(args:string[])=>run("scripts/admin-robinhood.ts",[...args,"--manifest",manifestPath,"--rpc-url",endpoint.url,"--private-directory",join(dir,"journal")],[STAND_IN_PRELOAD],env);
    const status=async()=>parse(await admin(["status"])).status;
    assert.equal((await status()).ownership.next,`transfer-ownership ${safe}`);
    // 4. transfer-ownership from the interim owner's key: a send on the production chain also needs --mainnet.
    const unacknowledged=await admin(["transfer-ownership",safe,"--send","--env",deployerEnv]);
    assert.equal(unacknowledged.status,1);assert.match(unacknowledged.stderr,/a send there, from its interim owner only, also needs --mainnet/);
    const transferred=await admin(["transfer-ownership",safe,"--send","--mainnet","--env",deployerEnv]);
    assert.equal(transferred.status,0,transferred.stderr);assert.match(transferred.stdout,/"sent": true/);
    const after=await admin(["status"]);
    assert.equal(JSON.parse(after.stdout).status.ownership.next,"accept-ownership");
    // 5. accept-ownership and 6. set-fee-recipient: Safe Transaction Builder files, which the Safe's signers execute.
    for(const [args,file] of [[["accept-ownership"],"accept.json"],[["set-fee-recipient",safe],"fee-recipient.json"]] as Array<[string[],string]>){
      const out=join(dir,file),written=await admin([...args,"--safe-out",out]);
      assert.equal(written.status,0,written.stderr);
      const batch=JSON.parse(await readFile(out,"utf8"));
      assert.equal(batch.chainId,"4663");assert.equal(batch.meta.createdFromSafeAddress,safe);assert.equal(batch.transactions.length,1);
      await execSafe(local,safe,batch.transactions[0]);
    }
    const done=await status();
    assert.equal(done.owner.address,safe);assert.equal(done.feeRecipient,safe);assert.equal(done.ownership.pending,false);
    assert.equal("manifestUpdate" in done.ownership,false);
  }finally{await endpoint.close();await local.close();}
});

test("the command line prints the plan, sends only with --send from the owner key, writes the Safe file, and refuses before any network access",async()=>{
  const dir=await tmp("cli"),missing=join(dir,"does-not-exist.env");
  // Refusals that need no network: undecided owner, sending on a production chain, --send without a key, and malformed arguments.
  const manifestFile=async(network:string,chainId:number)=>{
    const file=join(dir,`${network}-stub.json`);
    await writeFile(file,JSON.stringify({network,chainId,coordinatorContract:"D20VRFCoordinatorRobinhood",coordinator:"0x"+"11".repeat(20),coordinatorImplementation:"0x"+"22".repeat(20),
      coordinatorCodeHash:"0x"+"33".repeat(32),coordinatorImplementationCodeHash:"0x"+"44".repeat(32),storageLayoutHash:"0x"+"55".repeat(32)}));
    return file;
  };
  const testnetStub=await manifestFile("robinhood-testnet",46630),mainnetStub=await manifestFile("robinhood-mainnet",4663);
  // Both profiles are read as chains.json holds them, except where a preload reads the production owner as undecided, or stands its Safe in
  // without the interim owner (a set the Safe has taken over).
  const safeOnly={D20_STAND_IN_OWNER:"0x00000000000000000000000000000000005afE77",D20_STAND_IN_PROFILE:JSON.stringify({interimOwner:null})};
  const offline:Array<[string[],string[],RegExp,Record<string,string>?]>=[
    [[UNDECIDED_PRELOAD],["status","--manifest",mainnetStub],/The owner of robinhood-mainnet is not set, so/],
    [[STAND_IN_PRELOAD],["set-keeper-bps","7000","--manifest",mainnetStub,"--send","--env",missing],/robinhood-mainnet is a production chain, owned by a Safe: run without --send/,safeOnly],
    [[],["set-keeper-bps","7000","--manifest",testnetStub,"--send"],/--send needs --env/],
    [[],["set-keeper-bps","7001x","--manifest",testnetStub],/keeper share must be a whole number/],
    [[],["status","--manifest",testnetStub,"--send","--env",missing],/status sends nothing and proposes nothing/],
    [[],["set-keeper-bps","7000","--manifest",testnetStub,"--rpc-url","https://rpc.example"],/--rpc-url takes a local node's URL only/],
    [[],["set-keeper-bps","7000"],/Provide --manifest/],
    [[],["set-keeper-bps","7000","--manifest",join(dir,"none.json")],/could not be read as JSON/],
    [[],["set-keeper-bps","7000","--manifest",testnetStub,"--mainnet"],/--mainnet acknowledges a send on a production chain, and robinhood-testnet is a test network/],
  ];
  for(const [preloads,args,pattern,environment] of offline){
    const result=await run("scripts/admin-robinhood.ts",args,[NO_NETWORK,...preloads],environment);
    assert.equal(result.status,1,`${args.join(" ")}: ${result.stderr}`);
    assert.match(result.stderr,/^Robinhood administration stopped: /,args.join(" "));assert.match(result.stderr,pattern,args.join(" "));
    assert.doesNotMatch(result.stderr,/NETWORK ACCESS|does-not-exist/,args.join(" "));
  }
  // A production profile that names its interim owner (an owner overlay; a fixture of stand-in addresses): a send there also needs --mainnet.
  const unacknowledged=await run("scripts/admin-robinhood.ts",["set-keeper-bps","7000","--manifest",mainnetStub,"--send","--env",missing],[NO_NETWORK],
    {D20_PRIVATE_PROFILE_DIR:fileURLToPath(new URL("./fixtures/private-profile/",import.meta.url))});
  assert.equal(unacknowledged.status,1);
  assert.match(unacknowledged.stderr,/^Robinhood administration stopped: robinhood-mainnet is a production chain: a send there, from its interim owner only, also needs --mainnet/);
  assert.doesNotMatch(unacknowledged.stderr,/NETWORK ACCESS|does-not-exist/);

  // Against a local testnet whose coordinator a local test account owns: the command takes the owner from the coordinator, and the
  // testnet profile's own owner only passes its guard.
  const local=await localChain("robinhood-testnet",OWNER.address),endpoint=await rpcEndpoint(local);
  try{
    const {manifestPath}=await deployLocal(local,join(dir,"testnet"));
    const admin=(args:string[])=>run("scripts/admin-robinhood.ts",[...args,"--manifest",manifestPath,"--rpc-url",endpoint.url,"--private-directory",join(dir,"journal")]);
    const status=await admin(["status"]);
    assert.equal(status.status,0,status.stderr);
    assert.equal(JSON.parse(status.stdout).status.keeper,KEEPER.address);
    const dry=await admin(["set-keeper-bps","7000"]);
    assert.equal(dry.status,0,dry.stderr);
    const printed=JSON.parse(dry.stdout) as AdminPlan&{send:boolean};
    assert.equal(printed.send,false);assert.equal(printed.call?.function,"setKeeperFeeBps(uint16)");assert.equal(printed.call?.from,OWNER.address);
    const coordinator=new Contract(printed.coordinator,["function keeperFeeBps() view returns(uint16)"],local.provider);
    assert.equal(await coordinator.keeperFeeBps(),8000n,"a dry run sends nothing");
    const noSafe=await admin(["set-keeper-bps","7000","--safe-out",join(dir,"none-safe.json")]);
    assert.equal(noSafe.status,1);assert.match(noSafe.stderr,/is an account without code: there is no Safe transaction to write/);
    const wrongKey=await admin(["set-keeper-bps","7000","--send","--env",await envFile(join(dir,"stranger.env"),STRANGER)]);
    assert.equal(wrongKey.status,1);assert.match(wrongKey.stderr,/The signing wallet 0x\w+ is not the owner/);
    const sent=await admin(["set-keeper-bps","7000","--send","--env",await envFile(join(dir,"owner.env"),OWNER)]);
    assert.equal(sent.status,0,sent.stderr);
    assert.match(sent.stdout,/"sent": true/);assert.equal(await coordinator.keeperFeeBps(),7000n);
    assert.doesNotMatch(sent.stdout+sent.stderr,new RegExp(OWNER.privateKey.slice(2),"i"));
  }finally{await endpoint.close();await local.close();}

  // A production chain owned by its Safe: the Safe file is written, once.
  const mainnet=await localChain("robinhood-mainnet",OWNER.address),mainnetEndpoint=await rpcEndpoint(mainnet);
  try{
    const safe=await localSafe(mainnet);
    mainnet.chain={...mainnet.chain,...safeProfile(safe),owner:safe};
    const {manifestPath}=await deployLocal(mainnet,join(dir,"mainnet")),out=join(dir,"safe.json");
    const args=["set-refund-bps","9000","--manifest",manifestPath,"--rpc-url",mainnetEndpoint.url,"--safe-out",out];
    const env={D20_STAND_IN_OWNER:safe,D20_STAND_IN_PROFILE:JSON.stringify(safeProfile(safe))};
    const written=await run("scripts/admin-robinhood.ts",args,[STAND_IN_PRELOAD],env);
    assert.equal(written.status,0,written.stderr);
    const batch=JSON.parse(await readFile(out,"utf8")),face=new Interface(["function setRefundBps(uint16)"]);
    assert.equal(batch.chainId,"4663");assert.equal(batch.meta.createdFromSafeAddress,safe);
    assert.deepEqual(batch.transactions.map((t:any)=>[t.value,t.data]),[["0",face.encodeFunctionData("setRefundBps",[9000])]]);
    assert.deepEqual(JSON.parse(written.stdout).safeTransaction.transactions,batch.transactions);
    const again=await run("scripts/admin-robinhood.ts",args,[STAND_IN_PRELOAD],env);
    assert.equal(again.status,1);assert.match(again.stderr,/EEXIST|exists/);
  }finally{await mainnetEndpoint.close();await mainnet.close();}
});
