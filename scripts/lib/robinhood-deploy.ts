// The round coordinator's contract set on a Robinhood chain, deployed through the chain's deterministic CREATE2 factory: the plan (every
// address, init code and expected runtime code, computed from the compiled artifacts), the read-only preflight, the deployment itself,
// journaled and resumable, the check of what is deployed, and the public manifest. scripts/deploy-robinhood.ts is its command line, and
// scripts/lib/robinhood-admin.ts reads what it records.
//
// Every contract goes through the factory, so its address follows from its init code and its salt (config/robinhood-create2.json) alone:
// - D20BeaconVerifier, the chain-neutral drand verifier, with the salt it was first deployed with: it lands at the same address on every
//   chain that has the factory. Its init code hash, runtime code hash and address are pinned in the config and checked.
// - D20VRFProofVerifier and LinkedRandomnessMapping are stateless and take no constructor arguments: one salt gives one address on every
//   chain, so the coordinator implementation that embeds both has the same code on every chain.
// - D20VRFCoordinatorRobinhood is linked to the library and given the proof verifier; UUPS embeds the implementation's own address. With
//   the same salt and helpers it has the same address and runtime code hash on every chain.
// - D20Proxy runs initialize in its constructor, so its address binds the owner, keys, pricing and beacon it starts with.
// A run finds each contract that is already at its address and checks its code instead of deploying it again; other code there is refused.
// Only reviewed code is deployed: the compiled proof verifier, mapping library and implementation must have the runtime code hashes that
// config/robinhood-create2.json pins (those of the reviewed testnet deployment; with the same salts and no constructor arguments in the
// helpers they are the same on every chain), and the command line deploys only from a clean, pushed commit (reviewedSource).
// A backup keeper, when one is given, is allowed by one setBackupKeeper call from the deployer after the creations, so the deployer must be
// the owner the set starts with: a test network's owner account, or a production chain's interim owner before its Safe takes the set over.
import {mkdir,open,readFile,writeFile} from "node:fs/promises";
import {execFileSync} from "node:child_process";
import {dirname} from "node:path";
import {fileURLToPath} from "node:url";
import {isDeepStrictEqual} from "node:util";
import {AbiCoder,Contract,Interface,Transaction,concat,dataLength,getAddress,getBytes,getCreate2Address,hexlify,id,keccak256,toUtf8Bytes,zeroPadValue,
  type Block,type JsonRpcApiProvider,type Wallet} from "ethers";
import type {OperableChain} from "./chains.ts";
import {currentFee,tipBounds} from "./gas.ts";
import {Stop} from "./deployment.ts";
import {accountKind,checkProfileSafe,requireProfileSafe} from "./safe.ts";

export const IMPLEMENTATION_SLOT="0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";
/** OpenZeppelin Initializable's namespaced slot: its low 64 bits are the initialized version, all ones on an implementation that disabled them. */
export const INITIALIZABLE_SLOT="0xf0c57e16840df040f15088dc2f81fe391c3923bec73e23a9662efc9c229c6a00";
/** The reviewed contracts' constants (contracts/robinhood/): BeaconBook's round lead, longest period and schedule lead, the coordinator's
 * pricing bounds and backup keeper limit. The deployment checks ROUND_LEAD on chain; the others are checked before a call is proposed. */
export const ROUND_LEAD=3n;
export const LIMITS={maxBeaconPeriod:10n,maxBeacons:256n,minScheduleLead:600n,maxMinFee:10n**16n,maxMultiplier:20,minOverhead:100_000,maxOverhead:2_000_000,
  bps:10_000,minRefundBps:5_000,maxBackupKeepers:4n} as const;
export const CONFIG_DOMAIN=id("D20_VRF_ROUND_CONFIG");
export const ROUND_BEACON_DOMAIN=id("D20_ROUND_BEACON_V1");
/** The coordinator's UnknownRequest() selector, which getRoundRequest reverts with for an id that was never requested. */
const UNKNOWN_REQUEST=id("UnknownRequest()").slice(0,10);
/** getRequest(uint256), a view the round coordinator does not have: a caller using another coordinator's ABI must get a revert. */
const FOREIGN_GET_REQUEST=id("getRequest(uint256)").slice(0,10);
const abi=AbiCoder.defaultAbiCoder();
const root=new URL("../../",import.meta.url);

// ---- compiled artifacts
const SOURCES={D20BeaconVerifier:"D20BeaconVerifier.sol",D20Proxy:"D20Proxy.sol",D20VRFProofVerifier:"robinhood/D20VRFProofVerifier.sol",
  LinkedRandomnessMapping:"robinhood/LinkedRandomnessMapping.sol",D20VRFCoordinatorRobinhood:"robinhood/D20VRFCoordinatorRobinhood.sol",
  BeaconBookStorageProbe:"robinhood/test/BeaconBookStorageProbe.sol"} as const;
export type ContractName=keyof typeof SOURCES;
type Places=Array<{start:number;length:number}>;
type LinkReferences=Record<string,Record<string,Places>>;
type Artifact={abi:any[];bytecode:string;deployedBytecode:string;linkReferences:LinkReferences;deployedLinkReferences:LinkReferences;
  immutableReferences:Record<string,Places>;buildInfoId:string;inputSourceName:string};
type Compiled={name:ContractName;artifact:Artifact;output:any;sources:Record<string,{ast:unknown}>};
const builds=new Map<string,any>();
/** A contract as Hardhat compiled it, checked against its build output so that a stale artifact is refused. */
export async function compiled(name:ContractName):Promise<Compiled>{
  let artifact:Artifact;
  try{artifact=JSON.parse(await readFile(new URL(`artifacts/contracts/${SOURCES[name]}/${name}.json`,root),"utf8"));}
  catch{throw new Stop(`No compiled ${name}; run npm run compile first`);}
  if(!builds.has(artifact.buildInfoId)){
    const build=JSON.parse(await readFile(new URL(`artifacts/build-info/${artifact.buildInfoId}.output.json`,root),"utf8"));
    builds.set(artifact.buildInfoId,build.output??build);
  }
  const build=builds.get(artifact.buildInfoId),output=build.contracts?.[artifact.inputSourceName]?.[name];
  if(!output||"0x"+output.evm.deployedBytecode.object!==artifact.deployedBytecode||"0x"+output.evm.bytecode.object!==artifact.bytecode)
    throw new Stop(`The ${name} artifact differs from its build output; recompile`);
  return {name,artifact,output,sources:build.sources};
}
/** Code with every LinkedRandomnessMapping placeholder replaced by the library's address. Any other library is refused. */
export function link(code:string,references:LinkReferences,library:string):string{
  const groups=Object.values(references).flatMap(libraries=>Object.entries(libraries));
  if(groups.length===0)return code;
  if(groups.length!==1||groups[0][0]!=="LinkedRandomnessMapping")throw new Stop("Expected exactly one linked library, LinkedRandomnessMapping");
  const address=getAddress(library).slice(2).toLowerCase();
  let hex=code.slice(2);
  for(const {start,length} of groups[0][1]){
    const at=start*2;
    if(length!==20||!/^__\$[0-9a-f]{34}\$__$/.test(hex.slice(at,at+40)))throw new Stop("A library placeholder is not where the artifact says");
    hex=hex.slice(0,at)+address+hex.slice(at+40);
  }
  if(hex.includes("__$"))throw new Stop("Code still holds an unlinked library placeholder");
  return "0x"+hex;
}
const nodeById=(node:unknown,wanted:number):{name?:string}|undefined=>{
  if(node===null||typeof node!=="object")return undefined;
  if((node as {id?:unknown}).id===wanted&&typeof (node as {nodeType?:unknown}).nodeType==="string")return node as {name?:string};
  for(const child of Object.values(node)){const found=nodeById(child,wanted);if(found)return found;}
  return undefined;
};
/** Runtime code with each immutable set: values names every immutable of the contract by its variable name (UUPS's __self included). */
export function withImmutables(contract:Compiled,code:string,values:Record<string,string>):string{
  const bytes=getBytes(code),used=new Set<string>();
  for(const [astId,places] of Object.entries(contract.artifact.immutableReferences)){
    const name=Object.values(contract.sources).map(source=>nodeById(source.ast,Number(astId))?.name).find(Boolean);
    if(!name||!(name in values))throw new Stop(`${contract.name} has an immutable (${name??astId}) this deployment does not set`);
    const word=getBytes(zeroPadValue(getAddress(values[name]),32));
    for(const {start,length} of places){
      if(length!==32||start+32>bytes.length)throw new Stop("Invalid immutable reference");
      bytes.set(word,start);
    }
    used.add(name);
  }
  for(const name of Object.keys(values))if(!used.has(name))throw new Stop(`${contract.name} has no immutable ${name}`);
  return hexlify(bytes);
}
/** A library's runtime code at its address. A library with a state-changing function starts with solc's call guard, a PUSH20 of zeros
 * that its constructor fills with its own address; one with view and pure functions only, as LinkedRandomnessMapping, has none and runs
 * its compiled code unchanged. */
