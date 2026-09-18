// Bounded real-testnet measurements. No transactions without --apply (and --keeper-stopped for a local keeper); never final pricing.
import {parseArgs} from "node:util";
import {readFile,writeFile,open,access} from "node:fs/promises";
import {join,resolve,dirname} from "node:path";
import {spawn} from "node:child_process";
import {once} from "node:events";
import {DatabaseSync} from "node:sqlite";
import assert from "node:assert/strict";
import {Contract,JsonRpcProvider,keccak256,getBytes,getAddress,parseUnits,formatUnits,id,type TransactionRequest,type TransactionReceipt} from "ethers";
import {loadDeployer} from "./lib/deployer-env.ts";
import {loadChain} from "./lib/chains.ts";
import {DEFAULT_OPERATOR_DIRECTORY,loadOrCreateOperator,privateDirectory,validateNetwork,Stop} from "./lib/deployment.ts";
import {RemotePilot} from "./lib/remote-pilot.ts";
import {builtins,decodeEvidencePacket,replayCoordinator,resolveEpochCatalog,type RequestContext,type EpochRecord} from "../src/index.ts";

const sleep=(ms:number)=>new Promise(resolve=>setTimeout(resolve,ms));
/** Controller transactions signed and handed to the RPC in this run, and the journals; a later failure lists them. */
const broadcast:{journal?:string;keeperJournal?:string;transactions:Array<{stage:string;hash:string}>}={transactions:[]};
async function main(){
  const {values}=parseArgs({options:{manifest:{type:"string"},env:{type:"string"},"operator-directory":{type:"string"},"keeper-ssh":{type:"string"},"keeper-path":{type:"string"},apply:{type:"boolean",default:false},"keeper-stopped":{type:"boolean",default:false}}});
  if(!values.manifest||!values.env)throw new Stop("Manifest and authorized deployer env path required");
  const manifest=JSON.parse(await readFile(resolve(values.manifest),"utf8")),chain=await loadChain(manifest.network);
  if(!chain.testnet||chain.chainId!==manifest.chainId)throw new Stop("Only a configured testnet is allowed");
  const provider=new JsonRpcProvider(chain.rpcUrls[0],undefined,{batchMaxCount:1});
  let child:ReturnType<typeof spawn>|undefined;
  const remote=values["keeper-ssh"]?new RemotePilot(values["keeper-ssh"],values["keeper-path"]??"/opt/d20dao/keeper"):undefined;
  let remoteStarted=false;
  try {
    await validateNetwork(provider,chain);
    const deployer=await loadDeployer(resolve(values.env),provider),operatorDir=resolve(values["operator-directory"]??DEFAULT_OPERATOR_DIRECTORY);
    await access(join(operatorDir,"operator.json")); // A measurement must never provision replacement keys.
    const operator=await loadOrCreateOperator(operatorDir,deployer.address,chain.owner);
    assert.equal(operator.keeper.address,manifest.keeper);assert.equal(deployer.address,manifest.deployer);
    for(const name of ["registry","coordinator","client","epochImplementation","coordinatorImplementation","clientImplementation"]){
      assert.equal(keccak256(await provider.getCode(manifest[name])),manifest[`${name}CodeHash`]);
    }
    for(const [proxy,implementation] of [["registry","epochImplementation"],["coordinator","coordinatorImplementation"],["client","clientImplementation"]]){
      const slot=await provider.getStorage(manifest[proxy],"0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc");
      assert.equal(getAddress("0x"+slot.slice(-40)),manifest[implementation]);
    }
    const abi=async(name:string,file=name)=>JSON.parse(await readFile(`artifacts/contracts/${file}.sol/${name}.json`,"utf8")).abi;
    const registry=new Contract(manifest.registry,await abi("EpochEntropy"),provider);
    const rng=new Contract(manifest.coordinator,await abi("D20VRFCoordinator"),provider);
    const client=new Contract(manifest.client,await abi("D20CostClient","examples/D20CostClient"),provider);
    assert.equal(await registry.committer(),operator.keeper.address);assert.equal(await rng.protocolConfigurationHash(),manifest.protocolConfigurationHash);
    const [minFee]=await rng.pricing() as [bigint,bigint,bigint],targetBalance=parseUnits("0.2",18);
    // Off-chain quote from the header base fee plus 20%; the coordinator credits the excess to the tester's refund credit.
    const quote=async()=>(await rng.quoteFeeAt(100000,(await provider.getBlock("latest"))!.baseFeePerGas??0n) as bigint)*12n/10n;
    console.log(JSON.stringify({network:chain.key,requests:4,fulfilledCases:3,timeoutCase:1,minFee:formatUnits(minFee,18),currency:chain.nativeCurrency.symbol,
      keeperBootstrapCap:formatUnits(targetBalance,18),note:"Pilot fee is not final product pricing",apply:values.apply},null,2));
    if(!values.apply)return;
    // A local pilot runs its own keeper with the service keeper's wallet, so two keepers would share one nonce; remote mode
    // starts and stops the service keeper itself through its wrapper and never runs a second one.
    if(!remote&&!values["keeper-stopped"])throw new Stop("Stop the service keeper for this keeper wallet first, then rerun with --keeper-stopped");
    const directory=join(dirname(resolve(values.manifest)),"cost-smoke");await privateDirectory(directory);
    let keeperLog="";
    const journalPath=join(directory,"controller-transactions.jsonl"),journal=await open(journalPath,"ax",0o600);broadcast.journal=journalPath;
    const costs:Array<Record<string,unknown>>=[],traces:unknown[]=[];
    const recordCost=(stage:string,receipt:TransactionReceipt)=>{
      costs.push({stage,hash:receipt.hash,block:receipt.blockNumber,gasUsed:String(receipt.gasUsed),gasPriceWei:String(receipt.gasPrice),gasCostWei:String(receipt.fee),gasCostNative:formatUnits(receipt.fee,18)});
    };
    const send=async(stage:string,request:TransactionRequest)=>{
      const nonce=await deployer.getNonce("latest");assert.equal(await deployer.getNonce("pending"),nonce);
      const estimate=await provider.estimateGas({...request,from:deployer.address});
      const gasLimit=estimate*12n/10n+10000n;if(gasLimit>1000000n)throw new Stop("Controller gas cap exceeded");
      const raw=await deployer.signTransaction({...request,chainId:chain.chainId,type:2,nonce,gasLimit,maxFeePerGas:BigInt(chain.gas.maxFeePerGasWei),maxPriorityFeePerGas:1000000000n});
      const hash=keccak256(getBytes(raw));await journal.writeFile(JSON.stringify({stage,hash,raw})+"\n");await journal.sync();broadcast.transactions.push({stage,hash});
      const tx=await provider.broadcastTransaction(raw);assert.equal(tx.hash,hash);
      const receipt=await tx.wait(1,60000);assert.equal(receipt?.status,1);recordCost(stage,receipt!);return receipt!;
    };
    try {
      assert.equal(await provider.getTransactionCount(operator.keeper.address,"latest"),await provider.getTransactionCount(operator.keeper.address,"pending"));
      const balance=await provider.getBalance(operator.keeper.address);
      if(balance<targetBalance)await send("keeper-bootstrap",{to:operator.keeper.address,value:targetBalance-balance});
      const db=join(directory,"keeper.sqlite"),env:NodeJS.ProcessEnv={};
      for(const name of ["PATH","Path","SystemRoot","WINDIR","ProgramData","TEMP","TMP","USERPROFILE","HOME","LANG"]){if(process.env[name])env[name]=process.env[name];}
      Object.assign(env,{CHAIN_ID:String(chain.chainId),RPC_URLS:chain.rpcUrls.join(","),COORDINATOR_ADDRESS:manifest.coordinator,
        KEEPER_DB:db,TX_KEY_FILE:operator.keeper.keyFile,VRF_KEY_FILE:operator.vrf.keyFile,EXPECTED_CODE_HASH:manifest.coordinatorCodeHash,
        EXPECTED_PROTOCOL_HASH:manifest.protocolConfigurationHash,EXPECTED_IMPLEMENTATION_CODE_HASH:manifest.coordinatorImplementationCodeHash,
        EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH:manifest.epochImplementationCodeHash,SEND_TRANSACTIONS:"true",POLL_MS:"250",TICK_TIMEOUT_SECONDS:"20",
        MAX_TICK_FAILURES:"5",MAX_FEE_PER_GAS_WEI:chain.gas.maxFeePerGasWei,CANCEL_MAX_FEE_PER_GAS_WEI:chain.gas.cancelMaxFeePerGasWei,
        MAX_TX_COST_WEI:chain.gas.maxTxCostWei,MAX_GAS:String(chain.gas.maxGas),SEND_MARGIN_SECONDS:"5",PROGRESS_STUCK_SECONDS:"20",NONCE_STUCK_SECONDS:"120",RUST_LOG:"warn"});
      const binary=resolve(`keeper/target/release/d20dao-keeper${process.platform==="win32"?".exe":""}`);
      broadcast.keeperJournal=remote?`the service keeper's journal on ${remote.host}`:db;
      if(remote){remoteStarted=true;await remote.start();}
      else {child=spawn(binary,["run"],{env,windowsHide:true,stdio:["ignore","pipe","pipe"]});
      child.stdout?.on("data",chunk=>{keeperLog=(keeperLog+chunk).slice(-65536);});child.stderr?.on("data",chunk=>{keeperLog=(keeperLog+chunk).slice(-65536);});}
      const alive=async()=>{if(remote?!(await remote.observe<boolean>("running")):child?.exitCode!==null)throw new Stop("Keeper exited during pilot");};
      const rows=(sql:string)=>{const sqlite=new DatabaseSync(db,{readOnly:true});try{return sqlite.prepare(sql).all();}finally{sqlite.close();}};
      const preloadEnd=Date.now()+180000;
      for(;;){await alive();const block=await provider.getBlockNumber(),epoch=await registry.epochForBlock(block);let ready=false;
        try{ready=epoch>0n&&(remote?await remote.observe<boolean>("ready",epoch):rows(`SELECT api FROM epoch_work WHERE epoch=${epoch} AND state='prepared'`).some(row=>row.api!==null));}catch{/* Startup database not yet ready. */}
        if(ready)break;if(Date.now()>preloadEnd)throw new Stop("No valid local epoch packet before pilot timeout");await sleep(1500);
      }
      for(let index=0;index<3;index++){
        const fail=index===2,requested=await send(`request-${index+1}`,{to:manifest.client,data:client.interface.encodeFunctionData("request",[id(`d20-cost-${index}`),100000,fail]),value:await quote()});
        const requestId=await client.lastRequestId() as bigint,waitEnd=Date.now()+80000;
        let request=await rng.getRequest(requestId);
        while(!request.fulfilled){await alive();if(Date.now()>waitEnd||BigInt((await provider.getBlock("latest"))!.timestamp)>request.deadline)throw new Stop("Pilot request did not fulfill before expiry");await sleep(1000);request=await rng.getRequest(requestId);}
        assert.equal(request.delivered,!fail);
        const logsFound=await rng.queryFilter(rng.filters.FulfillmentEvidence(requestId),requested.blockNumber,"latest");assert.equal(logsFound.length,1);
        const accepted=await provider.getTransactionReceipt(logsFound[0].transactionHash);assert(accepted);
        const evidence=rng.interface.parseLog(logsFound[0])!;
        const epochLogs=await registry.queryFilter(registry.filters.EpochCommitted(request.epochId),Number(await registry.epochStart(request.epochId))-1,"latest");assert.equal(epochLogs.length,1);
        const epochEvent=registry.interface.parseLog(epochLogs[0])!,e=await registry.getEpoch(request.epochId);
        const epochRecord:EpochRecord={epochHash:e.epochHash,catalogHash:e.catalogHash,anchorHash:e.anchorHash,source:e.source,queryHash:e.queryHash,dataHash:e.dataHash,attestationHash:e.attestationHash,signedAt:e.signedAt,committedBlock:e.committedBlock};
        const context:RequestContext={chainId:BigInt(chain.chainId),coordinator:manifest.coordinator,keyHash:await rng.keyHash(),requestId,consumer:manifest.client,
          clientSeed:request.clientSeed,mapping:builtins.raw(),requestBlock:request.requestBlock,targetBlock:request.targetBlock,blockHash:request.blockHash,epochId:request.epochId,epochHash:request.epochHash};
        const replayInput={context,configuration:{publicKey:manifest.publicKey.map(BigInt) as [bigint,bigint],feeRecipient:await rng.initialFeeRecipient(),initialMinFee:await rng.initialMinFee(),confirmationBlocks:Number(await rng.confirmationBlocks()),registry:manifest.registry,catalogHash:await registry.catalogHash(),firstEpochStart:await registry.firstEpochStart()},
          protocolConfigurationHash:await rng.protocolConfigurationHash(),epoch:{catalog:resolveEpochCatalog({registry:manifest.registry,chainId:BigInt(chain.chainId),firstEpochStart:await registry.firstEpochStart()},await registry.catalogAt(request.epochId).then(([hash,recipes,signers]:[string,bigint[],string[]])=>({hash,recipes,signers}))),record:epochRecord,commitTimestamp:BigInt((await provider.getBlock(epochLogs[0].blockNumber))!.timestamp),packet:epochEvent.args.packet},
          requestedAt:BigInt((await provider.getBlock(requested.blockNumber))!.timestamp),deadline:request.deadline,acceptanceTimestamp:BigInt((await provider.getBlock(accepted.blockNumber))!.timestamp),acceptanceBlock:BigInt(accepted.blockNumber),
          vrfProof:decodeEvidencePacket(evidence.args.packet).proof,recorded:{fulfilled:request.fulfilled,randomness:request.randomness,proofHash:request.proofHash,transcriptHash:request.transcriptHash}};
        replayCoordinator(replayInput);traces.push({requestId,epochId:request.epochId,feePaid:await rng.requestFeePaid(requestId),delivered:request.delivered,tx:accepted.hash,replayInput});
        console.log(JSON.stringify({requestId:String(requestId),epochId:String(request.epochId),fulfilled:true,delivered:request.delivered,replay:true}));
        if(fail){await assert.rejects(rng.refundRequest.staticCall(requestId));await send("repair-client",{to:manifest.client,data:client.interface.encodeFunctionData("setCallbackFailure",[requestId,false])});
          await send("same-result-callback-retry",{to:manifest.coordinator,data:rng.interface.encodeFunctionData("retryCallback",[requestId,100000])});assert.equal(await client.results(requestId),request.randomness);}
      }
      await sleep(1000);
      if(remote){await remote.stop();remoteStarted=false;}
      else {child!.kill("SIGINT");await Promise.race([once(child!,"exit"),sleep(10000)]);if(child!.exitCode===null&&child!.signalCode===null){child!.kill("SIGKILL");await once(child!,"exit");}child=undefined;}
      const timedOut=await send("request-timeout",{to:manifest.client,data:client.interface.encodeFunctionData("request",[id("d20-cost-timeout"),100000,false]),value:await quote()});
      const expiredId=await client.lastRequestId(),expires=(await rng.getRequest(expiredId)).deadline;
      const refundWaitEnd=Date.now()+90000;
      while(BigInt((await provider.getBlock("latest"))!.timestamp)<=expires){if(Date.now()>refundWaitEnd)throw new Stop("Chain did not advance to refund eligibility within pilot budget");await sleep(1500);}
      await send("refund-expired",{to:manifest.coordinator,data:rng.interface.encodeFunctionData("refundRequest",[expiredId])});
      const expired=await rng.getRequest(expiredId);assert.equal(expired.fulfilled,false);assert.equal(expired.refunded,true);
      const attempts=remote?await remote.observe<Array<{hash:string;kind:string;job:unknown}>>("attempts"):rows("SELECT hash,kind,job FROM txs");
      for(const attempt of attempts){const receipt=await provider.getTransactionReceipt(String(attempt.hash));if(receipt?.status===1)recordCost(`keeper-${attempt.kind}`,receipt);}
      assert.equal(await provider.getTransactionCount(operator.keeper.address,"latest"),await provider.getTransactionCount(operator.keeper.address,"pending"));
      const report={network:chain.key,chainId:chain.chainId,currency:chain.nativeCurrency.symbol,sourceMode:"live API3 on public testnet",minFeeWei:String(minFee),keeperFeeBps:Number(await rng.keeperFeeBps()),costs,
        requests:traces,timeout:{requestId:String(expiredId),requestTransaction:timedOut.hash,refunded:true},keeperBalanceWei:String(await provider.getBalance(operator.keeper.address)),treasuryEarnedWei:String(await rng.earnedFees()),pricingDecided:false};
      await writeFile(join(directory,"report.json"),JSON.stringify(report,(_,value)=>typeof value==="bigint"?String(value):value,2)+"\n",{flag:"wx",mode:0o600});
      console.log(JSON.stringify({passed:true,accepted:3,callbackRepair:true,expiredRefund:true,report:join(directory,"report.json")},null,2));
    } finally {await writeFile(join(directory,"keeper.log"),keeperLog,{mode:0o600});await journal.close();}
  } finally {if(child){child.kill("SIGINT");}if(remote&&remoteStarted)await remote.stop();provider.destroy();}
}
// Every failure prints its reason, and one after a broadcast lists what was sent. Keys are read only by loadDeployer, loadOrCreateOperator
// and the keeper process, whose output goes to the private keeper.log; a parse error is not quoted, as it can echo file text.
main().catch(error=>{
  console.error(`Cost pilot stopped: ${error instanceof SyntaxError?"an input file could not be parsed":error instanceof Error?error.message:String(error)}`);
  if(broadcast.transactions.length)console.error(`Handed to the RPC, so funds may have moved; check each receipt: ${broadcast.transactions.map(({stage,hash})=>`${stage} ${hash}`).join(", ")}; journal ${broadcast.journal}`+
    (broadcast.keeperJournal?`; keeper transactions are journaled in ${broadcast.keeperJournal}`:""));
  if(!(error instanceof Stop))console.error("No credentials were logged; inspect private journals and chain receipts before retrying.");
  process.exitCode=1;
});
