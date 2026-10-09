import {test} from "node:test";
import assert from "node:assert/strict";
import {mkdtemp,readdir,readFile,writeFile} from "node:fs/promises";
import {spawnSync} from "node:child_process";
import {tmpdir} from "node:os";
import {join} from "node:path";
import {fileURLToPath,pathToFileURL} from "node:url";
import {loadChain,requireEpochDesign,requireEvmBlockSource,requireOperable,requireOwner,requireRoundCoordinator,type Chain} from "../lib/chains.ts";
import {Stop} from "../lib/deployment.ts";

// The tests read chains.json as committed: an operator's private owner overlay (deployments/private) is never merged into them.
process.env.D20_PRIVATE_PROFILE_DIR=fileURLToPath(new URL("./fixtures/no-private-profile/",import.meta.url));
const root=fileURLToPath(new URL("../..",import.meta.url));
const ARBITRUM=/Robinhood deployments use the Arbitrum path, which is not implemented in this script yet/;
const OWNER=/is not set, so/;
// An owner for a profile that has none, where a test needs one: no chain's real owner.
const STAND_IN_OWNER="0x00000000000000000000000000000000000000A1";
/** The call must throw a Stop (which every script prints) whose message matches all of the patterns. */
const refusal=(...patterns:RegExp[])=>(error:unknown)=>error instanceof Stop&&patterns.every(pattern=>pattern.test(error.message));

test("a chain whose contracts read block numbers through the Arbitrum path is refused by its block source, and every Arc profile passes",async()=>{
  for(const key of ["arc-testnet","arc-mainnet"]){
    const arc=await loadChain(key);
    assert.doesNotThrow(()=>requireEvmBlockSource(arc,"x.ts"),key);
    assert.doesNotThrow(()=>requireOperable(arc,"x.ts"),key);
    assert.doesNotThrow(()=>requireOwner(arc,"x.ts"),key);
  }
  for(const key of ["robinhood-testnet","robinhood-mainnet"]){
    const chain=await loadChain(key);
    assert.equal(chain.blockSource,"arbitrum-l2");
    assert.throws(()=>requireEvmBlockSource(chain,"sweep-keeper.ts"),refusal(ARBITRUM,/sweep-keeper\.ts/,new RegExp(`chain ${key}\\b`)),key);
    assert.throws(()=>requireOperable(chain,"sweep-keeper.ts"),refusal(ARBITRUM,/sweep-keeper\.ts/),key);
  }
  // Only the EVM source passes: the explicit value and the default. Any other source is refused until a script handles it.
  const arc=await loadChain("arc-testnet");
  assert.doesNotThrow(()=>requireEvmBlockSource({...arc,blockSource:undefined},"x.ts"));
  assert.doesNotThrow(()=>requireEvmBlockSource({...arc,blockSource:"evm"},"x.ts"));
  assert.throws(()=>requireEvmBlockSource({...arc,blockSource:"arbitrum-l2"},"x.ts"),refusal(ARBITRUM));
  assert.throws(()=>requireEvmBlockSource({...arc,blockSource:"op-stack"} as unknown as Chain,"x.ts"),refusal(ARBITRUM));
});

