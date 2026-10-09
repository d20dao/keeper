// Deploy the round coordinator's contract set on a Robinhood chain through the chain's deterministic CREATE2 factory: the drand beacon
// verifier, the VRF proof verifier, the mapping library, the coordinator implementation and its proxy, initialized with the operator's VRF
// key, the chain's owner as owner and fee recipient, the keeper wallet, the service profile's pricing and the beacon file's registration.
// scripts/lib/robinhood-deploy.ts holds the plan, the checks and the manifest.
//
//   node scripts/deploy-robinhood.ts --chain robinhood-testnet --env <deployer env file> [--operator-directory <dir>] [--new-operator]
//     [--keeper <address>] [--backup-keeper <address>] [--manifest deployments/robinhood-testnet.json]
//     [--private-directory deployments/private/robinhood-testnet] [--rpc-url <local node URL>] [--send] [--mainnet [--interim-owner]]
//
// Without --send it is a dry run: it reads the chain, estimates every creation it can, and prints each planned transaction (its factory
// call, salt, init code hash, calldata size and hash, gas) and the expected code hash at each address; nothing is signed, sent or written.
// With --send it deploys what is missing, one transaction at a time, journaled in the private directory before each broadcast, checks the
// whole set and writes the public manifest. A run after a partial or complete deployment deploys only what is missing and checks the rest.
// Either way it runs only from reviewed code: no uncommitted change under contracts/, config/, scripts/ or package*.json, and a commit that
// is on a remote branch; the compiled helpers and implementation must have the reviewed code hashes of config/robinhood-create2.json.
//
// A production chain also needs --mainnet, and its owner must be the Safe its profile names (scripts/lib/safe.ts). With --mainnet
// --interim-owner the set starts instead with the profile's interimOwner, an account without code that must be the deployer, as its owner
// and fee recipient; the command prints a warning, and admin-robinhood.ts, which reads the transfer from the chain, moves the set to the
// Safe later (transfer-ownership, accept-ownership, set-fee-recipient; docs/robinhood.md).
//
// The env file holds DEPLOYER_ADDRESS and DEPLOYER_KEY. The operator directory (by default ~/.config/d20dao/<chain>) holds operator.json, the
// keeper wallet's key and the VRF key, bound to the set's first owner and this deployer; --new-operator creates them there, once, in a
// directory without operator.json, and only in a dry run. --keeper names the keeper wallet in place of the operator's (whose VRF key is
// still used), so that a keeper wallet already running elsewhere serves this chain too; --backup-keeper names a backup keeper wallet that the
// deployment allows with one setBackupKeeper call from the deployer, which must then be the set's first owner. --rpc-url replaces the
// profile's public endpoint with a local node (127.0.0.1, localhost or [::1]), for a rehearsal on a local chain.
import {parseArgs} from "node:util";
import {access} from "node:fs/promises";
import {join,resolve} from "node:path";
import {JsonRpcProvider,getAddress} from "ethers";
import {loadChain,requireOwner,requireRoundCoordinator} from "./lib/chains.ts";
import {loadDeployer} from "./lib/deployer-env.ts";
import {DEFAULT_OPERATOR_DIRECTORY,Stop,compileContracts,loadOrCreateOperator,privateDirectory} from "./lib/deployment.ts";
import {deployRoundSet,loadBeaconFile,loadCreate2Config,loadRoundService,localRpcUrl,planRoundSet,reviewedSource,type JournalStep} from "./lib/robinhood-deploy.ts";

const broadcast:Array<{step:JournalStep;hash:string}>=[];
const walletArgument=(text:string|undefined,flag:string)=>{
  if(text===undefined)return undefined;
  if(!/^0x[0-9a-fA-F]{40}$/.test(text)||BigInt(text)===0n)throw new Stop(`${flag} must be a nonzero address`);
  return getAddress(text.toLowerCase());
};

