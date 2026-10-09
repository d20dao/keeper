import {getAddress} from "ethers";
import type {Chain,KeeperProfile} from "./chains.ts";
import {tipBounds} from "./gas.ts";

/** The pins of a deployment. epochImplementationCodeHash is the code hash of an epoch coordinator's registry implementation: required for
 * the keeper's default coordinator kind, and refused for a round coordinator, which has no registry. */
export type DeploymentRecord={chainId:number;coordinator:string;coordinatorCodeHash:string;protocolConfigurationHash:string;
  coordinatorImplementationCodeHash:string;epochImplementationCodeHash?:string};
/** What the operator supplies besides the deployment record: keyed endpoints and tokens, and the optional reporting endpoints. Each is
 * written only into the returned text. privateRpcUrl is one URL; privateRpcUrls is several, in the order the keeper tries them.
 * allowPublicOnly accepts a "private-first" chain without any private endpoint, so that the file lists its rate limited public endpoint alone. */
export type KeeperSecrets={privateRpcUrl?:string;privateRpcUrls?:string[];allowPublicOnly?:boolean;wsUrls?:string[];telegramBotToken?:string;telegramChatId?:string;neonDb?:string;
  healthApiUrl?:string;healthApiKey?:string;healthIntervalSeconds?:number;
  discordBotToken?:string;discordChannelId?:string;discordPublicExplorerUrl?:string};
export type KeeperRole="primary"|"follower";