test("a chain whose owner is not set is refused, and the guard narrows the owner to an address",async()=>{
  const arc=await loadChain("arc-mainnet"),undecided:Chain={...arc,key:"somewhere",owner:null};
  for(const run of [()=>requireOwner(undecided,"x.ts"),()=>requireOperable(undecided,"x.ts")])
    assert.throws(run,refusal(OWNER,/somewhere/,/x\.ts/,/chains\.json/));
  // Both Robinhood owners are decided (the testnet's deployer EOA, the production Safe) and pass; the production profile as it stood before its
  // owner was named, an explicit null, is still refused.
  const testnet=await loadChain("robinhood-testnet"),mainnet=await loadChain("robinhood-mainnet"),undecidedMainnet:Chain={...mainnet,owner:null};
  for(const chain of [testnet,mainnet])assert.doesNotThrow(()=>requireOwner(chain,"x.ts"),chain.key);
  assert.throws(()=>requireOwner(undecidedMainnet,"x.ts"),refusal(OWNER,/robinhood-mainnet\b/));
  // Both reasons are told together, the Arbitrum path first, and only the ones that apply: a decided Robinhood owner is refused for its Arbitrum path alone.
  assert.throws(()=>requireOperable(undecidedMainnet,"x.ts"),refusal(/Arbitrum path[\s\S]*is not set, so/));
  for(const chain of [testnet,mainnet])
    assert.throws(()=>requireOperable(chain,"x.ts"),(error:unknown)=>refusal(ARBITRUM)(error)&&!OWNER.test((error as Error).message),chain.key);
  assert.throws(()=>requireOperable({...arc,owner:null},"x.ts"),(error:unknown)=>refusal(OWNER)(error)&&!ARBITRUM.test((error as Error).message));
  // After the guard the owner is an address: this assignment compiles only because requireOperable narrows it.
  const chain:Chain=arc;
  requireOperable(chain,"x.ts");
  const owner:string=chain.owner;
  assert.equal(owner,arc.owner);
  // A script that implements the Arbitrum path drops the first check and keeps the second, which narrows the testnet's owner the same way.
  const ported:Chain=testnet;
  requireOwner(ported,"x.ts");
  const portedOwner:string=ported.owner;
  assert.match(portedOwner,/^0x[0-9a-fA-F]{40}$/);
});

// The scripts that send transactions or deploy, how each is started so that it reaches its chain profile, and what it prints first when it stops.
type Files={env:string;testnet:string;mainnet:string};
const GUARDED:Array<{script:string;prefix:string;args:(files:Files,chain:"testnet"|"mainnet")=>string[]}>=[
  {script:"create2-deploy.ts",prefix:"Deployment stopped",args:({env},chain)=>["prepare","--chain",`robinhood-${chain}`,"--env",env]},
  {script:"admin.ts",prefix:"Arc administration stopped",args:(files,chain)=>["keeper","--manifest",files[chain]]},
  {script:"request-smoke.ts",prefix:"Request smoke stopped",args:({env},chain)=>["--chain",`robinhood-${chain}`,"--env",env]},
  {script:"cost-smoke.ts",prefix:"Cost pilot stopped",args:(files,chain)=>["--manifest",files[chain],"--env",files.env]},
  {script:"sweep-keeper.ts",prefix:"Sweep stopped",args:(_,chain)=>["--chain",`robinhood-${chain}`]},
  {script:"batch-abort-live.ts",prefix:"Batch-abort live drill stopped",args:(_,chain)=>["--chain",`robinhood-${chain}`]},
  {script:"coordinator-upgrade-fork.ts",prefix:"Fork rehearsal stopped",args:(_,chain)=>["--chain",`robinhood-${chain}`]},
  {script:"registry-beacon-fork.ts",prefix:"Fork rehearsal stopped",args:(_,chain)=>["--chain",`robinhood-${chain}`]},
];
const NO_NETWORK=pathToFileURL(fileURLToPath(new URL("./fixtures/no-network.mjs",import.meta.url))).href;
const UNDECIDED_PRELOAD=pathToFileURL(fileURLToPath(new URL("./fixtures/undecided-owner.mjs",import.meta.url))).href;

test("every script that signs or deploys refuses both Robinhood profiles before any network access or key use",async()=>{
  const dir=await mkdtemp(join(tmpdir(),"d20-chain-guard-"));
  // The key file does not exist: a script that reached its key would stop with another message. The preload ends a process that opens a socket.
  const files:Files={env:join(dir,"does-not-exist.env"),testnet:join(dir,"testnet-manifest.json"),mainnet:join(dir,"mainnet-manifest.json")};
  await writeFile(files.testnet,JSON.stringify({network:"robinhood-testnet",chainId:46630}));
  await writeFile(files.mainnet,JSON.stringify({network:"robinhood-mainnet",chainId:4663}));
  // Both Robinhood owners are decided, so each refusal is the Arbitrum path alone; with the production owner read as undecided (the preload),
  // the owner reason follows it.
  for(const {script,prefix,args} of GUARDED)for(const [chain,undecided] of [["testnet",false],["mainnet",false],["mainnet",true]] as const){
    const preloads=undecided?[NO_NETWORK,UNDECIDED_PRELOAD]:[NO_NETWORK];
    const run=spawnSync(process.execPath,[...preloads.flatMap(preload=>["--import",preload]),`scripts/${script}`,...args(files,chain)],{cwd:root,encoding:"utf8"});
    const label=`${script} on robinhood-${chain}${undecided?" read as undecided":""}`;
    assert.equal(run.status,1,`${label}: ${run.stderr}`);
    assert.doesNotMatch(run.stderr,/NETWORK ACCESS/,`${label} reached the network first`);
    assert.match(run.stderr,new RegExp(`${prefix}: ${ARBITRUM.source} \\(${script.replace(".","\\.")}, chain robinhood-${chain}:`),label);
    if(undecided)assert.match(run.stderr,OWNER,label);
    else assert.doesNotMatch(run.stderr,OWNER,label);
    // Nothing a script was given is repeated in what it prints.
    assert.doesNotMatch(run.stderr,/does-not-exist/,label);
  }
});

