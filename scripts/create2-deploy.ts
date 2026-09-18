// Deterministic UUPS service and restricted pilot client: locked implementations, atomic proxies.
import {parseArgs} from "node:util";
import {readFile,writeFile,open,access} from "node:fs/promises";
import {resolve,join} from "node:path";
import {Contract,JsonRpcProvider,keccak256,getBytes,parseUnits,formatUnits,getAddress} from "ethers";
import {loadDeployer,loadEnvValue} from "./lib/deployer-env.ts";
import {currentFee} from "./lib/gas.ts";
import {loadChain} from "./lib/chains.ts";
import {initialCatalogSigners,loadServiceCatalog} from "./lib/catalog.ts";
import {loadOrCreateOperator,validateNetwork,initCode,initialization,miningPlan,candidate,privateDirectory,compileContracts,compiledRuntimeCodeHash,DEFAULT_OPERATOR_DIRECTORY,Stop} from "./lib/deployment.ts";
const IMPLEMENTATION_SLOT="0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";
/** Transactions signed and handed to the RPC in this run, and their journal; a later failure lists them. */
const broadcast:{journal?:string;transactions:Array<{name:string;hash:string}>}={transactions:[]};

async function main(){
  const {values,positionals}=parseArgs({allowPositionals:true,options:{chain:{type:"string",default:"arc-testnet"},env:{type:"string"},directory:{type:"string"},"operator-directory":{type:"string"},"epoch-implementation":{type:"string"},"coordinator-implementation":{type:"string"},"client-implementation":{type:"string"},registry:{type:"string"},coordinator:{type:"string"},client:{type:"string"},apply:{type:"boolean",default:false},resume:{type:"boolean",default:false},mainnet:{type:"boolean",default:false},"new-operator":{type:"boolean",default:false}}});
  const mode=positionals[0]??"prepare";
  if(!["prepare","registry-plan","coordinator-plan","client-plan","deploy","epoch-implementation","coordinator-implementation"].includes(mode)||!values.env)throw new Stop("Invalid mode or missing --env");
  if(values["new-operator"]&&mode!=="prepare")throw new Stop("--new-operator applies only to prepare");
  const chain=await loadChain(values.chain);
  if(!chain.testnet&&!values.mainnet)throw new Stop("A mainnet deployment requires --mainnet");
  const profile=JSON.parse(await readFile("config/service.json","utf8"));
  const provider=new JsonRpcProvider(chain.rpcUrls[0],undefined,{batchMaxCount:1});
  try {
    const wallet=await loadDeployer(resolve(values.env),provider);
    const directory=resolve(values.directory??`deployments/private/${chain.key}`);
    // Upgrade path: only a new implementation, for an existing proxy to adopt through its owner.
    if(mode==="epoch-implementation"||mode==="coordinator-implementation"){
      await deployImplementation(provider,wallet,chain,directory,UPGRADE_CONTRACTS[mode],values[mode],values.apply);return;
    }
    // Reuse the same protected identities when adding a chain, keeping CREATE2 input stable. Fresh keeper and VRF keys
    // are created only by prepare --new-operator in a directory without operator.json, never for a mistyped directory.
    const operatorDirectory=resolve(values["operator-directory"]??DEFAULT_OPERATOR_DIRECTORY);
    const hasOperator=await access(join(operatorDirectory,"operator.json")).then(()=>true,error=>{if(error?.code!=="ENOENT")throw new Stop("Operator metadata could not be read");return false;});
    if(hasOperator&&values["new-operator"])throw new Stop(`${operatorDirectory} already holds an operator identity; rerun without --new-operator to use it`);
    if(!hasOperator&&!values["new-operator"])throw new Stop(`No operator.json in ${operatorDirectory}; check --operator-directory, or run prepare with --new-operator to create a new operator identity there`);
    if(!chain.testnet){
      // The DAO treasury Safe owns every proxy and receives the protocol share of each fee.
      const treasury=await loadEnvValue(resolve(values.env),"DAO_TREASURY"); // Shape-checked first: a getAddress error would quote the value.
      if(!/^(0x)?[0-9a-fA-F]{40}$/.test(treasury)||getAddress(treasury.toLowerCase())!==chain.owner)throw new Stop("DAO_TREASURY differs from the chain profile owner");
      if(await provider.getCode(chain.owner)==="0x")throw new Stop("The mainnet owner has no code; expected the treasury Safe");
    }
    const operator=await loadOrCreateOperator(operatorDirectory,wallet.address,chain.owner);
    await privateDirectory(directory);
    await validateNetwork(provider,chain);
    const fee=BigInt(profile.minFeeWei),keeperBps=Number(profile.keeperFeeBps); // Initial minimum fee only; live requests pay quoteFeeAt.
    const epochImplCode=await initCode("EpochEntropy",[]),coordinatorImplCode=await initCode("D20VRFCoordinator",[]),clientImplCode=await initCode("D20CostClient",[]);
    const writePlan=async(name:string,code:string,extra:object={})=>{
      const file=join(directory,`${name}-search.json`);
      await writeFile(file,JSON.stringify({...miningPlan(code,chain),contract:name,...extra},null,2)+"\n");return file;
    };
    if(mode==="prepare"){
      const plans=[await writePlan("epoch-implementation",epochImplCode),await writePlan("coordinator-implementation",coordinatorImplCode),await writePlan("client-implementation",clientImplCode)];
      console.log(JSON.stringify({mode,network:chain.key,chainId:chain.chainId,owner:operator.owner,feeRecipient:operator.feeRecipient,deployer:wallet.address,
        deployerBalance:formatUnits(await provider.getBalance(wallet.address),18),keeper:operator.keeper.address,keeperBalance:formatUnits(await provider.getBalance(operator.keeper.address),18),
        searchPlans:plans,minFee:formatUnits(fee,chain.nativeCurrency.decimals),keeperFeeBps:keeperBps},null,2));return;
    }
    if(!values["epoch-implementation"])throw new Stop("Missing epoch implementation result");
    const epochImpl=await candidate(resolve(values["epoch-implementation"]),epochImplCode,chain);
    // A new registry registers the built-in recipes and starts on the initial catalog (recipes 0-3); its owner schedules the rollout catalog afterwards.
    const registryInit=await initialization("EpochEntropy",[initialCatalogSigners((await loadServiceCatalog()).signers),operator.owner,operator.keeper.address]);
    const registryCode=await initCode("D20Proxy",[epochImpl.address,registryInit]);
    if(mode==="registry-plan"){
      console.log(JSON.stringify({mode,searchPlan:await writePlan("registry",registryCode,{implementation:epochImpl.address})},null,2));return;
    }
    if(!values["coordinator-implementation"]||!values.registry)throw new Stop("Missing coordinator implementation or registry result");
    const coordinatorImpl=await candidate(resolve(values["coordinator-implementation"]),coordinatorImplCode,chain);
    const registry=await candidate(resolve(values.registry),registryCode,chain);
    const coordinatorInit=await initialization("D20VRFCoordinator",[operator.vrf.publicKey,operator.owner,operator.feeRecipient,fee,profile.confirmations,registry.address,keeperBps]);
    const coordinatorCode=await initCode("D20Proxy",[coordinatorImpl.address,coordinatorInit]);
    if(mode==="coordinator-plan"){
      console.log(JSON.stringify({mode,searchPlan:await writePlan("coordinator",coordinatorCode,{implementation:coordinatorImpl.address,registry:registry.address})},null,2));return;
    }
    if(!values.coordinator)throw new Stop("Missing coordinator result");
    const coordinator=await candidate(resolve(values.coordinator),coordinatorCode,chain);
    if(!values["client-implementation"])throw new Stop("Missing cost client implementation result");
    const clientImpl=await candidate(resolve(values["client-implementation"]),clientImplCode,chain);
    const clientInit=await initialization("D20CostClient",[coordinator.address,operator.owner,wallet.address]);
    const clientCode=await initCode("D20Proxy",[clientImpl.address,clientInit]);
    if(mode==="client-plan"){
      console.log(JSON.stringify({mode,searchPlan:await writePlan("client",clientCode,{implementation:clientImpl.address,coordinator:coordinator.address})},null,2));return;
    }
    if(!values.client)throw new Stop("Missing cost client result");
    const client=await candidate(resolve(values.client),clientCode,chain);
    const steps=[{name:"epochImplementation",...epochImpl},{name:"coordinatorImplementation",...coordinatorImpl},{name:"clientImplementation",...clientImpl},{name:"registry",...registry},{name:"coordinator",...coordinator},{name:"client",...client}];
    // Priced from current gas (twice the base fee plus the median tip), so the budget is what these
    // gas limits can cost now rather than at the chain's fee cap.
    const {baseFee,tip,maxFee}=await currentFee(provider,BigInt(chain.gas.maxFeePerGasWei)),gasCaps=[5500000n,6500000n,3000000n,3000000n,2500000n,1000000n];
    const maxCost=gasCaps.reduce((sum,gas)=>sum+gas*maxFee,0n),balance=await provider.getBalance(wallet.address);
    const nonce=await wallet.getNonce("latest");
    if(await wallet.getNonce("pending")!==nonce)throw new Stop("Deployer has unresolved transactions");
    const plan={network:chain.key,chainId:chain.chainId,factory:chain.create2.factory,deployer:wallet.address,owner:operator.owner,feeRecipient:operator.feeRecipient,keeper:operator.keeper.address,
      ...Object.fromEntries(steps.map(step=>[step.name,step.address])),minFee:formatUnits(fee,chain.nativeCurrency.decimals),keeperFeeBps:keeperBps,
      baseFeeGwei:formatUnits(baseFee,"gwei"),maxFeePerGasGwei:formatUnits(maxFee,"gwei"),priorityFeeGwei:formatUnits(tip,"gwei"),maxDeploymentCost:formatUnits(maxCost,18),balance:formatUnits(balance,18),currency:chain.nativeCurrency.symbol,apply:values.apply};
    console.log(JSON.stringify(plan,null,2));
    if(!values.apply)return;
    if(balance<maxCost)throw new Stop(`Deployer balance is below the ${formatUnits(maxCost,18)} ${chain.nativeCurrency.symbol} budget`);
    const journalPath=join(directory,"create2-deployment.jsonl"),confirmed=new Set<string>();
    if(values.resume){
      // Resume continues only the same journaled plan: every signed step must have been confirmed.
      const entries=(await readFile(journalPath,"utf8")).trim().split("\n").map(line=>JSON.parse(line));
      const journaled=entries[0]?.type==="plan"?entries[0].plan:undefined;
      if(!journaled||["network","chainId","factory","deployer","owner","feeRecipient","keeper","minFee","keeperFeeBps",...steps.map(step=>step.name)].some(key=>journaled[key]!==plan[key as keyof typeof plan]))throw new Stop("Resume plan differs from the journaled plan");
      for(const entry of entries)if(entry.type==="confirmed")confirmed.add(entry.name);
      if(entries.some(entry=>entry.type==="signed-before-broadcast"&&!confirmed.has(entry.name)))throw new Stop("Journaled transaction is unconfirmed; inspect its receipt before resuming");
    }
    // CREATE2 binds each address to its exact init code, and implementation constructors only disable
    // initializers, so an implementation already at its address is that same contract and is reused.
    // Proxies carry initialization state and are never reused.
    const existing=new Set<string>();
    for(const [index,step] of steps.entries()){
      const code=await provider.getCode(step.address);
      if(confirmed.has(step.name)){if(code==="0x")throw new Stop("Journaled deployment has no code");continue;}
      if(code==="0x")continue;
      if(index>=3)throw new Stop("Candidate already has code; inspect before reuse");
      existing.add(step.name);
    }
    const journal=await open(journalPath,values.resume?"a":"ax",0o600);
    const record=async(value:unknown)=>{await journal.writeFile(JSON.stringify(value)+"\n");await journal.sync();};
    try {
      await record({type:values.resume?"resume":"plan",plan});
      for(const step of steps)if(existing.has(step.name))await record({type:"existing",name:step.name,address:step.address});
      let sent=0;
      for(const [index,step] of steps.entries()){
        if(confirmed.has(step.name)||existing.has(step.name))continue;
        const fresh=await validateNetwork(provider,chain);
        if(!fresh.baseFeePerGas||fresh.baseFeePerGas+tip>maxFee)throw new Stop("Base fee rose above the deployment price; rerun with --resume");
        const latest=await wallet.getNonce("latest"),pending=await wallet.getNonce("pending");
        if(latest!==nonce+sent||pending!==latest)throw new Stop("Deployer nonce changed");
        const gas=await provider.estimateGas({from:wallet.address,to:chain.create2.factory,data:step.data,value:0});
        if(gas>gasCaps[index])throw new Stop("CREATE2 gas cap exceeded");
        const raw=await wallet.signTransaction({to:chain.create2.factory,data:step.data,value:0,chainId:chain.chainId,type:2,nonce:latest,gasLimit:gasCaps[index],maxFeePerGas:maxFee,maxPriorityFeePerGas:tip});
        const hash=keccak256(getBytes(raw));
        await record({type:"signed-before-broadcast",name:step.name,address:step.address,hash,raw});
        broadcast.journal=journalPath;broadcast.transactions.push({name:step.name,hash});
        const tx=await provider.broadcastTransaction(raw);
        if(tx.hash!==hash)throw new Stop("Unexpected broadcast hash");
        const receipt=await tx.wait(1,60000);
        if(receipt?.status!==1||await provider.getCode(step.address)==="0x")throw new Stop("Deployment not confirmed");
        await record({type:"confirmed",name:step.name,address:step.address,hash,block:receipt.blockNumber,gasUsed:String(receipt.gasUsed),gasPrice:String(receipt.gasPrice),gasCost:String(receipt.fee)});
        sent++;
      }
    } finally {await journal.close();}
    const registryView=new Contract(registry.address,["function owner() view returns(address)","function committer() view returns(address)","function firstEpochStart() view returns(uint64)"],provider);
    const coordinatorView=new Contract(coordinator.address,["function owner() view returns(address)","function feeRecipient() view returns(address)","function initialFeeRecipient() view returns(address)","function keeperFeeBps() view returns(uint16)","function protocolConfigurationHash() view returns(bytes32)"],provider);
    if(await registryView.owner()!==operator.owner||await registryView.committer()!==operator.keeper.address||await coordinatorView.owner()!==operator.owner||await coordinatorView.feeRecipient()!==operator.feeRecipient||await coordinatorView.initialFeeRecipient()!==operator.feeRecipient||Number(await coordinatorView.keeperFeeBps())!==keeperBps)throw new Stop("Deployed role verification failed");
    const clientView=new Contract(client.address,["function owner() view returns(address)","function tester() view returns(address)","function coordinator() view returns(address)"],provider);
    if(await clientView.owner()!==operator.owner||await clientView.tester()!==wallet.address||await clientView.coordinator()!==coordinator.address)throw new Stop("Cost client initialization mismatch");
    for(const [proxy,implementation] of [[registry.address,epochImpl.address],[coordinator.address,coordinatorImpl.address],[client.address,clientImpl.address]])if(getAddress("0x"+(await provider.getStorage(proxy,IMPLEMENTATION_SLOT)).slice(-40))!==implementation)throw new Stop("Proxy implementation mismatch");
    const manifest={...plan,apply:undefined,...Object.fromEntries(await Promise.all(steps.map(async step=>[`${step.name}CodeHash`,keccak256(await provider.getCode(step.address))]))),
      protocolConfigurationHash:await coordinatorView.protocolConfigurationHash(),firstEpochStart:String(await registryView.firstEpochStart()),publicKey:operator.vrf.publicKey,createdAt:new Date().toISOString()};
    await writeFile(join(directory,"deployment.json"),JSON.stringify(manifest,null,2)+"\n",{flag:"wx",mode:0o600});
    const settings={CHAIN_ID:String(chain.chainId),RPC_URLS:chain.rpcUrls.join(","),NATIVE_CURRENCY_SYMBOL:chain.nativeCurrency.symbol,EXPLORER_URL:chain.explorerUrl,COORDINATOR_ADDRESS:coordinator.address,
      EXPECTED_CODE_HASH:manifest.coordinatorCodeHash,EXPECTED_PROTOCOL_HASH:manifest.protocolConfigurationHash,
      EXPECTED_IMPLEMENTATION_CODE_HASH:manifest.coordinatorImplementationCodeHash,EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH:manifest.epochImplementationCodeHash,
      KEEPER_DB:join(directory,"keeper.sqlite"),TX_KEY_FILE:operator.keeper.keyFile,VRF_KEY_FILE:operator.vrf.keyFile,SEND_TRANSACTIONS:"false",POLL_MS:"250",
      TICK_TIMEOUT_SECONDS:"20",MAX_TICK_FAILURES:"5",MAX_FEE_PER_GAS_WEI:chain.gas.maxFeePerGasWei,CANCEL_MAX_FEE_PER_GAS_WEI:chain.gas.cancelMaxFeePerGasWei,
      MAX_TX_COST_WEI:chain.gas.maxTxCostWei,MAX_GAS:String(chain.gas.maxGas),SEND_MARGIN_SECONDS:"5",PROGRESS_STUCK_SECONDS:"20",NONCE_STUCK_SECONDS:"120"};
    await writeFile(join(directory,"keeper.env"),Object.entries(settings).map(([key,value])=>`${key}=${value}`).join("\n")+"\n",{flag:"wx",mode:0o600});
    console.log(JSON.stringify({deployed:true,registry:registry.address,coordinator:coordinator.address,keeper:operator.keeper.address,sendingEnabled:false},null,2));
  } finally {provider.destroy();}
}
/** Mines (plan only) or deploys a new vanity CREATE2 EpochEntropy implementation. Prints the plan unless --apply. */
/// The two upgradeable service implementations, deployed on their own for an owner upgrade. The gas caps hold the
/// measured deployment cost (about 4.3M for the registry with its built-in recipes, about 5.0M for the coordinator).
const UPGRADE_CONTRACTS={
  "epoch-implementation":{mode:"epoch-implementation",artifact:"EpochEntropy",field:"epochImplementation",gasCap:5_500_000n,
    next:"Owner, one Safe batch of both upgrades on mainnet: admin.ts upgrade-registry --implementation (upgradeToAndCall with initializeRecipeRegistry), then upgrade-coordinator; schedule-catalog follows in a separate transaction"},
  "coordinator-implementation":{mode:"coordinator-implementation",artifact:"D20VRFCoordinator",field:"coordinatorImplementation",gasCap:6_500_000n,
    next:"Owner, after the registry upgrade in the same Safe batch: admin.ts upgrade-coordinator --implementation (upgradeToAndCall with empty data)"},
} as const;
type UpgradeContract=typeof UPGRADE_CONTRACTS[keyof typeof UPGRADE_CONTRACTS];
async function deployImplementation(provider:JsonRpcProvider,wallet:Awaited<ReturnType<typeof loadDeployer>>,chain:Awaited<ReturnType<typeof loadChain>>,directory:string,contract:UpgradeContract,result:string|undefined,apply:boolean){
  compileContracts();
  await privateDirectory(directory);
  await validateNetwork(provider,chain);
  const code=await initCode(contract.artifact,[]);
  if(!result){
    const searchPlan=join(directory,`${contract.mode}-upgrade-search.json`);
    await writeFile(searchPlan,JSON.stringify({...miningPlan(code,chain),contract:contract.mode},null,2)+"\n");
    console.log(JSON.stringify({mode:contract.mode,network:chain.key,chainId:chain.chainId,searchPlan,initCodeHash:keccak256(code),
      next:`Mine a d20da0 salt with scripts/vanity/search.py, then rerun with --${contract.mode} <result file>`},null,2));return;
  }
  const implementation=await candidate(resolve(result),code,chain);
  // UUPS embeds the implementation address, so the expected runtime hash is specific to the mined address.
  const runtimeCodeHash=await compiledRuntimeCodeHash(contract.artifact,implementation.address),existing=await provider.getCode(implementation.address);
  if(existing!=="0x"){
    if(keccak256(existing)!==runtimeCodeHash)throw new Stop("The candidate address already has different code");
    console.log(JSON.stringify({deployed:true,existing:true,[contract.field]:implementation.address,runtimeCodeHash},null,2));return;
  }
  const gasCap=contract.gasCap,{baseFee,tip,maxFee}=await currentFee(provider,BigInt(chain.gas.maxFeePerGasWei));
  const maxCost=gasCap*maxFee,balance=await provider.getBalance(wallet.address);
  const plan={mode:contract.mode,network:chain.key,chainId:chain.chainId,factory:chain.create2.factory,deployer:wallet.address,[contract.field]:implementation.address,salt:implementation.salt,
    initCodeHash:keccak256(code),runtimeCodeHash,baseFeeGwei:formatUnits(baseFee,"gwei"),maxFeePerGasGwei:formatUnits(maxFee,"gwei"),priorityFeeGwei:formatUnits(tip,"gwei"),
    maxDeploymentCost:formatUnits(maxCost,18),balance:formatUnits(balance,18),currency:chain.nativeCurrency.symbol,apply};
  console.log(JSON.stringify(plan,null,2));
  if(!apply)return;
  if(balance<maxCost)throw new Stop(`Deployer balance is below the ${formatUnits(maxCost,18)} ${chain.nativeCurrency.symbol} budget`);
  const nonce=await wallet.getNonce("latest");
  if(await wallet.getNonce("pending")!==nonce)throw new Stop("Deployer has unresolved transactions");
  const journalPath=join(directory,`${contract.mode}-${implementation.address}.jsonl`);
  if(await access(journalPath).then(()=>true,()=>false))throw new Stop("A journal for this implementation exists; inspect its signed transaction before retrying");
  const gas=await provider.estimateGas({from:wallet.address,to:chain.create2.factory,data:implementation.data,value:0});
  if(gas>gasCap)throw new Stop("CREATE2 gas cap exceeded");
  const raw=await wallet.signTransaction({to:chain.create2.factory,data:implementation.data,value:0,chainId:chain.chainId,type:2,nonce,gasLimit:gasCap,maxFeePerGas:maxFee,maxPriorityFeePerGas:tip});
  const hash=keccak256(getBytes(raw)),journal=await open(journalPath,"ax",0o600);
  const record=async(value:unknown)=>{await journal.writeFile(JSON.stringify(value)+"\n");await journal.sync();};
  try {
    await record({type:"plan",plan});
    await record({type:"signed-before-broadcast",name:contract.field,address:implementation.address,hash,raw});
    broadcast.journal=journalPath;broadcast.transactions.push({name:contract.field,hash});
    const tx=await provider.broadcastTransaction(raw);
    if(tx.hash!==hash)throw new Stop("Unexpected broadcast hash");
    const receipt=await tx.wait(1,60000);
    if(receipt?.status!==1||keccak256(await provider.getCode(implementation.address))!==runtimeCodeHash)throw new Stop("Implementation deployment not confirmed with the expected runtime code");
    await record({type:"confirmed",name:contract.field,address:implementation.address,hash,block:receipt.blockNumber,gasUsed:String(receipt.gasUsed),gasPrice:String(receipt.gasPrice),gasCost:String(receipt.fee)});
    console.log(JSON.stringify({deployed:true,[contract.field]:implementation.address,runtimeCodeHash,transactionHash:hash,block:receipt.blockNumber,next:contract.next},null,2));
  } finally {await journal.close();}
}
// Every failure prints its reason, and one after a broadcast lists what was sent. Keys and env values are read only by loadDeployer,
// loadEnvValue and loadOrCreateOperator, whose errors never include them; a parse error is not quoted, as it can echo file text.
main().catch(error=>{
  console.error(`Deployment stopped: ${error instanceof SyntaxError?"an input file could not be parsed":error instanceof Error?error.message:String(error)}`);
  if(broadcast.transactions.length)console.error(`Handed to the RPC, so funds may have moved; check each receipt: ${broadcast.transactions.map(({name,hash})=>`${name} ${hash}`).join(", ")}; journal ${broadcast.journal}`);
  if(!(error instanceof Stop))console.error("Credentials were not logged; inspect local configuration and the deployment journal before retrying.");
  process.exitCode=1;
});
