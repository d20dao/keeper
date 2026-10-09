// What a Safe is, for the round coordinator's scripts: an account whose runtime code is a canonical SafeProxy, whose singleton (storage slot 0)
// is a canonical Safe or SafeL2 singleton with that singleton's code, and that answers VERSION(), getOwners() and getThreshold(). A contract
// that merely answers getThreshold() is not one. A production owner must also be the Safe its chain profile names (safe.address), with exactly
// the owners and threshold the profile records, a threshold of at least 2, and no module, which could act without the owners' signatures.
//
// The pins below are Safe's canonical deployments as the safe-deployments package lists them (v1.3.0 canonical and eip155, v1.4.1, v1.5.0).
// The package is not a dependency here, so they are pinned by hand and were checked on 6 October 2026 with read-only calls on Robinhood Chain
// (4663) and its testnet (46630): each singleton is deployed at its address on both, answers VERSION() as listed, and has the code hash listed;
// each SafeProxy code hash is the runtime code that the version's canonical SafeProxyFactory deploys (its proxyCreationCode() with a singleton
// as the constructor argument, simulated with eth_call). The proxy's runtime code does not depend on its singleton.
import {Contract,getAddress,keccak256,type JsonRpcApiProvider} from "ethers";
import type {Chain} from "./chains.ts";
import {Stop} from "./deployment.ts";

export type SafeVersion="1.3.0"|"1.4.1"|"1.5.0";
/** The runtime code hash of each canonical SafeProxy version. */
export const SAFE_PROXY_CODE_HASHES:Readonly<Record<string,SafeVersion>>={
  "0xb89c1b3bdf2cf8827818646bce9a8f6e372885f8c55e5c07acbd307cb133b000":"1.3.0", // GnosisSafeProxy, 171 bytes
  "0xd7d408ebcd99b2b70be43e20253d6d92a8ea8fab29bd3be7f55b10032331fb4c":"1.4.1", // SafeProxy, 171 bytes
  "0x4e381985ca68b3e5d27b4425fa581c19cf33146d3f887a3cfca96f55528ea46f":"1.5.0", // SafeProxy, 123 bytes
};
/** The canonical singletons: their address, version, kind and runtime code hash. Each has no immutable, so its code hash is the same at
 * every address and on every chain (1.3.0's canonical and eip155 singletons have the same code). */
export const SAFE_SINGLETONS:ReadonlyArray<{address:string;version:SafeVersion;kind:"Safe"|"SafeL2";codeHash:string}>=[
  {address:"0xd9Db270c1B5E3Bd161E8c8503c55cEABeE709552",version:"1.3.0",kind:"Safe",codeHash:"0xbba688fbdb21ad2bb58bc320638b43d94e7d100f6f3ebaab0a4e4de6304b1c2e"},
  {address:"0x3E5c63644E683549055b9Be8653de26E0B4CD36E",version:"1.3.0",kind:"SafeL2",codeHash:"0x21842597390c4c6e3c1239e434a682b054bd9548eee5e9b1d6a4482731023c0f"},
  {address:"0x69f4D1788e39c87893C980c06EdF4b7f686e2938",version:"1.3.0",kind:"Safe",codeHash:"0xbba688fbdb21ad2bb58bc320638b43d94e7d100f6f3ebaab0a4e4de6304b1c2e"},
  {address:"0xfb1bffC9d739B8D520DaF37dF666da4C687191EA",version:"1.3.0",kind:"SafeL2",codeHash:"0x21842597390c4c6e3c1239e434a682b054bd9548eee5e9b1d6a4482731023c0f"},
  {address:"0x41675C099F32341bf84BFc5382aF534df5C7461a",version:"1.4.1",kind:"Safe",codeHash:"0x1fe2df852ba3299d6534ef416eefa406e56ced995bca886ab7a553e6d0c5e1c4"},
  {address:"0x29fcB43b46531BcA003ddC8FCB67FFE91900C762",version:"1.4.1",kind:"SafeL2",codeHash:"0xb1f926978a0f44a2c0ec8fe822418ae969bd8c3f18d61e5103100339894f81ff"},
  {address:"0xFf51A5898e281Db6DfC7855790607438dF2ca44b",version:"1.5.0",kind:"Safe",codeHash:"0xdda019cbd7c867a533a2a86e5c53434fdc50b13122b5a5ddb4a8df61b31c20f2"},
  {address:"0xEdd160fEBBD92E350D4D398fb636302fccd67C7e",version:"1.5.0",kind:"SafeL2",codeHash:"0x180193227186ccb85316c94db1f0d156ed932b14712cfaac78901899178572dc"},
];
/** The modules list's sentinel, from which getModulesPaginated starts. */
const SENTINEL="0x0000000000000000000000000000000000000001";
const SAFE_ABI=["function VERSION() view returns(string)","function getOwners() view returns(address[])","function getThreshold() view returns(uint256)",
  "function getModulesPaginated(address start,uint256 pageSize) view returns(address[] array,address next)"];

/** What an address is, read from the chain: problems is empty when it is a Safe as described at the top of this file. */
export type SafeReading={address:string;problems:string[];proxyVersion?:SafeVersion;singleton?:string;singletonKind?:"Safe"|"SafeL2";version?:string;
  owners?:string[];threshold?:number;modules?:string[]};