test("the scripts that only read a deployment refuse an undecided owner and nothing else: an Arbitrum chain with an owner is theirs to read",async()=>{
  const dir=await mkdtemp(join(tmpdir(),"d20-chain-guard-read-"));
  const missing={env:join(dir,"does-not-exist.env"),deployment:join(dir,"no-deployment.json")};
  const run=(script:string,args:string[],preloads=[NO_NETWORK])=>
    spawnSync(process.execPath,[...preloads.flatMap(preload=>["--import",preload]),`scripts/${script}`,...args],{cwd:root,encoding:"utf8"});
  // The production profile read as undecided (the preload): both scripts refuse it for that alone.
  const network="robinhood-mainnet",manifest=join(dir,`${network}-manifest.json`),undecided=[NO_NETWORK,UNDECIDED_PRELOAD];
  await writeFile(manifest,JSON.stringify({network,chainId:4663}));
  const stopped=[run("keeper-env.ts",["--chain",network,"--env",missing.env,"--deployment",missing.deployment],undecided),
    run("failover-report.ts",["--manifest",manifest,"--from-block","1"],undecided)];
  for(const [index,script] of ["keeper-env.ts","failover-report.ts"].entries()){
    const result=stopped[index],label=`${script} on ${network}`;
    assert.equal(result.status,1,`${label}: ${result.stderr}`);
    assert.match(result.stderr,new RegExp(`The owner of ${network} is not set, so ${script.replace(".","\\.")} will not run against it`),label);
    assert.doesNotMatch(result.stderr,ARBITRUM,`${label} does not refuse an Arbitrum chain`);
    assert.doesNotMatch(result.stderr,/NETWORK ACCESS|does-not-exist/,label);
  }
  // With the owners chains.json names, still kept off the network, keeper-env.ts goes on and stops for the deployment record it was not given.
  for(const chain of ["robinhood-testnet","robinhood-mainnet"]){
    const owned=run("keeper-env.ts",["--chain",chain,"--env",missing.env,"--deployment",missing.deployment]);
    assert.equal(owned.status,1,`${chain}: ${owned.stderr}`);
    assert.match(owned.stderr,/no-deployment\.json/,chain);
    assert.doesNotMatch(owned.stderr,new RegExp(`${OWNER.source}|${ARBITRUM.source}`),chain);
  }
});

test("no chain profile is loaded by default: a missing key is refused, and a manifest that does not name its network is not read as Arc Testnet's",async()=>{
  for(const key of [undefined,""])await assert.rejects(loadChain(key as unknown as string),refusal(/^No chain named: name the chain profile to load/),String(key));
  const dir=await mkdtemp(join(tmpdir(),"d20-chain-guard-unnamed-")),manifest=join(dir,"manifest.json"),env=join(dir,"does-not-exist.env");
  // The manifest of a Robinhood testnet deployment that has lost its network field.
  await writeFile(manifest,JSON.stringify({chainId:46630}));
  const scripts=[["admin.ts","Arc administration stopped",["keeper","--manifest",manifest]],["cost-smoke.ts","Cost pilot stopped",["--manifest",manifest,"--env",env]],
    ["failover-report.ts","Failover report stopped",["--manifest",manifest,"--from-block","1"]]] as const;
  for(const [script,prefix,args] of scripts){
    const run=spawnSync(process.execPath,["--import",NO_NETWORK,`scripts/${script}`,...args],{cwd:root,encoding:"utf8"});
    assert.equal(run.status,1,`${script}: ${run.stderr}`);
    assert.match(run.stderr,new RegExp(`^${prefix}: No chain named`),script);
    assert.doesNotMatch(run.stderr,/NETWORK ACCESS|arc-testnet|does-not-exist/,script);
  }
});

