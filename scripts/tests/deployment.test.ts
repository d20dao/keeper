import {test} from "node:test";
import assert from "node:assert/strict";
import {mkdtemp,mkdir,writeFile,readFile,readdir} from "node:fs/promises";
import {spawnSync} from "node:child_process";
import {tmpdir} from "node:os";
import {join} from "node:path";
import {Wallet,SigningKey,toBeHex,keccak256} from "ethers";
import {loadDeployer,loadEnvValue} from "../lib/deployer-env.ts";
import {currentFee} from "../lib/gas.ts";
import {renderKeeperEnv} from "../lib/keeper-env.ts";
import {loadOrCreateOperator,runtimeCodeAt,INITIAL_OWNER} from "../lib/deployment.ts";
import {loadChain} from "../lib/chains.ts";

test("deployer loader accepts only matching authorized fields and never expands or leaks input",async()=>{
  const dir=await mkdtemp(join(tmpdir(),"d20-deployer-test-")),file=join(dir,"fixture.env");
  const wallet=new Wallet(toBeHex(42n,32)); // Public local fixture only.
  await writeFile(file,`UNRELATED_SECRET=never-import-this\nDEPLOYER_ADDRESS=${wallet.address.toLowerCase()}\nDEPLOYER_KEY='${wallet.privateKey.slice(2)}' # accepted hexadecimal\n`);
  assert.equal((await loadDeployer(file)).address,wallet.address);
  await writeFile(file,`DEPLOYER_ADDRESS=${wallet.address}\nDEPLOYER_KEY=secret-fixture-do-not-log\n`);
  await assert.rejects(loadDeployer(file),error=>error instanceof Error&&!error.message.includes("secret-fixture-do-not-log"));
  await writeFile(file,`DEPLOYER_ADDRESS=${new Wallet(toBeHex(43n,32)).address}\nDEPLOYER_KEY=${wallet.privateKey}\n`);
  await assert.rejects(loadDeployer(file),error=>error instanceof Error&&!error.message.includes(wallet.privateKey));
});

test("operator identity is bound to the chain owner it was created for",async()=>{
  const dir=await mkdtemp(join(tmpdir(),"d20-operator-owner-"));
  const deployer=new Wallet(toBeHex(45n,32)).address,multisig=new Wallet(toBeHex(46n,32)).address;
  const created=await loadOrCreateOperator(dir,deployer,multisig);
  assert.equal(created.owner,multisig);assert.equal(created.feeRecipient,multisig);
  assert.deepEqual(await loadOrCreateOperator(dir,deployer,multisig),created);
  await assert.rejects(loadOrCreateOperator(dir,deployer));
  assert.equal((await loadChain("arc-mainnet")).owner,"0xB57f656149749eff6b496dF090336491f977E744");
  assert.equal((await loadChain("arc-testnet")).owner,INITIAL_OWNER);
});

test("missing referenced key fails closed without provisioning another identity",async()=>{
  const dir=await mkdtemp(join(tmpdir(),"d20-operator-missing-"));
  const deployer=new Wallet(toBeHex(44n,32)).address;
  const metadata={chainId:5042002,deployer,owner:INITIAL_OWNER,feeRecipient:INITIAL_OWNER,
    keeper:{address:new Wallet(toBeHex(45n,32)).address,keyFile:join(dir,"missing.key")},vrf:{keyFile:join(dir,"also-missing.key"),publicKey:["1","2"]}};
  await writeFile(join(dir,"operator.json"),JSON.stringify(metadata));
  const before=await readFile(join(dir,"operator.json"),"utf8");
  await assert.rejects(loadOrCreateOperator(dir,deployer));
  assert.equal(await readFile(join(dir,"operator.json"),"utf8"),before);
  assert.deepEqual(await readdir(dir),["operator.json"]);
});