/** Reads an address as a Safe, without throwing: each way it is not one is a problem. */
export async function readSafe(provider:JsonRpcApiProvider,address:string):Promise<SafeReading>{
  const reading:SafeReading={address:getAddress(address),problems:[]},problems=reading.problems;
  const code=await provider.getCode(reading.address);
  if(code==="0x"){problems.push("it has no code");return reading;}
  reading.proxyVersion=SAFE_PROXY_CODE_HASHES[keccak256(code)];
  if(!reading.proxyVersion)problems.push(`its runtime code hash ${keccak256(code)} is not a canonical SafeProxy's (1.3.0, 1.4.1 or 1.5.0)`);
  const slot=await provider.getStorage(reading.address,0);
  const singleton=BigInt(slot)>>160n===0n?getAddress("0x"+slot.slice(-40)):undefined;
  const known=SAFE_SINGLETONS.find(entry=>entry.address===singleton);
  if(!known)problems.push(`its singleton (storage slot 0, ${singleton??slot}) is not a canonical Safe or SafeL2 singleton`);
  else {
    reading.singleton=known.address;reading.singletonKind=known.kind;
    const singletonCode=await provider.getCode(known.address);
    if(keccak256(singletonCode)!==known.codeHash)problems.push(`its singleton ${known.address} has ${singletonCode==="0x"?"no code":`runtime code hash ${keccak256(singletonCode)}`} on this chain, not the ${known.kind} ${known.version} code ${known.codeHash}`);
  }
  const safe=new Contract(reading.address,SAFE_ABI,provider);
  const read=async<T>(what:string,call:()=>Promise<T>):Promise<T|undefined>=>{try{return await call();}catch{problems.push(`it does not answer ${what}`);return undefined;}};
  reading.version=await read("VERSION()",()=>safe.VERSION() as Promise<string>);
  if(reading.version!==undefined&&known&&reading.version!==known.version)problems.push(`it answers VERSION() ${JSON.stringify(reading.version)}, not its singleton's ${known.version}`);
  const owners=await read("getOwners()",()=>safe.getOwners() as Promise<string[]>);
  if(owners)reading.owners=owners.map(owner=>getAddress(owner));
  const threshold=await read("getThreshold()",()=>safe.getThreshold() as Promise<bigint>);
  if(threshold!==undefined){
    reading.threshold=Number(threshold);
    if(threshold===0n||!reading.owners||threshold>BigInt(reading.owners.length))problems.push(`its threshold ${threshold} is not 1 to its ${reading.owners?.length??0} owners`);
  }
  const modules=await read("getModulesPaginated()",async()=>{const [page]=await safe.getModulesPaginated(SENTINEL,10) as [string[],string];return page;});
  if(modules)reading.modules=modules.map(module=>getAddress(module));
  return reading;
}
/** "EOA" for an address without code, "Safe" for a Safe as readSafe reads one, else "contract". */
export async function accountKind(provider:JsonRpcApiProvider,address:string):Promise<"EOA"|"Safe"|"contract">{
  if(await provider.getCode(address)==="0x")return "EOA";
  return (await readSafe(provider,address)).problems.length===0?"Safe":"contract";
}

/** The strict check of a chain's Safe, without throwing: a Safe as readSafe reads one, that is the profile's safe.address, with exactly the
 * profile's owners and threshold, a threshold of at least 2, and no module. problems is empty when it passes. */
export async function checkProfileSafe(provider:JsonRpcApiProvider,chain:Pick<Chain,"key"|"safe">,address:string):Promise<SafeReading>{
  const reading=await readSafe(provider,address),problems=reading.problems;
  const expected=chain.safe;
  if(!expected){problems.unshift(`the ${chain.key} profile names no Safe (safe in chains.json)`);return reading;}
  if(reading.address!==expected.address)problems.push(`it is not the ${chain.key} profile's Safe ${expected.address}`);
  if(expected.owners===null)problems.push(`the ${chain.key} profile does not record the Safe's owners yet (safe.owners in chains.json is null)`);
  if(reading.threshold!==undefined&&reading.threshold<2)problems.push(`its threshold is ${reading.threshold}: a production owner needs at least 2 signatures`);
  if(reading.threshold!==undefined&&reading.threshold!==expected.threshold)problems.push(`its threshold is ${reading.threshold}, not the profile's ${expected.threshold}`);
  if(reading.owners&&expected.owners){
    const have=new Set(reading.owners),want=new Set(expected.owners);
    if(have.size!==want.size||[...want].some(owner=>!have.has(owner)))problems.push(`its owners are ${reading.owners.join(", ")}, not the profile's ${expected.owners.join(", ")}`);
  }
  if(reading.modules&&reading.modules.length)problems.push(`it has modules enabled (${reading.modules.join(", ")}), which act without the owners' signatures`);
  return reading;
}
/** checkProfileSafe, refusing with every problem it found. `what` names the address's role, as "The proposed owner". */
export async function requireProfileSafe(provider:JsonRpcApiProvider,chain:Pick<Chain,"key"|"safe">,address:string,what:string):Promise<SafeReading>{
  const reading=await checkProfileSafe(provider,chain,address);
  if(reading.problems.length)throw new Stop(`${what} ${reading.address} is not the Safe a production owner must be: ${reading.problems.join("; ")}`);
  return reading;
}