test("create2-deploy.ts refuses a round coordinator's chain on its own, after its other guards and before any network access or key use",async()=>{
  const arc=await loadChain("arc-testnet");
  assert.doesNotThrow(()=>requireEpochDesign(arc,"x.ts"));
  assert.doesNotThrow(()=>requireEpochDesign({...arc,keeper:{coordinatorKind:"epoch"}},"x.ts"));
  for(const key of ["robinhood-testnet","robinhood-mainnet"]){
    const chain=await loadChain(key);
    assert.throws(()=>requireEpochDesign(chain,"create2-deploy.ts"),
      refusal(new RegExp(`^create2-deploy\\.ts deploys the epoch design, and ${key} runs a round coordinator, which this script does not deploy$`)),key);
    // With an owner and the Arbitrum refusal lifted, this refusal still stands.
    const ported:Chain={...chain,owner:STAND_IN_OWNER,blockSource:"evm"};
    assert.doesNotThrow(()=>requireOperable(ported,"x.ts"));
    assert.throws(()=>requireEpochDesign(ported,"x.ts"),refusal(/round coordinator/),key);
  }
  const source=await readFile(join(root,"scripts","create2-deploy.ts"),"utf8");
  const operable=source.search(/^\s*requireOperable\(chain,"create2-deploy\.ts"\);/m),design=source.search(/^\s*requireEpochDesign\(chain,"create2-deploy\.ts"\);/m);
  assert.ok(operable>0&&design>operable,"create2-deploy.ts calls requireEpochDesign after requireOperable");
  for(const early of ["new JsonRpcProvider(","loadDeployer(","loadOrCreateOperator(","privateDirectory(","readFile(\"config/service.json\""]){
    const at=source.indexOf(early);
    assert.ok(at<0||design<at,`create2-deploy.ts uses ${early} before it calls requireEpochDesign`);
  }
});