async function main(){
  const {values}=parseArgs({options:{chain:{type:"string"},env:{type:"string"},"operator-directory":{type:"string"},"new-operator":{type:"boolean",default:false},
    keeper:{type:"string"},"backup-keeper":{type:"string"},manifest:{type:"string"},"private-directory":{type:"string"},"rpc-url":{type:"string"},
    send:{type:"boolean",default:false},mainnet:{type:"boolean",default:false},"interim-owner":{type:"boolean",default:false}}});
  if(!values.chain)throw new Stop("Provide --chain with a Robinhood chain profile, such as robinhood-testnet");
  const chain=await loadChain(values.chain);
  // Refused before any network access or key use: a chain without a decided owner, and one that runs no round coordinator.
  requireOwner(chain,"deploy-robinhood.ts");
  requireRoundCoordinator(chain,"deploy-robinhood.ts");
  if(!chain.testnet&&!values.mainnet)throw new Stop(`${chain.key} is a production chain: a deployment there also needs --mainnet`);
  if(values["interim-owner"]&&chain.testnet)throw new Stop(`--interim-owner is for a production chain, whose Safe takes the set over later; ${chain.key}'s owner owns its set from the deployment`);
  if(values["interim-owner"]&&!chain.interimOwner)throw new Stop(`${chain.key}'s profile names no interimOwner: set it in chains.json or its owner overlay (deployments/private/<chain>.owner.json) to deploy with --interim-owner`);
  if(!values.env)throw new Stop("Provide --env with the deployer's env file (DEPLOYER_ADDRESS and DEPLOYER_KEY)");
  if(values["new-operator"]&&values.send)throw new Stop("--new-operator creates the operator identity in a dry run; run it without --send first");
  const keeper=walletArgument(values.keeper,"--keeper"),backupKeeper=walletArgument(values["backup-keeper"],"--backup-keeper");
  if(keeper!==undefined&&keeper===backupKeeper)throw new Stop("--keeper and --backup-keeper name the same wallet: a backup keeper needs its own wallet");
  const rpcUrl=values["rpc-url"]===undefined?chain.rpcUrls[0]:localRpcUrl(values["rpc-url"]);
  const service=await loadRoundService(chain.key),beacon=await loadBeaconFile(service.beacon,true),create2=await loadCreate2Config();
  const manifestPath=resolve(values.manifest??`deployments/${chain.key}.json`);
  const privateDir=resolve(values["private-directory"]??`deployments/private/${chain.key}`);
  const operatorDirectory=resolve(values["operator-directory"]??join(DEFAULT_OPERATOR_DIRECTORY,chain.key));
  const hasOperator=await access(join(operatorDirectory,"operator.json")).then(()=>true,error=>{if(error?.code!=="ENOENT")throw new Stop("Operator metadata could not be read");return false;});
  if(hasOperator&&values["new-operator"])throw new Stop(`${operatorDirectory} already holds an operator identity; run without --new-operator to use it`);
  if(!hasOperator&&!values["new-operator"])throw new Stop(`No operator.json in ${operatorDirectory}; check --operator-directory, or run a dry run with --new-operator to create the keeper wallet and VRF key there`);
  // The set's first owner, which is also its fee recipient: the chain's owner, or with --interim-owner the profile's interim owner.
  const owner=values["interim-owner"]?chain.interimOwner!:chain.owner;
  if(values["interim-owner"])console.error([
    `WARNING: ${chain.key}'s set starts with the interim owner ${owner}, an account without code, as its owner and fee recipient, not with its Safe ${chain.owner}.`,
    "Until the Safe accepts ownership and becomes the fee recipient, that one key can upgrade the coordinator and change its keepers, pricing and fee",
    "recipient. admin-robinhood.ts status reads the transfer from the chain. Move the set with admin-robinhood.ts transfer-ownership,",
    "accept-ownership and set-fee-recipient."].join("\n"));
  // Only reviewed code is deployed: a clean working tree at a pushed commit.
  const source=reviewedSource({localRpc:values["rpc-url"]!==undefined});
  compileContracts();
  const provider=new JsonRpcProvider(rpcUrl,chain.chainId,{staticNetwork:true,batchMaxCount:1});
  try {
    const wallet=await loadDeployer(resolve(values.env),provider);
    // The owner is passed explicitly: the operator identity is bound to the set's first owner, which is also the fee recipient.
    const operator=await loadOrCreateOperator(operatorDirectory,wallet.address,owner);
    if(values.send)await privateDirectory(privateDir);
    const plan=await planRoundSet({chain,service,beacon,create2,roles:{owner:operator.owner,feeRecipient:operator.feeRecipient,keeper:keeper??operator.keeper.address,
      publicKey:[BigInt(operator.vrf.publicKey[0]),BigInt(operator.vrf.publicKey[1])],...(backupKeeper===undefined?{}:{backupKeeper})},
      ...(values["interim-owner"]?{interim:{finalOwner:chain.owner}}:{})});
    const report=await deployRoundSet({provider,chain,plan,deployer:wallet.address,wallet,send:values.send,journalPath:join(privateDir,"round-deployment.jsonl"),manifestPath,
      source,onBroadcast:(step,hash)=>broadcast.push({step,hash})});
    console.log(JSON.stringify({...report,keeperSource:keeper===undefined?"operator":"--keeper",rpc:new URL(rpcUrl).origin},null,2));
  } finally {provider.destroy();}
}
// Every failure prints its reason, and one after a broadcast lists what was handed to the RPC. Keys are read only by loadDeployer and
// loadOrCreateOperator, whose errors never include them.
main().catch(error=>{
  console.error(`Robinhood deployment stopped: ${error instanceof SyntaxError?"an input file could not be parsed":error instanceof Error?error.message:String(error)}`);
  if(broadcast.length)console.error(`Handed to the RPC, so funds may have moved; check each receipt: ${broadcast.map(({step,hash})=>`${step} ${hash}`).join(", ")}`);
  if(!(error instanceof Stop))console.error("Credentials were not logged; inspect the local configuration and the deployment journal before running again.");
  process.exitCode=1;
});