test("deployment creates an operator identity only with prepare --new-operator, never over an existing one",async()=>{
  const dir=await mkdtemp(join(tmpdir(),"d20-new-operator-")),env=join(dir,"fixture.env"),wallet=new Wallet(toBeHex(50n,32)); // Public local fixture only.
  await writeFile(env,`DEPLOYER_ADDRESS=${wallet.address}
DEPLOYER_KEY=${wallet.privateKey}
`);
  // Every refusal comes before any RPC request; --directory keeps plans out of the repository.
  const run=(...args:string[])=>spawnSync(process.execPath,["scripts/create2-deploy.ts",...args,"--env",env,"--directory",join(dir,"plans")],{encoding:"utf8"});
  const mistyped=run("prepare","--operator-directory",join(dir,"mistyped"));
  assert.equal(mistyped.status,1);assert.match(mistyped.stderr,/Deployment stopped: No operator\.json in .*mistyped; check --operator-directory/);
  await assert.rejects(readdir(join(dir,"mistyped")));
  const existing=join(dir,"operator");await mkdir(existing);await writeFile(join(existing,"operator.json"),"{}");
  const again=run("prepare","--new-operator","--operator-directory",existing);
  assert.equal(again.status,1);assert.match(again.stderr,/already holds an operator identity; rerun without --new-operator/);
  assert.deepEqual(await readdir(existing),["operator.json"]);
  assert.match(run("registry-plan","--new-operator","--operator-directory",existing).stderr,/--new-operator applies only to prepare/);
});

test("existing operator metadata cannot reuse a transaction key for VRF",async()=>{
  const dir=await mkdtemp(join(tmpdir(),"d20-operator-separation-")),file=join(dir,"public-fixture.key");
  const wallet=new Wallet(toBeHex(46n,32)),deployer=new Wallet(toBeHex(47n,32)).address;
  const point=SigningKey.computePublicKey(wallet.privateKey,false);
  await writeFile(file,wallet.privateKey);
  await writeFile(join(dir,"operator.json"),JSON.stringify({chainId:5042002,deployer,owner:INITIAL_OWNER,feeRecipient:INITIAL_OWNER,
    keeper:{address:wallet.address,keyFile:file},vrf:{keyFile:file,publicKey:[BigInt("0x"+point.slice(4,68)).toString(),BigInt("0x"+point.slice(68)).toString()]}}));
  await assert.rejects(loadOrCreateOperator(dir,deployer));
  assert.equal(await readFile(file,"utf8"),wallet.privateKey);
});

test("chain selection uses the configured Arc network and rejects unknown entries",async()=>{
  const arc=await loadChain("arc-testnet");
  assert.equal(arc.chainId,5042002);assert.equal(arc.nativeCurrency.decimals,18);
  await assert.rejects(loadChain("__proto__"));
  await assert.rejects(loadChain("unknown-network"));
});

test("operator fees follow current gas: twice the base fee plus the median tip, bounded by the chain cap",async()=>{
  const gwei=10n**9n;
  const source=(base:bigint|null,rewards?:bigint[])=>({getBlock:async()=>({baseFeePerGas:base}),
    send:async()=>rewards?{reward:rewards.map(value=>["0x"+value.toString(16)])}:Promise.reject(new Error("no history"))});
  assert.deepEqual(await currentFee(source(42n*gwei,[5n*gwei,1n*gwei,4n*gwei]),2000n*gwei),{baseFee:42n*gwei,tip:4n*gwei,maxFee:88n*gwei});
  assert.equal((await currentFee(source(42n*gwei),2000n*gwei)).tip,gwei); // Unavailable history tips the minimum.
  assert.equal((await currentFee(source(42n*gwei,[100n,200n,300n]),2000n*gwei)).tip,gwei);
  assert.equal((await currentFee(source(42n*gwei,[900n*gwei]),2000n*gwei)).tip,50n*gwei);
  assert.equal((await currentFee(source(1500n*gwei,[gwei]),2000n*gwei)).maxFee,2000n*gwei); // Capped while base fee plus tip still fits.
  await assert.rejects(currentFee(source(2000n*gwei,[gwei]),2000n*gwei),/exceeds the chain fee cap/);
  await assert.rejects(currentFee(source(null,[gwei]),2000n*gwei),/no base fee/);
});

