import {test} from "node:test";
import assert from "node:assert/strict";
import {spawnSync} from "node:child_process";
import {readdir} from "node:fs/promises";
import {join} from "node:path";
import {fileURLToPath} from "node:url";
import {ROUND_SCRIPTS,SHARED_SIGNER,arcMarkers,legacyFindings,profileSurfaceText,robinhoodSurfaces} from "../lib/robinhood-profiles.ts";

const root=fileURLToPath(new URL("../..",import.meta.url));
const ARC_TESTNET_OWNER="0xcA35c280c5DF22958Adb74DD03911C8A0ec3eA03",ARC_MAINNET_OWNER="0xB57f656149749eff6b496dF090336491f977E744";
const markers=arcMarkers({"arc-testnet":{chainId:5042002,owner:ARC_TESTNET_OWNER},"arc-mainnet":{chainId:5042,owner:ARC_MAINNET_OWNER}});
const find=(text:string)=>legacyFindings({name:"surface",text},markers);

test("the gate reads every Robinhood profile, finds nothing of the epoch design or of Arc in them, and takes Arc's markers from Arc's profiles",async()=>{
  const {surfaces,markers:found}=await robinhoodSurfaces();
  // Robinhood deployment manifests join the list once they are committed under deployments/.
  assert.deepEqual(surfaces.map(surface=>surface.name).filter(name=>!name.startsWith("deployments/")),["chains.json robinhood-mainnet","chains.json robinhood-testnet",
    "config/service.robinhood-mainnet.json","config/service.robinhood-testnet.json","config/beacons/drand-evmnet.json","deploy/docker/keeper.robinhood.env.example",
    "keeper.env for robinhood-mainnet (primary)","keeper.env for robinhood-mainnet (follower)","keeper.env for robinhood-testnet (primary)","keeper.env for robinhood-testnet (follower)",
    "config/robinhood-create2.json","scripts/deploy-robinhood.ts","scripts/lib/robinhood-deploy.ts","scripts/admin-robinhood.ts","scripts/lib/robinhood-admin.ts","scripts/lib/safe.ts"]);
  // The generated files carry every optional part, so the gate reads all of what the generator can write.
  for(const surface of surfaces.filter(surface=>surface.name.startsWith("keeper.env")))
    for(const name of ["WS_URLS=","TELEGRAM_LOW_BALANCE_WEI=","NEON_DB=","HEALTH_API_URL=","DISCORD_PUBLIC_EXPLORER_URL=","COORDINATOR_KIND=","FEE_COVERAGE_BPS="])
      assert.ok(surface.text.includes(`\n${name}`),`${surface.name} has ${name}`);
  assert.deepEqual(found,{chainIds:[5042,5042002],owners:[ARC_TESTNET_OWNER,ARC_MAINNET_OWNER]});
  assert.deepEqual(surfaces.flatMap(surface=>legacyFindings(surface,found)),[]);
  const run=spawnSync(process.execPath,["scripts/check-robinhood-profiles.ts"],{cwd:root,encoding:"utf8"});
  assert.equal(run.status,0,run.stderr);
  assert.match(run.stdout,new RegExp(`^The ${surfaces.length} Robinhood profiles, scripts and manifests name no registry`));
});

test("every word of the epoch design, Arc's name, an Arc chain id and an Arc owner is found in any spelling, with its line",()=>{
  const cases:Array<[string,string[]]>=[
    ['"registryKind": "beacon"',['surface:1: "registry"']],
    ["EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH='0x01'",['surface:1: "REGISTRY"']],
    ['"registry": "EpochBeacon",\n"epochLength": 600',['surface:1: "registry"','surface:1: "Epoch"','surface:2: "epoch"']],
    ["config/recipes/passthrough-6.json",['surface:1: "recipe"']],
    ["AirnodeHub signers",['surface:1: "Airnode"']],
    ["BLOCK_NUDGE='true'\nblockNudgeAfterMs",['surface:1: "NUDGE"','surface:2: "Nudge"']],
    ["# 0.05 USDC on testnet",['surface:1: "USDC"']],
    ["the registry committer",['surface:1: "registry"','surface:1: "committer"']],
    ['"confirmations": 1',['surface:1: "confirmation"']],
    ["Arc Testnet's owner EOA",['surface:1: "Arc"']],
    ["https://rpc.testnet.arc.io",['surface:1: "arc"']],
    ["CHAIN_ID=5042002",["surface:1: Arc chain id 5042002"]],
    ['"chainId": 5042,',["surface:1: Arc chain id 5042"]],
    [`"owner": "${ARC_TESTNET_OWNER}"`,[`surface:1: Arc owner ${ARC_TESTNET_OWNER.toLowerCase()}`]],
    [`owner ${ARC_MAINNET_OWNER.toLowerCase()}`,[`surface:1: Arc owner ${ARC_MAINNET_OWNER.toLowerCase()}`]],
  ];
  for(const [text,findings] of cases)assert.deepEqual(find(text),findings,text);
  // Words that merely contain the letters, and digits inside a hash or a longer number, are not findings.
  for(const text of ["search","Arbitrum","arbitrum-l2","archive","0x04f1e9062b8a81f850420020","CHAIN_ID=46630","15042","50421","50420020",'"overhead": 405000'])
    assert.deepEqual(find(text),[],text);
});

