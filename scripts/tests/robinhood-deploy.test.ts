import {test} from "node:test";
import assert from "node:assert/strict";
import {mkdir,mkdtemp,readFile,readdir,writeFile} from "node:fs/promises";
import {execFileSync} from "node:child_process";
import {tmpdir} from "node:os";
import {join} from "node:path";
import {pathToFileURL,fileURLToPath} from "node:url";
import {concat,getAddress,getCreate2Address,keccak256} from "ethers";
import {loadChain} from "../lib/chains.ts";
import {Stop} from "../lib/deployment.ts";
import {REVIEWED_STEPS,UNREVIEWED_SOURCE_FOR_TESTS,coordinatorInterface,deployRoundSet,loadCreate2Config,parseBeaconFile,planRoundSet,reviewedSource,
  type DeployReport} from "../lib/robinhood-deploy.ts";
import {legacyFindings} from "../lib/robinhood-profiles.ts";
import {BACKUP,DEPLOYER,KEEPER,OWNER,SAFE_STAND_IN_CODE,STRANGER,TEST_SOURCE,deployLocal,envFile,localChain,localPlan,localSafe,operatorDirectory,profile,root,
  rpcEndpoint,run,safeProfile,type LocalChain} from "./round-local.ts";

const BEACON_VERIFIER="0xd20dA01Aa16AeD6b77Cd8DDb869151802599100a";
const NO_NETWORK=pathToFileURL(fileURLToPath(new URL("./fixtures/no-network.mjs",import.meta.url))).href;
const STAND_IN_PRELOAD=pathToFileURL(fileURLToPath(new URL("./fixtures/stand-in-owner.mjs",import.meta.url))).href;
const UNDECIDED_PRELOAD=pathToFileURL(fileURLToPath(new URL("./fixtures/undecided-owner.mjs",import.meta.url))).href;
const UNREVIEWED_PRELOAD=pathToFileURL(fileURLToPath(new URL("./fixtures/unreviewed-source.mjs",import.meta.url))).href;
/** The local production chain with a real local Safe as the profile's owner, and the profile's Safe fields (safeProfile) for it. */
async function withSafe(local:LocalChain,o:Parameters<typeof safeProfile>[1]={}){
  const safe=await localSafe(local);
  local.chain={...local.chain,...safeProfile(safe,o),owner:safe};
  return safe;
}
const refusal=(pattern:RegExp)=>(error:unknown)=>error instanceof Stop&&pattern.test(error.message);
const tmp=(name:string)=>mkdtemp(join(tmpdir(),`d20-round-${name}-`));

test("the beacon verifier's salt reproduces its address on every chain, and the helpers and implementation are the same on both networks",async()=>{
  const create2=await loadCreate2Config();
  assert.equal(getCreate2Address("0x4e59b44847b379578588920cA78FbF26c0B4956C",create2.beaconVerifier.salt,create2.beaconVerifier.initCodeHash),BEACON_VERIFIER);
  const testnet=await localPlan(await profile("robinhood-testnet",OWNER.address));
  assert.equal(testnet.step.beaconVerifier.address,BEACON_VERIFIER);
  assert.equal(testnet.step.beaconVerifier.initCodeHash,create2.beaconVerifier.initCodeHash);
  assert.equal(testnet.step.beaconVerifier.runtimeCodeHash,"0x4388250d26298224c4a39d030263550655390ca831ab44c4124f8b0be1f65351");
  // A Safe owns the mainnet set: its proxy differs, and nothing below the proxy does.
  const mainnet=await localPlan(await profile("robinhood-mainnet",STRANGER.address));
  for(const step of ["beaconVerifier","proofVerifier","mappingLibrary","coordinatorImplementation"] as const){
    assert.equal(mainnet.step[step].address,testnet.step[step].address,step);
    assert.equal(mainnet.step[step].runtimeCodeHash,testnet.step[step].runtimeCodeHash,step);
  }
  assert.notEqual(mainnet.step.coordinator.address,testnet.step.coordinator.address);
  // The reviewed code hashes pinned in the configuration are the ones the testnet deployment runs, and a compiled step that differs is refused.
  const deployed=JSON.parse(await readFile(join(root,"deployments/robinhood-testnet.json"),"utf8"));
  for(const step of REVIEWED_STEPS){
    assert.equal(create2[step].runtimeCodeHash,deployed[`${step}CodeHash`],step);
    assert.equal(testnet.step[step].runtimeCodeHash,create2[step].runtimeCodeHash,step);
    await assert.rejects(planRoundSet({chain:await profile("robinhood-testnet",OWNER.address),service:testnet.service,beacon:testnet.beacon,roles:testnet.roles,
      create2:{...create2,[step]:{...create2[step],runtimeCodeHash:"0x"+"ab".repeat(32)}}}),
      refusal(new RegExp(`^The compiled ${testnet.step[step].contract} would have runtime code hash ${testnet.step[step].runtimeCodeHash} at 0x\\w+, not the reviewed 0xabab\\w+ that config/robinhood-create2\\.json pins: only reviewed code is deployed$`)),step);
  }
  // Every address is the factory's CREATE2 address of the step's salt and init code.
  for(const step of testnet.steps)assert.equal(getCreate2Address(testnet.factory,step.salt,keccak256(step.initCode)),step.address,step.name);
  // A beacon file that names another verifier is refused, and so is a salt that no longer gives the pinned address.
  const beacon=parseBeaconFile((await readFile(join(root,testnet.service.beacon),"utf8")).replace(BEACON_VERIFIER,STRANGER.address),"beacon.json");
  const chain=await profile("robinhood-testnet",OWNER.address);
  await assert.rejects(planRoundSet({chain,service:testnet.service,beacon,create2,roles:testnet.roles}),
    refusal(/^The beacon file names the verifier 0x\w+, not the D20BeaconVerifier at 0xd20dA01Aa16AeD6b77Cd8DDb869151802599100a$/));
  await assert.rejects(planRoundSet({chain,service:testnet.service,beacon:testnet.beacon,roles:testnet.roles,
    create2:{...create2,beaconVerifier:{...create2.beaconVerifier,salt:"0x"+"00".repeat(32)}}}),refusal(/give 0x\w+ with runtime code hash 0x\w+, not the pinned 0xd20dA01A/));
  // The beacon file is checked as registerBeacon checks it.
  const good=await readFile(join(root,testnet.service.beacon),"utf8");
  for(const [from,to,pattern] of [['"period": 3','"period": 11',/period must be 1 to 10/],['"genesis": 1727521075','"genesis": 0',/genesis must be/],
    ['"sampleRound": 21072365','"sampleRound": 0',/sampleRound must be/]] as const)
    assert.throws(()=>parseBeaconFile(good.replace(from,to),"beacon.json"),refusal(pattern));
  assert.throws(()=>parseBeaconFile(good.replace('"period": 3,','"period": 3, "extra": 1,'),"beacon.json"),refusal(/exactly the fields/));
});

