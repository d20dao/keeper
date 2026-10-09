import {readFile} from "node:fs/promises";
import {join} from "node:path";
import {fileURLToPath} from "node:url";
import {getAddress} from "ethers";
import {tipBounds} from "./gas.ts";
import {Stop} from "./deployment.ts";
/** Keeper settings a chain sets away from the keeper's defaults. keeper-env.ts writes each as the setting it names (in capitals,
 * for example softDepthBlocks as SOFT_DEPTH_BLOCKS) and checks its range; a profile without the block gets the keeper's defaults.
 * coordinatorKind "round" is a coordinator that binds each request to a future drand round; the keeper's default, "epoch", is Arc's. */
export interface KeeperProfile {
  softDepthBlocks?:number;finalityAuditIntervalSeconds?:number;finalityAuditMaxLagSeconds?:number;
  gasModel?:"standard"|"arbitrum";l1GasMarginBps?:number;coordinatorKind?:"epoch"|"round";idleHeartbeatSeconds?:number;
  nonceStuckSeconds?:number;sequencerDropSeconds?:number;feeCoverageBps?:number;
  wsSilenceSeconds?:number;wsBackfillMaxBlocks?:number;wsBackfillRange?:number;
  indexFinality?:"finalized"|"soft";indexMaxBlocks?:number;indexLookbackBlocks?:number;
  sweepMinReserveWei?:string;telegramLowBalanceWei?:string;
}
export interface Chain {
  key:string;name:string;chainId:number;testnet:boolean;rpcUrls:string[];explorerUrl:string;
  nativeCurrency:{name:string;symbol:string;decimals:number};compilerEvmTarget:string;
  create2:{factory:string;codeHash:string};
  /** Owner and initial fee recipient of new deployments: a multisig on production chains. null while no owner is set:
   * requireOperable refuses such a chain, so no script signs or deploys for it. */
  owner:string|null;
  /** Why the owner is what it is, for the reader of chains.json; no script reads it. */
  ownerNote?:string;
  /** A production chain whose set is deployed before its Safe takes it over: the account without code that owns the set and receives its fees
   * until the transfer to owner (the Safe) completes. deploy-robinhood.ts uses it only with --mainnet --interim-owner, and admin-robinhood.ts
   * sends from it only with --send --mainnet. Absent or null where the owner owns the set from its deployment. */
  interimOwner?:string|null;
  /** Why the interim owner is what it is; no script reads it. */
  interimOwnerNote?:string;
  /** The Safe a production owner must be (scripts/lib/safe.ts): its address, which is owner, and the owners and threshold it must have. owners
   * is null until they are known, and every check of the Safe refuses until it is set. */
  safe?:{address:string;owners:string[]|null;threshold:number};
  /** Where contracts read block numbers and hashes: "evm" (default), or "arbitrum-l2" on an Arbitrum chain, where block.number counts parent-chain blocks. */
  blockSource?:"evm"|"arbitrum-l2";
  /** Whether keepers decide on "finalized" blocks (default) or, where that tag lags far behind, on sequencer-confirmed ones ("soft"). */
  finality?:"finalized"|"soft";
  /** How many blocks back the public RPC serves state. */
  stateHorizonBlocks?:number;
  /** The order of RPC_URLS in a generated keeper.env: the chain's first public endpoint, the operator's private ones, then its other public
   * endpoints ("public-first", the default); or the private ones before every public endpoint ("private-first", for a public endpoint that is
   * rate limited and keeps little state). */
  rpcOrder?:"public-first"|"private-first";
  keeper?:KeeperProfile;
  gas:{maxGas:number;maxFeePerGasWei:string;cancelMaxFeePerGasWei:string;maxTxCostWei:string;
    /** Bounds of the tip operator transactions pay, 1 and 50 gwei where unset (tipBounds). */
    minTipWei?:string;maxTipWei?:string;
    /** "fixed" (default): the deployment's fixed gas caps. "estimate": each step's estimate plus margin, at most maxDeployGas. */
    deployGasPolicy?:"fixed"|"estimate";maxDeployGas?:number;
    /** The chain's own gas limit for one transaction, where it reports one (ArbGasInfo.getGasAccountingParams on an Arbitrum chain); maxGas
     * and maxDeployGas must fit under it. */
    maxTxGasLimit?:number};
  expectedBlockMs?:number;
}
const isCount=(value:unknown)=>Number.isSafeInteger(value)&&(value as number)>0;
const isWei=(value:unknown)=>typeof value==="string"&&/^(0|[1-9]\d*)$/.test(value);
/** A profile checked: the required fields, and the optional ones only when present, so profiles without them are valid as they are. */
export function parseChain(key:string,entry:unknown):Chain{
  const chain=entry as Chain;
  if(typeof chain!=="object"||chain===null||typeof chain.gas!=="object"||chain.gas===null||!Number.isSafeInteger(chain.chainId)||chain.chainId<=0||!Array.isArray(chain.rpcUrls)||chain.rpcUrls.length===0||chain.rpcUrls.some(url=>new URL(url).protocol!=="https:")||!/^0x[0-9a-fA-F]{64}$/.test(chain.create2?.codeHash??""))throw new Error("Invalid chain configuration");
  getAddress(chain.create2.factory);
  // An owner is a checksummed address or an explicit null; a missing or misspelled field is never read as "undecided".
  if(chain.owner!==null&&(typeof chain.owner!=="string"||getAddress(chain.owner)!==chain.owner))throw new Error("Chain owner must be a checksummed address, or null while the owner is undecided");
  if(chain.ownerNote!==undefined&&typeof chain.ownerNote!=="string")throw new Error("Invalid chain owner note");
  const checksummed=(value:unknown)=>typeof value==="string"&&/^0x[0-9a-fA-F]{40}$/.test(value)&&getAddress(value)===value&&BigInt(value)!==0n;
  if(chain.interimOwner!==undefined&&chain.interimOwner!==null&&!checksummed(chain.interimOwner))throw new Error("Chain interim owner must be a checksummed address, or null");
  if(chain.interimOwnerNote!==undefined&&typeof chain.interimOwnerNote!=="string")throw new Error("Invalid chain interim owner note");
  if(chain.safe!==undefined){
    const safe=chain.safe as unknown as Record<string,unknown>|null;
    if(safe===null||typeof safe!=="object"||Array.isArray(safe)||Object.keys(safe).sort().join()!=="address,owners,threshold")throw new Error("Chain safe must have exactly address, owners and threshold");
    if(!checksummed(safe.address))throw new Error("Chain safe address must be a checksummed address");
    if(!Number.isSafeInteger(safe.threshold)||(safe.threshold as number)<2)throw new Error("Chain safe threshold must be a whole number of at least 2");
    if(safe.owners!==null){
      const owners=safe.owners;
      if(!Array.isArray(owners)||!owners.every(checksummed)||new Set(owners).size!==owners.length||owners.length<(safe.threshold as number))
        throw new Error("Chain safe owners must be distinct checksummed addresses, at least as many as its threshold, or null while unknown");
    }
    if(chain.owner!==safe.address)throw new Error("A chain that names a Safe has it as its owner");
    if(chain.interimOwner===safe.address)throw new Error("A chain's interim owner is not its Safe");
  } else if(chain.interimOwner)throw new Error("A chain with an interim owner names the Safe that takes the set over");
  if(chain.nativeCurrency.decimals!==18)throw new Error("This deployment profile requires native18 accounting");
  const {gas}=chain;
  if(chain.blockSource!==undefined&&chain.blockSource!=="evm"&&chain.blockSource!=="arbitrum-l2")throw new Error("Invalid chain block source");
  if(chain.finality!==undefined&&chain.finality!=="finalized"&&chain.finality!=="soft")throw new Error("Invalid chain finality");
  if(chain.stateHorizonBlocks!==undefined&&!isCount(chain.stateHorizonBlocks))throw new Error("Invalid chain state horizon");
  if(chain.rpcOrder!==undefined&&chain.rpcOrder!=="public-first"&&chain.rpcOrder!=="private-first")throw new Error("Invalid chain RPC order");
  if(chain.keeper!==undefined&&(typeof chain.keeper!=="object"||chain.keeper===null||Array.isArray(chain.keeper)))throw new Error("Invalid chain keeper settings");
  if(gas.minTipWei!==undefined&&!isWei(gas.minTipWei)||gas.maxTipWei!==undefined&&!isWei(gas.maxTipWei))throw new Error("Invalid chain tip bounds");
  tipBounds(gas);
  if(gas.deployGasPolicy!==undefined&&gas.deployGasPolicy!=="fixed"&&gas.deployGasPolicy!=="estimate"||gas.maxDeployGas!==undefined&&!isCount(gas.maxDeployGas)||gas.deployGasPolicy==="estimate"&&gas.maxDeployGas===undefined)throw new Error("Invalid chain deployment gas policy");
  if(gas.maxTxGasLimit!==undefined&&(!isCount(gas.maxTxGasLimit)||gas.maxGas>gas.maxTxGasLimit||(gas.maxDeployGas??0)>gas.maxTxGasLimit))
    throw new Error("Invalid chain transaction gas limit: maxGas and maxDeployGas must fit under maxTxGasLimit");
  return {...chain,key};
}
/** The profile chains.json holds under key. No chain is loaded by default: a caller without a key, such as a manifest that does not name
 * its network, is refused rather than handed Arc Testnet's profile. A script's own --chain default stays the script's. */