export function libraryRuntime(deployedBytecode:string,address:string):string{
  const bytes=getBytes(deployedBytecode);
  if(bytes[0]!==0x73||bytes.length<21||bytes.subarray(1,21).some(byte=>byte!==0))return hexlify(bytes);
  bytes.set(getBytes(getAddress(address)),1);
  return hexlify(bytes);
}

// ---- storage layout
/** A contract's compiled storage layout in the shape of its baseline in storage-layout/ (scripts/check-storage-layout.mjs writes it). */
async function storageSnapshot(name:"D20VRFCoordinatorRobinhood"|"BeaconBookStorageProbe"){
  const {output}=await compiled(name),layout=output.storageLayout;
  if(!layout)throw new Stop(`The ${name} artifact has no storage layout`);
  const type=(key:string):Record<string,unknown>=>{
    const t=layout.types[key],result:Record<string,unknown>={encoding:t.encoding,label:t.label,numberOfBytes:t.numberOfBytes};
    for(const part of ["key","value","base"])if(t[part])result[part]=type(t[part]);
    if(t.members)result.members=t.members.map((field:any)=>({label:field.label,slot:field.slot,offset:field.offset,type:type(field.type)}));
    return result;
  };
  return {contract:name,openzeppelin:"5.6.1",storage:layout.storage.map((field:any)=>({label:field.label,slot:field.slot,offset:field.offset,type:type(field.type)}))};
}
/** The hash of the reviewed storage layouts of the coordinator and of BeaconBook's namespace, after checking that the compiled code has
 * exactly those layouts. The manifest records it; an upgrade to another layout must be approved by its hash. */
export async function storageLayoutHash():Promise<string>{
  const baselines=[];
  for(const name of ["D20VRFCoordinatorRobinhood","BeaconBookStorageProbe"] as const){
    const baseline=JSON.parse(await readFile(new URL(`storage-layout/${name}.json`,root),"utf8"));
    if(!isDeepStrictEqual(await storageSnapshot(name),baseline))
      throw new Stop(`The compiled storage layout of ${name} differs from its reviewed baseline storage-layout/${name}.json (npm run storage:check)`);
    baselines.push(baseline);
  }
  return keccak256(toUtf8Bytes(JSON.stringify(baselines)));
}

// ---- inputs
const HEX32=/^0x[0-9a-fA-F]{64}$/,ADDRESS=/^0x[0-9a-fA-F]{40}$/;
const isWhole=(value:unknown,min:number,max:number)=>Number.isSafeInteger(value)&&(value as number)>=min&&(value as number)<=max;
const exactKeys=(value:unknown,keys:string[],what:string)=>{
  if(value===null||typeof value!=="object"||Array.isArray(value))throw new Stop(`${what} must be a JSON object`);
  const have=Object.keys(value);
  if(have.length!==keys.length||keys.some(key=>!have.includes(key)))throw new Stop(`${what} must have exactly the fields ${keys.join(", ")}`);
  return value as Record<string,unknown>;
};
/** What a deployment takes from config/service.<chain>.json: the contracts, the beacon file, and the initial fee, keeper share and pricing. */
export type RoundService={beacon:string;minFee:bigint;keeperFeeBps:number;multiplier:number;overhead:number};
export async function loadRoundService(chainKey:string):Promise<RoundService>{
  const file=`config/service.${chainKey}.json`;
  let text:string;
  try{text=await readFile(new URL(file,root),"utf8");}catch{throw new Stop(`${file} could not be read`);}
  const profile=exactKeys(JSON.parse(text),["coordinator","proofVerifier","mappingLibrary","beacon","minFeeWei","keeperFeeBps","pricing"],file);
  for(const [field,contract] of [["coordinator","D20VRFCoordinatorRobinhood"],["proofVerifier","D20VRFProofVerifier"],["mappingLibrary","LinkedRandomnessMapping"]])
    if(profile[field]!==contract)throw new Stop(`${file}: ${field} must be ${contract}, the contract this script deploys`);
  const pricing=exactKeys(profile.pricing,["multiplier","overhead"],`${file} pricing`);
  if(typeof profile.beacon!=="string"||!/^config\/beacons\/[\w.-]+\.json$/.test(profile.beacon))throw new Stop(`${file}: beacon must name a file under config/beacons/`);
  if(typeof profile.minFeeWei!=="string"||!/^(0|[1-9]\d{0,30})$/.test(profile.minFeeWei)||BigInt(profile.minFeeWei)>LIMITS.maxMinFee)
    throw new Stop(`${file}: minFeeWei must be a whole number of wei of at most ${LIMITS.maxMinFee}`);
  if(!isWhole(profile.keeperFeeBps,0,LIMITS.bps))throw new Stop(`${file}: keeperFeeBps must be 0 to ${LIMITS.bps}`);
  if(!isWhole(pricing.multiplier,0,LIMITS.maxMultiplier))throw new Stop(`${file}: pricing.multiplier must be 0 to ${LIMITS.maxMultiplier}`);
  if(!isWhole(pricing.overhead,LIMITS.minOverhead,LIMITS.maxOverhead))throw new Stop(`${file}: pricing.overhead must be ${LIMITS.minOverhead} to ${LIMITS.maxOverhead}`);
  if(BigInt(profile.minFeeWei)===0n&&pricing.multiplier===0)throw new Stop(`${file}: a zero minimum fee needs a multiplier`);
  return {beacon:profile.beacon,minFee:BigInt(profile.minFeeWei),keeperFeeBps:profile.keeperFeeBps as number,multiplier:pricing.multiplier as number,overhead:pricing.overhead as number};
}
/** A beacon registration with its sample round, as config/beacons/*.json holds it and BeaconBook's registerBeacon takes it. */
export type BeaconFile={verifier:string;chainHash:string;publicKey:string;genesis:bigint;period:bigint;sampleRound:bigint;sampleSignature:string};
/** A beacon file, checked as registerBeacon will check it except for what needs the chain: the verifier's answers and the sample's time. */
export function parseBeaconFile(text:string,name:string):BeaconFile{
  let json:unknown;
  try{json=JSON.parse(text);}catch{throw new Stop(`${name} is not valid JSON`);}
  const b=exactKeys(json,["verifier","chainHash","publicKey","genesis","period","sampleRound","sampleSignature"],name);
  const hex=(value:unknown,bytes:number,field:string)=>{
    if(typeof value!=="string"||!new RegExp(`^0x[0-9a-fA-F]{${bytes*2}}$`).test(value))throw new Stop(`${name}: ${field} must be ${bytes} bytes of 0x-prefixed hex`);
    return value.toLowerCase();
  };
  if(typeof b.verifier!=="string"||!ADDRESS.test(b.verifier))throw new Stop(`${name}: verifier must be an address`);
  const chainHash=hex(b.chainHash,32,"chainHash");
  if(BigInt(chainHash)===0n)throw new Stop(`${name}: chainHash must not be zero`);
  if(!isWhole(b.genesis,1,2**40-1))throw new Stop(`${name}: genesis must be a unix time below 2^40`);
  if(!isWhole(b.period,1,Number(LIMITS.maxBeaconPeriod)))throw new Stop(`${name}: period must be 1 to ${LIMITS.maxBeaconPeriod} seconds`);
  if(!isWhole(b.sampleRound,1,Number.MAX_SAFE_INTEGER))throw new Stop(`${name}: sampleRound must be a round number`);
  return {verifier:getAddress(b.verifier.toLowerCase()),chainHash,publicKey:hex(b.publicKey,128,"publicKey"),genesis:BigInt(b.genesis as number),
    period:BigInt(b.period as number),sampleRound:BigInt(b.sampleRound as number),sampleSignature:hex(b.sampleSignature,64,"sampleSignature")};
}
/** A beacon file by its path: relative to the repository for the service profile's beacon (repository true), else as given. */
export async function loadBeaconFile(path:string,repository=false):Promise<BeaconFile>{
  let text:string;
  try{text=await readFile(repository?new URL(path,root):path,"utf8");}catch{throw new Stop(`The beacon file ${path} could not be read`);}
  return parseBeaconFile(text,path);
}
/** When the beacon's sample round was scheduled; registerBeacon refuses a sample scheduled after its block's timestamp. */
export const sampleTime=(b:BeaconFile)=>b.genesis+(b.sampleRound-1n)*b.period;
/** BeaconBook.beaconIdentity: keccak256(abi.encode(ROUND_BEACON_DOMAIN, verifier, chainHash, keccak256(publicKey), genesis, period)). */
export const beaconIdentity=(b:Pick<BeaconFile,"verifier"|"chainHash"|"publicKey"|"genesis"|"period">)=>keccak256(abi.encode(
  ["bytes32","address","bytes32","bytes32","uint64","uint64"],[ROUND_BEACON_DOMAIN,b.verifier,b.chainHash,keccak256(b.publicKey),b.genesis,b.period]));