test("the round coordinator's scripts lift the Arbitrum refusal and nothing else: an undecided owner and a chain without a round coordinator are refused first",async()=>{
  for(const key of ["robinhood-testnet","robinhood-mainnet"]){
    const chain=await loadChain(key);
    assert.doesNotThrow(()=>requireRoundCoordinator(chain,"x.ts"),key);
    assert.throws(()=>requireRoundCoordinator({...chain,blockSource:"evm"},"x.ts"),
      refusal(/^x.ts deploys and administers the round contracts, which read block numbers through the Arbitrum path, and robinhood-\w+ does not use it/,new RegExp(key)),key);
    assert.throws(()=>requireRoundCoordinator({...chain,keeper:{...chain.keeper,coordinatorKind:"epoch"}},"x.ts"),refusal(/does not run one/),key);
  }
  for(const key of ["arc-testnet","arc-mainnet"]){
    const arc=await loadChain(key);
    assert.throws(()=>requireRoundCoordinator(arc,"x.ts"),refusal(/^x.ts deploys and administers the round coordinator, and arc-\w+ does not run one/,new RegExp(key)),key);
  }
  const dir=await mkdtemp(join(tmpdir(),"d20-chain-guard-round-")),env=join(dir,"does-not-exist.env");
  const manifest=async(network:string,chainId:number)=>{
    const file=join(dir,`${network}.json`);
    await writeFile(file,JSON.stringify({network,chainId,coordinatorContract:"D20VRFCoordinatorRobinhood",coordinator:"0x"+"11".repeat(20),coordinatorImplementation:"0x"+"22".repeat(20),
      coordinatorCodeHash:"0x"+"33".repeat(32),coordinatorImplementationCodeHash:"0x"+"44".repeat(32),storageLayoutHash:"0x"+"55".repeat(32)}));
    return file;
  };
  // The production profile read as undecided (the preload): both scripts refuse it for that.
  const undecided=/The owner of robinhood-mainnet is not set, so/,mainnetManifest=await manifest("robinhood-mainnet",4663);
  // The send case reads the production profile with an owner overlay (a fixture of stand-in addresses) that names an interim owner.
  const overlay={D20_PRIVATE_PROFILE_DIR:fileURLToPath(new URL("./fixtures/private-profile/",import.meta.url))};
  const cases:Array<[string,string,string[],RegExp,boolean?,Record<string,string>?]>=[
    ["deploy-robinhood.ts","Robinhood deployment stopped",["--chain","robinhood-mainnet","--env",env],undecided,true],
    ["admin-robinhood.ts","Robinhood administration stopped",["status","--manifest",mainnetManifest],undecided,true],
    // With the owners chains.json names, both scripts pass both guards on each Arbitrum chain and stop, before any network access, for the next
    // thing they were not given: on the production chain, the --mainnet acknowledgement.
    ["deploy-robinhood.ts","Robinhood deployment stopped",["--chain","robinhood-testnet"],/^Robinhood deployment stopped: Provide --env/],
    ["admin-robinhood.ts","Robinhood administration stopped",["set-keeper-bps","7000","--manifest",await manifest("robinhood-testnet",46630),"--send"],
      /^Robinhood administration stopped: --send needs --env/],
    ["deploy-robinhood.ts","Robinhood deployment stopped",["--chain","robinhood-mainnet","--env",env],
      /^Robinhood deployment stopped: robinhood-mainnet is a production chain: a deployment there also needs --mainnet$/m],
    ["admin-robinhood.ts","Robinhood administration stopped",["set-keeper-bps","7000","--manifest",mainnetManifest,"--send","--env",env],
      /^Robinhood administration stopped: robinhood-mainnet is a production chain: a send there, from its interim owner only, also needs --mainnet$/m,false,overlay],
  ];
  cases.push(["deploy-robinhood.ts","Robinhood deployment stopped",["--chain","arc-testnet","--env",env],/arc-testnet does not run one/]);
  cases.push(["admin-robinhood.ts","Robinhood administration stopped",["set-keeper","0x"+"66".repeat(20),"--manifest",await manifest("arc-testnet",5042002)],/arc-testnet does not run one/]);
  for(const [script,prefix,args,pattern,readUndecided,extra] of cases){
    const preloads=readUndecided?[NO_NETWORK,UNDECIDED_PRELOAD]:[NO_NETWORK];
    const run=spawnSync(process.execPath,[...preloads.flatMap(preload=>["--import",preload]),`scripts/${script}`,...args],{cwd:root,encoding:"utf8",env:{...process.env,...extra}});
    const label=`${script} ${args.join(" ")}`;
    assert.equal(run.status,1,`${label}: ${run.stderr}`);
    assert.match(run.stderr,new RegExp(`^${prefix}: `),label);assert.match(run.stderr,pattern,label);
    assert.doesNotMatch(run.stderr,new RegExp(`NETWORK ACCESS|does-not-exist|${ARBITRUM.source}`),label);
  }
});

