// Rehearse a coordinator implementation upgrade on a local anvil fork of a live network. Read-only against the network:
// every transaction goes to the fork. It deploys the mined CREATE2 implementation from the impersonated deployer (or
// checks one already deployed), calls upgradeToAndCall on the proxy through the owner's approval flow (the owner may
// be a multisig) and directly as the impersonated owner, compares state before and after, then serves request, refund
// and batch cycles on the upgraded proxy.
// To produce proofs on the fork only, the coordinator's VRF public key is replaced by the public test key after the
// state comparison, and an unpublished epoch record is written in place of the committer's commitEpoch.
// Usage: node scripts/coordinator-upgrade-fork.ts --chain arc-mainnet (--implementation-result <mined result> |
//        --implementation <deployed address>) [--fork-url <rpc>] [--backup-committer <address> ...] [--skip-control]
import {parseArgs} from "node:util";
import {spawn} from "node:child_process";
import {readFile} from "node:fs/promises";
import {Contract,Interface,JsonRpcProvider,Network,ZeroAddress,ZeroHash,AbiCoder,getAddress,keccak256,toBeHex,zeroPadValue,formatUnits,id as eventId,type TransactionReceipt} from "ethers";
import {loadChain} from "./lib/chains.ts";
import {initCode,candidate,compiledRuntimeCodeHash,Stop} from "./lib/deployment.ts";
import {publicKey,makeProof,proofOutput} from "../test/helpers/proof.ts";

const IMPLEMENTATION_SLOT="0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";
const ADMIN_SLOT="0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103";
const coder=AbiCoder.defaultAbiCoder();
const erc7201=(name:string)=>toBeHex(BigInt(keccak256(coder.encode(["uint256"],[BigInt(eventId(name))-1n])))&~0xffn,32);
const NAMESPACES=["openzeppelin.storage.Initializable","openzeppelin.storage.Ownable","openzeppelin.storage.Ownable2Step","openzeppelin.storage.ReentrancyGuard"].map(erc7201);
const SAFE=new Interface(["function getOwners() view returns(address[])","function getThreshold() view returns(uint256)","function nonce() view returns(uint256)",
  "function getTransactionHash(address,uint256,bytes,uint8,uint256,uint256,uint256,address,address,uint256) view returns(bytes32)",
  "function approveHash(bytes32)","function execTransaction(address,uint256,bytes,uint8,uint256,uint256,uint256,address,address,bytes) payable returns(bool)"]);

const artifact=async(path:string)=>JSON.parse(await readFile(`artifacts/contracts/${path}`,"utf8"));
const report:Record<string,unknown>={checks:[]};
const check=(ok:boolean,what:string)=>{if(!ok)throw new Stop(`Check failed: ${what}`);(report.checks as string[]).push(what);console.error(`ok  ${what}`);};
const step=(what:string)=>console.error(`${new Date().toISOString()}  ${what}`);

