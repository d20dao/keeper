import {test} from "node:test";
import assert from "node:assert/strict";
import {createHash} from "node:crypto";
import {readFile} from "node:fs/promises";
import {fileURLToPath} from "node:url";
import {loadChain,parseChain} from "../lib/chains.ts";
import {currentFee,tipBounds} from "../lib/gas.ts";
import {arcMarkers,profileSurfaceText} from "../lib/robinhood-profiles.ts";

const gwei=10n**9n;
const fingerprint=(value:unknown)=>createHash("sha256").update(JSON.stringify(value)).digest("hex");
const profiles=async()=>JSON.parse(await readFile(new URL("../../chains.json",import.meta.url),"utf8"));
const source=(base:bigint|null,rewards?:bigint[])=>({getBlock:async()=>({baseFeePerGas:base}),
  send:async()=>rewards?{reward:rewards.map(value=>["0x"+value.toString(16)])}:Promise.reject(new Error("no history"))});
const OPTIONAL=["blockSource","finality","stateHorizonBlocks","rpcOrder","keeper","ownerNote","interimOwner","interimOwnerNote","safe"],
  OPTIONAL_GAS=["minTipWei","maxTipWei","deployGasPolicy","maxDeployGas","maxTxGasLimit"];
// An owner for a profile that has none, where a test needs one: no chain's real owner. The lower-case spelling is not checksummed.
const STAND_IN_OWNER="0x00000000000000000000000000000000000000A1";

test("the Arc profiles are exactly the reviewed ones and carry none of the optional fields",async()=>{
  const chains=await profiles();
  // Fingerprints of each entry's JSON as it stood when keeper 0.4.1 was released.
  assert.equal(fingerprint(chains["arc-testnet"]),"6be12b1962c5fc7ddc8fb5657506175c840842aa7ce6cb69e160ed84d8ad03d3");
  assert.equal(fingerprint(chains["arc-mainnet"]),"dfac9e193256008702802cadf97f28e1cd3b74ad7730c47d0a7b9e01f83bad8d");
  for(const key of ["arc-testnet","arc-mainnet"]){
    const chain=await loadChain(key);
    for(const field of OPTIONAL)assert.equal(field in chain,false,`${key} has ${field}`);
    for(const field of OPTIONAL_GAS)assert.equal(field in chain.gas,false,`${key} has gas.${field}`);
    // No profile means today's tips: 1 to 50 gwei.
    assert.deepEqual(tipBounds(chain.gas),{minTip:gwei,maxTip:50n*gwei});
  }
});

// Both Robinhood profiles name the account that owns the set on chain: the deployer account. An operator's private owner overlay
// (deployments/private/<chain>.owner.json, which git ignores) may set the owner, a Safe and an interim owner over the committed profile; the
// tests never read the operator's, and use a fixture of stand-in addresses.
process.env.D20_PRIVATE_PROFILE_DIR=fileURLToPath(new URL("./fixtures/no-private-profile/",import.meta.url));
const TESTNET_OWNER="0x7ad78fc8097DFEA5c12DBb503D6EB6E60f34B40B",MAINNET_OWNER=TESTNET_OWNER;
const OVERLAY_DIR=fileURLToPath(new URL("./fixtures/private-profile/",import.meta.url));
const OVERLAY_SAFE={address:"0x00000000000000000000000000000000005afE77",
  owners:["0x00000000000000000000000000000000005a1601","0x00000000000000000000000000000000005A1602"],threshold:2};
const common={testnet:true,
  nativeCurrency:{name:"Ether",symbol:"ETH",decimals:18},compilerEvmTarget:"cancun",
  create2:{factory:"0x4e59b44847b379578588920cA78FbF26c0B4956C",codeHash:"0x2fa86add0aed31f33a762c9d88e807c475bd51d0f52bd0955754b2608f7e4989"},
  blockSource:"arbitrum-l2",finality:"soft",stateHorizonBlocks:6000,reviewedOn:"2026-10-03",documentation:"https://docs.robinhood.com/chain/connecting"};
const gas={blockGasLimit:32000000,maxGas:13000000,maxFeePerGasWei:"3000000000",cancelMaxFeePerGasWei:"3500000000",maxTxCostWei:"5000000000000000",
  minTipWei:"0",maxTipWei:"0",deployGasPolicy:"estimate",maxDeployGas:15000000,maxTxGasLimit:32000000};