export async function loadChain(key:string):Promise<Chain>{
  if(typeof key!=="string"||key==="")throw new Stop("No chain named: name the chain profile to load (a manifest names it as network); none is loaded by default");
  const chains=JSON.parse(await readFile(new URL("../../chains.json",import.meta.url),"utf8"));
  if(!Object.hasOwn(chains,key))throw new Error("Unknown chain in chains.json");
  return parseChain(key,{...chains[key],...await ownerOverlay(key)});
}
/** The owner fields an operator's private overlay may set over a committed profile. */
const OVERLAY_FIELDS=new Set(["owner","ownerNote","interimOwner","interimOwnerNote","safe"]);
/** The directory of the private owner overlays: deployments/private, which git ignores, or D20_PRIVATE_PROFILE_DIR. */
export const privateProfileDir=()=>process.env.D20_PRIVATE_PROFILE_DIR||fileURLToPath(new URL("../../deployments/private/",import.meta.url));
/** An operator's private owner fields for key, `<key>.owner.json` in privateProfileDir(), merged over the committed profile when present: the
 * owner, its note, interimOwner and its note, and safe, nothing else. Absent, the committed profile stands as it is. */
async function ownerOverlay(key:string):Promise<Record<string,unknown>>{
  const path=join(privateProfileDir(),`${key}.owner.json`);
  let text:string;
  try{text=await readFile(path,"utf8");}
  catch(error){if((error as NodeJS.ErrnoException).code==="ENOENT")return {};throw error;}
  const overlay=JSON.parse(text) as unknown;
  if(typeof overlay!=="object"||overlay===null||Array.isArray(overlay))throw new Stop(`${path}: an owner overlay is a JSON object`);
  const extra=Object.keys(overlay).filter(field=>!OVERLAY_FIELDS.has(field));
  if(extra.length>0)throw new Stop(`${path}: an owner overlay sets only ${[...OVERLAY_FIELDS].join(", ")}, not ${extra.join(", ")}`);
  return overlay as Record<string,unknown>;
}
/** A chain a script may sign for or deploy to: its owner is decided. */
export type OperableChain=Chain&{owner:string};
const arbitrumRefusal=(chain:Pick<Chain,"key"|"blockSource">,script:string)=>
  chain.blockSource===undefined||chain.blockSource==="evm"?undefined:
    `Robinhood deployments use the Arbitrum path, which is not implemented in this script yet (${script}, chain ${chain.key}: block.number there counts parent-chain blocks)`;
