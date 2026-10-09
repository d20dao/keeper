// The Robinhood profiles carry nothing of the epoch design and nothing of Arc (owner rule of 6 October 2026): no registry, epoch, recipe,
// AirnodeHub, block nudge or USDC wording, no Arc chain id and no Arc owner. robinhoodSurfaces collects every Robinhood profile the scripts
// keep or write; legacyFindings lists what each one says that it must not. There is no allowlist: no Robinhood surface needs any of these.
import {readFile,readdir} from "node:fs/promises";
import {parseChain,type Chain} from "./chains.ts";
import {renderKeeperEnv,type KeeperRole} from "./keeper-env.ts";

/** Words of the epoch design, matched anywhere and in any case, so REGISTRY_KIND, EpochBeacon and epochImplementationCodeHash are all caught:
 * the registry and its epochs, AirnodeHub's recipes and signers, the block nudge, USDC units, the committer role the round coordinator calls
 * keeper, and the confirmations the round coordinator does not take. "arc" is matched as a word, so search or Arbitrum do not count. */
export const LEGACY_WORDS=/registry|epoch|recipe|airnode|nudge|usdc|committer|confirmation|\barc\b/gi;
/** Arc's chain ids, which no Robinhood profile names. Arc's own profiles add theirs, and these two stay even if a profile is renamed. */
const ARC_CHAIN_IDS=[5042,5042002];
const STAND_IN_HASH="0x"+"ab".repeat(32);

export type Surface={name:string;text:string};
export type ArcMarkers={chainIds:number[];owners:string[]};

const root=new URL("../../",import.meta.url);
const read=(path:string)=>readFile(new URL(path,root),"utf8");

/** Arc's chain ids and owner addresses, from Arc's own entries in chains.json (keys "arc-…"), with Arc's two known ids always included. */
export function arcMarkers(chains:Record<string,{chainId?:unknown;owner?:unknown}>):ArcMarkers{
  const arc=Object.entries(chains).filter(([key])=>key.startsWith("arc-")).map(([,entry])=>entry);
  if(arc.length===0)throw new Error("chains.json has no Arc profile to take Arc's markers from");
  const chainIds=[...new Set([...ARC_CHAIN_IDS,...arc.map(entry=>Number(entry.chainId))])].filter(Number.isSafeInteger);
  const owners=arc.map(entry=>entry.owner).filter((owner):owner is string=>typeof owner==="string");
  return {chainIds,owners};
}

/** What a surface says that it must not: each legacy word, Arc chain id (as a number of its own, not digits inside a hash) and Arc owner
 * (in any case), with its line. */
export function legacyFindings(surface:Surface,markers:ArcMarkers):string[]{
  const ids=new RegExp(`(?<![0-9A-Za-z])(${[...markers.chainIds].sort((a,b)=>b-a).join("|")})(?![0-9A-Za-z])`,"g");
  const owners=markers.owners.map(owner=>owner.toLowerCase().replace(/^0x/,""));
  const findings:string[]=[];
  surface.text.split("\n").forEach((line,index)=>{
    const where=`${surface.name}:${index+1}`;
    for(const [word] of line.matchAll(LEGACY_WORDS))findings.push(`${where}: "${word}"`);
    for(const [id] of line.matchAll(ids))findings.push(`${where}: Arc chain id ${id}`);
    for(const owner of owners)if(line.toLowerCase().includes(owner))findings.push(`${where}: Arc owner 0x${owner}`);
  });
  return findings;
}

/** The round coordinator's deployment and administration scripts, their Safe check and their CREATE2 configuration, which the gate reads as they are. */
export const ROUND_SCRIPTS=["config/robinhood-create2.json","scripts/deploy-robinhood.ts","scripts/lib/robinhood-deploy.ts","scripts/admin-robinhood.ts","scripts/lib/robinhood-admin.ts",
  "scripts/lib/safe.ts"];
/** What a Safe signer that is also an Arc owner address reads as in a Robinhood profile (see profileSurfaceText). */
export const SHARED_SIGNER="(a Safe signer that also owns another network's set)";
/** A Robinhood chain profile as the gate reads it: as it is, except one exception. The rule keeps Arc's owners from owning a Robinhood set or
 * receiving its fees, so owner, interimOwner, safe.address and every other field are read as they are. A key that owns an Arc set may still
 * be one signer of the Robinhood Safe: a signer of a Safe whose threshold is at least 2 can do nothing
 * alone. So an entry of safe.owners that is an Arc owner reads as SHARED_SIGNER when safe.threshold is at least 2, and as itself otherwise. */
