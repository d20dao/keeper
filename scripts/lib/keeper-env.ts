import {getAddress} from "ethers";
import type {Chain} from "./chains.ts";

export type DeploymentRecord={chainId:number;coordinator:string;coordinatorCodeHash:string;protocolConfigurationHash:string;
  coordinatorImplementationCodeHash:string;epochImplementationCodeHash:string};
export type KeeperSecrets={privateRpcUrl?:string;telegramBotToken?:string;telegramChatId?:string;neonDb?:string};
export type KeeperRole="primary"|"follower";

const hash=(value:string,name:string)=>{if(!/^0x[0-9a-fA-F]{64}$/.test(value))throw new Error(`Invalid ${name}`);return value.toLowerCase();};
const quoted=(value:string)=>{if(/['\r\n]/.test(value))throw new Error("Setting contains an unsupported character");return `'${value}'`;};

/** Docker keeper.env for a deployment: pins from the private deployment record, caps from the chain
 * profile, sending off. The private RPC goes second so the public endpoint stays preferred for reads.
 * Secrets are only placed in the returned text; callers write it with mode 0600 and never log it.
 * A follower gets KEEPER_ROLE=follower, the join rule's defaults and the keys volume of the named instance
 * `<chain>-follower` (deploy/docker/README.md); its Telegram commands stay off by default. */
export function renderKeeperEnv(chain:Chain,deployment:DeploymentRecord,secrets:KeeperSecrets,role:KeeperRole="primary"):string{
  if(role!=="primary"&&role!=="follower")throw new Error("The keeper role must be primary or follower");
  if(deployment.chainId!==chain.chainId)throw new Error("Deployment record is for another chain");
  const rpcUrls=[chain.rpcUrls[0],...(secrets.privateRpcUrl?[secrets.privateRpcUrl]:[]),...chain.rpcUrls.slice(1)];
  if(rpcUrls.some(url=>new URL(url).protocol!=="https:"))throw new Error("RPC URLs must use HTTPS");
  if((secrets.telegramBotToken===undefined)!==(secrets.telegramChatId===undefined))throw new Error("Telegram needs both the bot token and a chat id");
  if(secrets.telegramChatId!==undefined&&!/^-?\d{1,20}$/.test(secrets.telegramChatId))throw new Error("Telegram chat id must be numeric");
  if(secrets.neonDb!==undefined&&!/^postgres(?:ql)?:\/\/.*sslmode=require.*channel_binding=require/.test(secrets.neonDb))throw new Error("NEON_DB must require TLS and channel binding");
  const settings:[string,string][]=[
    ["CHAIN_ID",String(chain.chainId)],["NATIVE_CURRENCY_SYMBOL",chain.nativeCurrency.symbol],["EXPLORER_URL",chain.explorerUrl],
    ["RPC_URLS",rpcUrls.join(",")],
    ["COORDINATOR_ADDRESS",getAddress(deployment.coordinator)],
    ["EXPECTED_CODE_HASH",hash(deployment.coordinatorCodeHash,"coordinator code hash")],
    ["EXPECTED_PROTOCOL_HASH",hash(deployment.protocolConfigurationHash,"protocol configuration hash")],
    ["EXPECTED_IMPLEMENTATION_CODE_HASH",hash(deployment.coordinatorImplementationCodeHash,"coordinator implementation hash")],
    ["EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH",hash(deployment.epochImplementationCodeHash,"registry implementation hash")],
    ["KEEPER_DB",`/var/lib/d20dao/keeper-${chain.key}.sqlite`],
    ["TX_KEY_FILE","/etc/d20dao/transaction.key"],["VRF_KEY_FILE","/etc/d20dao/vrf.key"],
    ["SEND_TRANSACTIONS","false"],["POLL_MS","250"],["MAX_TICK_FAILURES","5"],["TICK_TIMEOUT_SECONDS","20"],["SEND_MARGIN_SECONDS","5"],
    ["MAX_GAS",String(chain.gas.maxGas)],["MAX_FEE_PER_GAS_WEI",chain.gas.maxFeePerGasWei],
    ["CANCEL_MAX_FEE_PER_GAS_WEI",chain.gas.cancelMaxFeePerGasWei],["MAX_TX_COST_WEI",chain.gas.maxTxCostWei],
    ["MIN_PRIORITY_FEE_WEI","1000000000"],["MAX_PRIORITY_FEE_WEI","50000000000"],["FEE_COVERAGE_BPS","10000"],["FULFILL_BATCH_MAX","16"],
    ["NONCE_STUCK_SECONDS","120"],["PROGRESS_STUCK_SECONDS","20"],["RUST_LOG","d20dao_keeper=info"],
  ];
  if(role==="follower")settings.push(["KEEPER_ROLE","follower"],["FOLLOWER_DELAY_SECONDS","20"],["FOLLOWER_QUEUE_JOIN","150"],["PRIMARY_LIVENESS_SECONDS","10"]);
  if(secrets.telegramBotToken!==undefined)settings.push(["TELEGRAM_BOT_TOKEN",secrets.telegramBotToken],["TELEGRAM_CHAT_ID",secrets.telegramChatId!],
    // A fulfillment batch at mainnet gas needs well over the testnet default of 0.05.
    ["TELEGRAM_LOW_BALANCE_WEI",chain.testnet?"50000000000000000":"1000000000000000000"]);
  if(secrets.neonDb!==undefined)settings.push(["NEON_DB",secrets.neonDb]);
  const lines=settings.map(([name,value])=>`${name}=${quoted(value)}`);
  // keeper.sh reads the keys volume name only unquoted.
  lines.push(role==="follower"?`KEYS_VOLUME=d20dao-${chain.key}-follower-keys`:`KEYS_VOLUME=d20dao-keys-${chain.key}`);
  return `# Generated for ${chain.name} (chain ${chain.chainId}, ${role} keeper). Secret: keep mode 0600, never print or commit.\n${lines.join("\n")}\n`;
}