// Entry points that load a chain profile and cannot send a transaction for an Arbitrum chain, and why. Anything else that loads one calls requireOperable.
const READ_ONLY:Record<string,string>={
  "network-preflight.ts":"probes the profile's public endpoints read-only and never touches a key or a deployment, so it also serves a chain whose owner is not decided",
  "testnet-wallets.mjs":"creates unfunded random wallets on a testnet and opens no connection",
};
// Scripts that read a deployment and send nothing: they call requireOwner, so a deployment on a chain with no decided owner is refused, but not requireOperable.
const OWNER_CHECKED:Record<string,string>={
  "failover-report.ts":"reads the logs of a deployment and sends nothing",
  "keeper-env.ts":"writes a keeper.env from a deployment record and sends nothing",
};
const FIXED_TO_ARC:Record<string,string>={
  "approved-upgrade-drill.ts":"loads Arc Mainnet's profile only for its CREATE2 factory and runs on a local chain",
};
// The scripts that implement the Arbitrum path, for the round coordinator: they call requireOwner and requireRoundCoordinator in place of requireOperable.
const ROUND:Record<string,string>={
  "deploy-robinhood.ts":"deploys the round coordinator's contract set",
  "admin-robinhood.ts":"sends the round coordinator owner's calls",
};
const SIGNING=/loadDeployer\(|signTransaction\(|sendTransaction\(|broadcastTransaction\(|eth_sendRawTransaction|eth_sendTransaction/;

test("a script that loads a chain profile guards it, unless it is listed as one that sends nothing or is fixed to Arc",async()=>{
  const names=(await readdir(join(root,"scripts"))).filter(name=>/\.(ts|mjs)$/.test(name)).sort(),loaders:string[]=[];
  for(const name of names){
    const source=await readFile(join(root,"scripts",name),"utf8");
    if(!/\bloadChain\s*\(/.test(source))continue;
    loaders.push(name);
    if(name in READ_ONLY){
      assert.doesNotMatch(source,SIGNING,`${name} is listed as read-only (${READ_ONLY[name]}) but signs or sends`);
      assert.doesNotMatch(source,/\brequire(Operable|Owner|EvmBlockSource)\(/,`${name} is listed as touching no deployment`);
      continue;
    }
    if(name in OWNER_CHECKED){
      assert.doesNotMatch(source,SIGNING,`${name} is listed as read-only (${OWNER_CHECKED[name]}) but signs or sends`);
      const owner=/^\s*requireOwner\(chain,\s*"([\w.-]+)"\);/m.exec(source);
      assert.ok(owner,`${name} reads a deployment but never calls requireOwner`);
      assert.equal(owner[1],name,`${name} guards under another script's name`);
      for(const early of ["new JsonRpcProvider(","loadEnvValue(","privateDirectory(","writeFile("]){
        const at=source.indexOf(early);
        assert.ok(at<0||owner.index<at,`${name} uses ${early} before it calls requireOwner`);
      }
      continue;
    }
    if(name in ROUND){
      // Both guards are statements naming the script, before the first network access or key use, and requireOperable is not called.
      const owner=/^\s*requireOwner\(chain,\s*"([\w.-]+)"\);/m.exec(source),round=/^\s*requireRoundCoordinator\(chain,\s*"([\w.-]+)"\);/m.exec(source);
      assert.ok(owner&&round,`${name} (${ROUND[name]}) must call requireOwner and requireRoundCoordinator`);
      assert.equal(owner[1],name,`${name} guards under another script's name`);assert.equal(round[1],name,`${name} guards under another script's name`);
      assert.doesNotMatch(source,/requireOperable\(/,`${name} implements the Arbitrum path and must not call requireOperable`);
      for(const early of ["new JsonRpcProvider(","loadDeployer(","new Wallet(","loadOrCreateOperator(","privateDirectory(","spawn(","fetch("]){
        const at=source.indexOf(early);
        assert.ok(at<0||(owner.index<at&&round.index<at),`${name} uses ${early} before its guards`);
      }
      continue;
    }
    if(name in FIXED_TO_ARC){
      const calls=[...source.matchAll(/\bloadChain\s*\(([^)]*)\)/g)].map(match=>match[1].trim());
      assert.ok(calls.length>0&&calls.every(argument=>/^"arc-(testnet|mainnet)"$/.test(argument)),`${name} must load an Arc profile by name only (${FIXED_TO_ARC[name]})`);
      continue;
    }
    // The call names its own script, comes before the first network access or key use, and is a statement rather than a mention.
    const call=/^\s*requireOperable\(chain,\s*"([\w.-]+)"\);/m.exec(source);
    assert.ok(call,`${name} loads a chain profile but never calls requireOperable`);
    assert.equal(call[1],name,`${name} guards under another script's name`);
    for(const early of ["new JsonRpcProvider(","loadDeployer(","new Wallet(","loadOrCreateOperator(","privateDirectory(","spawn(","fetch("]){
      const at=source.indexOf(early);
      assert.ok(at<0||call.index<at,`${name} uses ${early} before it calls requireOperable`);
    }
  }
  // A listed script that no longer loads a profile is a stale entry, and a scan that found too little would pass for nothing.
  const listed=[...Object.keys(READ_ONLY),...Object.keys(OWNER_CHECKED),...Object.keys(FIXED_TO_ARC),...Object.keys(ROUND)];
  for(const name of listed)assert.ok(loaders.includes(name),`${name} is listed but does not load a chain profile`);
  for(const {script} of GUARDED)assert.ok(loaders.includes(script),`${script} is tested as guarded but does not load a chain profile`);
  assert.deepEqual(loaders.filter(name=>!listed.includes(name)).sort(),GUARDED.map(({script})=>script).sort());
});