test("a local deployment: a dry run that sends nothing, the whole set deployed and checked, its manifest, and an idempotent second run",async()=>{
  const local=await localChain("robinhood-testnet",OWNER.address),dir=await tmp("deploy");
  try{
    const plan=await localPlan(local.chain),journalPath=join(dir,"private","round-deployment.jsonl"),manifestPath=join(dir,"robinhood-testnet.json");
    const options={provider:local.provider,chain:local.chain,plan,deployer:DEPLOYER.address,wallet:DEPLOYER,journalPath,manifestPath,source:TEST_SOURCE};
    // The dry run plans five factory calls, estimates what it can, and the implementation's creation through the same-sized probe.
    const dry=await deployRoundSet({...options,send:false});
    assert.equal(dry.mode,"dry run");
    assert.deepEqual(dry.transactions.map(t=>[t.step,t.status]),[["beaconVerifier","planned"],["proofVerifier","planned"],["mappingLibrary","planned"],
      ["coordinatorImplementation","planned"],["coordinator","planned"]]);
    for(const t of dry.transactions){
      assert.equal(t.transaction?.to,local.chain.create2.factory);assert.equal(t.transaction?.from,DEPLOYER.address);
      assert.equal(t.transaction?.dataHash,keccak256(concat([t.salt,plan.step[t.step].initCode])));
    }
    assert.ok(dry.transactions.slice(0,3).every(t=>t.transaction?.estimatedGas!==undefined),"the three independent creations are estimated");
    assert.match(dry.transactions[3].transaction!.note!,/estimated when proofVerifier exist/);
    const creation=dry.preflight.implementationCreation as {probe:boolean;estimatedGas:string;runtimeCodeBytes:number;initCodeBytes:number};
    assert.equal(creation.probe,true);assert.ok(BigInt(creation.estimatedGas)>4_000_000n);
    assert.ok(creation.runtimeCodeBytes<=24_576,`runtime ${creation.runtimeCodeBytes} bytes`);
    assert.ok(BigInt(dry.preflight.budgetWei)>0n);
    assert.equal(dry.owner.kind,"EOA");assert.deepEqual(dry.ownership,{ownerTransferPending:false});assert.deepEqual(dry.calls,[]);
    for(const step of plan.steps)assert.equal(await local.provider.getCode(step.address),"0x",step.name);
    assert.deepEqual(await readdir(dir),[],"a dry run writes nothing");

    // Sent: each creation journaled, mined and checked, the set verified and the manifest written.
    const nonce=await local.provider.getTransactionCount(DEPLOYER.address);
    const sent=await deployRoundSet({...options,send:true});
    assert.deepEqual(sent.transactions.map(t=>t.status),["deployed","deployed","deployed","deployed","deployed"]);
    assert.equal(await local.provider.getTransactionCount(DEPLOYER.address),nonce+5);
    assert.equal(sent.manifest?.written,true);
    const v=sent.verification!;
    assert.equal(v.owner,OWNER.address);assert.equal(v.keeper,KEEPER.address);assert.equal(v.feeRecipient,OWNER.address);
    assert.equal(v.keeperFeeBps,"8000");assert.equal(v.pricing,"25000000000000,2,405000");assert.equal(v.refundBps,"10000");
    assert.equal(v.roundLead,"3");assert.equal(v.beaconSchedule,"0,0,0");assert.equal(v["getRoundRequest(unknown id)"],"0x6d080297");
    assert.equal(v["getRequest(uint256)"],"reverts");assert.equal(v.implementation,plan.step.coordinatorImplementation.address);
    assert.equal(v.protocolConfigurationHash,plan.protocolConfigurationHash);assert.equal(v.keyHash,plan.keyHash);
    const journal=(await readFile(journalPath,"utf8")).trim().split("\n").map(line=>JSON.parse(line));
    assert.deepEqual(journal.map(entry=>entry.type),["run",...Array(5).fill(["signed","receipt"]).flat()]);

    // The manifest: round-native, every address, code hash, salt and receipt, the beacon and the pricing, nothing of the epoch design.
    const manifestText=await readFile(manifestPath,"utf8"),manifest=JSON.parse(manifestText);
    assert.equal(manifest.coordinatorContract,"D20VRFCoordinatorRobinhood");assert.equal(manifest.network,"robinhood-testnet");assert.equal(manifest.chainId,46630);
    assert.equal(manifest.beaconVerifier,BEACON_VERIFIER);assert.equal(manifest.coordinator,plan.step.coordinator.address);
    for(const step of plan.steps){
      assert.equal(manifest[`${step.name}CodeHash`],step.runtimeCodeHash,step.name);
      assert.deepEqual(manifest.create2[step.name],{contract:step.contract,salt:step.salt,initCodeHash:step.initCodeHash},step.name);
    }
    assert.deepEqual(manifest.transactions.map((t:any)=>[t.contract,t.address,typeof t.hash,typeof t.block]),plan.steps.map(s=>[s.name,s.address,"string","number"]));
    assert.deepEqual(manifest.pricing,{minFeeWei:"25000000000000",feeMultiplier:2,fulfillGasOverhead:405000,keeperFeeBps:8000,refundBps:10000});
    assert.equal(manifest.roundLead,3);assert.equal(manifest.beacon.id,0);assert.equal(manifest.beacon.sampleRound,21072365);
    assert.equal(manifest.beacon.identity,plan.beaconIdentity);assert.equal(manifest.storageLayoutHash,plan.storageLayoutHash);
    assert.equal("ownerTransferPending" in manifest,false);assert.equal("finalOwner" in manifest,false);assert.deepEqual(manifest.backupKeepers,[]);
    assert.equal(manifest.sourceCommit,null);assert.equal(manifest.sourceUnreviewed,TEST_SOURCE.unreviewed);
    assert.deepEqual(legacyFindings({name:"manifest",text:manifestText},{chainIds:[5042,5042002],owners:[]}),[]);
    for(const field of ["registry","epochImplementation","epochImplementationCodeHash","firstEpochStart","confirmations"])assert.equal(field in manifest,false,field);
    assert.doesNotMatch(manifestText,/\.key"|keyFile|private/,"the public manifest names no key file or private path");

    // A second run deploys nothing, checks everything and leaves the manifest as it is.
    const again=await deployRoundSet({...options,send:true});
    assert.deepEqual(again.transactions.map(t=>t.status),["present","present","present","present","present"]);
    assert.equal(again.manifest?.written,false);assert.equal(await readFile(manifestPath,"utf8"),manifestText);
    assert.equal(await local.provider.getTransactionCount(DEPLOYER.address),nonce+5);
    const check=await deployRoundSet({...options,send:false});
    assert.equal(check.verification?.owner,OWNER.address);assert.match(check.next!,/the manifest records it/);

    // A manifest of this path that records another deployment is never overwritten or reused; one written before backup keepers were
    // recorded is read as allowing none.
    const other=await localPlan(local.chain,{keeper:STRANGER.address});
    await assert.rejects(deployRoundSet({...options,plan:other,send:false}),refusal(/records another deployment \(keeper, coordinator\)/));
    const older={...manifest};delete older.backupKeepers;
    await writeFile(manifestPath,JSON.stringify(older,null,2)+"\n");
    assert.equal((await deployRoundSet({...options,send:false})).verification?.backupKeeperCount,"0");
    await assert.rejects(deployRoundSet({...options,plan:await localPlan(local.chain,{backupKeeper:BACKUP.address}),deployer:OWNER.address,wallet:undefined,send:false}),
      refusal(/records another deployment \(backupKeepers\)/));
  }finally{await local.close();}
});

test("a partial deployment is completed: what is already at its address is checked and kept, and recorded as deployed earlier",async()=>{
  const local=await localChain("robinhood-testnet",OWNER.address),dir=await tmp("partial");
  try{
    const plan=await localPlan(local.chain);
    // The beacon verifier and the proof verifier are already there, as on a chain where another deployment used them.
    for(const step of [plan.step.beaconVerifier,plan.step.proofVerifier])
      await (await DEPLOYER.connect(local.provider).sendTransaction({to:plan.factory,data:concat([step.salt,step.initCode])})).wait();
    const {report,manifestPath}=await deployLocal(local,dir);
    assert.deepEqual(report.transactions.map(t=>t.status),["present","present","deployed","deployed","deployed"]);
    const manifest=JSON.parse(await readFile(manifestPath,"utf8"));
    assert.deepEqual(manifest.transactions.slice(0,2),[{contract:"beaconVerifier",address:plan.step.beaconVerifier.address,deployedEarlier:true},
      {contract:"proofVerifier",address:plan.step.proofVerifier.address,deployedEarlier:true}]);
    assert.equal(typeof manifest.transactions[2].hash,"string");
  }finally{await local.close();}
});

test("other code at a CREATE2 address, a chain other than the profile's and a production owner that is not the profile's Safe are refused",async()=>{
  const local=await localChain("robinhood-testnet",OWNER.address),dir=await tmp("refusals");
  try{
    const plan=await localPlan(local.chain),options={provider:local.provider,chain:local.chain,plan,deployer:DEPLOYER.address,wallet:DEPLOYER,
      journalPath:join(dir,"journal.jsonl"),manifestPath:join(dir,"manifest.json"),source:TEST_SOURCE};
    await local.request("hardhat_setCode",[BEACON_VERIFIER,SAFE_STAND_IN_CODE]);
    for(const send of [false,true])
      await assert.rejects(deployRoundSet({...options,send}),refusal(/^beaconVerifier \(D20BeaconVerifier\) at 0xd20dA01Aa16AeD6b77Cd8DDb869151802599100a has runtime code hash 0x\w+, not the expected 0x4388250d.*refusing to use it or deploy over it$/));
    assert.equal(await local.provider.getTransactionCount(DEPLOYER.address),0);
    // The testnet profile's plan against the same chain under the mainnet profile: its chain id differs.
    const mainnetProfile=await profile("robinhood-mainnet",OWNER.address);
    await assert.rejects(deployRoundSet({...options,chain:mainnetProfile,plan:await localPlan(mainnetProfile),send:false}),refusal(/^The RPC serves chain 46630, not robinhood-mainnet \(chain 4663\)$/));
    // A plan for one chain is not run against another profile.
    await assert.rejects(deployRoundSet({...options,chain:mainnetProfile,send:false}),refusal(/plan is for another chain/));
    await assert.rejects(deployRoundSet({...options,send:true,wallet:STRANGER}),refusal(/needs the deployer's wallet/));
  }finally{await local.close();}
  const mainnet=await localChain("robinhood-mainnet",OWNER.address);
  try{
    const plan=await localPlan(mainnet.chain),options={provider:mainnet.provider,chain:mainnet.chain,plan,deployer:DEPLOYER.address,send:false,
      journalPath:join(dir,"m-journal.jsonl"),manifestPath:join(dir,"m-manifest.json"),source:TEST_SOURCE};
    // A profile that names a Safe (an operator's owner overlay; here a stand-in address): another owner is refused, and so is the Safe's own
    // address where it has no Safe's code (on this local chain), and a profile without its Safe.
    const SAFE="0x00000000000000000000000000000000005afE77";
    assert.equal(mainnet.chain.safe,undefined,"the committed profile names no Safe");
    const named={...mainnet.chain,...safeProfile(SAFE),owner:OWNER.address};
    await assert.rejects(deployRoundSet({...options,chain:named,plan:await localPlan(named)}),refusal(new RegExp(`^The owner of robinhood-mainnet ${OWNER.address} is not the Safe a production owner must be: it has no code; it is not the robinhood-mainnet profile's Safe ${SAFE}$`)));
    const absent={...named,owner:SAFE};
    await assert.rejects(deployRoundSet({...options,chain:absent,plan:await localPlan(absent)}),
      refusal(new RegExp(`^The owner of robinhood-mainnet ${SAFE} is not the Safe a production owner must be: it has no code$`)));
    const safeless={...mainnet.chain,safe:undefined};
    await assert.rejects(deployRoundSet({...options,chain:safeless,plan:await localPlan(safeless)}),
      refusal(/^The owner of robinhood-mainnet 0x\w+ is not the Safe a production owner must be: the robinhood-mainnet profile names no Safe \(safe in chains\.json\); it has no code$/));
    // A contract that answers getThreshold() with 1 is not a Safe, even as the profile's Safe.
    const standIn=getAddress("0x00000000000000000000000000000000005afe01");
    await mainnet.request("hardhat_setCode",[standIn,SAFE_STAND_IN_CODE]);
    const posing={...mainnet.chain,...safeProfile(standIn),owner:standIn};
    await assert.rejects(deployRoundSet({...options,chain:posing,plan:await localPlan(posing)}),
      refusal(/is not the Safe a production owner must be: its runtime code hash 0x\w+ is not a canonical SafeProxy's \(1\.3\.0, 1\.4\.1 or 1\.5\.0\); its singleton \(storage slot 0, 0x\w+\) is not a canonical Safe or SafeL2 singleton; it does not answer VERSION\(\)/));
    // A real Safe 1.5.0 owns it only as the profile's Safe, with the profile's owners and a threshold of at least 2.
    const safe=await localSafe(mainnet);
    const unnamed={...mainnet.chain,safe:undefined,owner:safe};
    await assert.rejects(deployRoundSet({...options,chain:unnamed,plan:await localPlan(unnamed)}),refusal(/is not the Safe a production owner must be: the robinhood-mainnet profile names no Safe/));
    const unknownOwners={...mainnet.chain,...safeProfile(safe,{owners:null}),owner:safe};
    await assert.rejects(deployRoundSet({...options,chain:unknownOwners,plan:await localPlan(unknownOwners)}),refusal(/does not record the Safe's owners yet/));
    const chain={...mainnet.chain,...safeProfile(safe),owner:safe},report=await deployRoundSet({...options,chain,plan:await localPlan(chain)});
    assert.deepEqual(report.owner,{address:safe,kind:"Safe"});assert.deepEqual(report.ownership,{ownerTransferPending:false});
    // A backup keeper is allowed by the deployer's call, so the deployer must own the set.
    await assert.rejects(deployRoundSet({...options,chain,plan:await localPlan(chain,{backupKeeper:BACKUP.address})}),
      refusal(/setBackupKeeper call from the deployer 0x\w+, which is not the owner 0x\w+: allow it with admin-robinhood\.ts set-backup-keeper instead/));
  }finally{await mainnet.close();}
});

test("a production set deployed with its interim owner: the deployer owns it and receives its fees, allows the backup keeper, and the manifest records the pending transfer",async()=>{
  const local=await localChain("robinhood-mainnet",OWNER.address),dir=await tmp("interim");
  try{
    const safe=await withSafe(local,{interimOwner:DEPLOYER.address});
    const roles={owner:DEPLOYER.address,feeRecipient:DEPLOYER.address,keeper:KEEPER.address,backupKeeper:BACKUP.address};
    const plan=await localPlan(local.chain,roles,{finalOwner:safe}),journalPath=join(dir,"private","round-deployment.jsonl"),manifestPath=join(dir,"robinhood-mainnet.json");
    const options={provider:local.provider,chain:local.chain,plan,deployer:DEPLOYER.address,wallet:DEPLOYER,journalPath,manifestPath,source:TEST_SOURCE};
    const dry=await deployRoundSet({...options,send:false});
    assert.deepEqual(dry.owner,{address:DEPLOYER.address,kind:"EOA"});assert.equal(dry.feeRecipient,DEPLOYER.address);assert.equal(dry.backupKeeper,BACKUP.address);
    assert.deepEqual(dry.ownership,{ownerTransferPending:true,finalOwner:{address:safe,kind:"Safe",safeProblems:[]}});
    assert.equal(dry.calls.length,1);
    const call=dry.calls[0],face=await coordinatorInterface();
    assert.deepEqual([call.step,call.function,call.args,call.status],["backupKeeper","setBackupKeeper(address,bool)",[BACKUP.address,"true"],"planned"]);
    assert.deepEqual([call.transaction.from,call.transaction.to,call.transaction.data],[DEPLOYER.address,plan.step.coordinator.address,face.encodeFunctionData("setBackupKeeper",[BACKUP.address,true])]);
    assert.match(call.transaction.note!,/estimated when the coordinator exists; budgeted at 300000 gas/);
    assert.match(dry.next!,/to send beaconVerifier, proofVerifier, mappingLibrary, coordinatorImplementation, coordinator, backupKeeper$/);

    const nonce=await local.provider.getTransactionCount(DEPLOYER.address);
    const sent=await deployRoundSet({...options,send:true});
    assert.equal(await local.provider.getTransactionCount(DEPLOYER.address),nonce+6);
    assert.equal(sent.calls[0].status,"sent");assert.equal(typeof sent.calls[0].receipt?.hash,"string");
    const v=sent.verification!;
    assert.equal(v.owner,DEPLOYER.address);assert.equal(v.feeRecipient,DEPLOYER.address);assert.equal(v.keeper,KEEPER.address);
    assert.equal(v.backupKeeperCount,"1");assert.equal(v.backupKeeper,"true");assert.equal(v.pendingOwner,"0x0000000000000000000000000000000000000000");
    assert.match(sent.next!,/The owner is the interim owner: move the set to the Safe 0x\w+ with admin-robinhood\.ts transfer-ownership, accept-ownership and set-fee-recipient/);
    const journal=(await readFile(journalPath,"utf8")).trim().split("\n").map(line=>JSON.parse(line));
    assert.deepEqual(journal.filter(entry=>entry.type==="signed").map(entry=>entry.step),[...plan.steps.map(s=>s.name),"backupKeeper"]);
    const manifest=JSON.parse(await readFile(manifestPath,"utf8"));
    assert.equal("ownerTransferPending" in manifest,false);assert.equal("finalOwner" in manifest,false);
    assert.equal(manifest.owner,DEPLOYER.address);assert.equal(manifest.feeRecipient,DEPLOYER.address);assert.deepEqual(manifest.backupKeepers,[BACKUP.address]);
    const last=manifest.transactions.at(-1);
    assert.deepEqual([last.call,last.address,last.account,last.hash],["setBackupKeeper",plan.step.coordinator.address,BACKUP.address,sent.calls[0].receipt?.hash]);

    // A second run sends nothing and finds the call made.
    const again=await deployRoundSet({...options,send:true});
    assert.deepEqual([...again.transactions.map(t=>t.status),again.calls[0].status],[...Array(5).fill("present"),"present"]);
    assert.equal(await local.provider.getTransactionCount(DEPLOYER.address),nonce+6);

    // The interim owner is the profile's, an account without code, the deployer itself, and the set moves to the profile's owner.
    await assert.rejects(deployRoundSet({...options,deployer:STRANGER.address,wallet:STRANGER,send:false}),refusal(/^The interim owner 0x\w+ is not the deployer 0x\w+: the deployer owns the set until the transfer$/));
    const otherInterim={...local.chain,interimOwner:STRANGER.address};
    await assert.rejects(deployRoundSet({...options,chain:otherInterim,send:false}),refusal(/^The set's owner 0x\w+ is not robinhood-mainnet's interimOwner \(0x\w+\)$/));
    await assert.rejects(deployRoundSet({...options,plan:await localPlan(local.chain,roles,{finalOwner:STRANGER.address}),send:false}),refusal(/^The set is to move to 0x\w+, not to robinhood-mainnet's owner 0x\w+$/));
  }finally{await local.close();}
  // A test network's owner owns its set from the deployment.
  const testnet=await localChain("robinhood-testnet",DEPLOYER.address);
  try{
    const plan=await localPlan(testnet.chain,{keeper:KEEPER.address},{finalOwner:STRANGER.address});
    await assert.rejects(deployRoundSet({provider:testnet.provider,chain:testnet.chain,plan,deployer:DEPLOYER.address,send:false,source:TEST_SOURCE,
      journalPath:join(dir,"t-journal.jsonl"),manifestPath:join(dir,"t-manifest.json")}),refusal(/^robinhood-testnet is a test network: its owner owns its set from the deployment, without an interim owner$/));
  }finally{await testnet.close();}
});

test("a plan keeps keeper wallets apart from the owner and from each other, and an interim owner apart from its Safe",async()=>{
  const chain=await profile("robinhood-testnet",OWNER.address);
  await assert.rejects(localPlan(chain,{keeper:OWNER.address}),refusal(/^The keeper 0x\w+ is also the owner or the fee recipient: a keeper wallet needs its own key$/));
  await assert.rejects(localPlan(chain,{backupKeeper:OWNER.address}),refusal(/^The backup keeper 0x\w+ is also the owner or the fee recipient/));
  await assert.rejects(localPlan(chain,{backupKeeper:KEEPER.address}),refusal(/^The backup keeper 0x\w+ is the keeper: a backup keeper needs its own wallet$/));
  await assert.rejects(localPlan(chain,{backupKeeper:"0x"+"00".repeat(20)}),refusal(/^The backup keeper must be a nonzero address$/));
  await assert.rejects(localPlan(chain,{},{finalOwner:OWNER.address}),refusal(/^An interim owner is the set's owner and fee recipient, and not the Safe that takes it over$/));
  await assert.rejects(localPlan(chain,{feeRecipient:STRANGER.address},{finalOwner:BACKUP.address}),refusal(/^An interim owner is the set's owner and fee recipient/));
  const plan=await localPlan(chain,{backupKeeper:BACKUP.address.toLowerCase()},{finalOwner:STRANGER.address.toLowerCase()});
  assert.equal(plan.roles.backupKeeper,BACKUP.address);assert.deepEqual(plan.interim,{finalOwner:STRANGER.address});
});

test("the command line refuses an undecided owner, a chain without a round coordinator and a production chain without --mainnet before any network access",async()=>{
  const dir=await tmp("cli-refusals"),env=join(dir,"does-not-exist.env");
  // Both profiles are read as chains.json holds them, except where a preload reads the production owner as undecided, or stands the Safe in
  // without its interim owner.
  const overlay={D20_PRIVATE_PROFILE_DIR:fileURLToPath(new URL("./fixtures/private-profile/",import.meta.url))};
  const none=join(dir,"none"),noInterim={D20_STAND_IN_OWNER:"0x00000000000000000000000000000000005afE77",D20_STAND_IN_PROFILE:JSON.stringify({interimOwner:null})};
  const cases:Array<[string[],string[],RegExp,Record<string,string>?]>=[
    [[UNDECIDED_PRELOAD],["--chain","robinhood-mainnet","--env",env],/^Robinhood deployment stopped: The owner of robinhood-mainnet is not set, so deploy-robinhood\.ts will not run against it/],
    [[],["--chain","arc-testnet","--env",env],/^Robinhood deployment stopped: deploy-robinhood\.ts deploys and administers the round coordinator, and arc-testnet does not run one/],
    [[],["--chain","robinhood-mainnet","--env",env],/^Robinhood deployment stopped: robinhood-mainnet is a production chain: a deployment there also needs --mainnet/],
    // With --mainnet the production profile and, from an owner overlay (a fixture of stand-in addresses), its Safe and interim owner pass every
    // check that needs no network, with and without --interim-owner, up to the operator identity this test does not give. The committed profile
    // names no interim owner.
    [[],["--chain","robinhood-mainnet","--env",env,"--mainnet","--operator-directory",none],/^Robinhood deployment stopped: No operator\.json in .*none; check --operator-directory/,overlay],
    [[],["--chain","robinhood-mainnet","--env",env,"--mainnet","--interim-owner","--operator-directory",none],/^Robinhood deployment stopped: No operator\.json in .*none; check --operator-directory/,overlay],
    [[],["--chain","robinhood-mainnet","--env",env,"--mainnet","--interim-owner","--operator-directory",none],/^Robinhood deployment stopped: robinhood-mainnet's profile names no interimOwner/],
    [[],["--chain","robinhood-testnet"],/^Robinhood deployment stopped: Provide --env/],
    [[],["--chain","robinhood-testnet","--env",env,"--new-operator","--send"],/--new-operator creates the operator identity in a dry run/],
    [[],["--chain","robinhood-testnet","--env",env,"--rpc-url","https://rpc.example/key"],/--rpc-url takes a local node's URL only/],
    [[],["--chain","robinhood-testnet","--env",env,"--operator-directory",join(dir,"none")],/No operator\.json in .*none; check --operator-directory/],
    [[],[],/Provide --chain/],
    [[],["--chain","robinhood-testnet","--env",env,"--interim-owner"],/^Robinhood deployment stopped: --interim-owner is for a production chain, whose Safe takes the set over later/],
    [[],["--chain","robinhood-mainnet","--env",env,"--interim-owner"],/^Robinhood deployment stopped: robinhood-mainnet is a production chain: a deployment there also needs --mainnet/],
    [[STAND_IN_PRELOAD],["--chain","robinhood-mainnet","--env",env,"--mainnet","--interim-owner"],/^Robinhood deployment stopped: robinhood-mainnet's profile names no interimOwner: set it in chains\.json or its owner overlay/,noInterim],
    [[],["--chain","robinhood-testnet","--env",env,"--keeper","0x1234"],/^Robinhood deployment stopped: --keeper must be a nonzero address/],
    [[],["--chain","robinhood-testnet","--env",env,"--backup-keeper","0x"+"00".repeat(20)],/^Robinhood deployment stopped: --backup-keeper must be a nonzero address/],
    [[],["--chain","robinhood-testnet","--env",env,"--keeper",KEEPER.address,"--backup-keeper",KEEPER.address.toLowerCase()],/--keeper and --backup-keeper name the same wallet/],
  ];
  for(const [preloads,args,pattern,environment] of cases){
    const result=await run("scripts/deploy-robinhood.ts",args,[NO_NETWORK,...preloads],environment);
    assert.equal(result.status,1,`${args.join(" ")}: ${result.stderr}`);
    assert.match(result.stderr,pattern,args.join(" "));
    assert.doesNotMatch(result.stderr,/NETWORK ACCESS|does-not-exist/,args.join(" "));
  }
  // The epoch design's deployment script still refuses a round chain whose owner is decided, for its Arbitrum path alone.
  const old=await run("scripts/create2-deploy.ts",["prepare","--chain","robinhood-testnet","--env",env],[NO_NETWORK]);
  assert.equal(old.status,1);
  assert.match(old.stderr,/^Deployment stopped: Robinhood deployments use the Arbitrum path, which is not implemented in this script yet \(create2-deploy\.ts, chain robinhood-testnet/);
  assert.doesNotMatch(old.stderr,/NETWORK ACCESS|is not set, so/);
});

test("the command line deploys a local chain end to end: a dry run, the deployment, and a run that finds it all in place",async()=>{
  // The testnet profile as it is, with its own owner (the deployer EOA, an account without code on the local chain), read without a preload;
  // the local chain serves the testnet's chain id on 127.0.0.1.
  const {owner}=await loadChain("robinhood-testnet");
  assert.ok(owner,"the testnet profile names its owner");
  const local=await localChain("robinhood-testnet",owner),endpoint=await rpcEndpoint(local),dir=await tmp("cli");
  try{
    const operator=await operatorDirectory(join(dir,"operator"),owner),env=await envFile(join(dir,"deployer.env"),DEPLOYER);
    const args=["--chain","robinhood-testnet","--env",env,"--operator-directory",operator,"--manifest",join(dir,"robinhood-testnet.json"),
      "--private-directory",join(dir,"private"),"--rpc-url",endpoint.url];
    const parse=(result:{status:number|null;stdout:string;stderr:string})=>{assert.equal(result.status,0,result.stderr);return JSON.parse(result.stdout) as DeployReport&{rpc:string};};
    const deploy=(extra:string[]=[])=>run("scripts/deploy-robinhood.ts",[...args,...extra],[UNREVIEWED_PRELOAD]);
    const dry=parse(await deploy());
    assert.equal(dry.mode,"dry run");assert.equal(dry.rpc,new URL(endpoint.url).origin);
    assert.deepEqual(dry.source,{commit:dry.source.commit,unreviewed:"not checked: a test's rehearsal against a local node (scripts/tests/fixtures/unreviewed-source.mjs)"});
    assert.equal(dry.keeper,KEEPER.address);assert.equal(dry.owner.address,owner);
    assert.equal(dry.transactions.filter(t=>t.status==="planned").length,5);
    assert.doesNotMatch(JSON.stringify(dry),new RegExp(DEPLOYER.privateKey.slice(2),"i"));
    const sent=parse(await deploy(["--send"]));
    assert.deepEqual(sent.transactions.map(t=>t.status),Array(5).fill("deployed"));
    assert.equal(sent.manifest?.written,true);
    const again=parse(await deploy(["--send"]));
    assert.deepEqual(again.transactions.map(t=>t.status),Array(5).fill("present"));
    assert.equal(again.manifest?.written,false);
    const manifest=JSON.parse(await readFile(join(dir,"robinhood-testnet.json"),"utf8"));
    assert.equal(manifest.owner,owner);assert.equal(manifest.deployer,DEPLOYER.address);
    assert.match(manifest.sourceUnreviewed,/^not checked: a test's rehearsal against a local node/);
    // The keeper settings generator reads the coordinator's pins from it.
    const keeperEnv=await run("scripts/keeper-env.ts",["--chain","robinhood-testnet","--env",env,"--deployment",join(dir,"robinhood-testnet.json"),"--allow-public-only","--out",join(dir,"keeper.env")]);
    assert.equal(keeperEnv.status,0,keeperEnv.stderr);
    const settings=await readFile(join(dir,"keeper.env"),"utf8");
    assert.match(settings,new RegExp(`^COORDINATOR_ADDRESS='${manifest.coordinator}'$`,"m"));
    assert.match(settings,new RegExp(`^EXPECTED_IMPLEMENTATION_CODE_HASH='${manifest.coordinatorImplementationCodeHash}'$`,"m"));
    // Without a local node the preload is not honored: the source is checked (a clean, pushed tree goes on, and here meets the network guard).
    const unhonored=await run("scripts/deploy-robinhood.ts",["--chain","robinhood-testnet","--env",env,"--operator-directory",operator,"--manifest",join(dir,"other.json")],
      [NO_NETWORK,UNREVIEWED_PRELOAD]);
    assert.equal(unhonored.status===1||unhonored.status===97,true,unhonored.stderr);
    assert.match(unhonored.stderr,/uncommitted changes under contracts, config, scripts, package\.json, package-lock\.json|is on no remote branch|NETWORK ACCESS/);
  }finally{await endpoint.close();await local.close();}
});

test("only reviewed source is deployed: uncommitted changes under the reviewed paths and a commit on no remote branch are refused, and the test preload counts only against a local node",async()=>{
  const repository=await tmp("source"),git=(...args:string[])=>execFileSync("git",args,{cwd:repository,encoding:"utf8",windowsHide:true});
  git("init","--quiet","--initial-branch=main");git("config","user.email","test@example.invalid");git("config","user.name","Test");git("config","commit.gpgsign","false");git("config","core.autocrlf","false");
  for(const path of ["contracts","config","scripts","docs"])await mkdir(join(repository,path),{recursive:true});
  await writeFile(join(repository,"contracts","A.sol"),"// a\n");await writeFile(join(repository,"package.json"),"{}\n");await writeFile(join(repository,"docs","notes.md"),"notes\n");
  git("add","-A");git("commit","--quiet","-m","first");
  const check=(localRpc=false)=>reviewedSource({localRpc,repository});
  // Committed, but on no remote branch.
  assert.throws(()=>check(),refusal(/^The commit [0-9a-f]{40} is on no remote branch \(git branch -r --contains\): push it for review, fetch, and deploy from the pushed commit$/));
  // A remote-tracking branch that contains it: reviewed.
  git("update-ref","refs/remotes/origin/main","HEAD");
  const head=git("rev-parse","HEAD").trim();
  assert.deepEqual(check(),{commit:head,remoteBranches:["origin/main"]});
  // A change outside the reviewed paths does not count; a change, an untracked file or a staged file under them does.
  await writeFile(join(repository,"docs","notes.md"),"changed\n");
  assert.deepEqual(check(),{commit:head,remoteBranches:["origin/main"]});
  for(const [path,text] of [["contracts/A.sol","// changed\n"],["config/new.json","{}\n"],["scripts/new.ts","\n"],["package.json","{\"a\":1}\n"],["package-lock.json","{}\n"]] as const){
    const file=join(repository,...path.split("/")),before=await readFile(file,"utf8").catch(()=>undefined);
    await writeFile(file,text);
    assert.throws(()=>check(),refusal(new RegExp(`^The working tree has uncommitted changes under contracts, config, scripts, package\\.json, package-lock\\.json \\(${path.replace(".","\\.")}\\): deploy only committed, reviewed code$`)),path);
    if(before===undefined)git("clean","--quiet","-f","--",path);else await writeFile(file,before);
  }
  // A new commit that is not pushed yet is refused again.
  git("commit","--quiet","-am","docs");
  assert.throws(()=>check(),refusal(/is on no remote branch/));
  // The test preload's global counts only for a deployment against a local node.
  const global=globalThis as Record<symbol,unknown>;
  global[UNREVIEWED_SOURCE_FOR_TESTS]=true;
  try{
    assert.throws(()=>check(false),refusal(/is on no remote branch/));
    assert.deepEqual(check(true),{commit:git("rev-parse","HEAD").trim(),unreviewed:"not checked: a test's rehearsal against a local node (scripts/tests/fixtures/unreviewed-source.mjs)"});
  }finally{delete global[UNREVIEWED_SOURCE_FOR_TESTS];}
  assert.throws(()=>check(true),refusal(/is on no remote branch/));
  // Outside a git checkout nothing is deployed.
  const notGit=await tmp("not-git");
  assert.throws(()=>reviewedSource({localRpc:false,repository:notGit}),refusal(/^git status failed in .*: a deployment runs from a git checkout of the reviewed code$/));
});