/** registerBeacon's and initialize's BeaconRegistration tuple. */
export const registrationOf=(b:BeaconFile)=>[b.verifier,b.chainHash,b.publicKey,b.genesis,b.period,b.sampleRound,b.sampleSignature] as const;

export type StepName="beaconVerifier"|"proofVerifier"|"mappingLibrary"|"coordinatorImplementation"|"coordinator";
/** The deployment order, and the contract each step deploys. */
export const STEPS:ReadonlyArray<readonly [StepName,ContractName]>=[["beaconVerifier","D20BeaconVerifier"],["proofVerifier","D20VRFProofVerifier"],
  ["mappingLibrary","LinkedRandomnessMapping"],["coordinatorImplementation","D20VRFCoordinatorRobinhood"],["coordinator","D20Proxy"]];
/** The steps whose reviewed runtime code hash config/robinhood-create2.json pins, besides the beacon verifier's. */
export const REVIEWED_STEPS=["proofVerifier","mappingLibrary","coordinatorImplementation"] as const;
export type Create2Config=Record<StepName,{contract:string;salt:string}>&{beaconVerifier:{initCodeHash:string;runtimeCodeHash:string;address:string}}&
  Record<typeof REVIEWED_STEPS[number],{runtimeCodeHash:string}>;
/** config/robinhood-create2.json: each step's contract and salt, the beacon verifier's pinned init code hash, runtime code hash and address,
 * and the reviewed runtime code hash of the proof verifier, the mapping library and the coordinator implementation. */
export async function loadCreate2Config():Promise<Create2Config>{
  const file="config/robinhood-create2.json",config=exactKeys(JSON.parse(await readFile(new URL(file,root),"utf8")),STEPS.map(([step])=>step),file);
  for(const [step,contract] of STEPS){
    const fields=step==="beaconVerifier"?["contract","salt","initCodeHash","runtimeCodeHash","address"]:(REVIEWED_STEPS as readonly string[]).includes(step)?["contract","salt","runtimeCodeHash"]:["contract","salt"];
    const entry=exactKeys(config[step],fields,`${file} ${step}`);
    if(entry.contract!==contract)throw new Stop(`${file}: ${step} deploys ${contract}`);
    for(const field of ["salt","initCodeHash","runtimeCodeHash"])if(field in entry&&(typeof entry[field]!=="string"||!HEX32.test(entry[field] as string)))
      throw new Stop(`${file}: ${step}.${field} must be 32 bytes of hex`);
    if("address" in entry&&(typeof entry.address!=="string"||getAddress(entry.address)!==entry.address))throw new Stop(`${file}: ${step}.address must be a checksummed address`);
  }
  return config as unknown as Create2Config;
}

// ---- the plan
/** Who the coordinator starts with: the owner, the fee recipient, the primary keeper wallet and the VRF public key, and the backup keeper
 * wallet the deployment allows, if any. */
export type Roles={owner:string;feeRecipient:string;keeper:string;publicKey:readonly [bigint,bigint];backupKeeper?:string};
/** A production set deployed with its chain's interim owner: the Safe that is to take it over (the chain's owner). */
export type InterimOwnership={finalOwner:string};
export type Step={name:StepName;contract:ContractName;salt:string;initCode:string;initCodeHash:string;address:string;runtimeCodeHash:string;
  /** Steps whose contracts must exist before this one's creation can run (and be estimated). */
  needs:StepName[];
  /** The implementation's creation with the factory standing in for the proof verifier: the same size, for the preflight's estimate while
   * the proof verifier is not deployed yet (the constructor only asks that it has code). */
  probeInitCode?:string};
export type RoundPlan={network:string;chainId:number;factory:string;steps:Step[];step:Record<StepName,Step>;roles:Roles;service:RoundService;beacon:BeaconFile;
  beaconIdentity:string;keyHash:string;protocolConfigurationHash:string;storageLayoutHash:string;initializeData:string;runtimeCodeBytes:number;
  /** setBackupKeeper(backupKeeper, true), sent by the deployer once the proxy exists, when roles name a backup keeper. */
  backupKeeperData?:string;
  /** Set when the owner is the chain's interim owner, which hands the set to finalOwner later. */
  interim?:InterimOwnership};