test("the profiles of the epoch design that the round profiles replaced would fail the gate",()=>{
  // config/service.robinhood-*.json and a Robinhood keeper block as they stood before the round design, and the keeper.env lines they gave.
  const service=JSON.stringify({registry:"EpochBeacon",coordinator:"D20VRFCoordinatorArbitrum",epochLength:600,beacon:"config/beacons/drand-evmnet.json",
    minFeeWei:"40000000000000",keeperFeeBps:6000,confirmations:1,pricing:{multiplier:4,overhead:300000}},null,2);
  const keeper=JSON.stringify({owner:ARC_TESTNET_OWNER,ownerNote:"Arc Testnet's owner EOA",keeper:{registryKind:"beacon",blockNudge:true,blockNudgeAfterMs:1500}},null,2);
  const env="EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH='0x04'\nREGISTRY_KIND='beacon'\nBLOCK_NUDGE='true'\nBLOCK_NUDGE_AFTER_MS='1500'";
  assert.deepEqual(find(service),['surface:2: "registry"','surface:2: "Epoch"','surface:4: "epoch"','surface:8: "confirmation"']);
  assert.deepEqual(find(keeper),[`surface:2: Arc owner ${ARC_TESTNET_OWNER.toLowerCase()}`,'surface:3: "Arc"','surface:5: "registry"','surface:6: "Nudge"','surface:7: "Nudge"']);
  assert.deepEqual(find(env),['surface:1: "REGISTRY"','surface:2: "REGISTRY"','surface:3: "NUDGE"','surface:4: "NUDGE"']);
});

test("the gate reads the round coordinator's deployment and administration scripts, their CREATE2 configuration and every committed Robinhood manifest",async()=>{
  const {surfaces,markers}=await robinhoodSurfaces(),names=surfaces.map(surface=>surface.name);
  for(const name of ROUND_SCRIPTS)assert.ok(names.includes(name),name);
  for(const file of (await readdir(join(root,"deployments"))).filter(file=>/^robinhood-.+\.json$/.test(file)))assert.ok(names.includes(`deployments/${file}`),file);
  assert.deepEqual(surfaces.flatMap(surface=>legacyFindings(surface,markers)),[]);
  // A manifest that carried the epoch design's fields would fail.
  assert.deepEqual(legacyFindings({name:"m",text:JSON.stringify({registry:"0x01",epochImplementationCodeHash:"0x02",confirmations:1})},markers),
    ['m:1: "registry"','m:1: "epoch"','m:1: "confirmation"']);
});

test("an Arc owner address may be one signer of a Robinhood Safe whose threshold is at least 2, and nothing else in a Robinhood profile",()=>{
  const SAFE="0x00000000000000000000000000000000005aFE01",SIGNER="0x00000000000000000000000000000000005a1600",INTERIM="0x7ad78fc8097DFEA5c12DBb503D6EB6E60f34B40B";
  const profile=(patch:object={},safe:object={})=>({owner:SAFE,interimOwner:INTERIM,safe:{address:SAFE,owners:[ARC_TESTNET_OWNER,SIGNER],threshold:2,...safe},...patch});
  const findings=(entry:object)=>legacyFindings({name:"p",text:profileSurfaceText(entry,markers)},markers);
  // A signer of a 2 of 2 Safe: allowed, and read as the shared signer; the other signer, the Safe and the interim owner read as they are.
  const text=profileSurfaceText(profile(),markers);
  assert.deepEqual(findings(profile()),[]);
  assert.ok(text.includes(SHARED_SIGNER)&&text.includes(SIGNER)&&text.includes(SAFE)&&text.includes(INTERIM));
  assert.doesNotMatch(text,new RegExp(ARC_TESTNET_OWNER,"i"));
  // As owner, interim owner or the Safe's address it is found, and so is a signer of a Safe that one signature can move.
  assert.deepEqual(findings(profile({owner:ARC_TESTNET_OWNER})),[`p:2: Arc owner ${ARC_TESTNET_OWNER.toLowerCase()}`]);
  assert.deepEqual(findings(profile({interimOwner:ARC_MAINNET_OWNER})),[`p:3: Arc owner ${ARC_MAINNET_OWNER.toLowerCase()}`]);
  assert.deepEqual(findings(profile({},{address:ARC_TESTNET_OWNER})),[`p:5: Arc owner ${ARC_TESTNET_OWNER.toLowerCase()}`]);
  assert.deepEqual(findings(profile({},{threshold:1})),[`p:7: Arc owner ${ARC_TESTNET_OWNER.toLowerCase()}`]);
  // A profile without a Safe, or with its owners unknown, reads as it is.
  assert.equal(profileSurfaceText({owner:null},markers),JSON.stringify({owner:null},null,2));
  assert.deepEqual(findings(profile({},{owners:null})),[]);
});