const ownerRefusal=(chain:Pick<Chain,"key"|"owner">,script:string)=>
  chain.owner!==null?undefined:`The owner of ${chain.key} is not set, so ${script} will not run against it; set "owner" in chains.json first`;
/** Refuses a chain whose contracts read block numbers through the Arbitrum path (blockSource "arbitrum-l2"), which Arc's contracts and tools
 * do not implement. Anything but the EVM source is refused, so a source added later is refused until a script handles it. */
export function requireEvmBlockSource(chain:Pick<Chain,"key"|"blockSource">,script:string):void{
  const refusal=arbitrumRefusal(chain,script);
  if(refusal!==undefined)throw new Stop(refusal);
}
/** Refuses a chain whose owner is not decided: its profile holds null instead of an address. Every script that deploys, signs or reads a deployment
 * calls it (requireOperable includes it), right after loading the profile; the read-only network preflight and the testnet wallet helper, which
 * touch no deployment, do not. */
export function requireOwner(chain:Chain,script:string):asserts chain is OperableChain{
  const refusal=ownerRefusal(chain,script);
  if(refusal!==undefined)throw new Stop(refusal);
}
/** Refuses a chain whose profile runs a round coordinator (keeper.coordinatorKind "round"). A script that deploys the epoch design, its
 * registry and catalog signers with it, calls it after requireOperable, so that it never deploys there even once a chain's other refusals are lifted. */
export function requireEpochDesign(chain:Pick<Chain,"key"|"keeper">,script:string):void{
  if(chain.keeper?.coordinatorKind==="round")throw new Stop(`${script} deploys the epoch design, and ${chain.key} runs a round coordinator, which this script does not deploy`);
}
/** The other side of requireEpochDesign, for the scripts that deploy and administer the round coordinator (deploy-robinhood.ts and
 * admin-robinhood.ts): the chain's profile runs a round coordinator (keeper.coordinatorKind "round") and reads block numbers through the
 * Arbitrum path (blockSource "arbitrum-l2"), the only path the round contracts implement. Those scripts call requireOwner and then this,
 * right after loading the chain, in place of requireOperable: they are the scripts that implement the Arbitrum path. */
export function requireRoundCoordinator(chain:Pick<Chain,"key"|"keeper"|"blockSource">,script:string):void{
  if(chain.keeper?.coordinatorKind!=="round")throw new Stop(`${script} deploys and administers the round coordinator, and ${chain.key} does not run one (its keeper.coordinatorKind is not "round")`);
  if(chain.blockSource!=="arbitrum-l2")throw new Stop(`${script} deploys and administers the round contracts, which read block numbers through the Arbitrum path, and ${chain.key} does not use it (its blockSource is not "arbitrum-l2")`);
}
/** The guard of a script that sends transactions or deploys, called right after the chain is loaded, before any network access or key use:
 * both refusals above, in one message when both apply. A script drops the first check only when it implements the Arbitrum path. */
export function requireOperable(chain:Chain,script:string):asserts chain is OperableChain{
  const refusals=[arbitrumRefusal(chain,script),ownerRefusal(chain,script)].filter((refusal):refusal is string=>refusal!==undefined);
  if(refusals.length)throw new Stop(refusals.join(". "));
}