/** Every address, init code and expected runtime code hash of the set, from the compiled artifacts; reads nothing from a chain. */
export async function planRoundSet(o:{chain:Pick<OperableChain,"key"|"chainId"|"create2">;service:RoundService;beacon:BeaconFile;create2:Create2Config;roles:Roles;
  interim?:InterimOwnership}):Promise<RoundPlan>{
  const {chain,service,beacon,create2}=o,factory=getAddress(chain.create2.factory);
  const named:Array<[string,string|undefined]>=[["owner",o.roles.owner],["fee recipient",o.roles.feeRecipient],["keeper",o.roles.keeper],["backup keeper",o.roles.backupKeeper],
    ["final owner",o.interim?.finalOwner]];
  for(const [role,address] of named)if(address!==undefined&&(!ADDRESS.test(address)||BigInt(address)===0n))throw new Stop(`The ${role} must be a nonzero address`);
  const checksum=(address:string)=>getAddress(address.toLowerCase());
  const roles:Roles={owner:checksum(o.roles.owner),feeRecipient:checksum(o.roles.feeRecipient),keeper:checksum(o.roles.keeper),publicKey:o.roles.publicKey,
    ...(o.roles.backupKeeper===undefined?{}:{backupKeeper:checksum(o.roles.backupKeeper)})};
  // A keeper wallet is a hot key: it is never the owner or the fee recipient, and a backup keeper is never the keeper.
  for(const [role,address] of [["keeper",roles.keeper],["backup keeper",roles.backupKeeper]] as const)
    if(address!==undefined&&(address===roles.owner||address===roles.feeRecipient))throw new Stop(`The ${role} ${address} is also the owner or the fee recipient: a keeper wallet needs its own key`);
  if(roles.backupKeeper===roles.keeper)throw new Stop(`The backup keeper ${roles.keeper} is the keeper: a backup keeper needs its own wallet`);
  const interim=o.interim===undefined?undefined:{finalOwner:checksum(o.interim.finalOwner)};
  if(interim&&(interim.finalOwner===roles.owner||roles.feeRecipient!==roles.owner))throw new Stop("An interim owner is the set's owner and fee recipient, and not the Safe that takes it over");
  const at=(salt:string,code:string)=>getCreate2Address(factory,salt,keccak256(code));
  const step=(name:StepName,contract:ContractName,initCode:string,runtime:(address:string)=>string,needs:StepName[]=[]):Step=>{
    const address=at(create2[name].salt,initCode);
    return {name,contract,salt:create2[name].salt,initCode,initCodeHash:keccak256(initCode),address,runtimeCodeHash:keccak256(runtime(address)),needs};
  };
  const plain=async(name:ContractName)=>{
    const c=await compiled(name);
    if(Object.keys(c.artifact.immutableReferences).length||Object.keys(c.artifact.linkReferences).length)throw new Stop(`${name} is expected to have no immutables and no libraries`);
    return c;
  };
  const verifier=await plain("D20BeaconVerifier"),pinned=create2.beaconVerifier;
  if(keccak256(verifier.artifact.bytecode)!==pinned.initCodeHash)
    throw new Stop(`The compiled D20BeaconVerifier's init code hash ${keccak256(verifier.artifact.bytecode)} is not the pinned ${pinned.initCodeHash}, so it would not land at ${pinned.address}`);
  const beaconVerifier=step("beaconVerifier","D20BeaconVerifier",verifier.artifact.bytecode,()=>verifier.artifact.deployedBytecode);
  if(beaconVerifier.address!==pinned.address||beaconVerifier.runtimeCodeHash!==pinned.runtimeCodeHash)
    throw new Stop(`The beacon verifier's salt and code give ${beaconVerifier.address} with runtime code hash ${beaconVerifier.runtimeCodeHash}, not the pinned ${pinned.address} and ${pinned.runtimeCodeHash}`);
  if(beacon.verifier!==beaconVerifier.address)throw new Stop(`The beacon file names the verifier ${beacon.verifier}, not the D20BeaconVerifier at ${beaconVerifier.address}`);
  const proof=await plain("D20VRFProofVerifier");
  const proofVerifier=step("proofVerifier","D20VRFProofVerifier",proof.artifact.bytecode,()=>proof.artifact.deployedBytecode);
  const library=await compiled("LinkedRandomnessMapping");
  if(Object.keys(library.artifact.immutableReferences).length||Object.keys(library.artifact.linkReferences).length)throw new Stop("LinkedRandomnessMapping is expected to have no immutables and no libraries");
  const mappingLibrary=step("mappingLibrary","LinkedRandomnessMapping",library.artifact.bytecode,address=>libraryRuntime(library.artifact.deployedBytecode,address));
  const coordinatorContract=await compiled("D20VRFCoordinatorRobinhood"),linked=link(coordinatorContract.artifact.bytecode,coordinatorContract.artifact.linkReferences,mappingLibrary.address);
  const runtime=link(coordinatorContract.artifact.deployedBytecode,coordinatorContract.artifact.deployedLinkReferences,mappingLibrary.address);
  const implementation=step("coordinatorImplementation","D20VRFCoordinatorRobinhood",concat([linked,abi.encode(["address"],[proofVerifier.address])]),
    address=>withImmutables(coordinatorContract,runtime,{__self:address,proofVerifier:proofVerifier.address}),["proofVerifier"]);
  implementation.probeInitCode=concat([linked,abi.encode(["address"],[factory])]);
  // Only reviewed code: the helpers and the implementation must be the code the reviewed deployment runs.
  for(const s of [proofVerifier,mappingLibrary,implementation]){
    const reviewed=create2[s.name as typeof REVIEWED_STEPS[number]].runtimeCodeHash;
    if(s.runtimeCodeHash!==reviewed)
      throw new Stop(`The compiled ${s.contract} would have runtime code hash ${s.runtimeCodeHash} at ${s.address}, not the reviewed ${reviewed} that config/robinhood-create2.json pins: only reviewed code is deployed`);
  }
  const params=[roles.publicKey,roles.owner,roles.feeRecipient,roles.keeper,service.keeperFeeBps,service.minFee,service.multiplier,service.overhead];
  const coordinatorFace=new Interface(coordinatorContract.artifact.abi);
  const initializeData=coordinatorFace.encodeFunctionData("initialize",[params,registrationOf(beacon)]);
  const proxy=await plain("D20Proxy");
  const coordinator=step("coordinator","D20Proxy",concat([proxy.artifact.bytecode,abi.encode(["address","bytes"],[implementation.address,initializeData])]),
    ()=>proxy.artifact.deployedBytecode,["coordinatorImplementation","beaconVerifier"]);
  const steps=[beaconVerifier,proofVerifier,mappingLibrary,implementation,coordinator];
  const identity=beaconIdentity(beacon);
  return {network:chain.key,chainId:chain.chainId,factory,steps,step:Object.fromEntries(steps.map(s=>[s.name,s])) as Record<StepName,Step>,roles,service,beacon,
    beaconIdentity:identity,keyHash:keccak256(abi.encode(["uint256[2]"],[roles.publicKey])),
    protocolConfigurationHash:keccak256(abi.encode(["bytes32","uint256[2]","address","uint256","bytes32"],[CONFIG_DOMAIN,roles.publicKey,roles.feeRecipient,service.minFee,identity])),
    storageLayoutHash:await storageLayoutHash(),initializeData,runtimeCodeBytes:dataLength(runtime),
    ...(roles.backupKeeper===undefined?{}:{backupKeeperData:coordinatorFace.encodeFunctionData("setBackupKeeper",[roles.backupKeeper,true])}),
    ...(interim===undefined?{}:{interim})};
}

// ---- reading the chain
/** A local node's JSON-RPC URL (http or https on 127.0.0.1, localhost or [::1], without credentials), for a rehearsal against a local chain in
 * place of the profile's public endpoint. Anything else is refused: a keyed endpoint does not belong on a command line. */
export function localRpcUrl(text:string):string{
  let url:URL;
  try{url=new URL(text);}catch{throw new Stop("--rpc-url is not a URL");}
  if(!["http:","https:"].includes(url.protocol)||!["127.0.0.1","localhost","[::1]"].includes(url.hostname)||url.username||url.password||url.search||url.hash)
    throw new Stop("--rpc-url takes a local node's URL only (http or https on 127.0.0.1, localhost or [::1], without credentials or query); the profile's endpoint is used otherwise");
  return url.href;
}
/** The chain the RPC serves is the profile's, its head is current, and (unless factory is false) the deterministic factory is there.
 * Returns the head block. */
export async function checkNetwork(provider:JsonRpcApiProvider,chain:Pick<OperableChain,"key"|"chainId"|"create2">,{factory=true}={}):Promise<Block>{
  const served=BigInt(await provider.send("eth_chainId",[]));
  if(served!==BigInt(chain.chainId))throw new Stop(`The RPC serves chain ${served}, not ${chain.key} (chain ${chain.chainId})`);
  const head=await provider.getBlock("latest");
  if(!head||Math.abs(Math.floor(Date.now()/1000)-head.timestamp)>120)throw new Stop("The RPC's latest block is more than two minutes from this computer's clock");
  if(factory&&keccak256(await provider.getCode(chain.create2.factory))!==chain.create2.codeHash)throw new Stop(`The deterministic factory ${chain.create2.factory} is missing or has other code`);
  return head;
}
/** The revert data of a failed call, when the RPC gave it. */
export function revertData(error:unknown):string|undefined{
  const failure=error as {data?:unknown;info?:{error?:{data?:unknown}};error?:{data?:unknown}}|null;
  const data=failure?.data??failure?.info?.error?.data??failure?.error?.data;
  return typeof data==="string"&&/^0x[0-9a-fA-F]*$/.test(data)?data:undefined;
}
/** Why a call failed, in one line: the custom error decoded by one of the interfaces when it can be, else the RPC's message. */
export function failureReason(error:unknown,...interfaces:Interface[]):string{
  const data=revertData(error);
  if(data&&data.length>=10)for(const face of interfaces){try{const parsed=face.parseError(data);if(parsed)return `${parsed.name}(${parsed.args.join(",")})`;}catch{/* next */}}
  if(data==="0x")return "reverted without a reason";
  const failure=error as {shortMessage?:unknown;message?:unknown}|null;
  return String(failure?.shortMessage??failure?.message??error).split(/\r?\n/)[0];
}
export async function coordinatorInterface():Promise<Interface>{return new Interface((await compiled("D20VRFCoordinatorRobinhood")).artifact.abi);}
/** What is at each step's address: nothing, or exactly the planned code. Other code is refused: CREATE2 binds an address to its init code,
 * so different code there means another factory, another chain or a different build. */
export async function inspect(provider:JsonRpcApiProvider,plan:RoundPlan):Promise<Record<StepName,boolean>>{
  const present={} as Record<StepName,boolean>;
  for(const s of plan.steps){
    const code=await provider.getCode(s.address);
    present[s.name]=code!=="0x";
    if(present[s.name]&&keccak256(code)!==s.runtimeCodeHash)
      throw new Stop(`${s.name} (${s.contract}) at ${s.address} has runtime code hash ${keccak256(code)}, not the expected ${s.runtimeCodeHash}; refusing to use it or deploy over it`);
  }
  return present;
}