const expected:Record<string,object>={
  "robinhood-testnet":{...common,owner:TESTNET_OWNER,name:"Robinhood Chain Testnet",chainId:46630,rpcUrls:["https://rpc.testnet.chain.robinhood.com"],
    explorerUrl:"https://explorer.testnet.chain.robinhood.com",gas:{...gas,documentedMinimumBaseFeeWei:"10000000"},expectedBlockMs:285},
  "robinhood-mainnet":{...common,testnet:false,owner:MAINNET_OWNER,name:"Robinhood Chain",chainId:4663,
    rpcUrls:["https://rpc.mainnet.chain.robinhood.com"],explorerUrl:"https://robinhoodchain.blockscout.com",gas:{...gas,documentedMinimumBaseFeeWei:"20000000"},expectedBlockMs:100},
};

test("the Robinhood profiles hold the reviewed values and only public endpoints",async()=>{
  for(const [key,want] of Object.entries(expected)){
    // rpcOrder and the keeper settings are pinned where keeper.env is generated (keeper-env.test.ts).
    const {key:loaded,rpcOrder,keeper,ownerNote,...chain}=await loadChain(key);
    assert.equal(loaded,key);
    assert.deepEqual(chain,want,key);
    assert.ok(rpcOrder!==undefined&&keeper!==undefined,`${key} carries its keeper settings`);
    // Each profile names its owner in a short, factual note.
    assert.ok((ownerNote??"").length>0,key);
    assert.doesNotMatch(ownerNote??"",/decision|until|interim|planned/i,key);
    for(const url of chain.rpcUrls){
      const parsed=new URL(url);
      assert.ok(parsed.protocol==="https:"&&parsed.pathname==="/"&&!parsed.search&&!parsed.username&&!parsed.password,`${key} lists a public endpoint without keys: ${url}`);
    }
  }
  // A provider URL carries its key in the path, so none belongs in these entries.
  const chains=await profiles();
  for(const key of Object.keys(expected))assert.doesNotMatch(JSON.stringify(chains[key]),/alchemy|drpc|quicknode|\/v2\/|api[_-]?key/i,key);
});

test("the Robinhood fee caps keep the margins the keeper checks at startup",async()=>{
  for(const key of Object.keys(expected)){
    const {gas}=await loadChain(key),cap=BigInt(gas.maxFeePerGasWei),cancel=BigInt(gas.cancelMaxFeePerGasWei),minimum=cap*9n/8n+1n;
    assert.ok(cancel>=minimum,`${key}: a cancellation must outbid a fulfillment by 12.5% and 1 wei`);
    // Nonce recovery on Arbitrum costs up to 100,000 gas with the parent-chain share.
    assert.ok(minimum*100_000n<=BigInt(gas.maxTxCostWei),`${key}: the transaction cost cap funds a recovery`);
  }
});

test("operator fees on both Robinhood profiles pay no tip: twice the base fee, bounded by the chain cap",async()=>{
  for(const key of Object.keys(expected)){
    const {gas}=await loadChain(key),cap=BigInt(gas.maxFeePerGasWei),bounds=tipBounds(gas),base=key.endsWith("testnet")?10_000_000n:23_754_000n;
    assert.deepEqual(bounds,{minTip:0n,maxTip:0n});
    assert.deepEqual(await currentFee(source(base,[0n,0n,0n]),cap,bounds),{baseFee:base,tip:0n,maxFee:2n*base},key);
    assert.equal((await currentFee(source(base),cap,bounds)).tip,0n,"history unavailable");
    assert.equal((await currentFee(source(base,[7n*gwei,9n*gwei]),cap,bounds)).tip,0n,"a tip the chain ignores is not paid");
    assert.equal((await currentFee(source(cap-gwei),cap,bounds)).maxFee,cap,"capped while base fee still fits");
    await assert.rejects(currentFee(source(cap+1n),cap,bounds),/exceeds the chain fee cap/);
    await assert.rejects(currentFee(source(null,[0n]),cap,bounds),/no base fee/);
    // Without the profile's bounds the same call still pays the 1 gwei minimum, as every Arc call does.
    assert.equal((await currentFee(source(base,[0n]),cap)).tip,gwei);
  }
});