const hash=(value:string|undefined,name:string)=>{if(value===undefined||!/^0x[0-9a-fA-F]{64}$/.test(value))throw new Error(`Invalid ${name}`);return value.toLowerCase();};
const quoted=(value:string)=>{if(/['\r\n]/.test(value))throw new Error("Setting contains an unsupported character");return `'${value}'`;};
const whole=(min:number,max=Number.MAX_SAFE_INTEGER)=>(value:unknown)=>Number.isSafeInteger(value)&&(value as number)>=min&&(value as number)<=max;
const oneOf=(...names:string[])=>(value:unknown)=>typeof value==="string"&&names.includes(value);
const isWei=(value:unknown)=>typeof value==="string"&&/^(0|[1-9]\d{0,37})$/.test(value);
/** The keeper settings a chain profile's keeper block may carry: profile key, setting, rule (the keeper's own ranges). They are written in
 * this order after the generator's fixed settings; the ones in IN_PLACE replace values the generator always wrote. */
const PROFILE_SETTINGS:ReadonlyArray<readonly [keyof KeeperProfile,string,(value:unknown)=>boolean]>=[
  ["softDepthBlocks","SOFT_DEPTH_BLOCKS",whole(0,16)],
  ["finalityAuditIntervalSeconds","FINALITY_AUDIT_INTERVAL_SECONDS",whole(5,300)],
  ["finalityAuditMaxLagSeconds","FINALITY_AUDIT_MAX_LAG_SECONDS",whole(600,7200)],
  ["gasModel","GAS_MODEL",oneOf("standard","arbitrum")],
  ["l1GasMarginBps","L1_GAS_MARGIN_BPS",whole(0,20000)],
  ["coordinatorKind","COORDINATOR_KIND",oneOf("epoch","round")],
  ["idleHeartbeatSeconds","IDLE_HEARTBEAT_SECONDS",whole(2,30)],
  ["sequencerDropSeconds","SEQUENCER_DROP_SECONDS",whole(2,60)],
  ["wsSilenceSeconds","WS_SILENCE_SECONDS",whole(5,600)],
  ["wsBackfillMaxBlocks","WS_BACKFILL_MAX_BLOCKS",whole(1,100_000)],
  ["wsBackfillRange","WS_BACKFILL_RANGE",whole(1,10_000)],
  ["indexFinality","INDEX_FINALITY",oneOf("finalized","soft")],
  ["indexMaxBlocks","INDEX_MAX_BLOCKS",whole(1,10_000)],
  ["indexLookbackBlocks","INDEX_LOOKBACK_BLOCKS",whole(1,10_000)],
  ["sweepMinReserveWei","SWEEP_MIN_RESERVE_WEI",isWei],
  ["nonceStuckSeconds","NONCE_STUCK_SECONDS",whole(1,3600)],
  ["feeCoverageBps","FEE_COVERAGE_BPS",whole(0,100_000)],
  ["telegramLowBalanceWei","TELEGRAM_LOW_BALANCE_WEI",isWei],
];
const IN_PLACE=new Set<keyof KeeperProfile>(["nonceStuckSeconds","feeCoverageBps","telegramLowBalanceWei"]);
// Settings that only mean something under another mode: soft finality, or the Arbitrum gas model.
const SOFT_ONLY=new Set<keyof KeeperProfile>(["softDepthBlocks","finalityAuditIntervalSeconds","finalityAuditMaxLagSeconds"]);
const ARBITRUM_ONLY=new Set<keyof KeeperProfile>(["l1GasMarginBps","sequencerDropSeconds"]);
// A round keeper's idle heartbeat: an epoch keeper refuses IDLE_HEARTBEAT_SECONDS. Its 2 to 30 seconds, with the generator's
// SEND_MARGIN_SECONDS of 5 and a 10-second fulfillment round, always fit the keeper's 60-second request deadline.
const ROUND_ONLY=new Set<keyof KeeperProfile>(["idleHeartbeatSeconds"]);
// A round coordinator pays the keeper 80% of each fee, so the keeper sends only when 1.25 times the expected cost is escrowed; the keeper
// refuses a lower FEE_COVERAGE_BPS, or none, on such a chain.
const ROUND_MIN_FEE_COVERAGE_BPS=12_500;
// The keeper's own defaults for the backfill window (ChainSettings::default in keeper/src/config.rs). It compares the values it ends up with, so a
// setting the profile leaves out counts as its default there.
const KEEPER_DEFAULT_WS_BACKFILL={maxBlocks:5_000,range:500};
/** The profile's keeper settings, checked, as profile key to value. A key the table does not know, a value outside its range and a setting
 * for a mode the profile does not use are refused by name; no value is quoted. The ranges and the rules between settings are the keeper's:
 * a profile this accepts is one the keeper starts with. A round coordinator's profile writes its fee coverage down, at least 12500. */
function profileValues(chain:Chain):Map<keyof KeeperProfile,string>{
  const keeper=chain.keeper as Record<string,unknown>|undefined,values=new Map<keyof KeeperProfile,string>();
  if(keeper===undefined)return values;
  if(typeof keeper!=="object"||keeper===null||Array.isArray(keeper))throw new Error("The chain profile's keeper settings must be an object");
  const known=new Set<string>(PROFILE_SETTINGS.map(([key])=>key));
  for(const key of Object.keys(keeper))if(!known.has(key))throw new Error(`Unknown keeper setting ${key} in the chain profile`);
  for(const [key,,valid] of PROFILE_SETTINGS){
    const value=keeper[key];
    if(value===undefined)continue;
    if(!valid(value))throw new Error(`Invalid keeper setting ${key} in the chain profile`);
    if(SOFT_ONLY.has(key)&&chain.finality!=="soft"||key==="indexFinality"&&value==="soft"&&chain.finality!=="soft")throw new Error(`Keeper setting ${key} needs soft finality`);
    if(ARBITRUM_ONLY.has(key)&&keeper.gasModel!=="arbitrum")throw new Error(`Keeper setting ${key} needs the arbitrum gas model`);
    if(ROUND_ONLY.has(key)&&keeper.coordinatorKind!=="round")throw new Error(`Keeper setting ${key} needs coordinatorKind round`);
    values.set(key,String(value));
  }
  const number=(key:keyof KeeperProfile,fallback:number)=>values.has(key)?Number(values.get(key)):fallback;
  if(values.get("coordinatorKind")==="round"&&number("feeCoverageBps",0)<ROUND_MIN_FEE_COVERAGE_BPS)
    throw new Error(`Keeper setting feeCoverageBps must be set to at least ${ROUND_MIN_FEE_COVERAGE_BPS} with coordinatorKind round`);
  if(number("wsBackfillRange",KEEPER_DEFAULT_WS_BACKFILL.range)>number("wsBackfillMaxBlocks",KEEPER_DEFAULT_WS_BACKFILL.maxBlocks))
    throw new Error("Keeper setting wsBackfillRange must not exceed wsBackfillMaxBlocks (one the profile leaves out counts as the keeper's default)");
  return values;
}
/** A reporting endpoint the keeper accepts: HTTPS, a host, and no credentials, query or fragment. */
const publicUrl=(value:string,max=4096)=>{
  const url=new URL(value);
  return value.length<=max&&url.protocol==="https:"&&url.hostname!==""&&!url.username&&!url.password&&!url.search&&!url.hash;
};
// The official sequencer feed speaks its own protocol, not JSON-RPC: the keeper's event stream cannot use it. A host name may end in a dot
// (feed.mainnet.chain.robinhood.com.) and still name the same host, so the host is lower-cased and loses its trailing dots before the check.
const SEQUENCER_FEED=/^feed\.[a-z0-9.-]+\.chain\.robinhood\.com$/;
const isSequencerFeed=(url:string)=>SEQUENCER_FEED.test(new URL(url).hostname.toLowerCase().replace(/\.+$/,""));

/** Docker keeper.env for a deployment: pins from the private deployment record, caps and keeper settings from the chain profile, sending
 * off. Private RPC URLs go after the chain's first public endpoint (rpcOrder "public-first", the default, so the public endpoint stays
 * preferred for reads) or before all of them ("private-first", for a public endpoint that is rate limited and keeps little state, which is
 * why a private-first chain given no private endpoint is refused unless secrets.allowPublicOnly says the public endpoint alone is meant).
 * A profile's finality and keeper block add the settings of keeper 0.5.0; a profile without them yields exactly the earlier file. A profile
 * with coordinatorKind "round" gets no registry pin: its coordinator's own pins are the only ones.
 * Secrets are only placed in the returned text; callers write it with mode 0600 and never log it.
 * A follower gets KEEPER_ROLE=follower, the join rule's defaults and the keys volume of the named instance
 * `<chain>-follower` (deploy/docker/README.md); its Telegram commands stay off by default. */
export function renderKeeperEnv(chain:Chain,deployment:DeploymentRecord,secrets:KeeperSecrets,role:KeeperRole="primary"):string{
  if(role!=="primary"&&role!=="follower")throw new Error("The keeper role must be primary or follower");
  if(deployment.chainId!==chain.chainId)throw new Error("Deployment record is for another chain");
  if(chain.rpcOrder!==undefined&&chain.rpcOrder!=="public-first"&&chain.rpcOrder!=="private-first")throw new Error("Invalid RPC order in the chain profile");
  if(chain.finality!==undefined&&chain.finality!=="finalized"&&chain.finality!=="soft")throw new Error("Invalid finality in the chain profile");
  const privateUrls=[...(secrets.privateRpcUrl?[secrets.privateRpcUrl]:[]),...(secrets.privateRpcUrls??[])];
  // A private-first chain's public endpoint is rate limited and keeps little state (about 6,000 blocks on Robinhood): alone it is a choice to state.
  if(chain.rpcOrder==="private-first"&&privateUrls.length===0&&secrets.allowPublicOnly!==true)
    throw new Error(`${chain.key} lists its public endpoint last because it is rate limited and keeps little state: give a private endpoint (--rpc-var), or accept the public endpoint alone with --allow-public-only`);
  const rpcUrls=chain.rpcOrder==="private-first"?[...privateUrls,...chain.rpcUrls]:[chain.rpcUrls[0],...privateUrls,...chain.rpcUrls.slice(1)];
  if(rpcUrls.some(url=>new URL(url).protocol!=="https:"))throw new Error("RPC URLs must use HTTPS");
  const wsUrls=secrets.wsUrls??[];
  if(wsUrls.some(url=>new URL(url).protocol!=="wss:"))throw new Error("WebSocket URLs must use WSS");
  if(wsUrls.some(isSequencerFeed))throw new Error("WebSocket URLs must be JSON-RPC endpoints, not a sequencer feed");
  if((secrets.telegramBotToken===undefined)!==(secrets.telegramChatId===undefined))throw new Error("Telegram needs both the bot token and a chat id");
  if(secrets.telegramChatId!==undefined&&!/^-?\d{1,20}$/.test(secrets.telegramChatId))throw new Error("Telegram chat id must be numeric");
  if(secrets.neonDb!==undefined&&!/^postgres(?:ql)?:\/\/.*sslmode=require.*channel_binding=require/.test(secrets.neonDb))throw new Error("NEON_DB must require TLS and channel binding");
  const health=secrets.healthApiUrl!==undefined||secrets.healthApiKey!==undefined;
  if(health&&(secrets.healthApiUrl===undefined||secrets.healthApiKey===undefined))throw new Error("Health reporting needs both the endpoint and the key");
  if(health&&!publicUrl(secrets.healthApiUrl!))throw new Error("The health endpoint must be HTTPS without credentials, query or fragment");
  if(health&&!/^[\x21-\x7e]{1,4096}$/.test(secrets.healthApiKey!))throw new Error("Invalid health key");
  if(!health&&secrets.healthIntervalSeconds!==undefined)throw new Error("A health interval needs the health endpoint and key");
  if(secrets.healthIntervalSeconds!==undefined&&!whole(5,3600)(secrets.healthIntervalSeconds))throw new Error("The health interval must be 5 to 3600 seconds");
  const discord=secrets.discordBotToken!==undefined||secrets.discordChannelId!==undefined;
  if(discord&&(secrets.discordBotToken===undefined||secrets.discordChannelId===undefined))throw new Error("Discord needs both the bot token and a channel id");
  if(discord&&!/^[\x21-\x7e]{16,256}$/.test(secrets.discordBotToken!))throw new Error("Invalid Discord bot token");
  if(discord&&!(/^[1-9]\d{0,19}$/.test(secrets.discordChannelId!)&&BigInt(secrets.discordChannelId!)<2n**64n))throw new Error("Discord channel id must be a number");
  if(secrets.discordPublicExplorerUrl!==undefined&&(!discord||!publicUrl(secrets.discordPublicExplorerUrl,256)))throw new Error("The public explorer URL needs Discord and must be HTTPS without credentials, query or fragment");
  const keeper=profileValues(chain),{minTip,maxTip}=tipBounds(chain.gas),round=keeper.get("coordinatorKind")==="round";
  // A round coordinator has no registry, so a record that pins one is not a record of this deployment, and the keeper refuses the pin.
  if(round&&"epochImplementationCodeHash" in deployment)throw new Error("The deployment record pins a registry implementation, which a round coordinator does not have");
  const settings:[string,string][]=[
    ["CHAIN_ID",String(chain.chainId)],["NATIVE_CURRENCY_SYMBOL",chain.nativeCurrency.symbol],["EXPLORER_URL",chain.explorerUrl],
    ["RPC_URLS",rpcUrls.join(",")],
    ...(wsUrls.length?[["WS_URLS",wsUrls.join(",")] as [string,string]]:[]),
    ["COORDINATOR_ADDRESS",getAddress(deployment.coordinator)],
    ["EXPECTED_CODE_HASH",hash(deployment.coordinatorCodeHash,"coordinator code hash")],
    ["EXPECTED_PROTOCOL_HASH",hash(deployment.protocolConfigurationHash,"protocol configuration hash")],
    ["EXPECTED_IMPLEMENTATION_CODE_HASH",hash(deployment.coordinatorImplementationCodeHash,"coordinator implementation hash")],
    ...(round?[]:[["EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH",hash(deployment.epochImplementationCodeHash,"registry implementation hash")] as [string,string]]),
    ["KEEPER_DB",`/var/lib/d20dao/keeper-${chain.key}.sqlite`],
    ["TX_KEY_FILE","/etc/d20dao/transaction.key"],["VRF_KEY_FILE","/etc/d20dao/vrf.key"],
    ["SEND_TRANSACTIONS","false"],["POLL_MS","250"],["MAX_TICK_FAILURES","5"],["TICK_TIMEOUT_SECONDS","20"],["SEND_MARGIN_SECONDS","5"],
    ["MAX_GAS",String(chain.gas.maxGas)],["MAX_FEE_PER_GAS_WEI",chain.gas.maxFeePerGasWei],
    ["CANCEL_MAX_FEE_PER_GAS_WEI",chain.gas.cancelMaxFeePerGasWei],["MAX_TX_COST_WEI",chain.gas.maxTxCostWei],
    ["MIN_PRIORITY_FEE_WEI",String(minTip)],["MAX_PRIORITY_FEE_WEI",String(maxTip)],["FEE_COVERAGE_BPS",keeper.get("feeCoverageBps")??"10000"],["FULFILL_BATCH_MAX","16"],
    ["NONCE_STUCK_SECONDS",keeper.get("nonceStuckSeconds")??"120"],["PROGRESS_STUCK_SECONDS","20"],["RUST_LOG","d20dao_keeper=info"],
    ...(chain.finality===undefined?[]:[["FINALITY_MODE",chain.finality] as [string,string]]),
    ...PROFILE_SETTINGS.flatMap(([key,setting])=>IN_PLACE.has(key)||!keeper.has(key)?[]:[[setting,keeper.get(key)!] as [string,string]]),
  ];
  if(role==="follower")settings.push(["KEEPER_ROLE","follower"],["FOLLOWER_DELAY_SECONDS","20"],["FOLLOWER_QUEUE_JOIN","150"],["PRIMARY_LIVENESS_SECONDS","10"]);
  if(secrets.telegramBotToken!==undefined)settings.push(["TELEGRAM_BOT_TOKEN",secrets.telegramBotToken],["TELEGRAM_CHAT_ID",secrets.telegramChatId!],
    // A fulfillment batch at mainnet gas needs well over the testnet default of 0.05.
    ["TELEGRAM_LOW_BALANCE_WEI",keeper.get("telegramLowBalanceWei")??(chain.testnet?"50000000000000000":"1000000000000000000")]);
  if(secrets.neonDb!==undefined)settings.push(["NEON_DB",secrets.neonDb]);
  if(health)settings.push(["HEALTH_API_URL",secrets.healthApiUrl!],["HEALTH_API_KEY",secrets.healthApiKey!],["HEALTH_INTERVAL_SECONDS",String(secrets.healthIntervalSeconds??30)]);
  if(discord)settings.push(["DISCORD_BOT_TOKEN",secrets.discordBotToken!],["DISCORD_PROTOCOL_CHANNEL_ID",secrets.discordChannelId!],
    ...(secrets.discordPublicExplorerUrl===undefined?[]:[["DISCORD_PUBLIC_EXPLORER_URL",secrets.discordPublicExplorerUrl] as [string,string]]));
  const lines=settings.map(([name,value])=>`${name}=${quoted(value)}`);
  // keeper.sh reads the keys volume name only unquoted.
  lines.push(role==="follower"?`KEYS_VOLUME=d20dao-${chain.key}-follower-keys`:`KEYS_VOLUME=d20dao-keys-${chain.key}`);
  return `# Generated for ${chain.name} (chain ${chain.chainId}, ${role} keeper). Secret: keep mode 0600, never print or commit.\n${lines.join("\n")}\n`;
}