test("named operator settings are read one at a time and never leak into errors",async()=>{
  const dir=await mkdtemp(join(tmpdir(),"d20-env-value-")),file=join(dir,"fixture.env");
  await writeFile(file,`DEPLOYER_KEY=never-read-this
DAO_TREASURY="0xB57f656149749eff6b496dF090336491f977E744" # Safe
EMPTY=
`);
  assert.equal(await loadEnvValue(file,"DAO_TREASURY"),"0xB57f656149749eff6b496dF090336491f977E744");
  await assert.rejects(loadEnvValue(file,"EMPTY"),error=>error instanceof Error&&/EMPTY is not set/.test(error.message));
  await writeFile(file,`d20daoarcmainnetbot="123:lowercase-name"
`);
  assert.equal(await loadEnvValue(file,"d20daoarcmainnetbot"),"123:lowercase-name");
  await writeFile(file,`TG_BOT_TOKEN='unterminated-secret-value
`);
  await assert.rejects(loadEnvValue(file,"TG_BOT_TOKEN"),error=>error instanceof Error&&!error.message.includes("unterminated-secret-value"));
});

test("Docker keeper.env pins the deployment, keeps sending off and places secrets only in the output",async()=>{
  const chain=await loadChain("arc-mainnet"),hash=(n:number)=>"0x"+n.toString(16).padStart(64,"0");
  const deployment={chainId:5042,coordinator:"0xd20da0ff9087d053f0291524eac12aba1adbd945",coordinatorCodeHash:hash(1),protocolConfigurationHash:hash(2),
    coordinatorImplementationCodeHash:hash(3),epochImplementationCodeHash:hash(4)};
  const neon="postgresql://user:secret-password@host/db?sslmode=require&channel_binding=require";
  const text=renderKeeperEnv(chain,deployment,{privateRpcUrl:"https://arc-mainnet.example/v2/secret-key",telegramBotToken:"123:secret-token",telegramChatId:"-1001234",neonDb:neon});
  const settings=new Map(text.split(/\n/).filter(line=>/^[A-Z]/.test(line)).map(line=>[line.slice(0,line.indexOf("=")),line.slice(line.indexOf("=")+1)]));
  assert.equal(settings.get("RPC_URLS"),`'${[chain.rpcUrls[0],"https://arc-mainnet.example/v2/secret-key",...chain.rpcUrls.slice(1)].join(",")}'`);
  assert.equal(settings.get("COORDINATOR_ADDRESS"),"'0xd20DA0FF9087d053f0291524Eac12abA1ADBd945'");
  assert.equal(settings.get("SEND_TRANSACTIONS"),"'false'");
  assert.equal(settings.get("KEYS_VOLUME"),"d20dao-keys-arc-mainnet");
  assert.equal(settings.get("KEEPER_DB"),"'/var/lib/d20dao/keeper-arc-mainnet.sqlite'");
  assert.equal(settings.get("TELEGRAM_CHAT_ID"),"'-1001234'");assert.equal(settings.get("NEON_DB"),`'${neon}'`);
  assert.equal(settings.get("MAX_FEE_PER_GAS_WEI"),`'${chain.gas.maxFeePerGasWei}'`);
  assert.throws(()=>renderKeeperEnv(chain,{...deployment,chainId:5042002},{}),/another chain/);
  assert.throws(()=>renderKeeperEnv(chain,deployment,{telegramBotToken:"123:secret-token"}),error=>error instanceof Error&&!error.message.includes("secret-token"));
  assert.throws(()=>renderKeeperEnv(chain,deployment,{telegramBotToken:"123:secret-token",telegramChatId:"@channel"}),/numeric/);
  assert.throws(()=>renderKeeperEnv(chain,deployment,{neonDb:"postgresql://user:secret-password@host/db"}),error=>error instanceof Error&&!error.message.includes("secret-password"));
  assert.throws(()=>renderKeeperEnv(chain,deployment,{privateRpcUrl:"http://plain.example/secret-key"}),error=>error instanceof Error&&!error.message.includes("secret-key"));
  assert.equal(renderKeeperEnv(chain,deployment,{}).includes("TELEGRAM"),false);
  assert.equal(text.includes("KEEPER_ROLE"),false);
  // A follower differs only in its role, delay and the keys volume of its named instance.
  const follower=renderKeeperEnv(chain,deployment,{privateRpcUrl:"https://arc-mainnet.example/v2/secret-key",telegramBotToken:"123:secret-token",telegramChatId:"-1001234",neonDb:neon},"follower");
  const followerSettings=new Map(follower.split(/\n/).filter(line=>/^[A-Z]/.test(line)).map(line=>[line.slice(0,line.indexOf("=")),line.slice(line.indexOf("=")+1)]));
  assert.equal(followerSettings.get("KEEPER_ROLE"),"'follower'");assert.equal(followerSettings.get("FOLLOWER_DELAY_SECONDS"),"'20'");
  assert.equal(followerSettings.get("FOLLOWER_QUEUE_JOIN"),"'150'");assert.equal(followerSettings.get("PRIMARY_LIVENESS_SECONDS"),"'10'");
  assert.equal(followerSettings.get("KEYS_VOLUME"),"d20dao-arc-mainnet-follower-keys");
  for(const [name,value] of settings)if(name!=="KEYS_VOLUME")assert.equal(followerSettings.get(name),value,name);
  assert.equal(followerSettings.has("TELEGRAM_COMMANDS"),false);
  assert.throws(()=>renderKeeperEnv(chain,deployment,{},"backup" as never),/primary or follower/);
});