test("tip bounds must be wei amounts in order, and the optional profile fields are checked only when present",async()=>{
  assert.deepEqual(tipBounds({minTipWei:"0"}),{minTip:0n,maxTip:50n*gwei});
  assert.throws(()=>tipBounds({maxTipWei:"7"}),/exceeds/); // below the default minimum
  assert.throws(()=>tipBounds({minTipWei:"5",maxTipWei:"4"}),/exceeds/);
  // The median tip is clamped to the bounds, and unavailable history tips their minimum.
  const cap=100n*gwei,bounds={minTip:5n,maxTip:9n};
  assert.equal((await currentFee(source(gwei,[3n,3n,3n]),cap,bounds)).tip,5n);
  assert.equal((await currentFee(source(gwei,[7n]),cap,bounds)).tip,7n);
  assert.equal((await currentFee(source(gwei,[11n]),cap,bounds)).tip,9n);
  assert.equal((await currentFee(source(gwei),cap,bounds)).tip,5n);
  assert.equal((await currentFee(source(gwei),cap,{minTip:0n,maxTip:50n*gwei})).tip,0n);
  const base=(await profiles())["robinhood-testnet"];
  assert.equal(parseChain("kept",base).key,"kept");
  const bad=(patch:object,gasPatch:object={})=>()=>parseChain("x",{...base,...patch,gas:{...base.gas,...gasPatch}});
  assert.throws(bad({blockSource:"bridge"}),/block source/);
  assert.throws(bad({finality:"safe"}),/finality/);
  for(const horizon of [0,-1,1.5,"6000",null])assert.throws(bad({stateHorizonBlocks:horizon}),/state horizon/);
  for(const tip of ["-1","1.5","01",1,null])assert.throws(bad({},{minTipWei:tip}),/tip bounds/);
  assert.throws(bad({},{maxTipWei:"1e9"}),/tip bounds/);
  assert.throws(bad({},{minTipWei:"5",maxTipWei:"4"}),/exceeds/);
  assert.throws(bad({},{deployGasPolicy:"cheap"}),/deployment gas policy/);
  assert.throws(bad({},{maxDeployGas:0}),/deployment gas policy/);
  assert.throws(bad({},{maxDeployGas:undefined}),/deployment gas policy/); // "estimate" needs its bound
  assert.doesNotThrow(bad({},{deployGasPolicy:"fixed",maxDeployGas:undefined}));
  // The required fields are still required.
  assert.throws(()=>parseChain("x",null),/Invalid chain configuration/);
  assert.throws(()=>parseChain("x",{...base,gas:undefined}),/Invalid chain configuration/);
  assert.throws(()=>parseChain("x",{...base,owner:STAND_IN_OWNER.toLowerCase()}),/checksummed/);
  assert.throws(()=>parseChain("x",{...base,rpcUrls:["http://rpc.example"]}),/Invalid chain configuration/);
  await assert.rejects(loadChain("robinhood"),/Unknown chain/);
});

test("both Robinhood profiles name the deployer account, an operator's owner overlay is merged over them, and no Arc owner owns either set",async()=>{
  const arcTestnet=await loadChain("arc-testnet"),arcMainnet=await loadChain("arc-mainnet"),testnet=await loadChain("robinhood-testnet"),mainnet=await loadChain("robinhood-mainnet");
  for(const chain of [testnet,mainnet]){
    assert.equal(chain.owner,TESTNET_OWNER,chain.key);
    assert.match(chain.ownerNote??"",/^Deployer account$/,chain.key);
    for(const field of ["safe","interimOwner","interimOwnerNote"])assert.equal(field in chain,false,`${chain.key} has ${field}`);
  }
  const chains=await profiles();
  for(const key of ["robinhood-testnet","robinhood-mainnet"])assert.equal(chains[key].owner,TESTNET_OWNER,key);
  // The overlay: the owner fields it sets replace the committed ones, and the rest of the profile stands.
  process.env.D20_PRIVATE_PROFILE_DIR=OVERLAY_DIR;
  try{
    const overlaid=await loadChain("robinhood-mainnet");
    assert.equal(overlaid.owner,OVERLAY_SAFE.address);assert.deepEqual(overlaid.safe,OVERLAY_SAFE);assert.equal(overlaid.interimOwner,MAINNET_OWNER);
    assert.equal(overlaid.chainId,4663);assert.deepEqual(overlaid.rpcUrls,mainnet.rpcUrls);
    assert.equal((await loadChain("robinhood-testnet")).owner,TESTNET_OWNER,"a chain without an overlay is read as committed");
    process.env.D20_PRIVATE_PROFILE_DIR=fileURLToPath(new URL("./fixtures/private-profile-bad/",import.meta.url));
    await assert.rejects(loadChain("robinhood-mainnet"),/an owner overlay sets only owner, ownerNote, interimOwner, interimOwnerNote, safe, not rpcUrls/);
  }finally{process.env.D20_PRIVATE_PROFILE_DIR=fileURLToPath(new URL("./fixtures/no-private-profile/",import.meta.url));}
  // No Arc owner owns a Robinhood set or receives its fees.
  const markers=arcMarkers(chains);
  for(const key of ["robinhood-testnet","robinhood-mainnet"]){
    const chain=key==="robinhood-testnet"?testnet:mainnet;
    for(const arc of [arcTestnet,arcMainnet]){
      assert.notEqual(String(chain.owner).toLowerCase(),String(arc.owner).toLowerCase(),`${key} is owned by ${arc.key}'s owner`);
      assert.doesNotMatch(profileSurfaceText(chains[key],markers),new RegExp(String(arc.owner),"i"),`${key} names ${arc.key}'s owner`);
    }
  }
  // Arc still names its owners.
  assert.equal(arcMainnet.owner,"0xB57f656149749eff6b496dF090336491f977E744");
  assert.equal(arcTestnet.owner,"0xcA35c280c5DF22958Adb74DD03911C8A0ec3eA03");
});
test("a profile's owner is a checksummed address or an explicit null: a missing, empty or misspelled owner is not read as undecided",async()=>{
  const base=(await profiles())["robinhood-testnet"];
  assert.equal(parseChain("x",{...base,owner:null}).owner,null);
  assert.equal(parseChain("x",{...base,owner:STAND_IN_OWNER}).owner,STAND_IN_OWNER);
  const {owner:_omitted,...without}=base;
  assert.throws(()=>parseChain("x",without),/checksummed address, or null/);
  for(const bad of [undefined,"",0,false,[],{},"null",STAND_IN_OWNER.toLowerCase(),`${STAND_IN_OWNER} `,"0x1234"])
    assert.throws(()=>parseChain("x",{...base,owner:bad}),Error,JSON.stringify(bad));
  assert.throws(()=>parseChain("x",{...base,ownerNote:5}),/owner note/);
  assert.equal(parseChain("x",{...base,ownerNote:"a reason"}).ownerNote,"a reason");
});