// ---- verification
export type Verification={since:bigint;values:Record<string,string>};
/** Every check of a deployed set, run after a deployment and on every later run: each code hash and where the proxy points, the
 * configuration hash, the VRF key, the roles, the pricing, the beacon and its schedule, the round lead, and that an unknown request and a
 * view the coordinator does not have both revert. All failures are reported together. */
export async function verifyRoundSet(provider:JsonRpcApiProvider,plan:RoundPlan):Promise<Verification>{
  const face=await coordinatorInterface(),proxy=plan.step.coordinator.address,implementation=plan.step.coordinatorImplementation.address;
  const c=new Contract(proxy,face,provider),failures:string[]=[],values:Record<string,string>={};
  const expect=(name:string,actual:unknown,expected:unknown)=>{
    const shown=(value:unknown)=>Array.isArray(value)?value.map(String).join(","):String(value);
    values[name]=shown(actual);
    if(shown(actual)!==shown(expected))failures.push(`${name} is ${shown(actual)}, expected ${shown(expected)}`);
  };
  for(const s of plan.steps)expect(`${s.name}CodeHash`,keccak256(await provider.getCode(s.address)),s.runtimeCodeHash);
  expect("implementation",getAddress("0x"+(await provider.getStorage(proxy,IMPLEMENTATION_SLOT)).slice(-40)),implementation);
  expect("implementationInitializers",BigInt(await provider.getStorage(implementation,INITIALIZABLE_SLOT))&((1n<<64n)-1n),(1n<<64n)-1n);
  expect("proxiableUUID",await new Contract(implementation,face,provider).proxiableUUID(),IMPLEMENTATION_SLOT);
  expect("proofVerifier",await c.proofVerifier(),plan.step.proofVerifier.address);
  expect("protocolConfigurationHash",await c.protocolConfigurationHash(),plan.protocolConfigurationHash);
  expect("keyHash",await c.keyHash(),plan.keyHash);
  expect("publicKey",[await c.publicKeyX(),await c.publicKeyY()],plan.roles.publicKey);
  expect("owner",await c.owner(),plan.roles.owner);
  expect("pendingOwner",await c.pendingOwner(),"0x0000000000000000000000000000000000000000");
  expect("keeper",await c.keeper(),plan.roles.keeper);
  expect("backupKeeperCount",await c.backupKeeperCount(),plan.roles.backupKeeper===undefined?0n:1n);
  if(plan.roles.backupKeeper!==undefined)expect("backupKeeper",await c.isBackupKeeper(plan.roles.backupKeeper),true);
  expect("feeRecipient",await c.feeRecipient(),plan.roles.feeRecipient);
  expect("initialFeeRecipient",await c.initialFeeRecipient(),plan.roles.feeRecipient);
  expect("keeperFeeBps",await c.keeperFeeBps(),plan.service.keeperFeeBps);
  expect("pricing",Array.from(await c.pricing()),[plan.service.minFee,plan.service.multiplier,plan.service.overhead]);
  expect("initialMinFee",await c.initialMinFee(),plan.service.minFee);
  expect("refundBps",await c.refundBps(),LIMITS.bps);
  expect("roundLead",await c.ROUND_LEAD(),ROUND_LEAD);
  expect("beaconCount",await c.beaconCount(),1n);
  const b=await c.getBeacon(0);
  expect("beacon",[b.verifier,b.chainHash,b.publicKey,b.genesis,b.period],[plan.beacon.verifier,plan.beacon.chainHash,plan.beacon.publicKey,plan.beacon.genesis,plan.beacon.period]);
  expect("beaconIdentity",await c.beaconIdentity(0),plan.beaconIdentity);
  const [beaconId,since,nextBeaconId,nextFrom]=await c.beaconSchedule();
  expect("beaconSchedule",[beaconId,nextBeaconId,nextFrom],[0n,0n,0n]);
  values.beaconSince=String(since);
  const next=await c.nextRequestId() as bigint;
  expect("nextRequestId",next,1n);
  // getRoundRequest of an id never requested reverts UnknownRequest; getRequest(uint256), which this coordinator does not have, reverts.
  const unknown=await provider.call({to:proxy,data:face.encodeFunctionData("getRoundRequest",[next])}).then(()=>"returned",error=>revertData(error)?.slice(0,10)??"reverted");
  expect("getRoundRequest(unknown id)",unknown,UNKNOWN_REQUEST);
  const foreign=await provider.call({to:proxy,data:concat([FOREIGN_GET_REQUEST,abi.encode(["uint256"],[1n])])}).then(()=>"returned",()=>"reverts");
  expect("getRequest(uint256)",foreign,"reverts");
  if(failures.length)throw new Stop(`The deployed coordinator set at ${proxy} is not the planned one:\n  ${failures.join("\n  ")}`);
  return {since,values};
}

// ---- reviewed source
/** Where the deployed code came from, for the manifest: a clean commit on a remote branch, or, in a test's rehearsal only, why that was not checked. */
export type SourceRecord={commit:string;remoteBranches:string[]}|{commit:string|null;unreviewed:string};
/** reviewedSource's escape hatch for tests, and only for them: scripts/tests/fixtures/unreviewed-source.mjs sets this global when a test
 * preloads it (node --import) into deploy-robinhood.ts, whose tests run from a working tree under review, and in-process tests pass their own
 * SourceRecord to deployRoundSet. It is honored only for a deployment against a local node (--rpc-url), never against a chain's endpoint, and
 * no option of the command line sets it. */
export const UNREVIEWED_SOURCE_FOR_TESTS=Symbol.for("d20dao.tests.unreviewedSource");
/** The paths whose uncommitted changes refuse a deployment: the contracts, the configuration, the scripts that plan, check and send it, and
 * the package files that pin the compiler and libraries. */
export const REVIEWED_PATHS=["contracts","config","scripts","package.json","package-lock.json"] as const;
/** The source a deployment runs from, which must be reviewed: no uncommitted change under REVIEWED_PATHS (untracked files included), and
 * HEAD on a remote branch (git branch -r --contains, against the remote-tracking branches as last fetched; nothing is fetched). Refuses
 * otherwise. It runs git and reads no network. */
export function reviewedSource(o:{localRpc:boolean;repository?:string}):SourceRecord{
  const cwd=o.repository??fileURLToPath(root);
  const git=(args:string[])=>{
    try{return execFileSync("git",args,{cwd,encoding:"utf8",stdio:["ignore","pipe","ignore"],windowsHide:true});}
    catch{throw new Stop(`git ${args[0]} failed in ${cwd}: a deployment runs from a git checkout of the reviewed code`);}
  };
  if((globalThis as Record<symbol,unknown>)[UNREVIEWED_SOURCE_FOR_TESTS]===true&&o.localRpc){
    let commit:string|null=null;
    try{commit=git(["rev-parse","HEAD"]).trim();}catch{/* recorded as null */}
    return {commit,unreviewed:"not checked: a test's rehearsal against a local node (scripts/tests/fixtures/unreviewed-source.mjs)"};
  }
  const changes=git(["status","--porcelain","--untracked-files=all","--",...REVIEWED_PATHS]).split(/\r?\n/).filter(line=>line.trim()!=="");
  if(changes.length)throw new Stop(`The working tree has uncommitted changes under ${REVIEWED_PATHS.join(", ")} (${changes.slice(0,5).map(line=>line.slice(3)).join(", ")}${
    changes.length>5?` and ${changes.length-5} more`:""}): deploy only committed, reviewed code`);
  const commit=git(["rev-parse","HEAD"]).trim();
  const remoteBranches=git(["branch","-r","--contains",commit]).split(/\r?\n/).map(line=>line.trim()).filter(line=>line!==""&&!line.includes(" -> "));
  if(!remoteBranches.length)throw new Stop(`The commit ${commit} is on no remote branch (git branch -r --contains): push it for review, fetch, and deploy from the pushed commit`);
  return {commit,remoteBranches};
}