test("implementation runtime hashes embed the UUPS self address and reproduce the recorded registry pins",async()=>{
  const deployed=JSON.parse(await readFile("test/fixtures/epoch-entropy-deployed-640b60c.json","utf8"));
  const testnetCode=JSON.parse(await readFile("test/fixtures/epoch-entropy-deployed-96cc722.json","utf8"));
  const mainnet=JSON.parse(await readFile("deployments/arc-mainnet.json","utf8")),testnet=JSON.parse(await readFile("deployments/arc-testnet.json","utf8"));
  // A manifest runs an implementation now or records it in implementationUpgrades as one its registry upgraded from.
  const recorded=(manifest:any,implementation:string,codeHash:string)=>
    (manifest.epochImplementation===implementation&&manifest.epochImplementationCodeHash===codeHash)||
    (manifest.implementationUpgrades??[]).some((entry:any)=>entry.contract==="registry"&&(
      (entry.implementation===implementation&&entry.implementationCodeHash===codeHash)||
      (entry.previousImplementation===implementation&&entry.previousImplementationCodeHash===codeHash)));
  // The 640b60c creation bytecode, placed at its live address, hashes to the pin both networks recorded before
  // their recipe-registry upgrades; 96cc722 was recorded from Arc Testnet with eth_getCode.
  assert.equal(keccak256(runtimeCodeAt(deployed.deployedBytecode,deployed.immutableReferences,deployed.implementation)),deployed.runtimeCodeHash);
  assert.ok(recorded(mainnet,deployed.implementation,deployed.runtimeCodeHash),"640b60c is Arc Mainnet's registry implementation or one it upgraded from");
  assert.ok(recorded(testnet,deployed.implementation,deployed.runtimeCodeHash),"640b60c is Arc Testnet's registry implementation or one it upgraded from");
  assert.equal(keccak256(testnetCode.deployedBytecode),testnetCode.runtimeCodeHash);
  assert.ok(recorded(testnet,testnetCode.implementation,testnetCode.runtimeCodeHash),"96cc722 is Arc Testnet's registry implementation or one it upgraded from");
  assert.notEqual(keccak256(runtimeCodeAt(deployed.deployedBytecode,deployed.immutableReferences,new Wallet(toBeHex(48n,32)).address)),deployed.runtimeCodeHash);
  const [references]=Object.values(deployed.immutableReferences) as Array<Array<{start:number;length:number}>>;
  assert.throws(()=>runtimeCodeAt(deployed.deployedBytecode,{...deployed.immutableReferences,other:references},deployed.implementation),/exactly one immutable/);
  assert.throws(()=>runtimeCodeAt(deployed.deployedBytecode,{self:[{start:references[0].start,length:20}]},deployed.implementation),/Invalid immutable reference/);
});