export function profileSurfaceText(profile:unknown,markers:ArcMarkers):string{
  const entry=structuredClone(profile) as {safe?:{owners?:unknown;threshold?:unknown}}|null;
  const arcOwners=markers.owners.map(owner=>owner.toLowerCase()),safe=entry?.safe;
  if(safe&&Array.isArray(safe.owners)&&Number.isSafeInteger(safe.threshold)&&(safe.threshold as number)>=2)
    safe.owners=safe.owners.map(owner=>typeof owner==="string"&&arcOwners.includes(owner.toLowerCase())?SHARED_SIGNER:owner);
  return JSON.stringify(entry,null,2);
}
/** Every Robinhood profile: the chains.json entries (keys "robinhood-…"), the service profiles config/service.robinhood-*.json and the beacon
 * files they name, the Robinhood keeper.env example, the keeper.env the generator writes for each Robinhood chain and role, with every
 * optional part filled from stand-in inputs, the round coordinator's deployment and administration scripts (ROUND_SCRIPTS), and every
 * Robinhood deployment manifest committed under deployments/. Each Robinhood network must have its chain profile and its service profile. */
export async function robinhoodSurfaces():Promise<{surfaces:Surface[];markers:ArcMarkers}>{
  const chains=JSON.parse(await read("chains.json")) as Record<string,unknown>;
  const markers=arcMarkers(chains as Record<string,{chainId?:unknown;owner?:unknown}>);
  const keys=Object.keys(chains).filter(key=>key.startsWith("robinhood-")).sort();
  const services=(await readdir(new URL("config/",root))).filter(file=>/^service\.robinhood-.+\.json$/.test(file)).sort();
  for(const network of ["robinhood-testnet","robinhood-mainnet"]){
    if(!keys.includes(network))throw new Error(`chains.json has no ${network} profile`);
    if(!services.includes(`service.${network}.json`))throw new Error(`config/service.${network}.json is missing`);
  }
  const surfaces:Surface[]=keys.map(key=>({name:`chains.json ${key}`,text:profileSurfaceText(chains[key],markers)}));
  const beacons=new Set<string>();
  for(const file of services){
    const text=await read(`config/${file}`);
    surfaces.push({name:`config/${file}`,text});
    const beacon=(JSON.parse(text) as {beacon?:unknown}).beacon;
    if(typeof beacon==="string")beacons.add(beacon);
  }
  for(const beacon of [...beacons].sort())surfaces.push({name:beacon,text:await read(beacon)});
  surfaces.push({name:"deploy/docker/keeper.robinhood.env.example",text:await read("deploy/docker/keeper.robinhood.env.example")});
  for(const key of keys){
    const chain:Chain=parseChain(key,chains[key]);
    const record={chainId:chain.chainId,coordinator:"0x"+"11".repeat(20),coordinatorCodeHash:STAND_IN_HASH,protocolConfigurationHash:STAND_IN_HASH,coordinatorImplementationCodeHash:STAND_IN_HASH};
    const secrets={privateRpcUrls:["https://private-rpc.example/key"],wsUrls:["wss://private-ws.example/key"],telegramBotToken:"123:token",telegramChatId:"-1001",
      neonDb:"postgresql://user:password@host/db?sslmode=require&channel_binding=require",healthApiUrl:"https://health.example/report",healthApiKey:"health-key",
      discordBotToken:"discord-token-0123456789",discordChannelId:"1234567890",discordPublicExplorerUrl:"https://site.example"};
    for(const role of ["primary","follower"] as KeeperRole[])surfaces.push({name:`keeper.env for ${key} (${role})`,text:renderKeeperEnv(chain,record,secrets,role)});
  }
  for(const file of ROUND_SCRIPTS)surfaces.push({name:file,text:await read(file)});
  const manifests=(await readdir(new URL("deployments/",root))).filter(file=>/^robinhood-.+\.json$/.test(file)).sort();
  for(const file of manifests)surfaces.push({name:`deployments/${file}`,text:await read(`deployments/${file}`)});
  return {surfaces,markers};
}
