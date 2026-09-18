import {mkdir,readFile,writeFile} from "node:fs/promises";
import {execFileSync} from "node:child_process";
import {resolve,join} from "node:path";
import {homedir} from "node:os";
import {ContractFactory,Interface,Wallet,SigningKey,getAddress,getCreate2Address,getBytes,hexlify,keccak256,concat,zeroPadValue,type JsonRpcProvider} from "ethers";
import type {Chain} from "./chains.ts";

export const INITIAL_OWNER="0xcA35c280c5DF22958Adb74DD03911C8A0ec3eA03";
/** A stop with a message written by these scripts; it never carries credentials, so it may be printed. */
export class Stop extends Error {}
export const DEFAULT_OPERATOR_DIRECTORY=join(homedir(),".config","d20dao");
export type Operator={owner:string;feeRecipient:string;deployer:string;keeper:{address:string;keyFile:string};vrf:{keyFile:string;publicKey:[string,string]}};

export async function privateDirectory(directory:string){
  await mkdir(directory,{recursive:true,mode:0o700});
  if(process.platform==="win32"){
    const sid=execFileSync("powershell.exe",["-NoProfile","-Command","[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value"],{encoding:"utf8",windowsHide:true}).trim();
    if(!/^S-1-[0-9-]+$/.test(sid))throw new Stop("Cannot establish private directory owner");
    execFileSync("icacls.exe",[directory,"/inheritance:r","/grant:r",`*${sid}:(OI)(CI)F`],{windowsHide:true,stdio:"pipe"});
  }
}
/** owner defaults to the Arc testnet pilot owner; deployment scripts pass the selected chain's owner. */
export async function loadOrCreateOperator(directory:string,deployer:string,owner=INITIAL_OWNER):Promise<Operator>{
  await privateDirectory(directory);
  const file=join(directory,"operator.json");
  let saved:string|undefined;
  try {saved=await readFile(file,"utf8");}
  catch(error){if(!(error&&typeof error==="object"&&"code" in error&&error.code==="ENOENT"))throw new Stop("Operator metadata could not be read");}
  if(saved!==undefined){
  try {
    const operator=JSON.parse(saved) as Operator;
    if(operator.deployer!==deployer||operator.owner!==owner||operator.feeRecipient!==owner)throw new Stop("Operator identity/configuration mismatch");
    const addresses:string[]=[];
    for(const entry of [operator.keeper,operator.vrf]){
      const key=(await readFile(entry.keyFile,"utf8")).trim();
      const wallet=new Wallet(key);
      addresses.push(wallet.address);
      if("address" in entry&&wallet.address!==entry.address)throw new Stop("Keeper key identity mismatch");
      if("publicKey" in entry){const point=SigningKey.computePublicKey(wallet.privateKey,false);if(BigInt(entry.publicKey[0])!==BigInt("0x"+point.slice(4,68))||BigInt(entry.publicKey[1])!==BigInt("0x"+point.slice(68)))throw new Stop("VRF key identity mismatch");}
    }
    if(addresses[0]===addresses[1]||addresses.includes(deployer))throw new Stop("Operational roles require separate keys");
    return operator;
  } catch {throw new Stop("Operator metadata or keys could not be validated; existing identity preserved");}
  }
  // Exclusively create metadata/keys; never replace an existing operational key.
  const keeper=Wallet.createRandom(),vrf=Wallet.createRandom();
  const keeperFile=join(directory,"keeper-tx.key"),vrfFile=join(directory,"vrf.key");
  await writeFile(keeperFile,keeper.privateKey+"\n",{flag:"wx",mode:0o600});
  await writeFile(vrfFile,vrf.privateKey+"\n",{flag:"wx",mode:0o600});
  const point=SigningKey.computePublicKey(vrf.privateKey,false);
  const operator:Operator={owner,feeRecipient:owner,deployer,
    keeper:{address:keeper.address,keyFile:keeperFile},vrf:{keyFile:vrfFile,publicKey:[BigInt("0x"+point.slice(4,68)).toString(),BigInt("0x"+point.slice(68)).toString()]}};
  await writeFile(file,JSON.stringify(operator,null,2)+"\n",{flag:"wx",mode:0o600});return operator;
}
export async function validateNetwork(provider:JsonRpcProvider,chain:Chain){
  if((await provider.getNetwork()).chainId!==BigInt(chain.chainId))throw new Stop("Wrong configured chain");
  const head=await provider.getBlock("latest");
  if(!head||Math.abs(Math.floor(Date.now()/1000)-head.timestamp)>120)throw new Stop("Stale RPC head");
  if(keccak256(await provider.getCode(chain.create2.factory))!==chain.create2.codeHash)throw new Stop("Unexpected deterministic factory code");
  return head;
}
type Deployable="EpochEntropy"|"D20VRFCoordinator"|"D20Proxy"|"D20CostClient";
async function artifactFor(name:Deployable){
  return JSON.parse(await readFile(resolve(`artifacts/contracts/${name==="D20CostClient"?"examples/":""}${name}.sol/${name}.json`),"utf8"));
}
export async function initCode(name:Deployable,args:unknown[]):Promise<string>{
  const artifact=await artifactFor(name);
  return (await new ContractFactory(artifact.abi,artifact.bytecode).getDeployTransaction(...args)).data;
}
export async function initialization(name:"EpochEntropy"|"D20VRFCoordinator"|"D20CostClient",args:unknown[]):Promise<string>{
  const artifact=await artifactFor(name);
  return new Interface(artifact.abi).encodeFunctionData("initialize",args);
}
/** Compile current sources (a no-op when artifacts are current) so code checks never trust stale artifacts. */
export function compileContracts(){
  execFileSync(process.execPath,[resolve("node_modules/hardhat/dist/src/cli.js"),"compile","--quiet"],{stdio:["ignore","ignore","inherit"],windowsHide:true});
}
export type ImmutableReferences=Record<string,ReadonlyArray<{start:number;length:number}>>;
/** Runtime code of an implementation deployed at `address`: UUPSUpgradeable embeds that address as its only immutable. */
export function runtimeCodeAt(deployedBytecode:string,immutableReferences:ImmutableReferences,address:string):string{
  const code=getBytes(deployedBytecode),word=getBytes(zeroPadValue(getAddress(address),32)),references=Object.values(immutableReferences);
  if(references.length!==1)throw new Stop("Expected exactly one immutable, the UUPS self address");
  for(const {start,length} of references[0]){
    if(length!==32||start+32>code.length)throw new Stop("Invalid immutable reference");
    code.set(word,start);
  }
  return hexlify(code);
}
/** Runtime code hash a compiled implementation has at `address`, from an artifact that matches its build output. */
export async function compiledRuntimeCodeHash(name:"EpochEntropy"|"D20VRFCoordinator",address:string):Promise<string>{
  const artifact=await artifactFor(name);
  const build=JSON.parse(await readFile(resolve(`artifacts/build-info/${artifact.buildInfoId}.output.json`),"utf8"));
  const output=(build.output??build).contracts[artifact.inputSourceName][name];
  if("0x"+output.evm.deployedBytecode.object!==artifact.deployedBytecode)throw new Stop("Artifact differs from its build output; recompile");
  return keccak256(runtimeCodeAt(artifact.deployedBytecode,artifact.immutableReferences,address));
}
export function miningPlan(code:string,chain:Chain){return {chainId:chain.chainId,factory:chain.create2.factory,initCodeHash:keccak256(code),prefix:"d20da0"};}
export async function candidate(resultFile:string,code:string,chain:Chain){
  const result=JSON.parse(await readFile(resultFile,"utf8"));
  if(result.factory!==chain.create2.factory||result.initCodeHash!==keccak256(code)||result.prefix!=="d20da0"||!result.match)throw new Stop("Mining result does not match current deployment bytecode");
  const address=getCreate2Address(chain.create2.factory,result.match.salt,keccak256(code));
  if(address!==result.match.address||!address.toLowerCase().startsWith("0xd20da0"))throw new Stop("Invalid mined address");
  return {address,salt:result.match.salt,data:concat([getBytes(result.match.salt),getBytes(code)])};
}