// ---- the deployment
/** The calls a deployment makes after the creations: setBackupKeeper from the deployer, when a backup keeper is given. */
type CallName="backupKeeper";
export type JournalStep=StepName|CallName;
type Receipt={step:JournalStep;address:string;hash:string;block:number;gasUsed:string;effectiveGasPriceWei:string;feeWei:string;status:number};
type JournalEntry={type:"run";at:string}|({type:"signed";step:JournalStep;address:string;hash:string;raw:string})|({type:"receipt"}&Receipt)|{type:"replaced";step:JournalStep;hash:string};
async function readJournal(path:string):Promise<JournalEntry[]>{
  let text:string;
  try{text=await readFile(path,"utf8");}catch(error){if((error as {code?:string})?.code==="ENOENT")return [];throw new Stop(`The journal ${path} could not be read`);}
  try{return text.split("\n").filter(line=>line.trim()!=="").map(line=>JSON.parse(line));}catch{throw new Stop(`The journal ${path} is not valid`);}
}
/** A setBackupKeeper call's gas cap, and its budget before the proxy exists to estimate it. */
const CALL_GAS_CAP=300_000n;
export type PlannedTransaction={step:StepName;contract:ContractName;address:string;salt:string;initCodeHash:string;expectedRuntimeCodeHash:string;
  status:"present"|"planned"|"deployed";transaction?:{from:string;to:string;value:"0";dataBytes:number;dataHash:string;estimatedGas?:string;gasLimit:string;note?:string};
  receipt?:Omit<Receipt,"step"|"address">};
export type PlannedCall={step:CallName;function:string;args:string[];status:"present"|"planned"|"sent";
  transaction:{from:string;to:string;value:"0";data:string;estimatedGas?:string;gasLimit:string;note?:string};receipt?:Omit<Receipt,"step"|"address">};
export type DeployReport={mode:"dry run"|"send";network:string;chainId:number;factory:string;deployer:string;owner:{address:string;kind:string};
  /** Whether the set starts with the chain's interim owner, and the Safe it is to move to with that Safe's strict check, which the transfer must pass. */
  ownership:{ownerTransferPending:boolean;finalOwner?:{address:string;kind:string;safeProblems:string[]}};
  feeRecipient:string;keeper:string;backupKeeper?:string;
  publicKey:[string,string];pricing:{minFeeWei:string;feeMultiplier:number;fulfillGasOverhead:number;keeperFeeBps:number};
  beacon:{verifier:string;chainHash:string;genesis:string;period:string;sampleRound:string;sampleTime:string;secondsBeforeHead:string;identity:string};
  preflight:{headBlock:number;headTime:number;baseFeeWei:string;maxFeePerGasWei:string;priorityFeeWei:string;deployerBalanceWei:string;budgetWei:string;
    implementationCreation:{initCodeBytes:number;runtimeCodeBytes:number;estimatedGas:string;probe:boolean}|{deployed:true}};
  source:SourceRecord;transactions:PlannedTransaction[];calls:PlannedCall[];verification?:Record<string,string>;manifest?:{path:string;written:boolean};next?:string};

/** The read-only preflight and, with send, the deployment of every missing contract, the backup keeper's call, the check of the set, and the
 * manifest. Without send nothing is signed, sent or written. A run after a partial or complete deployment sends only what is missing; a
 * manifest already written must describe the same deployment and is left as it is. source is what reviewedSource found, recorded in the manifest. */
