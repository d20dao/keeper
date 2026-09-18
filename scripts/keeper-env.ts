// Write the Docker keeper.env for a deployed chain. Secret values come from named settings in the operator
// env file and are written only to the new 0600 output file; nothing but setting names is printed.
import {parseArgs} from "node:util";
import {readFile,writeFile} from "node:fs/promises";
import {resolve,join,dirname} from "node:path";
import {loadChain} from "./lib/chains.ts";
import {loadEnvValue} from "./lib/deployer-env.ts";
import {privateDirectory} from "./lib/deployment.ts";
import {renderKeeperEnv} from "./lib/keeper-env.ts";

async function main(){
  const {values}=parseArgs({options:{chain:{type:"string",default:"arc-testnet"},env:{type:"string"},deployment:{type:"string"},out:{type:"string"},
    "rpc-var":{type:"string"},"telegram-token-var":{type:"string"},"telegram-chat-id":{type:"string"},"telegram-pairing":{type:"string"},"neon-var":{type:"string"},
    role:{type:"string",default:"primary"}}});
  if(values.role!=="primary"&&values.role!=="follower")throw new Error("--role must be primary or follower");
  if(!values.env)throw new Error("Provide --env with the operator settings file");
  const chain=await loadChain(values.chain);
  const deploymentPath=resolve(values.deployment??`deployments/private/${chain.key}/deployment.json`);
  const deployment=JSON.parse(await readFile(deploymentPath,"utf8"));
  const env=resolve(values.env);
  const secrets={
    privateRpcUrl:values["rpc-var"]?await loadEnvValue(env,values["rpc-var"]):undefined,
    telegramBotToken:values["telegram-token-var"]?await loadEnvValue(env,values["telegram-token-var"]):undefined,
    // A pairing record from scripts/pair-telegram.py keeps the chat id off the command line.
    telegramChatId:values["telegram-pairing"]?String(JSON.parse(await readFile(resolve(values["telegram-pairing"]),"utf8")).chatId):values["telegram-chat-id"],
    neonDb:values["neon-var"]?await loadEnvValue(env,values["neon-var"]):undefined,
  };
  const out=resolve(values.out??join(dirname(deploymentPath),values.role==="follower"?"keeper.follower.docker.env":"keeper.docker.env"));
  await privateDirectory(dirname(out));
  await writeFile(out,renderKeeperEnv(chain,deployment,secrets,values.role),{flag:"wx",mode:0o600});
  console.log(JSON.stringify({written:out,chain:chain.key,role:values.role,coordinator:deployment.coordinator,sendTransactions:false,
    privateRpc:Boolean(secrets.privateRpcUrl),telegram:Boolean(secrets.telegramBotToken),neonIndexer:Boolean(secrets.neonDb)},null,2));
}
main().catch(error=>{console.error(error instanceof Error?error.message:"keeper.env generation failed");process.exit(1);});
