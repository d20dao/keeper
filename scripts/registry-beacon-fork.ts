// Rehearse the drand beacon rollout of the registry on a local anvil fork of a live network: the upgrade to the beacon registry and
// the registration of drand's evmnet (Safe batch A), the catalog that lists it (Safe batch B), and a request served from a real
// drand round. Read-only against the network: every transaction goes to the fork; the network sees public RPC reads, and the
// drand relay GETs.
// Where the owner is the DAO Safe, batch A is one MultiSendCallOnly delegatecall and batch B one direct call, each approved with
// approveHash by the threshold owners and executed with execTransaction, as the Safe app sends them. Where the owner is an
// account (Arc Testnet), each call is its own transaction from the impersonated owner.
// Time: every block, transactions included, is mined with an explicit timestamp: the wall clock's second, but never below the
// previous block and never more than one second after it. No time is added to the chain, so the fork never runs ahead of the wall
// clock and the drand rounds of its blocks exist; a round the relay has not published yet is retried, and a fork that starts ahead
// of the wall clock is waited for before anything is mined.
// To produce proofs on the fork only, the coordinator's VRF public key is replaced by the public test key.
// Usage: node scripts/registry-beacon-fork.ts --chain arc-mainnet (--local-deploy | --implementation-result <mined result> |
//        --implementation <deployed address>) (--local-deploy | --verifier-result <mined result> | --verifier <deployed address>)
//        [--fork-url <rpc>] [--port 8561] [--relay https://api.drand.sh] [--backup-committer <address> ...] [--multisend <address>|none]
// --local-deploy deploys what has no other source on the fork with plain CREATE from the manifest's deployer, for the run before
// any salt is mined. --multisend none rehearses the fallback for a Safe without MultiSendCallOnly: sequential calls from the Safe.
// The registry's backup committers are found by the script itself, from its BackupCommitterSet events between its deployment block and
// the fork's block, read from the network's RPC a page of 10,000 blocks at a time, and their storage words are compared with the
// rest. --backup-committer names extra accounts to compare besides those.
// The JSON report on stdout lists every check that ran; progress goes to stderr.
import {parseArgs} from "node:util";
import {spawn} from "node:child_process";
import {readFile} from "node:fs/promises";
import {Contract,Interface,JsonRpcProvider,Network,ZeroAddress,ZeroHash,AbiCoder,dataLength,getAddress,getBytes,hexlify,keccak256,toBeHex,toQuantity,toUtf8Bytes,zeroPadValue,solidityPacked,formatUnits,id as eventId,type TransactionReceipt} from "ethers";
import {loadChain,requireOperable} from "./lib/chains.ts";
import {initCode,candidate,compileContracts,compiledRuntimeCodeHash,Stop} from "./lib/deployment.ts";
import {BEACON_PRESETS,DEFAULT_RELAY,checkBeaconSample,fetchBeaconSample,relayBase,type BeaconSample} from "./lib/drand-relay.ts";
import {BEACON_ABI,registerBeaconArgs} from "./lib/beacon-admin.ts";
import {LOG_PAGE_BLOCKS,readBackupCommitters,type LogSource} from "./lib/backup-committers.ts";
import {calmReads} from "./lib/calm-rpc.ts";
import {BEACON_TEMPLATE,beaconCanonicalRequest,beaconRoundAt,beaconRoundTime,beaconSlotSigner,encodeBeaconRound,verifyBeaconRound} from "../src/beacon.ts";
import {decodeEpochEvidencePacket,epochCatalogHash,readEpochRecipes,replayEpochCommitment,resolveEpochCatalog} from "../src/epoch.ts";
import {publicKey,makeProof,proofOutput} from "../test/helpers/proof.ts";

const IMPLEMENTATION_SLOT="0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";
const ADMIN_SLOT="0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103";
const coder=AbiCoder.defaultAbiCoder();
const erc7201=(name:string)=>toBeHex(BigInt(keccak256(coder.encode(["uint256"],[BigInt(eventId(name))-1n])))&~0xffn,32);
const NAMESPACES=["Initializable","Ownable","Ownable2Step"].map(name=>erc7201(`openzeppelin.storage.${name}`));
const SAFE=new Interface(["function getOwners() view returns(address[])","function getThreshold() view returns(uint256)","function nonce() view returns(uint256)","function VERSION() view returns(string)",
  "function getTransactionHash(address,uint256,bytes,uint8,uint256,uint256,uint256,address,address,uint256) view returns(bytes32)",
  "function approveHash(bytes32)","function execTransaction(address,uint256,bytes,uint8,uint256,uint256,uint256,address,address,bytes) payable returns(bool)"]);
/// Storage slot of the Safe's transaction guard, which would check every execTransaction.
const GUARD_SLOT="0x4a204f620c8c5ccdca3fd54d003badd85ba500436a431f0cbda4f558c93c34c8";
const MULTISEND=new Interface(["function multiSend(bytes)"]);
/// Safe's MultiSendCallOnly deployments, the DAO Safe's version first: its app batches with the one of the Safe's version.
const MULTISEND_CALL_ONLY:ReadonlyArray<readonly [string,string]>=[["1.4.1","0x9641d764fc13c8B624c04430C7356C1C7C8102e2"],["1.3.0","0x40A2aCCbd92BCA938b02010E17A5b8929b49130D"],["1.3.0 (eip155)","0xA1dabEF33b3B82c7814B6D82A79e50F4AC44102B"]];
/// The keeper takes the round current at the epoch's start only while it is no older than this many seconds (BEACON_MAX_AGE
/// less 20 in keeper/src/beacon.rs, so that the packet is still fresh when its commit is signed); the registry accepts up to 240.
const MAX_ROUND_AGE=180n;
const EXECUTE_GAS=3_000_000n;
/// The pause between two pages of the event scan, and how many storage reads the fork asks the network for at once: the public RPCs rate-limit
/// a burst, and about a hundred requests a minute. A read that is refused for it is repeated after 20 seconds (calmReads).
const LOG_PAUSE_MS=1000,STORAGE_READS_AT_ONCE=6;
const waiting=(method:string,attempt:number,pauseMs:number)=>console.error(`!!  the network's node refused ${method} for its rate limit; waiting ${pauseMs/1000} s before attempt ${attempt+1}`);

const artifact=async(path:string)=>JSON.parse(await readFile(`artifacts/contracts/${path}`,"utf8"));
const plain=(value:unknown)=>JSON.parse(JSON.stringify(value,(_key,item)=>typeof item==="bigint"?item.toString():item));
const same=(a:unknown,b:unknown)=>JSON.stringify(plain(a))===JSON.stringify(plain(b));
const numbers=(value:unknown)=>Array.from(value as Iterable<unknown>,Number);
const sleep=(ms:number)=>new Promise(resolve=>setTimeout(resolve,ms));
const checks:string[]=[],report:Record<string,any>={};
const check=(ok:boolean,what:string)=>{if(!ok)throw new Stop(`Check failed: ${what}`);checks.push(what);console.error(`ok  ${what}`);};
const step=(what:string)=>console.error(`${new Date().toISOString()}  ${what}`);
const revertReason=(error:any)=>String(error?.reason??error?.shortMessage??error?.info?.error?.message??error?.message??error).split("\n")[0];
const printReport=()=>console.log(JSON.stringify({...report,checks},(_key,value)=>typeof value==="bigint"?value.toString():value,2));