export async function deployRoundSet(o:{provider:JsonRpcApiProvider;chain:OperableChain;plan:RoundPlan;deployer:string;wallet?:Wallet;send:boolean;
  journalPath:string;manifestPath:string;source:SourceRecord;onBroadcast?:(step:JournalStep,hash:string)=>void}):Promise<DeployReport>{
  const {provider,chain,plan,send}=o,deployer=getAddress(o.deployer);
  if(send&&(!o.wallet||getAddress(o.wallet.address)!==deployer))throw new Stop("Sending needs the deployer's wallet");
  if(plan.chainId!==chain.chainId||plan.network!==chain.key)throw new Stop("The plan is for another chain");
  if(chain.gas.deployGasPolicy!=="estimate"||chain.gas.maxDeployGas===undefined)throw new Stop(`${chain.key}'s profile must set gas.deployGasPolicy "estimate" and gas.maxDeployGas`);
  const head=await checkNetwork(provider,chain);
  const ownerKind=await accountKind(provider,plan.roles.owner);
  const ownership:DeployReport["ownership"]={ownerTransferPending:plan.interim!==undefined};
  if(plan.interim){
    // The set starts with the chain's interim owner, the deployer, an account without code, until its Safe takes it over.
    if(chain.testnet)throw new Stop(`${chain.key} is a test network: its owner owns its set from the deployment, without an interim owner`);
    if(!chain.interimOwner||plan.roles.owner!==chain.interimOwner)throw new Stop(`The set's owner ${plan.roles.owner} is not ${chain.key}'s interimOwner (${chain.interimOwner??"none"})`);
    if(plan.interim.finalOwner!==chain.owner)throw new Stop(`The set is to move to ${plan.interim.finalOwner}, not to ${chain.key}'s owner ${chain.owner}`);
    if(ownerKind!=="EOA")throw new Stop(`The interim owner ${plan.roles.owner} has code: an interim owner is an account without code`);
    if(plan.roles.owner!==deployer)throw new Stop(`The interim owner ${plan.roles.owner} is not the deployer ${deployer}: the deployer owns the set until the transfer`);
    // Reported, not required: the transfer to the Safe (admin-robinhood.ts transfer-ownership) refuses until the Safe passes it.
    const safe=await checkProfileSafe(provider,chain,chain.owner);
    ownership.finalOwner={address:chain.owner,kind:await accountKind(provider,chain.owner),safeProblems:safe.problems};
  }else if(!chain.testnet)await requireProfileSafe(provider,chain,plan.roles.owner,`The owner of ${chain.key}`);
  if(plan.roles.backupKeeper!==undefined&&plan.roles.owner!==deployer)
    throw new Stop(`The deployment allows the backup keeper with a setBackupKeeper call from the deployer ${deployer}, which is not the owner ${plan.roles.owner}: allow it with admin-robinhood.ts set-backup-keeper instead`);
  const sampled=sampleTime(plan.beacon);
  if(sampled>BigInt(head.timestamp))throw new Stop(`The beacon's sample round ${plan.beacon.sampleRound} is scheduled at ${sampled}, after the latest block time ${head.timestamp}: initialize would refuse it`);
  const existing=await readManifest(o.manifestPath);
  if(existing&&!sameDeployment(existing,manifestIdentity(plan)))
    throw new Stop(`${o.manifestPath} records another deployment (${differences(existing,manifestIdentity(plan)).join(", ")}); name another --manifest or check the inputs`);

  // Settle what an earlier run signed: each journaled transaction has a receipt, or a later one took its nonce.
  const journal=await readJournal(o.journalPath),receipts=new Map<JournalStep,Receipt>();
  for(const entry of journal)if(entry.type==="receipt"&&entry.status===1)receipts.set(entry.step,entry);
  const unsettled=journal.filter((entry):entry is Extract<JournalEntry,{type:"signed"}>=>entry.type==="signed"&&
    !journal.some(other=>(other.type==="receipt"||other.type==="replaced")&&other.hash===entry.hash));
  const settle:JournalEntry[]=[];
  for(const signed of unsettled){
    const receipt=await provider.getTransactionReceipt(signed.hash);
    if(receipt){
      const settled:Receipt={step:signed.step,address:signed.address,hash:signed.hash,block:receipt.blockNumber,gasUsed:String(receipt.gasUsed),
        effectiveGasPriceWei:String(receipt.gasPrice),feeWei:String(receipt.fee),status:receipt.status??0};
      settle.push({type:"receipt",...settled});
      if(settled.status===1)receipts.set(signed.step,settled);
      continue;
    }
    const {nonce,from}=Transaction.from(signed.raw);
    if(from&&await provider.getTransactionCount(from,"latest")>nonce){settle.push({type:"replaced",step:signed.step,hash:signed.hash});continue;}
    throw new Stop(`The journal ${o.journalPath} holds the ${signed.step} transaction ${signed.hash}, which has no receipt and whose nonce ${nonce} is not used yet: it may still be pending. Wait for it, or replace that nonce, before running again`);
  }

  const present=await inspect(provider,plan);
  const {baseFee,tip,maxFee}=await currentFee(provider,BigInt(chain.gas.maxFeePerGasWei),tipBounds(chain.gas));
  const maxGas=BigInt(chain.gas.maxDeployGas!),face=await coordinatorInterface(),proxy=plan.step.coordinator.address;
  const estimate=async(s:Step,data:string,what:string)=>{
    let gas:bigint;
    try{gas=await provider.estimateGas({from:deployer,to:plan.factory,data,value:0});}
    catch(error){throw new Stop(`Estimating ${what} failed: ${failureReason(error,face)}`);}
    if(gas>maxGas)throw new Stop(`${what} needs ${gas} gas, above the profile's maxDeployGas ${maxGas}`);
    const limit=gas*125n/100n;
    return {gas,limit:limit>maxGas?maxGas:limit};
  };
  const estimateCall=async(data:string,what:string)=>{
    let gas:bigint;
    try{gas=await provider.estimateGas({from:deployer,to:proxy,data,value:0});}
    catch(error){throw new Stop(`Estimating ${what} failed: ${failureReason(error,face)}`);}
    if(gas>CALL_GAS_CAP)throw new Stop(`${what} needs ${gas} gas, above its cap of ${CALL_GAS_CAP}`);
    const limit=gas*125n/100n;
    return {gas,limit:limit>CALL_GAS_CAP?CALL_GAS_CAP:limit};
  };
  const dataOf=(s:Step)=>concat([s.salt,s.initCode]);
  const transactions:PlannedTransaction[]=[];
  let budget=0n,creation:DeployReport["preflight"]["implementationCreation"]={deployed:true};
  for(const s of plan.steps){
    const entry:PlannedTransaction={step:s.name,contract:s.contract,address:s.address,salt:s.salt,initCodeHash:s.initCodeHash,expectedRuntimeCodeHash:s.runtimeCodeHash,status:present[s.name]?"present":"planned"};
    const receipt=receipts.get(s.name);
    if(present[s.name]&&receipt)entry.receipt={hash:receipt.hash,block:receipt.block,gasUsed:receipt.gasUsed,effectiveGasPriceWei:receipt.effectiveGasPriceWei,feeWei:receipt.feeWei,status:receipt.status};
    if(!present[s.name]){
      const data=dataOf(s),ready=s.needs.every(need=>present[need]);
      const transaction:NonNullable<PlannedTransaction["transaction"]>={from:deployer,to:plan.factory,value:"0",dataBytes:dataLength(data),dataHash:keccak256(data),gasLimit:String(maxGas)};
      if(ready){const {gas,limit}=await estimate(s,data,`the ${s.name} creation`);transaction.estimatedGas=String(gas);transaction.gasLimit=String(limit);budget+=limit;}
      else {transaction.note=`estimated when ${s.needs.filter(need=>!present[need]).join(" and ")} exist; budgeted at maxDeployGas`;budget+=maxGas;}
      entry.transaction=transaction;
    }
    transactions.push(entry);
  }
  const calls:PlannedCall[]=[];
  if(plan.roles.backupKeeper!==undefined){
    const allowed=present.coordinator&&await new Contract(proxy,face,provider).isBackupKeeper(plan.roles.backupKeeper) as boolean;
    const call:PlannedCall={step:"backupKeeper",function:"setBackupKeeper(address,bool)",args:[plan.roles.backupKeeper,"true"],status:allowed?"present":"planned",
      transaction:{from:deployer,to:proxy,value:"0",data:plan.backupKeeperData!,gasLimit:String(CALL_GAS_CAP)}};
    const receipt=receipts.get("backupKeeper");
    if(allowed&&receipt)call.receipt={hash:receipt.hash,block:receipt.block,gasUsed:receipt.gasUsed,effectiveGasPriceWei:receipt.effectiveGasPriceWei,feeWei:receipt.feeWei,status:receipt.status};
    if(!allowed){
      if(present.coordinator){const {gas,limit}=await estimateCall(call.transaction.data,"the backupKeeper call");call.transaction.estimatedGas=String(gas);call.transaction.gasLimit=String(limit);budget+=limit;}
      else {call.transaction.note=`estimated when the coordinator exists; budgeted at ${CALL_GAS_CAP} gas`;budget+=CALL_GAS_CAP;}
    }
    calls.push(call);
  }
  const implementation=plan.step.coordinatorImplementation;
  if(!present.coordinatorImplementation){
    // The creation-size check: the implementation's creation estimated now, through the factory, with its real init code once the proof
    // verifier exists and with the same-sized probe before. A chain that cannot take the code fails here, before anything is sent.
    const probe=!present.proofVerifier,data=probe?concat([implementation.salt,implementation.probeInitCode!]):dataOf(implementation);
    const {gas}=await estimate(implementation,data,"the coordinator implementation's creation");
    creation={initCodeBytes:dataLength(implementation.initCode),runtimeCodeBytes:plan.runtimeCodeBytes,estimatedGas:String(gas),probe};
  }
  budget*=maxFee;
  const balance=await provider.getBalance(deployer);
  const report:DeployReport={mode:send?"send":"dry run",network:plan.network,chainId:plan.chainId,factory:plan.factory,deployer,owner:{address:plan.roles.owner,kind:ownerKind},ownership,
    feeRecipient:plan.roles.feeRecipient,keeper:plan.roles.keeper,...(plan.roles.backupKeeper===undefined?{}:{backupKeeper:plan.roles.backupKeeper}),
    publicKey:[String(plan.roles.publicKey[0]),String(plan.roles.publicKey[1])],
    pricing:{minFeeWei:String(plan.service.minFee),feeMultiplier:plan.service.multiplier,fulfillGasOverhead:plan.service.overhead,keeperFeeBps:plan.service.keeperFeeBps},
    beacon:{verifier:plan.beacon.verifier,chainHash:plan.beacon.chainHash,genesis:String(plan.beacon.genesis),period:String(plan.beacon.period),sampleRound:String(plan.beacon.sampleRound),
      sampleTime:String(sampled),secondsBeforeHead:String(BigInt(head.timestamp)-sampled),identity:plan.beaconIdentity},
    preflight:{headBlock:head.number,headTime:head.timestamp,baseFeeWei:String(baseFee),maxFeePerGasWei:String(maxFee),priorityFeeWei:String(tip),deployerBalanceWei:String(balance),
      budgetWei:String(budget),implementationCreation:creation},source:o.source,transactions,calls};
  const missing=plan.steps.filter(s=>!present[s.name]),pendingCalls=calls.filter(call=>call.status==="planned");
  const transfer=plan.interim?` The owner is the interim owner: move the set to the Safe ${plan.interim.finalOwner} with admin-robinhood.ts transfer-ownership, accept-ownership and set-fee-recipient (docs/robinhood.md).`:"";
  if(!send){
    if(missing.length===0&&pendingCalls.length===0){report.verification=(await verifyRoundSet(provider,plan)).values;report.next=(existing?"Deployed and checked; the manifest records it.":"Deployed and checked; run with --send to write the manifest.")+transfer;}
    else report.next=`Dry run: nothing was signed or sent. Run with --send to send ${[...missing.map(s=>s.name),...pendingCalls.map(call=>call.step)].join(", ")}`;
    return report;
  }

  if((missing.length||pendingCalls.length)&&balance<budget)throw new Stop(`The deployer's balance ${balance} wei is below the budget of ${budget} wei`);
  await mkdir(dirname(o.journalPath),{recursive:true});
  const file=await open(o.journalPath,"a",0o600);
  const record=async(entry:JournalEntry)=>{await file.writeFile(JSON.stringify(entry)+"\n");await file.sync();};
  /** Sign one transaction from the deployer, journal it before it is broadcast, and wait for its successful receipt. */
  const sendOne=async(step:JournalStep,address:string,to:string,data:string,limit:bigint):Promise<Receipt>=>{
    const latest=await provider.getTransactionCount(deployer,"latest");
    if(await provider.getTransactionCount(deployer,"pending")!==latest)throw new Stop("The deployer has a pending transaction; let it settle first");
    const fee=await currentFee(provider,BigInt(chain.gas.maxFeePerGasWei),tipBounds(chain.gas));
    const raw=await o.wallet!.signTransaction({to,data,value:0,chainId:chain.chainId,type:2,nonce:latest,gasLimit:limit,maxFeePerGas:fee.maxFee,maxPriorityFeePerGas:fee.tip});
    const hash=keccak256(raw);
    await record({type:"signed",step,address,hash,raw});
    o.onBroadcast?.(step,hash);
    const sent=await provider.broadcastTransaction(raw);
    if(sent.hash!==hash)throw new Stop("The RPC returned another transaction hash");
    const receipt=await provider.waitForTransaction(hash,1,120_000);
    if(!receipt)throw new Stop(`No receipt for the ${step} transaction ${hash} within two minutes; run again once it is mined`);
    const done:Receipt={step,address,hash,block:receipt.blockNumber,gasUsed:String(receipt.gasUsed),effectiveGasPriceWei:String(receipt.gasPrice),feeWei:String(receipt.fee),status:receipt.status??0};
    await record({type:"receipt",...done});
    if(done.status!==1)throw new Stop(`The ${step} transaction ${hash} reverted`);
    receipts.set(step,done);
    return done;
  };
  try{
    await record({type:"run",at:new Date().toISOString()});
    for(const entry of settle)await record(entry);
    for(const s of missing){
      const data=dataOf(s),{limit}=await estimate(s,data,`the ${s.name} creation`);
      const done=await sendOne(s.name,s.address,plan.factory,data,limit);
      const code=await provider.getCode(s.address);
      if(keccak256(code)!==s.runtimeCodeHash)throw new Stop(`${s.name} at ${s.address} has runtime code hash ${keccak256(code)} after its deployment, not the expected ${s.runtimeCodeHash}`);
      const entry=transactions.find(t=>t.step===s.name)!;
      entry.status="deployed";entry.transaction!.gasLimit=String(limit);
      entry.receipt={hash:done.hash,block:done.block,gasUsed:done.gasUsed,effectiveGasPriceWei:done.effectiveGasPriceWei,feeWei:done.feeWei,status:done.status};
    }
    for(const call of pendingCalls){
      const {limit}=await estimateCall(call.transaction.data,`the ${call.step} call`);
      const done=await sendOne(call.step,proxy,proxy,call.transaction.data,limit);
      if(!await new Contract(proxy,face,provider).isBackupKeeper(plan.roles.backupKeeper))throw new Stop(`The backupKeeper call ${done.hash} succeeded, but ${plan.roles.backupKeeper} is not a backup keeper`);
      call.status="sent";call.transaction.gasLimit=String(limit);
      call.receipt={hash:done.hash,block:done.block,gasUsed:done.gasUsed,effectiveGasPriceWei:done.effectiveGasPriceWei,feeWei:done.feeWei,status:done.status};
    }
  } finally {await file.close();}

  const verification=await verifyRoundSet(provider,plan);
  report.verification=verification.values;
  if(existing){report.manifest={path:o.manifestPath,written:false};report.next=`Deployed and checked; the manifest already records this deployment.${transfer}`;return report;}
  const manifest=buildManifest(plan,deployer,verification,receipts,o.source);
  await mkdir(dirname(o.manifestPath),{recursive:true});
  await writeFile(o.manifestPath,JSON.stringify(manifest,null,2)+"\n",{flag:"wx"});
  report.manifest={path:o.manifestPath,written:true};
  report.next=`Deployed and checked. Commit ${o.manifestPath}; write the keeper settings with scripts/keeper-env.ts --chain ${plan.network} --deployment ${o.manifestPath}.${transfer}`;
  return report;
}