async function main(){
  const {values}=parseArgs({options:{chain:{type:"string",default:"arc-mainnet"},"fork-url":{type:"string"},port:{type:"string",default:"8547"},
    "implementation-result":{type:"string"},implementation:{type:"string"},"backup-committer":{type:"string",multiple:true,default:[]},"skip-control":{type:"boolean",default:false}}});
  const chain=await loadChain(values.chain);
  const manifest=JSON.parse(await readFile(`deployments/${chain.key}.json`,"utf8"));
  const forkUrl=values["fork-url"]??chain.rpcUrls[0],port=Number(values.port),url=`http://127.0.0.1:${port}`;
  if(!values["implementation-result"]&&!values.implementation)throw new Stop("Pass --implementation-result <mined result> or --implementation <deployed address>");
  step(`starting anvil on ${url}, forking ${forkUrl}`);
  const anvil=spawn("anvil",["--fork-url",forkUrl,"--port",String(port),"--chain-id",String(chain.chainId),"--silent"],{stdio:["ignore","ignore","pipe"],windowsHide:true});
  let anvilErr="";anvil.stderr.on("data",d=>{anvilErr+=d;});
  let provider:JsonRpcProvider|undefined;
  try {
    for(let i=0;;i++){
      const up=await fetch(url,{method:"POST",headers:{"content-type":"application/json"},body:JSON.stringify({jsonrpc:"2.0",id:1,method:"eth_chainId",params:[]})}).then(r=>r.ok,()=>false);
      if(up)break;
      if(i>240||anvil.exitCode!==null)throw new Stop(`anvil did not start: ${anvilErr.slice(-400)}`);
      await new Promise(r=>setTimeout(r,500));
    }
    const rpc=new JsonRpcProvider(url,Network.from(chain.chainId),{staticNetwork:Network.from(chain.chainId),batchMaxCount:1});provider=rpc;
    const send=(method:string,params:unknown[])=>rpc.send(method,params);
    report.forkBlock=Number(await send("eth_blockNumber",[]));
    const impersonate=async(address:string)=>{await send("anvil_impersonateAccount",[address]);if(await rpc.getBalance(address)<10n**18n)await send("anvil_setBalance",[address,toBeHex(10n**18n)]);return address;};
    const mine=async(blocks:number,seconds=0)=>{if(seconds)await send("evm_increaseTime",[seconds]);for(let i=0;i<blocks;i++)await send("evm_mine",[]);};
    /// Every fork transaction goes through eth_sendTransaction with a bounded receipt wait.
    const tx=async(from:string,to:string|null,data:string,value=0n,gas?:bigint):Promise<TransactionReceipt>=>{
      const params:Record<string,string>={from,data,value:toBeHex(value)};if(to)params.to=to;if(gas)params.gas=toBeHex(gas);
      const hash:string=await send("eth_sendTransaction",[params]);
      for(let i=0;i<300;i++){const receipt=await rpc.getTransactionReceipt(hash);if(receipt)return receipt;if(i%10===9)await send("evm_mine",[]);await new Promise(r=>setTimeout(r,100));}
      throw new Stop(`No receipt for ${hash}`);
    };
    const events=(receipt:TransactionReceipt,contract:Contract,name:string)=>receipt.logs.filter(l=>l.address===contract.target).map(l=>{try{return contract.interface.parseLog(l);}catch{return null;}}).filter(e=>e?.name===name);

    const coordinatorAbi=(await artifact("D20VRFCoordinator.sol/D20VRFCoordinator.json")).abi;
    const proxy=getAddress(manifest.coordinator),registryAddress=getAddress(manifest.registry),owner=chain.owner;
    const rng=new Contract(proxy,coordinatorAbi,rpc),registry=new Contract(registryAddress,(await artifact("EpochEntropy.sol/EpochEntropy.json")).abi,rpc);
    const slotImplementation=async()=>getAddress("0x"+(await rpc.getStorage(proxy,IMPLEMENTATION_SLOT)).slice(-40));
    const oldImplementation=await slotImplementation();
    report.oldImplementation=oldImplementation;report.oldImplementationCodeHash=keccak256(await rpc.getCode(oldImplementation));
    check(oldImplementation===getAddress(manifest.coordinatorImplementation)&&report.oldImplementationCodeHash===manifest.coordinatorImplementationCodeHash,"live implementation matches the deployment manifest");

    // State before the upgrade: every declared word and the gap, OpenZeppelin namespaces, the proxy admin slot, the
    // latest requests' storage words, the views an integrator or the keeper reads, and the registry roles.
    step("reading state before the upgrade");
    const nextRequestId=BigInt(await rng.nextRequestId());
    const recent=Array.from({length:Number(nextRequestId>41n?40n:nextRequestId-1n)},(_,i)=>nextRequestId-1n-BigInt(i));
    const slots=[...Array.from({length:60},(_,i)=>toBeHex(i,32)),...NAMESPACES,ADMIN_SLOT];
    for(const request of recent){const base=BigInt(keccak256(coder.encode(["uint256","uint256"],[request,18n])));for(let i=0n;i<11n;i++)slots.push(toBeHex(base+i,32));}
    const storage=async()=>{const out:Record<string,string>={};for(const slot of slots)out[slot]=await rpc.getStorage(proxy,slot);return out;};
    const backups=values["backup-committer"].map(a=>getAddress(a));
    const pending=async()=>{const ids:string[]=[];let cursor=1n;const end=BigInt(await rng.nextRequestId());while(cursor<end){const [page,next]=await rng.getPendingRequestIds(cursor,256);ids.push(...page.map(String));cursor=next;}return ids;};
    const views=async()=>({pricing:(await rng.pricing()).map(String),keeperFeeBps:String(await rng.keeperFeeBps()),refundBps:String(await rng.refundBps()),
      earnedFees:String(await rng.earnedFees()),totalKeeperCredits:String(await rng.totalKeeperCredits()),totalRefundCredits:String(await rng.totalRefundCredits()),
      owner:await rng.owner(),pendingOwner:await rng.pendingOwner(),feeRecipient:await rng.feeRecipient(),initialFeeRecipient:await rng.initialFeeRecipient(),
      nextRequestId:String(await rng.nextRequestId()),lastServedIndex:String(await rng.lastServedIndex()),lastServedRequestId:String(await rng.lastServedRequestId()),
      protocolConfigurationHash:await rng.protocolConfigurationHash(),keyHash:await rng.keyHash(),publicKey:[String(await rng.publicKeyX()),String(await rng.publicKeyY())],
      epochRegistry:await rng.epochRegistry(),confirmationBlocks:String(await rng.confirmationBlocks()),initialMinFee:String(await rng.initialMinFee()),
      balance:String(await rpc.getBalance(proxy)),pendingRequests:await pending(),
      committer:await registry.committer(),backupCommitterCount:String(await registry.backupCommitterCount()),
      backupCommitters:Object.fromEntries(await Promise.all(backups.map(async a=>[a,[await registry.isBackupCommitter(a),await registry.isAuthorizedCommitter(a)]]))),
      requests:await Promise.all(recent.slice(0,10).map(async request=>(await rng.getRequest(request)).toArray().map(String)))});
    const beforeViews=await views(),beforeStorage=await storage();
    report.before={...beforeViews,requests:undefined};
    for(const a of backups)check(beforeViews.backupCommitters[a][1]===true,`backup committer ${a} is authorized before the upgrade`);

    const keeper=await impersonate(beforeViews.committer);
    const testKey=async()=>{const [x,y]=publicKey();await send("anvil_setStorageAt",[proxy,toBeHex(2,32),toBeHex(x,32)]);await send("anvil_setStorageAt",[proxy,toBeHex(3,32),toBeHex(y,32)]);
      await send("anvil_setStorageAt",[proxy,toBeHex(4,32),keccak256(coder.encode(["uint256[2]"],[[x,y]]))]);};
    /// Publish the epochs of the given requests, as the keeper does on demand, then mine past their target blocks.
    const publishFor=async(ids:bigint[])=>{
      for(const epoch of new Set(await Promise.all(ids.map(async id=>BigInt((await rng.getRequest(id)).epochId))))){
        if((await registry.getEpoch(epoch)).epochHash!==ZeroHash)continue;
        const base=BigInt(keccak256(coder.encode(["uint256","uint256"],[epoch,6n])));
        await send("anvil_setStorageAt",[registryAddress,toBeHex(base,32),keccak256(coder.encode(["string","uint256"],["fork rehearsal publication",epoch]))]);
        await send("anvil_setStorageAt",[registryAddress,toBeHex(base+8n,32),toBeHex(BigInt(await send("eth_blockNumber",[])),32)]);
      }
      await mine(3,1);};
    const deploy=async(path:string,args:unknown[])=>{const a=await artifact(path),factory=new Interface(a.abi);
      const receipt=await tx(keeper,null,a.bytecode+factory.encodeDeploy(args).slice(2));
      if(receipt.status!==1||!receipt.contractAddress)throw new Stop(`Deployment of ${path} failed`);return new Contract(receipt.contractAddress,a.abi,rpc);};
    const write=(from:string,contract:Contract,method:string,args:unknown[],value=0n,gas?:bigint)=>tx(from,contract.target as string,contract.interface.encodeFunctionData(method,args),value,gas);
    const quote=async(gas:bigint)=>BigInt(await rng.quoteFeeAt(gas,(await rpc.getBlock("latest"))!.baseFeePerGas!*2n));
    /// The keeper's sizing: eth_estimateGas at gas price 0, as Arc nodes run it, then 1.2 × estimate + 50000.
    const keeperBatch=async(from:string,ids:bigint[],proofs:unknown[])=>{const data=rng.interface.encodeFunctionData("fulfillRandomnessBatch",[ids,proofs]);
      const estimate=BigInt(await send("eth_estimateGas",[{from,to:proxy,data,gasPrice:"0x0"}]).catch(async error=>{
        const probe=await send("eth_call",[{from,to:proxy,data,gas:toBeHex(20_000_000)},"latest"]).then(r=>`call succeeds (${r})`,e=>`call fails: ${e?.info?.error?.message??e?.message} ${e?.data??e?.info?.error?.data??""}`);
        throw new Stop(`Batch estimate failed (${error?.info?.error?.message??error?.shortMessage}); ${probe}`);}));
      const gasLimit=estimate*12n/10n+50_000n;return {estimate,gasLimit,send:()=>tx(from,proxy,data,0n,gasLimit)};};
    const attackRound=async(attacker:Contract)=>{const fee=await quote(100_000n),helperFee=await quote(1_000_000n);
      const armed=await write(keeper,attacker,"drawAndArm",[fee,helperFee],fee+2n*helperFee,3_000_000n);if(armed.status!==1)throw new Stop("drawAndArm failed");
      const draw=BigInt(await attacker.drawId()),ids=[draw,draw+1n,draw+2n];await publishFor(ids);
      const proofs=await Promise.all(ids.map(async request=>makeProof(BigInt(await rng.requestSeed(request)))));
      return {ids,proofs,won:BigInt(proofOutput(proofs[0]))%8n===0n};};

    // Control on the live implementation (fork only): a losing draw aborts its keeper-sized batch.
    const fork=await send("evm_snapshot",[]);
    if(!values["skip-control"]){
      step("control: the reviewed attack against the live implementation");
      await testKey();
      const attacker=await deploy("test/BatchGuardAttacker.sol/BatchGuardAttacker.json",[proxy]);await write(keeper,attacker,"setMode",[1]);
      let aborted=0,rounds=0;
      for(;rounds<24&&aborted===0;rounds++){
        const {ids,proofs,won}=await attackRound(attacker),plan=await keeperBatch(keeper,ids,proofs),receipt=await plan.send();
        if(won){check(receipt.status===1,"control: a winning draw's batch lands");continue;}
        check(receipt.status===0&&!(await rng.getRequest(ids[0])).fulfilled,"control: on the live implementation a losing draw's keeper-sized batch reverts");
        aborted++;
      }
      check(aborted===1,"control: the live implementation lets a losing draw abort its batch");
      report.controlRounds=rounds;
    }
    await send("evm_revert",[fork]);

    // Deploy the implementation exactly as create2-deploy.ts will: the mined salt and init code through the factory.
    step("deploying the implementation through the CREATE2 factory");
    const code=await initCode("D20VRFCoordinator",[]);
    let implementation:string;
    if(values["implementation-result"]){
      const mined=await candidate(values["implementation-result"],code,chain);implementation=mined.address;
      if(await rpc.getCode(implementation)==="0x"){
        const deployer=await impersonate(manifest.deployer),receipt=await tx(deployer,chain.create2.factory,mined.data,0n,6_500_000n);
        check(receipt.status===1,"CREATE2 deployment from the deployer succeeds");
        report.deployment={implementation,salt:mined.salt,gasUsed:String(receipt.gasUsed),costAt21GweiUSDC:formatUnits(receipt.gasUsed*21_000_000_000n,18)};
      } else report.deployment={implementation,alreadyDeployed:true};
    } else implementation=getAddress(values.implementation!);
    const runtimeCodeHash=await compiledRuntimeCodeHash("D20VRFCoordinator",implementation);
    check(keccak256(await rpc.getCode(implementation))===runtimeCodeHash,"implementation runtime code equals this build at its address");
    report.newImplementation=implementation;report.newImplementationCodeHash=runtimeCodeHash;
    const data=rng.interface.encodeFunctionData("upgradeToAndCall",[implementation,"0x"]);
    report.upgradeCalldata=data;

    // The owner's approval path when it is a multisig: every threshold owner approves the transaction hash, then it executes.
    step("upgrade through the Safe's execTransaction");
    const beforeSafe=await send("evm_snapshot",[]);
    const safe=new Contract(owner,SAFE,rpc);
    const owners:string[]=(await safe.getOwners()).map((a:string)=>getAddress(a)),threshold=Number(await safe.getThreshold()),safeNonce=BigInt(await safe.nonce());
    const safeArgs=[proxy,0,data,0,0,0,0,ZeroAddress,ZeroAddress] as const;
    const safeTxHash=await safe.getTransactionHash(...safeArgs,safeNonce);
    const signers=owners.slice(0,threshold);
    for(const signer of signers)check((await write(await impersonate(signer),safe,"approveHash",[safeTxHash])).status===1,`Safe owner ${signer} approves the hash`);
    const signatures="0x"+[...signers].sort((a,b)=>BigInt(a)<BigInt(b)?-1:1).map(a=>zeroPadValue(a,32).slice(2)+"00".repeat(32)+"01").join("");
    const exec=await write(signers[0],safe,"execTransaction",[...safeArgs,signatures],0n,1_000_000n);
    check(exec.status===1&&exec.logs.some(l=>l.address===owner&&l.topics[0]===eventId("ExecutionSuccess(bytes32,uint256)")),"Safe execTransaction of the batch calldata succeeds");
    check(await slotImplementation()===implementation,"the Safe path sets the implementation slot");
    report.safe={owners,threshold,nonce:String(safeNonce),safeTxHash,execGasUsed:String(exec.gasUsed)};
    await send("evm_revert",[beforeSafe]);

    // The upgrade itself, sent by the impersonated owner.
    step("upgradeToAndCall as the impersonated Safe");
    const upgrade=await tx(await impersonate(owner),proxy,data,0n,500_000n);
    check(upgrade.status===1&&upgrade.logs.some(l=>l.address===proxy&&l.topics[0]===eventId("Upgraded(address)")&&getAddress("0x"+l.topics[1].slice(-40))===implementation),"upgradeToAndCall emits Upgraded(new implementation)");
    report.upgradeGasUsed=String(upgrade.gasUsed);
    check(await slotImplementation()===implementation,"implementation slot holds the new implementation");
    const afterStorage=await storage(),changed=slots.filter(slot=>afterStorage[slot]!==beforeStorage[slot]);
    check(changed.length===0,`all ${slots.length} compared storage words unchanged`);
    const afterViews=await views();
    check(JSON.stringify(afterViews)===JSON.stringify(beforeViews),"pricing, fees, earnedFees, credits, owner, pendingOwner, committer, backup committers, pending requests and balance unchanged");

    // Service on the upgraded proxy (fork only: test VRF key from here on).
    step("request, fulfil and refund on the upgraded proxy");
    await testKey();
    const consumerPath="test/TestConsumers.sol/TestConsumer.json",consumer=await deploy(consumerPath,[proxy]);
    let seed=BigInt(Date.now());
    const request=async(gas:bigint,c:Contract=consumer)=>{const fee=await quote(gas);const r=await write(keeper,c,"request",[keccak256(toBeHex(++seed,32)),gas,keeper],fee,1_000_000n);
      if(r.status!==1)throw new Stop("request failed");return BigInt(await c.lastRequestId());};
    const earned0=BigInt(await rng.earnedFees()),single=await request(200_000n);
    await publishFor([single]);
    const singleProof=makeProof(BigInt(await rng.requestSeed(single))),paid=BigInt(await rng.requestFeePaid(single));
    const fulfil=await write(keeper,rng,"fulfillRandomness",[single,singleProof],0n,2_000_000n);
    const keeperShare=paid/10000n*BigInt(afterViews.keeperFeeBps)+paid%10000n*BigInt(afterViews.keeperFeeBps)/10000n;
    const [paidEvent]=events(fulfil,rng,"KeeperFeePaid");
    check(fulfil.status===1&&(await rng.getRequest(single)).delivered&&BigInt(await rng.earnedFees())-earned0===paid-keeperShare&&paidEvent?.args.keeper===afterViews.committer&&paidEvent?.args.amount===keeperShare&&paidEvent?.args.paid===true,
      "request and fulfillRandomness: callback delivered, keeper share to the committer, DAO share to earnedFees");
    const refunded=await request(100_000n);await mine(1,61);
    const refundFee=BigInt(await rng.requestFeePaid(refunded)),refundBps=BigInt(await rng.requestRefundBps(refunded));
    const refund=await write(keeper,rng,"refundRequest",[refunded],0n,500_000n),[refundEvent]=events(refund,rng,"RequestRefundedTo");
    check(refund.status===1&&refundEvent?.args.amount===refundFee*refundBps/10000n&&refundEvent?.args.paid===true,"an expired request refunds its escrowed fee at its snapshotted ratio");
    // An honest mixed batch sized like the keeper, submitted by each authorized wallet in turn.
    const batches:unknown[]=[];
    for(const submitter of [afterViews.committer,...backups]){
      step(`mixed batch submitted by ${submitter}`);
      const members=[consumer,await deploy(consumerPath,[proxy]),await deploy(consumerPath,[proxy])];
      const ids=[await request(200_000n,members[0]),await request(1_000_000n,members[1]),await request(60_000n,members[2]),await request(100_000n,members[0])];
      await publishFor(ids);
      const proofs=await Promise.all(ids.map(async r=>makeProof(BigInt(await rng.requestSeed(r)))));
      const from=await impersonate(submitter),plan=await keeperBatch(from,ids,proofs),receipt=await plan.send();
      const payees=events(receipt,rng,"KeeperFeePaid").map(e=>e!.args.keeper);
      check(receipt.status===1&&(await Promise.all(ids.map(async r=>(await rng.getRequest(r)).delivered))).every(Boolean)&&payees.length===4&&payees.every(p=>p===submitter),
        `mixed batch of 4 (200k/1M/60k/100k callbacks) sized from eth_estimateGas lands for ${submitter}, which receives the keeper share`);
      batches.push({submitter,estimate:String(plan.estimate),gasLimit:String(plan.gasLimit),gasUsed:String(receipt.gasUsed)});
    }
    report.batches=batches;
    // The reviewed attack against the upgraded live proxy: losing draws now land.
    step("the reviewed attack against the upgraded proxy");
    const attacker=await deploy("test/BatchGuardAttacker.sol/BatchGuardAttacker.json",[proxy]);await write(keeper,attacker,"setMode",[1]);
    let losses=0,rounds=0;
    for(;rounds<24&&losses<2;rounds++){
      const {ids,proofs,won}=await attackRound(attacker),plan=await keeperBatch(keeper,ids,proofs),receipt=await plan.send();
      check(receipt.status===1&&(await rng.getRequest(ids[0])).fulfilled&&(await rng.getRequest(ids[2])).fulfilled,"attack batch lands with every member served");
      if(!won){check(receipt.gasUsed>2_000_000n&&!(await rng.getRequest(ids[1])).delivered&&!(await rng.getRequest(ids[2])).delivered,"a losing draw lands although both helpers burned their callbacks");losses++;}
    }
    check(losses===2,"two losing draws landed on the upgraded proxy");
    report.attackRounds=rounds;
    console.log(JSON.stringify(report,null,2));
  } finally {
    provider?.destroy();
    anvil.kill();
  }
}
main().catch(error=>{console.log(JSON.stringify(report,null,2));console.error(`Fork rehearsal stopped: ${error instanceof Error?error.message:String(error)}`);process.exitCode=1;});
