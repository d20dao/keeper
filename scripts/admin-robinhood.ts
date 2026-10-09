// The owner's tool for a round coordinator deployed by scripts/deploy-robinhood.ts. Each action prints exactly what it will call (the
// function, its arguments, the calldata, the sender) with every check it made and its simulation from the owner. It sends only with --send,
// from the owner key in --env, when the owner is an account without code: on a test network, or on a production chain its interim owner,
// with --mainnet too. A production chain's owner is otherwise the Safe its profile names, strictly checked (scripts/lib/safe.ts): there the
// action prints the Safe Transaction Builder file to load in the Safe app (written to --safe-out too), and --send is refused.
//
//   node scripts/admin-robinhood.ts <action> [arguments] --manifest deployments/robinhood-testnet.json [--send --env <owner env file> [--mainnet]]
//     [--safe-out <file>] [--private-directory deployments/private/<chain>/administration] [--rpc-url <local node URL>]
//
//   status [--address <wallet>]                    every view, whether the proxy runs the manifest's implementation, and on a production
//                                                  chain each step of the ownership transfer to its Safe and the next one
//   set-keeper <address>                           setKeeper: the primary keeper wallet
//   set-backup-keeper <address> <true|false>       setBackupKeeper: allow or remove a backup keeper wallet (at most 4)
//   set-pricing <minFeeWei> <multiplier> <gas>     setPricing: minimum fee, base-fee multiplier and fulfillment gas overhead
//   set-fee-recipient <address>                    setFeeRecipient
//   set-keeper-bps <bps>                           setKeeperFeeBps: the keeper's share of each fee, 0 to 10000
//   set-refund-bps <bps>                           setRefundBps: the share refunded on expiry, 5000 to 10000
//   register-beacon <beacon file>                  registerBeacon, from a file shaped like config/beacons/drand-evmnet.json
//   schedule-beacon <beaconId> <fromTime>          scheduleBeacon: at least 600 seconds ahead of the latest block (660 when sent here)
//   cancel-beacon-schedule                         cancelBeaconSchedule: drop the pending change
//   transfer-ownership <address>                   transferOwnership (on a production chain only to its profile's Safe, strictly checked)
//   accept-ownership                               acceptOwnership, from the pending owner
//   upgrade <implementation> --approve-code-hash <hash> [--approve-layout-hash <hash>]
//                                                  upgradeToAndCall(implementation, 0x), once the code is the approved and compiled one
//
// The env file holds the owner key as DEPLOYER_ADDRESS and DEPLOYER_KEY. --rpc-url replaces the profile's public endpoint with a local node
// (127.0.0.1, localhost or [::1]), for a rehearsal on a local chain.
import {parseArgs} from "node:util";
import {readFile,writeFile} from "node:fs/promises";
import {resolve} from "node:path";
import {JsonRpcProvider} from "ethers";
import {loadChain,requireOwner,requireRoundCoordinator} from "./lib/chains.ts";
import {loadDeployer} from "./lib/deployer-env.ts";
import {Stop,compileContracts,privateDirectory} from "./lib/deployment.ts";
import {localRpcUrl} from "./lib/robinhood-deploy.ts";
import {ADMIN_ACTIONS,ACTION_ARGUMENTS,parseAdminRequest,parseRoundManifest,planAdminAction,sendAdminAction} from "./lib/robinhood-admin.ts";

const USAGE=`Usage: node scripts/admin-robinhood.ts <action> [arguments] --manifest <deployments/robinhood-…json> [--send --env <owner env file>]; actions: ${ADMIN_ACTIONS.map(action=>`${action} ${ACTION_ARGUMENTS[action]}`.trim()).join("; ")}`;

async function main(){
  const {values,positionals}=parseArgs({allowPositionals:true,options:{manifest:{type:"string"},env:{type:"string"},send:{type:"boolean",default:false},
    "safe-out":{type:"string"},"private-directory":{type:"string"},"rpc-url":{type:"string"},"approve-code-hash":{type:"string"},"approve-layout-hash":{type:"string"},address:{type:"string"},
    mainnet:{type:"boolean",default:false}}});
  const [action,...args]=positionals;
  if(!values.manifest)throw new Stop(`Provide --manifest. ${USAGE}`);
  const manifestPath=resolve(values.manifest);
  let json:unknown;
  try{json=JSON.parse(await readFile(manifestPath,"utf8"));}catch{throw new Stop(`${manifestPath} could not be read as JSON`);}
  const manifest=parseRoundManifest(json,manifestPath);
  const chain=await loadChain(manifest.network);
  // Refused before any network access or key use: a chain without a decided owner, and one that runs no round coordinator.
  requireOwner(chain,"admin-robinhood.ts");
  requireRoundCoordinator(chain,"admin-robinhood.ts");
  if(manifest.chainId!==chain.chainId)throw new Stop("The manifest and the chain profile differ");
  if(values.mainnet&&chain.testnet)throw new Stop(`--mainnet acknowledges a send on a production chain, and ${chain.key} is a test network`);
  if(values.send&&!chain.testnet){
    if(!chain.interimOwner)throw new Stop(`${chain.key} is a production chain, owned by a Safe: run without --send and propose the printed Safe transaction`);
    if(!values.mainnet)throw new Stop(`${chain.key} is a production chain: a send there, from its interim owner only, also needs --mainnet`);
  }
  if(values.send&&!values.env)throw new Stop("--send needs --env with the owner's key file");
  if(values.send&&values["safe-out"])throw new Stop("--safe-out writes a Safe transaction, which is proposed in the Safe, not sent");
  const request=await parseAdminRequest(action,args,values);
  if(request.action==="status"&&(values.send||values["safe-out"]))throw new Stop("status sends nothing and proposes nothing");
  if(request.action==="upgrade"||request.action==="register-beacon")compileContracts();
  const rpcUrl=values["rpc-url"]===undefined?chain.rpcUrls[0]:localRpcUrl(values["rpc-url"]);
  const provider=new JsonRpcProvider(rpcUrl,chain.chainId,{staticNetwork:true,batchMaxCount:1});
  try {
    const plan=await planAdminAction({provider,chain,manifest,request,send:values.send});
    if(values["safe-out"]){
      if(!plan.safeTransaction)throw new Stop(`The ${plan.sender?.role??"owner"} is an account without code: there is no Safe transaction to write`);
      await writeFile(resolve(values["safe-out"]),JSON.stringify(plan.safeTransaction,null,2)+"\n",{flag:"wx"});
    }
    console.log(JSON.stringify({...plan,send:values.send,rpc:new URL(rpcUrl).origin,safeOut:values["safe-out"]?resolve(values["safe-out"]):undefined},null,2));
    if(!values.send)return;
    const signer=await loadDeployer(resolve(values.env!),provider);
    const journalDirectory=resolve(values["private-directory"]??`deployments/private/${chain.key}/administration`);
    await privateDirectory(journalDirectory);
    console.log(JSON.stringify({sent:true,action:plan.action,...await sendAdminAction({provider,chain,plan,signer,journalDirectory})},null,2));
  } finally {provider.destroy();}
}
// Every failure prints its reason. The key file is read only by loadDeployer, whose errors never include its contents.
main().catch(error=>{
  console.error(`Robinhood administration stopped: ${error instanceof Error?error.message:String(error)}`);
  if(!(error instanceof Stop))console.error("Credentials were not logged; inspect the plan and the local journal before running again.");
  process.exitCode=1;
});