// Storage words of the registry, addressed as Solidity lays them out.
const hex=(value:bigint)=>toBeHex(value,32);
const base=(slot:bigint)=>BigInt(keccak256(hex(slot)));
const mapped=(key:bigint,slot:bigint)=>BigInt(keccak256(coder.encode(["uint256","uint256"],[key,slot])));
/// The words a string or bytes value keeps behind its head slot: none while it is short and lives in the head slot itself.
const dataWords=(head:bigint,slot:bigint)=>head%2n===1n?Array.from({length:Number(((head-1n)/2n+31n)/32n)},(_,i)=>base(slot)+BigInt(i)):[];
/// Every word to compare: slots 0 to 60 (the declared state and the gap), the OpenZeppelin namespaces, the proxy admin slot, the
/// records and anchors of the given epochs, the backup committer entries and, followed through the lengths read with `read`, the
/// catalog array (slot 9), the recipe array (slot 10) and the beacon registration mapping (slot 13) for each recipe id.
async function layout(read:(slot:bigint)=>Promise<bigint>,epochs:bigint[],backups:string[]):Promise<bigint[]>{
  const slots=new Set<bigint>([...Array.from({length:61},(_,i)=>BigInt(i)),...NAMESPACES.map(word=>BigInt(word)),BigInt(ADMIN_SLOT)]);
  for(const epoch of epochs){for(let i=0n;i<9n;i++)slots.add(mapped(epoch,6n)+i);slots.add(mapped(epoch,7n));}
  for(const account of backups)slots.add(mapped(BigInt(account),11n));
  const catalogs=await read(9n),recipes=await read(10n);
  for(let i=0n;i<catalogs;i++){
    const head=base(9n)+4n*i;
    for(let j=0n;j<4n;j++)slots.add(head+j);
    const [recipeLength,signerLength]=[await read(head+2n),await read(head+3n)];
    for(let j=0n;j<(recipeLength+31n)/32n;j++)slots.add(base(head+2n)+j);
    for(let j=0n;j<signerLength;j++)slots.add(base(head+3n)+j);
  }
  for(let i=0n;i<recipes;i++){
    for(let j=0n;j<3n;j++){const slot=base(10n)+3n*i+j;slots.add(slot);for(const word of dataWords(await read(slot),slot))slots.add(word);}
    const entry=mapped(i,13n);
    for(let j=0n;j<4n;j++)slots.add(entry+j);
    for(const word of dataWords(await read(entry+3n),entry+3n))slots.add(word);
  }
  return [...slots];
}
/// The words registerBeacon writes for the recipe with this id, and the recipe array length that follows: the recipe's three
/// members and the registration's four, each string or bytes value in its head slot and behind it.
function beaconWords(id:bigint,request:string,template:string,b:{verifier:string;genesis:bigint;period:bigint;chainHash:string;publicKey:string}):Map<bigint,bigint>{
  const words=new Map<bigint,bigint>([[10n,id+1n]]);
  const put=(slot:bigint,data:Uint8Array)=>{
    if(data.length<32){const word=new Uint8Array(32);word.set(data);word[31]=data.length*2;words.set(slot,BigInt(hexlify(word)));return;}
    words.set(slot,BigInt(data.length)*2n+1n);
    for(let i=0;i*32<data.length;i++){const word=new Uint8Array(32);word.set(data.subarray(i*32,i*32+32));words.set(base(slot)+BigInt(i),BigInt(hexlify(word)));}
  };
  const head=base(10n)+3n*id,entry=mapped(id,13n);
  put(head,toUtf8Bytes(request));put(head+1n,getBytes(template));put(head+2n,toUtf8Bytes(request));
  words.set(entry,BigInt(b.verifier)|(b.genesis<<160n));words.set(entry+1n,b.period);words.set(entry+2n,BigInt(b.chainHash));put(entry+3n,getBytes(b.publicKey));
  return words;
}

