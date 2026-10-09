// Write the Docker keeper.env for a deployed chain. Secret values come from named settings in the operator
// env file and are written only to the new 0600 output file; nothing but setting names is printed.
// --rpc-var and --ws-var may be given several times; their endpoints keep that order (the chain profile's rpcOrder places the public ones).
// A chain whose profile says "private-first" (a rate limited public endpoint that keeps little state) needs at least one --rpc-var, unless
// --allow-public-only states that the public endpoint alone is meant.
// --health-url, --health-key-var and --health-interval-seconds add the health report; --discord-token-var, --discord-channel-id and
// --discord-explorer-url add the public proof feed.
import {parseArgs} from "node:util";
import {readFile,writeFile} from "node:fs/promises";
import {resolve,join,dirname} from "node:path";
import {loadChain,requireOwner} from "./lib/chains.ts";
import {loadEnvValue} from "./lib/deployer-env.ts";
import {privateDirectory} from "./lib/deployment.ts";
import {renderKeeperEnv} from "./lib/keeper-env.ts";

async function main(){
  const {values}=parseArgs({options:{chain:{type:"string",default:"arc-testnet"},env:{type:"string"},deployment:{type:"string"},out:{type:"string"},
    "rpc-var":{type:"string",multiple:true},"ws-var":{type:"string",multiple:true},"allow-public-only":{type:"boolean",default:false},
    "telegram-token-var":{type:"string"},"telegram-chat-id":{type:"string"},"telegram-pairing":{type:"string"},"neon-var":{type:"string"},
    "health-url":{type:"string"},"health-key-var":{type:"string"},"health-interval-seconds":{type:"string"},
    "discord-token-var":{type:"string"},"discord-channel-id":{type:"string"},"discord-explorer-url":{type:"string"},
    role:{type:"string",default:"primary"}}});
  if(values.role!=="primary"&&values.role!=="follower")throw new Error("--role must be primary or follower");
  if(!values.env)throw new Error("Provide --env with the operator settings file");
  const chain=await loadChain(values.chain);
  // Sends nothing, so an Arbitrum chain is fine; a deployment record for a chain whose owner is not decided is not.
  requireOwner(chain,"keeper-env.ts");
  const deploymentPath=resolve(values.deployment??`deployments/private/${chain.key}/deployment.json`);
  const deployment=JSON.parse(await readFile(deploymentPath,"utf8"));
  const env=resolve(values.env);
  const named=(names:string[]|undefined)=>Promise.all((names??[]).map(name=>loadEnvValue(env,name)));
  const secrets={
    privateRpcUrls:await named(values["rpc-var"]),
    allowPublicOnly:values["allow-public-only"],
    wsUrls:await named(values["ws-var"]),
    telegramBotToken:values["telegram-token-var"]?await loadEnvValue(env,values["telegram-token-var"]):undefined,
    // A pairing record from scripts/pair-telegram.py keeps the chat id off the command line.
    telegramChatId:values["telegram-pairing"]?String(JSON.parse(await readFile(resolve(values["telegram-pairing"]),"utf8")).chatId):values["telegram-chat-id"],
    neonDb:values["neon-var"]?await loadEnvValue(env,values["neon-var"]):undefined,
    healthApiUrl:values["health-url"],
    healthApiKey:values["health-key-var"]?await loadEnvValue(env,values["health-key-var"]):undefined,
    healthIntervalSeconds:values["health-interval-seconds"]===undefined?undefined:Number(values["health-interval-seconds"]),
    discordBotToken:values["discord-token-var"]?await loadEnvValue(env,values["discord-token-var"]):undefined,
    discordChannelId:values["discord-channel-id"],discordPublicExplorerUrl:values["discord-explorer-url"],
  };
  const out=resolve(values.out??join(dirname(deploymentPath),values.role==="follower"?"keeper.follower.docker.env":"keeper.docker.env"));
  await privateDirectory(dirname(out));
  await writeFile(out,renderKeeperEnv(chain,deployment,secrets,values.role),{flag:"wx",mode:0o600});
  console.log(JSON.stringify({written:out,chain:chain.key,role:values.role,coordinator:deployment.coordinator,sendTransactions:false,
    privateRpc:secrets.privateRpcUrls.length>0,privateRpcUrls:secrets.privateRpcUrls.length,wsUrls:secrets.wsUrls.length,
    telegram:Boolean(secrets.telegramBotToken),neonIndexer:Boolean(secrets.neonDb),health:Boolean(secrets.healthApiUrl),discord:Boolean(secrets.discordBotToken)},null,2));
}
main().catch(error=>{console.error(error instanceof Error?error.message:"keeper.env generation failed");process.exit(1);});