test("a production profile's Safe, interim owner and transaction gas limit are checked when present",async()=>{
  const base=(await profiles())["robinhood-mainnet"];
  const SAFE="0x00000000000000000000000000000000005aFE01",SIGNERS=["0x00000000000000000000000000000000000000A2","0x00000000000000000000000000000000000000A3"];
  const owned={...base,owner:SAFE,safe:{address:SAFE,owners:SIGNERS,threshold:2}};
  const chain=parseChain("x",{...owned,interimOwner:TESTNET_OWNER,interimOwnerNote:"why"});
  assert.deepEqual(chain.safe,{address:SAFE,owners:SIGNERS,threshold:2});assert.equal(chain.interimOwner,TESTNET_OWNER);
  // Owners unknown yet, and no interim owner, are both explicit.
  assert.equal(parseChain("x",{...owned,safe:{address:SAFE,owners:null,threshold:2}}).safe?.owners,null);
  assert.equal(parseChain("x",{...owned,interimOwner:null}).interimOwner,null);
  const bad=(patch:object,pattern:RegExp)=>assert.throws(()=>parseChain("x",{...owned,...patch}),pattern,JSON.stringify(patch));
  bad({safe:{address:SAFE,owners:SIGNERS}},/exactly address, owners and threshold/);
  bad({safe:{address:SAFE,owners:SIGNERS,threshold:2,modules:[]}},/exactly address, owners and threshold/);
  bad({safe:{address:SAFE.toLowerCase(),owners:SIGNERS,threshold:2}},/safe address must be a checksummed address/);
  for(const threshold of [1,0,2.5,"2"])bad({safe:{address:SAFE,owners:SIGNERS,threshold}},/threshold must be a whole number of at least 2/);
  for(const owners of [[SIGNERS[0]],[SIGNERS[0],SIGNERS[0]],[SIGNERS[0],SIGNERS[1].toLowerCase()],"none",[]])
    bad({safe:{address:SAFE,owners,threshold:2}},/owners must be distinct checksummed addresses/);
  bad({owner:SIGNERS[0]},/names a Safe has it as its owner/);
  bad({owner:null},/names a Safe has it as its owner/);
  bad({interimOwner:SAFE},/interim owner is not its Safe/);
  bad({interimOwner:TESTNET_OWNER.toLowerCase()},/interim owner must be a checksummed address/);
  bad({interimOwner:"0x"+"00".repeat(20)},/interim owner must be a checksummed address/);
  bad({interimOwnerNote:7},/interim owner note/);
  // The committed production profile passes as it is, without a Safe; an interim owner without a Safe has no one to hand the set to.
  assert.equal(parseChain("x",base).safe,undefined);
  assert.throws(()=>parseChain("x",{...base,interimOwner:TESTNET_OWNER}),/names the Safe that takes the set over/);
  // The chain's own per-transaction gas limit bounds the profile's caps.
  assert.equal(parseChain("x",{...base,gas:{...base.gas,maxTxGasLimit:32_000_000}}).gas.maxTxGasLimit,32_000_000);
  for(const gas of [{maxTxGasLimit:0},{maxTxGasLimit:"32000000"},{maxTxGasLimit:14_000_000},{maxTxGasLimit:15_000_000,maxGas:15_000_001}])
    assert.throws(()=>parseChain("x",{...base,gas:{...base.gas,...gas}}),/transaction gas limit/,JSON.stringify(gas));
});