async function main(){
  const {values}=parseArgs({options:{chain:{type:"string",default:"arc-mainnet"},"fork-url":{type:"string"},port:{type:"string",default:"8561"},relay:{type:"string",default:DEFAULT_RELAY},
    "implementation-result":{type:"string"},implementation:{type:"string"},"verifier-result":{type:"string"},verifier:{type:"string"},"local-deploy":{type:"boolean",default:false},
    "backup-committer":{type:"string",multiple:true,default:[]},multisend:{type:"string"}}});
  const chain=await loadChain(values.chain);
  // The rehearsal impersonates the owner and replays Arc's registry upgrade; a chain it cannot rehearse is refused before any network access.
  requireOperable(chain,"registry-beacon-fork.ts");
  const manifest=JSON.parse(await readFile(`deployments/${chain.key}.json`,"utf8"));
  const forkUrl=values["fork-url"]??chain.rpcUrls[0],port=Number(values.port),url=`http://127.0.0.1:${port}`,relay=relayBase(values.relay),preset=BEACON_PRESETS.evmnet;
  if(!Number.isInteger(port)||port<1||port>65535)throw new Stop("--port must be a port number");
  // An RPC URL may carry an API key: only its origin is printed.
  const forkOrigin=(()=>{try{return new URL(forkUrl).origin;}catch{throw new Stop("--fork-url is not a URL");}})();
  const sources=(name:string,result?:string,address?:string)=>{
    if(result&&address)throw new Stop(`Give the ${name} as --${name}-result or --${name}, not both`);
    if(!result&&!address&&!values["local-deploy"])throw new Stop(`Pass --${name}-result <mined result>, --${name} <deployed address> or --local-deploy`);
  };
  sources("implementation",values["implementation-result"],values.implementation);sources("verifier",values["verifier-result"],values.verifier);
  compileContracts();
  const answers=()=>fetch(url,{method:"POST",headers:{"content-type":"application/json"},body:JSON.stringify({jsonrpc:"2.0",id:1,method:"eth_chainId",params:[]})}).then(r=>r.ok,()=>false);
  if(await answers())throw new Stop(`A node already answers on ${url}; pass another --port`);
  // The registry's backup committers come from its BackupCommitterSet events since its deployment block. The scan is some 200 pages of
  // 10,000 blocks, which is what the network's RPC answers eth_getLogs for (its other public nodes do not serve logs that old at all), so it
  // runs first and alone, before the fork asks the network for state: one page at a time with a pause between them, so that the node does
  // not see a burst it would rate-limit, and a page that fails is asked again after a pause. The fork's block is a few blocks later than
  // the scan's; those are read once the fork is up.
  const providers:JsonRpcProvider[]=[];
  const deploymentBlock=(manifest.transactions as Array<{contract?:string;block?:number}>|undefined)?.find(entry=>entry.contract==="registry")?.block;
  if(!Number.isSafeInteger(deploymentBlock))throw new Stop("The manifest records no deployment block for the registry, from which its BackupCommitterSet events are read");
  const logNetwork=Network.from(chain.chainId),logNode=calmReads(new JsonRpcProvider(forkUrl,logNetwork,{staticNetwork:logNetwork,batchMaxCount:1,cacheTimeout:-1}),{onWait:waiting});
  providers.push(logNode);
  const logSource:LogSource={getLogs:filter=>logNode.getLogs(filter)};
  const scanned=await (async()=>{
    try{
      const toBlock=await logNode.getBlockNumber();
      step(`reading the BackupCommitterSet events of the registry from its deployment block ${deploymentBlock} to block ${toBlock} from ${forkOrigin}`);
      const history=await readBackupCommitters(logSource,getAddress(manifest.registry),deploymentBlock!,toBlock,{pauseMs:LOG_PAUSE_MS,progress:(page,pages)=>{if(page%25===0||page===pages)step(`  events page ${page} of ${pages}`);}});
      return {toBlock,history};
    }catch(error){for(const node of providers)node.destroy();throw error;}
  })();
  step(`starting anvil on ${url}, forking ${forkOrigin}`);
  const anvil=spawn("anvil",["--fork-url",forkUrl,"--port",String(port),"--chain-id",String(chain.chainId),"--no-mining","--retries","20","--fork-retry-backoff","1000","--silent"],{stdio:["ignore","ignore","pipe"],windowsHide:true});
  let anvilErr="",anvilFailed=false;anvil.stderr.on("data",d=>{anvilErr+=d;});anvil.on("error",error=>{anvilFailed=true;anvilErr+=String(error);});
  process.once("SIGINT",()=>{anvil.kill();process.exit(130);});
  try {
    for(let i=0;;i++){
      if(await answers())break;
      if(i>240||anvilFailed||anvil.exitCode!==null)throw new Stop(`anvil did not start: ${anvilErr.split(forkUrl).join(forkOrigin).slice(-400)}`);
      await sleep(500);
    }
    const network=Network.from(chain.chainId);
    // No result cache: state changes between reads that come within its 250 ms.
    const connect=(endpoint:string)=>{const p=calmReads(new JsonRpcProvider(endpoint,network,{staticNetwork:network,batchMaxCount:1,cacheTimeout:-1}),{onWait:waiting});providers.push(p);return p;};
    const rpc=connect(url);
    const send=(method:string,params:unknown[])=>rpc.send(method,params);
    const forkBlock=await rpc.getBlockNumber();
    Object.assign(report,{chain:chain.key,fork:forkOrigin,forkBlock});

    // Time: the fork's blocks carry explicit timestamps that never pass the wall clock.
    const wall=()=>Math.floor(Date.now()/1000);
    let lastTs=Number((await rpc.getBlock("latest"))!.timestamp),maxAhead=-Infinity,waited=0;
    const forkStartTs=lastTs,startWall=wall(),nextStamp=()=>Math.max(lastTs,Math.min(wall(),lastTs+1));
    const mine=async(blocks=1)=>{for(let i=0;i<blocks;i++){const timestamp=nextStamp();await send("evm_mine",[{timestamp}]);lastTs=timestamp;maxAhead=Math.max(maxAhead,timestamp-wall());}};
    const resync=async()=>{lastTs=Number((await rpc.getBlock("latest"))!.timestamp);};
    /// Wait until the wall clock is past the fork's latest block, so that the relay has the rounds up to it.
    const catchUp=async()=>{const ahead=lastTs-wall();if(ahead>=0){waited+=ahead+1;await sleep((ahead+1)*1000);}};
    const snapshot=()=>send("evm_snapshot",[]);
    const revert=async(id:unknown)=>{if(!await send("evm_revert",[id]))throw new Stop("evm_revert failed");await resync();};
    const impersonate=async(address:string)=>{await send("anvil_impersonateAccount",[address]);if(await rpc.getBalance(address)<10n**18n)await send("anvil_setBalance",[address,toBeHex(10n**18n)]);return address;};
    /// Every fork transaction is sent, then mined in a block of its own.
    const tx=async(from:string,to:string|null,data:string,value=0n,gas=EXECUTE_GAS):Promise<TransactionReceipt>=>{
      const params:Record<string,string>={from,data,value:toBeHex(value),gas:toBeHex(gas)};if(to)params.to=to;
      const hash:string=await send("eth_sendTransaction",[params]);
      await mine();
      const receipt=await rpc.getTransactionReceipt(hash);
      if(!receipt)throw new Stop(`No receipt for ${hash}`);
      return receipt;
    };
    const write=(from:string,contract:Contract,method:string,args:unknown[],value=0n,gas=EXECUTE_GAS)=>tx(from,contract.target as string,contract.interface.encodeFunctionData(method,args),value,gas);
    const events=(receipt:TransactionReceipt,contract:Contract,name:string)=>receipt.logs.filter(l=>l.address===contract.target).map(l=>{try{return contract.interface.parseLog(l);}catch{return null;}}).filter(e=>e?.name===name);
    const emitted=(receipt:TransactionReceipt,address:string,signature:string)=>receipt.logs.some(l=>l.address===address&&l.topics[0]===eventId(signature));
    await catchUp();

    // 1. The proxies run what the manifest says.
    const registryAbi=(await artifact("EpochEntropy.sol/EpochEntropy.json")).abi,coordinatorAbi=(await artifact("D20VRFCoordinator.sol/D20VRFCoordinator.json")).abi;
    const registryAddress=getAddress(manifest.registry),coordinatorAddress=getAddress(manifest.coordinator);
    const registry=new Contract(registryAddress,registryAbi,rpc),rng=new Contract(coordinatorAddress,coordinatorAbi,rpc);
    const slotImplementation=async(address:string)=>getAddress("0x"+(await rpc.getStorage(address,IMPLEMENTATION_SLOT)).slice(-40));
    const oldImplementation=await slotImplementation(registryAddress);
    for(const [name,proxy,field] of [["registry",registryAddress,"epochImplementation"],["coordinator",coordinatorAddress,"coordinatorImplementation"]] as const){
      const implementation=await slotImplementation(proxy);
      check(keccak256(await rpc.getCode(proxy))===manifest[`${name}CodeHash`]&&implementation===getAddress(manifest[field])&&keccak256(await rpc.getCode(implementation))===manifest[`${field}CodeHash`],
        `the ${name} proxy runs the manifest's ${field}, and the proxy and implementation code hashes match`);
    }
    const owner=getAddress(await registry.owner());
    check(owner===chain.owner,"the registry owner is the chain profile's owner");
    const committer=getAddress(await registry.committer());
    // The backup committers, from the registry's own events since its deployment block, read before the fork started: their entries are storage
    // words that the upgrade must leave alone, so a backup committer nobody named must not go unchecked. Only the blocks the network added
    // since are read now, up to the fork block.
    const history=forkBlock>scanned.toBlock?await readBackupCommitters(logSource,registryAddress,scanned.toBlock+1,forkBlock,{before:scanned.history,pauseMs:LOG_PAUSE_MS}):scanned.history;
    const extras=values["backup-committer"].map(a=>getAddress(a)),backups=[...new Set([...history.state.keys(),...extras])];
    const allowedNow=await Promise.all([...history.state.keys()].map(async account=>[account,Boolean(await registry.isBackupCommitter(account))] as const));
    check(allowedNow.every(([account,allowed])=>allowed===history.state.get(account)),`every account named by a BackupCommitterSet event is in the state its last event left it: ${history.active.length} of ${history.state.size} are backup committers now`);
    check(BigInt(history.active.length)===BigInt(await registry.backupCommitterCount()),`backupCommitterCount() equals the ${history.active.length} backup committers the ${history.events} events name, so none is missing from the comparison`);
    report.backupCommitters={deploymentBlock,toBlock:forkBlock,pageBlocks:LOG_PAGE_BLOCKS,pages:history.pages,events:history.events,active:history.active,everNamed:[...history.state.keys()],extras};
    const safe=new Contract(owner,SAFE,rpc);
    const safeInfo=await (async()=>{
      try{const [owners,threshold,nonce,version]=await Promise.all([safe.getOwners(),safe.getThreshold(),safe.nonce(),safe.VERSION()]);
        return {owners:(owners as string[]).map(a=>getAddress(a)),threshold:Number(threshold),nonce:BigInt(nonce),version:String(version),guard:getAddress("0x"+(await rpc.getStorage(owner,GUARD_SLOT)).slice(-40))};}
      catch{return undefined;}
    })();
    if(values.multisend&&!safeInfo)throw new Stop("--multisend applies only where the owner is a Safe");
    const multiSendCodes=await Promise.all(MULTISEND_CALL_ONLY.map(async([version,address])=>({version,address,code:(await rpc.getCode(address))!=="0x"})));
    let multiSend:string|undefined;
    if(safeInfo){
      if(values.multisend==="none")multiSend=undefined;
      else if(values.multisend){multiSend=getAddress(values.multisend.toLowerCase());if(await rpc.getCode(multiSend)==="0x")throw new Stop(`--multisend ${multiSend} has no code on the fork`);}
      else multiSend=multiSendCodes.find(c=>c.code)?.address;
    }
    report.owner={address:owner,kind:safeInfo?`Safe ${safeInfo.version}, ${safeInfo.threshold} of ${safeInfo.owners.length}`:"account",safe:safeInfo,
      multiSendCallOnly:safeInfo&&{used:multiSend??null,withCode:multiSendCodes.filter(c=>c.code).map(c=>`${c.version} ${c.address}`),withoutCode:multiSendCodes.filter(c=>!c.code).map(c=>`${c.version} ${c.address}`)}};
    if(safeInfo&&!multiSend)console.error(`!!  ${values.multisend==="none"?"--multisend none":"no MultiSendCallOnly has code on the fork"}: batch A falls back to sequential calls from the impersonated Safe, and its atomicity is not rehearsed`);

    // 2. The registry before the upgrade: raw storage and views.
    step("reading the registry before the upgrade");
    const count=Number(await registry.recipeCount()),current=BigInt(await registry.epochForBlock(forkBlock)),firstEpochStart=BigInt(await registry.firstEpochStart());
    check(count===11,"the registry holds recipes 0-10, so the beacon becomes recipe 11");
    check(same(numbers((await registry.catalogAt(current))[1]),[6,7,8,9,10]),"the catalog in force is the passthrough catalog [6,7,8,9,10]");
    const beaconId=BigInt(count),request=beaconCanonicalRequest(preset.chainHash);
    // The latest published epochs join the recent ones, so that real records must survive too. The newest requests name their epochs, and a published epoch has an epoch hash.
    const published:bigint[]=[],nextRequestId=BigInt(await rng.nextRequestId());
    for(let id=nextRequestId-1n;id>=1n&&id>=nextRequestId-16n&&published.length<2;id--){
      const epoch=BigInt((await rng.getRequest(id)).epochId);
      if(epoch<=current&&!published.includes(epoch)&&(await registry.getEpoch(epoch)).epochHash!==ZeroHash)published.push(epoch);
    }
    const recent=[...new Set([...Array.from({length:7},(_,i)=>current-4n+BigInt(i)),...published])].filter(epoch=>epoch>=1n);
    report.snapshot={epochs:recent.map(String),publishedEpochsFound:published.map(String)};
    const readWords=async(slots:bigint[])=>{const out=new Map<bigint,bigint>();for(let i=0;i<slots.length;i+=STORAGE_READS_AT_ONCE)await Promise.all(slots.slice(i,i+STORAGE_READS_AT_ONCE).map(async slot=>{out.set(slot,BigInt(await rpc.getStorage(registryAddress,slot)));}));return out;};
    const slots=await layout(async slot=>BigInt(await rpc.getStorage(registryAddress,slot)),recent,backups);
    const storageBefore=await readWords(slots);
    report.snapshot.words=slots.length;
    const views=async()=>({owner:await registry.owner(),pendingOwner:await registry.pendingOwner(),committer:await registry.committer(),backupCommitterCount:await registry.backupCommitterCount(),
      backups:Object.fromEntries(await Promise.all(backups.map(async a=>[a,[await registry.isBackupCommitter(a),await registry.isAuthorizedCommitter(a)]]))),
      catalogHash:await registry.catalogHash(),firstEpochStart:await registry.firstEpochStart(),
      signers:[await registry.hyperliquidSigner(),await registry.ethereumBlockSigner(),await registry.btcTradeSigner(),await registry.ethTradeSigner()],
      recipes:await Promise.all(Array.from({length:count},(_,i)=>registry.getRecipe(i))),
      catalogs:await Promise.all([current,current+1n,current+2n].map(async epoch=>[await registry.catalogAt(epoch),await registry.sourceCountAt(epoch)])),
      epochs:await Promise.all(recent.filter(epoch=>epoch<=current).map(async epoch=>[await registry.getEpoch(epoch),await registry.epochAnchors(epoch)])),
      selection:await registry.getEpochSelection(current),epochAtFork:await registry.epochForBlock(forkBlock),epochStart:await registry.epochStart(current),
      protocolConfigurationHash:await rng.protocolConfigurationHash(),epochRegistry:await rng.epochRegistry()});
    const viewsBefore=await views();

    // 3. The verifier and the implementation, as this run's options say, each checked against this build.
    step("placing the verifier and the implementation on the fork");
    const place=async(name:"EpochEntropy"|"D20BeaconVerifier",label:string,gas:bigint,result?:string,given?:string)=>{
      if(given){const address=getAddress(given.toLowerCase());if(await rpc.getCode(address)==="0x")throw new Stop(`The ${label} ${address} has no code on the fork`);return {address,mode:"deployed address"};}
      const deployer=await impersonate(manifest.deployer),code=await initCode(name,[]);
      if(result){
        const mined=await candidate(result,code,chain);
        if(await rpc.getCode(mined.address)!=="0x")return {address:mined.address,mode:"CREATE2 mined result, already deployed"};
        const receipt=await tx(deployer,chain.create2.factory,mined.data,0n,gas);
        check(receipt.status===1&&await rpc.getCode(mined.address)!=="0x",`the ${label} deploys through the CREATE2 factory from the deployer`);
        return {address:mined.address,mode:"CREATE2 mined result",salt:mined.salt,gasUsed:receipt.gasUsed,costAt21GweiUSDC:formatUnits(receipt.gasUsed*21_000_000_000n,18)};
      }
      const receipt=await tx(deployer,null,code,0n,gas);
      if(receipt.status!==1||!receipt.contractAddress)throw new Stop(`Deployment of the ${label} failed`);
      return {address:getAddress(receipt.contractAddress),mode:"plain CREATE on the fork from the deployer",gasUsed:receipt.gasUsed,costAt21GweiUSDC:formatUnits(receipt.gasUsed*21_000_000_000n,18)};
    };
    const placedVerifier=await place("D20BeaconVerifier","verifier",3_000_000n,values["verifier-result"],values.verifier);
    const placedImplementation=await place("EpochEntropy","implementation",8_000_000n,values["implementation-result"],values.implementation);
    const verifier=placedVerifier.address,implementation=placedImplementation.address;
    for(const [name,label,address,placed] of [["D20BeaconVerifier","verifier",verifier,placedVerifier],["EpochEntropy","implementation",implementation,placedImplementation]] as const){
      const code=await rpc.getCode(address);
      check(keccak256(code)===await compiledRuntimeCodeHash(name,address),`the ${label} runtime code equals the freshly compiled ${name} at ${address}`);
      report[label]={...placed,runtimeBytes:dataLength(code),runtimeCodeHash:keccak256(code)};
    }
    check(report.implementation.runtimeBytes<=24_576,`the implementation's ${report.implementation.runtimeBytes} runtime bytes fit the 24,576-byte limit`);
    check(implementation!==oldImplementation,"the new implementation is not the live one");
    const registration={verifier,genesis:preset.genesis,period:preset.period,chainHash:preset.chainHash,publicKey:preset.publicKey};
    const expected=beaconWords(beaconId,request,BEACON_TEMPLATE,registration);
    const emptyBefore=await readWords([...expected.keys()].filter(slot=>slot!==10n));
    check([...emptyBefore.values()].every(word=>word===0n),`the ${emptyBefore.size} words that recipe ${beaconId} and its registration will occupy are empty before`);

    // 4. A live sample round from the relay.
    step("fetching a sample round from the relay");
    const sample=await fetchBeaconSample(relay,preset);
    const sampleTime=checkBeaconSample(preset,sample,lastTs);
    const verifierView=new Contract(verifier,BEACON_ABI,rpc);
    check(verifyBeaconRound(preset.publicKey,sample.round,sample.signature)&&await verifierView.isValidPublicKey(preset.publicKey)&&await verifierView.verifyRound(preset.publicKey,sample.round,sample.signature),
      `sample round ${sample.round} verifies under the evmnet group key, locally and in the verifier on the fork, and is scheduled before the fork's head block`);
    report.sample={round:sample.round,time:sampleTime,timeUtc:new Date(Number(sampleTime)*1000).toISOString(),secondsBeforeHeadBlock:BigInt(lastTs)-sampleTime,source:relay};

    // The live node's own answer, before anything is deployed there: the fork runs Ethereum's rules, the network's node its own,
    // precompiles and gas included. It simulates the upgrade and the registration from the owner on the current state, the new code
    // injected by state override (runtime code at these addresses, and the implementation slot for the registration). Nothing is sent.
    step("simulating the upgrade and the registration on the live node");
    {
      const live=connect(forkUrl),overrideCode={[verifier]:{code:await rpc.getCode(verifier)},[implementation]:{code:await rpc.getCode(implementation)}};
      const upgraded={...overrideCode,[registryAddress]:{stateDiff:{[IMPLEMENTATION_SLOT]:zeroPadValue(implementation,32)}}};
      const registerData=registry.interface.encodeFunctionData("registerBeacon",registerBeaconArgs(verifier,preset,sample));
      const upgradeData=registry.interface.encodeFunctionData("upgradeToAndCall",[implementation,"0x"]);
      const verifyData=verifierView.interface.encodeFunctionData("verifyRound",[preset.publicKey,sample.round,sample.signature]);
      const simulate=async(call:Record<string,string>,overrides:object)=>({answer:await live.send("eth_call",[call,"latest",overrides]) as string,gas:BigInt(await live.send("eth_estimateGas",[call,"latest",overrides]))});
      try{
        const register=await simulate({from:owner,to:registryAddress,data:registerData},upgraded);
        const upgrade=await simulate({from:owner,to:registryAddress,data:upgradeData},overrideCode);
        const verify=await simulate({to:verifier,data:verifyData},overrideCode);
        check(BigInt(register.answer)===beaconId,`the live node runs registerBeacon on the new code from the owner and returns recipe ${beaconId}, estimating ${register.gas} gas`);
        check(upgrade.answer==="0x","the live node runs upgradeToAndCall(new implementation, 0x) from the owner, estimating "+upgrade.gas+" gas");
        check(BigInt(verify.answer)===1n&&verify.gas<400_000n,`the live node verifies round ${sample.round} in the verifier for an estimated ${verify.gas} gas, inside the registry's 400,000-gas allowance`);
        // The registry's gas rule under this node's own EVM: too little gas for the verifier's whole allowance is BeaconGasTooLow, never a refused key or signature.
        const short=await live.send("eth_call",[{from:owner,to:registryAddress,data:registerData,gas:toQuantity(300_000)},"latest",upgraded]).then(()=>"succeeded",(error:any)=>String(error?.data??""));
        check(short===registry.interface.getError("BeaconGasTooLow")!.selector,"with a gas limit of 300,000 the live node's registerBeacon reverts BeaconGasTooLow, not InvalidConfig");
        report.liveNode={registerBeaconEstimatedGas:register.gas,upgradeToAndCallEstimatedGas:upgrade.gas,verifyRoundEstimatedGas:verify.gas};
      }catch(error){
        if(error instanceof Stop)throw error;
        if(/revert/i.test(String((error as Error)?.message)))throw new Stop(`The live node reverts the simulated upgrade or registration: ${revertReason(error)}`);
        report.liveNode={notSimulated:revertReason(error)};
        console.error(`!!  the live node did not simulate with state overrides: ${revertReason(error)}`);
      }
    }

    // 5. Batch A: the upgrade, then the registration.
    const upgradeCall={to:registryAddress,data:registry.interface.encodeFunctionData("upgradeToAndCall",[implementation,"0x"])};
    const registerCall=(s:BeaconSample)=>({to:registryAddress,data:registry.interface.encodeFunctionData("registerBeacon",registerBeaconArgs(verifier,preset,s))});
    const multiSendData=(calls:Array<{to:string;data:string}>)=>MULTISEND.encodeFunctionData("multiSend",["0x"+calls.map(c=>solidityPacked(["uint8","address","uint256","uint256","bytes"],[0,c.to,0,dataLength(c.data),c.data]).slice(2)).join("")]);
    /// The Safe's approval flow: every threshold owner approves the transaction hash, then the first owner executes with the approvals
    /// as signatures. `exact` sends with the gas limit eth_estimateGas answers, as a wallet does, instead of a generous one.
    const safeExec=async(to:string,data:string,operation:0|1,exact:boolean)=>{
      const info=safeInfo!,nonce=BigInt(await safe.nonce()),args=[to,0,data,operation,0,0,0,ZeroAddress,ZeroAddress] as const;
      const safeTxHash:string=await safe.getTransactionHash(...args,nonce),signers=info.owners.slice(0,info.threshold),approvals:boolean[]=[];
      for(const signer of signers)approvals.push((await write(await impersonate(signer),safe,"approveHash",[safeTxHash])).status===1);
      check(approvals.every(Boolean),`the ${signers.length} threshold owners approve the Safe transaction hash`);
      const signatures="0x"+[...signers].sort((a,b)=>BigInt(a)<BigInt(b)?-1:1).map(a=>zeroPadValue(a,32).slice(2)+"00".repeat(32)+"01").join("");
      const execData=safe.interface.encodeFunctionData("execTransaction",[...args,signatures]);
      const call={from:signers[0],to:owner,data:execData};
      const simulated=await rpc.call({...call,gasLimit:EXECUTE_GAS}).then(()=>"succeeds",revertReason);
      const estimate=simulated==="succeeds"?await rpc.estimateGas(call):undefined;
      const receipt=await tx(signers[0],owner,execData,0n,exact&&estimate?estimate:EXECUTE_GAS);
      return {receipt,safeTxHash,nonce,simulated,estimate};
    };
    const flipped=(s:BeaconSample):BeaconSample=>({...s,signature:toBeHex(BigInt(s.signature)^1n,64)});
    const viaMultiSend=Boolean(safeInfo&&multiSend);
    if(viaMultiSend){
      step("batch A through the Safe: a batch with a tampered sample signature");
      const snap=await snapshot(),nonceBefore=safeInfo!.nonce;
      const bad=await safeExec(multiSend!,multiSendData([upgradeCall,registerCall(flipped(sample))]),1,false);
      check(bad.receipt.status===0,"the batch with a tampered sample signature fails as a whole: execTransaction reverts");
      check(await slotImplementation(registryAddress)===oldImplementation&&BigInt(await safe.nonce())===nonceBefore&&Number(await registry.recipeCount())===count&&!emitted(bad.receipt,registryAddress,"Upgraded(address)"),
        "after the failed batch the implementation slot, the recipe count and the Safe nonce are unchanged, and no Upgraded event was emitted");
      report.atomicity={tamperedSampleSignature:flipped(sample).signature,executeSimulation:bad.simulated,failedExecuteGasUsed:bad.receipt.gasUsed};
      await revert(snap);
      // Each call's own gas, sent by the owner on a snapshot.
      const direct=await snapshot(),from=await impersonate(owner);
      const upgrade=await tx(from,registryAddress,upgradeCall.data),register=await tx(from,registryAddress,registerCall(sample).data);
      check(upgrade.status===1&&register.status===1,"the upgrade and the registration also succeed as two plain calls from the owner");
      check(register.gasUsed<=950_000n,`registerBeacon used ${register.gasUsed} gas, within the 950,000 cap of admin.ts`);
      report.gas={upgradeToAndCall:upgrade.gasUsed,registerBeacon:register.gasUsed};
      await revert(direct);
    }
    step(viaMultiSend?"batch A through the Safe: MultiSendCallOnly delegatecall with execTransaction":`batch A as sequential calls from the ${safeInfo?"impersonated Safe":"owner"}`);
    let receiptsA:TransactionReceipt[];
    if(viaMultiSend){
      const {receipt,safeTxHash,nonce,estimate}=await safeExec(multiSend!,multiSendData([upgradeCall,registerCall(sample)]),1,true);
      check(receipt.status===1&&emitted(receipt,owner,"ExecutionSuccess(bytes32,uint256)")&&!emitted(receipt,owner,"ExecutionFailure(bytes32,uint256)"),"the Safe executes batch A with the estimated gas limit: ExecutionSuccess");
      check(BigInt(await safe.nonce())===nonce+1n,"the Safe nonce advanced by one");
      report.batchA={path:"MultiSendCallOnly delegatecall through execTransaction",multiSend,safeTxHash,nonce,estimatedGas:estimate,gasUsed:receipt.gasUsed,transactionHash:receipt.hash};
      receiptsA=[receipt];
    } else {
      const from=await impersonate(owner);
      const upgrade=await tx(from,registryAddress,upgradeCall.data);
      check(upgrade.status===1&&await slotImplementation(registryAddress)===implementation&&Number(await registry.recipeCount())===count,"upgradeToAndCall alone swaps the implementation and registers nothing");
      const refused=await tx(from,registryAddress,registerCall(flipped(sample)).data);
      check(refused.status===0&&Number(await registry.recipeCount())===count,"registerBeacon with a tampered sample signature reverts and registers nothing");
      const register=await tx(from,registryAddress,registerCall(sample).data);
      check(register.status===1&&register.gasUsed<=950_000n,`registerBeacon succeeds with ${register.gasUsed} gas, within the 950,000 cap of admin.ts`);
      report.gas={upgradeToAndCall:upgrade.gasUsed,registerBeacon:register.gasUsed};
      report.batchA={path:safeInfo?"sequential calls from the impersonated Safe":"sequential calls from the owner",gasUsed:upgrade.gasUsed+register.gasUsed};
      receiptsA=[upgrade,register];
    }
    check(await slotImplementation(registryAddress)===implementation,"the implementation slot holds the new implementation");
    check(receiptsA.some(r=>emitted(r,registryAddress,"Upgraded(address)")),"batch A emits Upgraded from the registry proxy");
    const registered=receiptsA.flatMap(r=>events(r,registry,"BeaconRegistered")),recipeEvents=receiptsA.flatMap(r=>events(r,registry,"RecipeRegistered"));
    check(registered.length===1&&registered[0]!.args.recipe===beaconId&&registered[0]!.args.verifier===verifier&&registered[0]!.args.chainHash===preset.chainHash&&registered[0]!.args.publicKey===preset.publicKey&&
      registered[0]!.args.genesis===preset.genesis&&registered[0]!.args.period===preset.period&&recipeEvents.length===1&&recipeEvents[0]!.args.recipe===beaconId&&recipeEvents[0]!.args.canonicalRequest===request&&
      recipeEvents[0]!.args.body===request&&recipeEvents[0]!.args.template===BEACON_TEMPLATE,`batch A emits RecipeRegistered and BeaconRegistered for recipe ${beaconId} with the evmnet registration and the canonical request`);

    // 6. After A: every old word and view is as it was.
    step("comparing the registry after batch A");
    const storageAfter=await readWords(slots),changed=slots.filter(slot=>storageAfter.get(slot)!==storageBefore.get(slot));
    check(changed.length===1&&changed[0]===10n&&storageBefore.get(10n)===BigInt(count)&&storageAfter.get(10n)===BigInt(count+1),
      `all ${slots.length} compared storage words are unchanged except the recipe array length, slot 10: ${count} to ${count+1}`);
    const written=await readWords([...expected.keys()]);
    check([...expected].every(([slot,word])=>written.get(slot)===word),`the ${expected.size} words of recipe ${beaconId} and its registration hold exactly the encoded registration, the recipe array length included`);
    check(same(await views(),viewsBefore),"every old view is identical: owner, pendingOwner, committer, backup committers, recipes 0-10, catalogs and source counts of the current and next two epochs, catalogHash, signers, recent epochs and anchors, the current selection, and the coordinator's configuration hash");
    check(BigInt(await registry.recipeCount())===BigInt(count+1),`recipeCount is ${count+1}`);
    const slot=getAddress(await registry.slotSigner(beaconId)),onchain=await registry.beaconOf(beaconId);
    check(same(Array.from(onchain),[verifier,preset.genesis,preset.period,preset.chainHash,preset.publicKey]),`beaconOf(${beaconId}) is the verifier and the evmnet registration`);
    check(slot===beaconSlotSigner(registration),`slotSigner(${beaconId}) equals the library's beaconSlotSigner, ${slot}`);
    check(await registry.verifyBeacon(beaconId,sample.round,sample.signature)===true&&await registry.verifyBeacon(beaconId,sample.round,flipped(sample).signature)===false,`verifyBeacon(${beaconId}) accepts the sample and refuses a tampered one`);
    // Too little gas for the verifier's allowance is an error of its own, for the valid sample and for the tampered one alike, never a false.
    for(const [name,given] of [["the sample",sample],["a tampered sample",flipped(sample)]] as const){
      const refused=await rpc.call({to:registryAddress,data:registry.interface.encodeFunctionData("verifyBeacon",[beaconId,given.round,given.signature]),gasLimit:300_000}).then(()=>"answered",(error:any)=>String(error?.data??""));
      check(refused===registry.interface.getError("BeaconGasTooLow")!.selector,`verifyBeacon(${beaconId}) for ${name} at a gas limit of 300,000 reverts BeaconGasTooLow instead of answering`);
    }
    const [queryHash,canonicalRequest,template,body]=await registry.getRecipe(beaconId);
    check(canonicalRequest===request&&template===BEACON_TEMPLATE&&body===request&&queryHash===eventId(request)&&template==="0x040113",`getRecipe(${beaconId}) is ["drand","${preset.chainHash.slice(0,6)}…"], the template 0x040113 and the same string as body`);
    const signed=await Promise.all(Array.from({length:count},async(_,i)=>[(await registry.beaconOf(i)).verifier,await registry.slotSigner(i)]));
    check(signed.every(([v,s])=>BigInt(v)===0n&&BigInt(s)===0n),`recipes 0-${count-1} are signed recipes: no verifier and a zero slotSigner`);
    const book=await readEpochRecipes(rpc,registryAddress,Array.from({length:count+1},(_,i)=>i));
    check(book[Number(beaconId)].beacon?.verifier===verifier&&book[Number(beaconId)].beacon?.publicKey===preset.publicKey&&Object.keys(book).length===count+1,"the replay library reads every recipe, the beacon's registration included");

    // 7. Batch B: the catalog. Its first epoch leaves room for the blocks it takes to execute, so it cannot straddle an epoch boundary.
    step("batch B: scheduleCatalog for the beacon");
    const nowEpoch=BigInt(await registry.epochForBlock(BigInt(await rpc.getBlockNumber())+6n)),fromEpoch=nowEpoch+2n;
    const scheduleData=registry.interface.encodeFunctionData("scheduleCatalog",[[beaconId],[slot],fromEpoch]);
    let scheduled:TransactionReceipt;
    if(safeInfo){
      const snap=await snapshot(),direct=await tx(await impersonate(owner),registryAddress,scheduleData);
      check(direct.status===1,"scheduleCatalog also succeeds as a plain call from the owner");
      report.gas={...report.gas,scheduleCatalog:direct.gasUsed};
      await revert(snap);
      const {receipt,safeTxHash,nonce,estimate}=await safeExec(registryAddress,scheduleData,0,true);scheduled=receipt;
      check(receipt.status===1&&emitted(receipt,owner,"ExecutionSuccess(bytes32,uint256)"),"the Safe executes batch B with the estimated gas limit: ExecutionSuccess");
      report.batchB={path:"direct call through execTransaction",safeTxHash,nonce,estimatedGas:estimate,gasUsed:receipt.gasUsed,transactionHash:receipt.hash};
    } else {
      scheduled=await tx(await impersonate(owner),registryAddress,scheduleData);
      check(scheduled.status===1,"scheduleCatalog succeeds from the owner");
      report.gas={...report.gas,scheduleCatalog:scheduled.gasUsed};
      report.batchB={path:"call from the owner",gasUsed:scheduled.gasUsed};
    }
    const [scheduledEvent]=events(scheduled,registry,"CatalogScheduled"),beaconCatalogHash=epochCatalogHash([slot],[Number(beaconId)]);
    check(scheduledEvent?.args.fromEpoch===fromEpoch&&scheduledEvent.args.catalogHash===beaconCatalogHash&&same(numbers(scheduledEvent.args.recipes),[Number(beaconId)])&&same(scheduledEvent.args.signers,[slot]),
      `scheduleCatalog emits CatalogScheduled for epoch ${fromEpoch}: recipes [${beaconId}] and the beacon's slotSigner`);
    const [hashAt,recipesAt,signersAt]=await registry.catalogAt(fromEpoch);
    check(hashAt===beaconCatalogHash&&same(numbers(recipesAt),[Number(beaconId)])&&same(signersAt,[slot])&&BigInt(await registry.sourceCountAt(fromEpoch))===1n,`catalogAt(${fromEpoch}) is the beacon alone`);
    check(same(numbers((await registry.catalogAt(nowEpoch))[1]),[6,7,8,9,10])&&same(numbers((await registry.catalogAt(nowEpoch+1n))[1]),[6,7,8,9,10]),"the current and next epoch keep the passthrough catalog");
    check(await registry.catalogHash()===viewsBefore.catalogHash,"catalogHash(), and with it the coordinator's protocolConfigurationHash, is unchanged");

    // 8. Mine to the beacon catalog's first epoch.
    const start=BigInt(await registry.epochStart(fromEpoch)),head=BigInt(await rpc.getBlockNumber());
    step(`mining ${start-head} blocks to epoch ${fromEpoch}'s first block ${start}`);
    if(start>head)await mine(Number(start-head));
    check(BigInt(await rpc.getBlockNumber())===start,`the fork stands at the first block of epoch ${fromEpoch}`);
    const selection=await registry.getEpochSelection(fromEpoch);
    check(selection.recipe===beaconId&&getAddress(selection.airnode)===slot&&selection.canonicalRequest===request&&selection.source===0n,`epoch ${fromEpoch} selects the beacon recipe ${beaconId}`);
    const startTimestamp=Number((await rpc.getBlock(Number(start)))!.timestamp);

    // 9. A request served from a real drand epoch. The fork's proofs use the public test key.
    step("request, real drand round, commitEpoch, fulfilment");
    const [x,y]=publicKey();
    await send("anvil_setStorageAt",[coordinatorAddress,toBeHex(2,32),toBeHex(x,32)]);await send("anvil_setStorageAt",[coordinatorAddress,toBeHex(3,32),toBeHex(y,32)]);
    await send("anvil_setStorageAt",[coordinatorAddress,toBeHex(4,32),keccak256(coder.encode(["uint256[2]"],[[x,y]]))]);
    const keeper=await impersonate(committer),consumerArtifact=await artifact("test/TestConsumers.sol/TestConsumer.json");
    const deployed=await tx(keeper,null,consumerArtifact.bytecode+new Interface(consumerArtifact.abi).encodeDeploy([coordinatorAddress]).slice(2));
    if(deployed.status!==1||!deployed.contractAddress)throw new Stop("Deployment of the test consumer failed");
    const consumer=new Contract(deployed.contractAddress,consumerArtifact.abi,rpc),callbackGas=200_000n;
    const baseFee=(await rpc.getBlock("latest"))!.baseFeePerGas!,fee=BigInt(await rng.quoteFeeAt(callbackGas,baseFee*2n));
    const requested=await write(keeper,consumer,"request",[eventId("beacon rehearsal request"),callbackGas,keeper],fee,1_000_000n);
    check(requested.status===1,"the consumer's request escrows its fee before the epoch is published");
    const requestId=BigInt(await consumer.lastRequestId()),pending=await rng.getRequest(requestId);
    check(pending.epochId===fromEpoch&&pending.targetBlock===0n&&pending.epochHash===ZeroHash,`request ${requestId} waits in epoch ${fromEpoch}: no target and no epoch hash yet`);
    // The keeper's round: the one current at the start block's timestamp, or the latest round minus one when that is over 180 s old at submission.
    const latestRound=async()=>{
      const response=await fetch(`${relay}/${preset.chainHash.slice(2)}/public/latest`,{signal:AbortSignal.timeout(10_000)});
      if(!response.ok)throw new Stop(`The relay answered HTTP ${response.status} for its latest round`);
      return BigInt(((await response.json()) as {round:number}).round);
    };
    const keeperRound=async(startAt:number,submitAt:number,latest:()=>Promise<bigint>)=>{
      const wanted=beaconRoundAt(preset,BigInt(startAt)),age=BigInt(submitAt)-beaconRoundTime(preset,wanted);
      return age<=MAX_ROUND_AGE?{round:wanted,policy:"the round current at the start block's timestamp",age}:{round:(await latest())-1n,policy:"the latest round minus one: the start round was older than 180 s",age};
    };
    {
      const startRound=beaconRoundAt(preset,BigInt(startTimestamp)),roundTime=Number(beaconRoundTime(preset,startRound)),stub=async()=>1_000_000n;
      check((await keeperRound(startTimestamp,roundTime+180,stub)).round===startRound&&(await keeperRound(startTimestamp,roundTime+181,stub)).round===999_999n,
        "the round policy keeps the start round up to 180 s of age and uses the latest round minus one beyond it");
    }
    await catchUp();
    const submitAt=nextStamp(),chosen=await keeperRound(startTimestamp,submitAt,latestRound);
    let round:BeaconSample|undefined,attempts=0;
    for(;!round;attempts++){
      try{round=await fetchBeaconSample(relay,preset,{round:chosen.round});}
      catch(error){if(!(error instanceof Stop)||!/HTTP (404|425)\b/.test(error.message)||attempts>=30)throw error;await sleep(1000);}
    }
    const attestation={timestamp:beaconRoundTime(preset,round.round),data:encodeBeaconRound(round.round),signature:round.signature};
    // Sent as the keeper sends it: the gas limit is eth_estimateGas × 1.2 + 50,000. The registry reverts BeaconGasTooLow below the limit at
    // which the verifier gets its whole allowance, so the estimate must be a limit that succeeds.
    const commitData=registry.interface.encodeFunctionData("commitEpoch",[fromEpoch,attestation]);
    const commitEstimate=BigInt(await send("eth_estimateGas",[{from:keeper,to:registryAddress,data:commitData}]));
    const commit=await tx(keeper,registryAddress,commitData,0n,commitEstimate*12n/10n+50_000n);
    const commitTimestamp=BigInt((await rpc.getBlock(commit.blockNumber))!.timestamp);
    check(commit.status===1,`the committer's commitEpoch publishes epoch ${fromEpoch} from drand round ${round.round}, ${commitTimestamp-attestation.timestamp} s old in its block`);
    check(attestation.timestamp<=commitTimestamp&&commitTimestamp-attestation.timestamp<=240n,"the round is not in the future and at most 240 seconds old at the commit block, the registry's freshness bound");
    check(commit.gasUsed<=700_000n&&commit.gasUsed<commitEstimate,`commitEpoch used ${commit.gasUsed} gas, in line with the ~477,000 measured locally, under its eth_estimateGas of ${commitEstimate}`);
    const record=await registry.getEpoch(fromEpoch),[committed]=events(commit,registry,"EpochCommitted");
    check(record.epochHash!==ZeroHash&&committed?.args.epochHash===record.epochHash&&record.committedBlock===BigInt(commit.blockNumber)&&record.signedAt===attestation.timestamp&&record.catalogHash===beaconCatalogHash&&record.queryHash===eventId(request)&&record.source===0n,
      "the epoch record binds the beacon's query, the round's time, the catalog and the commit block");
    check(same(decodeEpochEvidencePacket(committed!.args.packet),{canonicalRequest:request,attestation}),"the published packet is the beacon's request and the signed round, exactly");
    const resolved=await rng.getRequest(requestId);
    check(resolved.targetBlock===BigInt(commit.blockNumber)+1n&&resolved.epochHash===record.epochHash&&resolved.deadline===pending.deadline,"the request resolves to the block after the commit, and its deadline has not moved");
    check(await registry.verifyBeacon(beaconId,round.round,round.signature)===true,`verifyBeacon(${beaconId}) accepts round ${round.round}`);
    await mine(Number(await rng.confirmationBlocks())+1);
    const proof=makeProof(BigInt(await rng.requestSeed(requestId))),fulfilled=await write(keeper,rng,"fulfillRandomness",[requestId,proof],0n,2_000_000n);
    const [paid]=events(fulfilled,rng,"KeeperFeePaid");
    check(fulfilled.status===1&&(await rng.getRequest(requestId)).delivered===true&&await consumer.results(requestId)===proofOutput(proof)&&paid?.args.keeper===committer&&paid.args.paid===true,
      "fulfillRandomness delivers the callback with the VRF output, and the keeper share goes to the committer");
    const fulfilledAt=BigInt((await rpc.getBlock(fulfilled.blockNumber))!.timestamp),requestedAt=BigInt((await rpc.getBlock(requested.blockNumber))!.timestamp);
    check(fulfilledAt<=requestedAt+60n,`the proof is accepted ${fulfilledAt-requestedAt} s after the request, within its 60 seconds`);
    // Replay from public data: the catalog in force and the registry's recipe definitions, read over the fork's RPC.
    const [catalogHashAt,catalogRecipes,catalogSigners]=await registry.catalogAt(fromEpoch);
    const catalog=resolveEpochCatalog({registry:registryAddress,chainId:BigInt(chain.chainId),firstEpochStart,recipeBook:await readEpochRecipes(rpc,registryAddress,catalogRecipes.map(Number))},{hash:catalogHashAt,recipes:catalogRecipes,signers:catalogSigners});
    const replayed=replayEpochCommitment({catalog,epochId:fromEpoch,record,commitTimestamp,packet:committed!.args.packet});
    check(replayed.epochHash===record.epochHash&&getAddress(replayed.signer)===slot&&replayed.selected.recipe===Number(beaconId)&&replayed.selected.beacon?.verifier===verifier,
      "replayEpochCommitment verifies the epoch from public data: selection, the beacon's signature under its registration, and the commitment");
    check(maxAhead<=0,"no fork block was stamped after the wall clock");

    Object.assign(report.gas,{batchA:report.batchA.gasUsed,batchB:report.batchB.gasUsed,commitEpoch:commit.gasUsed,commitEpochEstimate:commitEstimate,fulfillRandomness:fulfilled.gasUsed,deployVerifier:placedVerifier.gasUsed,deployImplementation:placedImplementation.gasUsed});
    report.addresses={registry:registryAddress,coordinator:coordinatorAddress,owner,committer,oldImplementation,implementation,verifier,slotSigner:slot,consumer:consumer.target};
    report.rounds={sample:report.sample,epoch:{epoch:fromEpoch,startBlock:start,startBlockTimestamp:startTimestamp,startRound:beaconRoundAt(preset,BigInt(startTimestamp)),usedRound:round.round,
      roundTime:attestation.timestamp,roundTimeUtc:new Date(Number(attestation.timestamp)*1000).toISOString(),ageAtSubmission:chosen.age,policy:chosen.policy,relayAttempts:attempts+1,
      requestId,requestBlock:requested.blockNumber,commitBlock:commit.blockNumber,commitBlockTimestamp:commitTimestamp,targetBlock:resolved.targetBlock,fulfilBlock:fulfilled.blockNumber}};
    report.time={policy:"explicit evm_mine timestamps: the wall clock's second, never below the previous block, never more than one second after it; no evm_increaseTime",
      forkHeadSecondsBehindWallClockAtStart:startWall-forkStartTs,maxSecondsAheadOfWallClock:maxAhead,waitedForWallClockSeconds:waited,forkHeadTimestamp:lastTs,wallClock:wall()};
    console.error(`${checks.length} checks passed`);
    printReport();
  } finally {
    for(const p of providers)p.destroy();
    anvil.kill();
  }
}
main().catch(error=>{report.failure=error instanceof Error?error.message:String(error);printReport();console.error(`Fork rehearsal stopped: ${report.failure}`);process.exitCode=1;});