// ---- the manifest
/** The fields of a manifest that identify its deployment: a later run must plan exactly these. */
export function manifestIdentity(plan:RoundPlan){
  return {network:plan.network,chainId:plan.chainId,coordinatorContract:"D20VRFCoordinatorRobinhood",factory:plan.factory,owner:plan.roles.owner,feeRecipient:plan.roles.feeRecipient,
    keeper:plan.roles.keeper,backupKeepers:plan.roles.backupKeeper===undefined?[]:[plan.roles.backupKeeper],
    coordinator:plan.step.coordinator.address,coordinatorImplementation:plan.step.coordinatorImplementation.address,
    proofVerifier:plan.step.proofVerifier.address,mappingLibrary:plan.step.mappingLibrary.address,beaconVerifier:plan.step.beaconVerifier.address,
    coordinatorCodeHash:plan.step.coordinator.runtimeCodeHash,coordinatorImplementationCodeHash:plan.step.coordinatorImplementation.runtimeCodeHash,
    proofVerifierCodeHash:plan.step.proofVerifier.runtimeCodeHash,mappingLibraryCodeHash:plan.step.mappingLibrary.runtimeCodeHash,
    beaconVerifierCodeHash:plan.step.beaconVerifier.runtimeCodeHash,publicKey:[String(plan.roles.publicKey[0]),String(plan.roles.publicKey[1])],
    keyHash:plan.keyHash,protocolConfigurationHash:plan.protocolConfigurationHash};
}
type Identity=ReturnType<typeof manifestIdentity>;
/** What a manifest written before a field of the identity existed holds for it: none of its deployments allowed a backup keeper. */
const IDENTITY_DEFAULTS:Partial<Record<keyof Identity,unknown>>={backupKeepers:[]};
const differences=(manifest:Record<string,unknown>,identity:Identity)=>Object.entries(identity)
  .filter(([key,value])=>!isDeepStrictEqual(Object.hasOwn(manifest,key)?manifest[key]:IDENTITY_DEFAULTS[key as keyof Identity],value)).map(([key])=>key);
const sameDeployment=(manifest:Record<string,unknown>,identity:Identity)=>differences(manifest,identity).length===0;
export async function readManifest(path:string):Promise<Record<string,unknown>|undefined>{
  let text:string;
  try{text=await readFile(path,"utf8");}catch(error){if((error as {code?:string})?.code==="ENOENT")return undefined;throw new Stop(`${path} could not be read`);}
  try{return JSON.parse(text);}catch{throw new Stop(`${path} is not valid JSON`);}
}
/** The public record of a deployment: every address, code hash, salt and init code hash, the receipts, the beacon registration, the pricing,
 * and the reviewed source it was deployed from. It records no owner transfer: admin-robinhood.ts status reads that from the chain (owner(),
 * pendingOwner()). It names no file or key of the operator; the journal of signed transactions stays in the private directory. scripts/keeper-env.ts
 * reads the coordinator's pins from it, and scripts/admin-robinhood.ts the addresses and hashes it checks before every call. */
export function buildManifest(plan:RoundPlan,deployer:string,verification:Verification,receipts:Map<JournalStep,Receipt>,source:SourceRecord){
  const receiptOf=(receipt:Receipt)=>({hash:receipt.hash,block:receipt.block,gasUsed:receipt.gasUsed,effectiveGasPriceWei:receipt.effectiveGasPriceWei,feeWei:receipt.feeWei});
  const backup=plan.roles.backupKeeper,backupReceipt=receipts.get("backupKeeper");
  return {...manifestIdentity(plan),deployer,
    create2:Object.fromEntries(plan.steps.map(s=>[s.name,{contract:s.contract,salt:s.salt,initCodeHash:s.initCodeHash}])),
    storageLayoutHash:plan.storageLayoutHash,roundLead:Number(ROUND_LEAD),
    beacon:{id:0,verifier:plan.beacon.verifier,chainHash:plan.beacon.chainHash,publicKey:plan.beacon.publicKey,genesis:Number(plan.beacon.genesis),period:Number(plan.beacon.period),
      sampleRound:Number(plan.beacon.sampleRound),sampleSignature:plan.beacon.sampleSignature,identity:plan.beaconIdentity,inForceSince:Number(verification.since)},
    pricing:{minFeeWei:String(plan.service.minFee),feeMultiplier:plan.service.multiplier,fulfillGasOverhead:plan.service.overhead,keeperFeeBps:plan.service.keeperFeeBps,refundBps:LIMITS.bps},
    sourceCommit:source.commit,...("remoteBranches" in source?{sourceRemoteBranches:source.remoteBranches}:{sourceUnreviewed:source.unreviewed}),
    createdAt:new Date().toISOString(),
    transactions:[...plan.steps.map(s=>{
      const receipt=receipts.get(s.name);
      return receipt?{contract:s.name,address:s.address,...receiptOf(receipt)}:{contract:s.name,address:s.address,deployedEarlier:true};
    }),...(backup===undefined?[]:[{call:"setBackupKeeper",address:plan.step.coordinator.address,account:backup,
      ...(backupReceipt?receiptOf(backupReceipt):{sentEarlier:true})}])],
    implementationUpgrades:[]};
}
