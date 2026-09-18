// Read-only failover evidence from chain data: which wallet published each epoch and fulfilled each request, refunds,
// and keeper transactions that reverted or only cancelled a nonce. Used to verify a primary/follower drill.
import {Contract,Interface,getAddress,type Log,type Provider} from "ethers";

const REGISTRY=new Interface([
  "event EpochCommitted(uint64 indexed epochId, bytes32 indexed epochHash, bytes packet)",
  "event BackupCommitterSet(address indexed account, bool allowed)",
  "event CommitterChanged(address indexed previousCommitter, address indexed newCommitter)",
  "function committer() view returns(address)","function isBackupCommitter(address) view returns(bool)",
  "function getEpoch(uint64) view returns((bytes32 epochHash,bytes32 catalogHash,bytes32 anchorHash,uint8 source,bytes32 queryHash,bytes32 dataHash,bytes32 attestationHash,uint256 signedAt,uint64 committedBlock))",
  "function epochStart(uint64) view returns(uint64)","function catalogAt(uint64) view returns(bytes32 hash,uint8[] recipes,address[] signers)",
  "function commitEpoch(uint64,(uint256,bytes,bytes))","function commitEpochFallback(uint64,uint8,(uint256,bytes,bytes))",
]);
const COORDINATOR=new Interface([
  "event RandomnessRequested(uint256 indexed requestId, address indexed consumer, bytes32 indexed keyHash, bytes32 clientSeed, uint64 requestBlock, uint32 callbackGasLimit, uint256 feePaid, address refundAddress, uint64 deadline)",
  "event RandomnessFulfilled(uint256 indexed requestId, bytes32 randomness, address indexed submitter)",
  "event FulfillmentSkipped(uint256 indexed requestId, uint8 reason)",
  "event RequestRefundedTo(uint256 indexed requestId, address indexed refundAddress, uint256 amount, bool paid)",
  "event KeeperFeePaid(uint256 indexed requestId, address indexed keeper, uint256 amount, bool paid)",
  "function fulfillRandomness(uint256,(uint256[2],uint256[2],uint256,uint256,uint256,address,uint256[2],uint256[2],uint256))",
  "function fulfillRandomnessBatch(uint256[],(uint256[2],uint256[2],uint256,uint256,uint256,address,uint256[2],uint256[2],uint256)[])",
]);
const SKIP_REASONS:Record<number,string>={1:"already fulfilled",2:"refunded",3:"past deadline"};
export const MAX_REPORT_BLOCKS=50_000;
const LOG_CHUNK=10_000,CONCURRENCY=8;

export interface FailoverReportInput {registry:string;coordinator:string;fromBlock:number;toBlock:number;wallets?:readonly string[]}
type Role="committer"|"backup committer"|"listed wallet"|"other";

async function logs(provider:Provider,address:string,fromBlock:number,toBlock:number):Promise<Log[]> {
  const out:Log[]=[];
  for(let start=fromBlock;start<=toBlock;start+=LOG_CHUNK)out.push(...await provider.getLogs({address,fromBlock:start,toBlock:Math.min(toBlock,start+LOG_CHUNK-1)}));
  return out;
}
async function mapLimit<T,R>(items:readonly T[],run:(item:T)=>Promise<R>):Promise<R[]> {
  const out:R[]=new Array(items.length);let next=0;
  await Promise.all(Array.from({length:Math.min(CONCURRENCY,items.length)},async()=>{while(next<items.length){const i=next++;out[i]=await run(items[i]);}}));
  return out;
}
/// Build the report. Roles are read at toBlock; wallets are the committer, every backup committer named by
/// BackupCommitterSet or CommitterChanged in the range, and any listed wallet. Duplicate attempts are the wallets'
/// transactions whose whole work, every batch member or the epoch, was already settled by an earlier transaction:
/// with a primary and a follower, they are the contention the delays are meant to avoid.
export async function failoverReport(provider:Provider,input:FailoverReportInput) {
  const {fromBlock,toBlock}=input;
  if(!Number.isSafeInteger(fromBlock)||!Number.isSafeInteger(toBlock)||fromBlock<0||toBlock<fromBlock)throw new Error("Invalid block range");
  if(toBlock-fromBlock+1>MAX_REPORT_BLOCKS)throw new Error(`The block range exceeds ${MAX_REPORT_BLOCKS} blocks; report a shorter drill window`);
  const registryAddress=getAddress(input.registry),coordinatorAddress=getAddress(input.coordinator);
  const registry=new Contract(registryAddress,REGISTRY,provider);
  const [registryLogs,coordinatorLogs]=await Promise.all([logs(provider,registryAddress,fromBlock,toBlock),logs(provider,coordinatorAddress,fromBlock,toBlock)]);
  const parsedRegistry=registryLogs.map(log=>({log,event:REGISTRY.parseLog(log)})).filter(entry=>entry.event);
  const parsedCoordinator=coordinatorLogs.map(log=>({log,event:COORDINATOR.parseLog(log)})).filter(entry=>entry.event);
  const committer=getAddress(await registry.committer({blockTag:toBlock}));
  const wallets=new Set<string>([committer,...(input.wallets??[]).map(address=>getAddress(address))]);
  for(const {event} of parsedRegistry){
    if(event!.name==="BackupCommitterSet")wallets.add(getAddress(event!.args.account));
    if(event!.name==="CommitterChanged"){wallets.add(getAddress(event!.args.previousCommitter));wallets.add(getAddress(event!.args.newCommitter));}
  }
  const roles=new Map<string,Role>();
  for(const wallet of wallets){
    const backup=wallet!==committer&&await registry.isBackupCommitter(wallet,{blockTag:toBlock}).then((allowed:boolean)=>allowed,()=>false);
    roles.set(wallet,wallet===committer?"committer":backup?"backup committer":(input.wallets??[]).some(address=>getAddress(address)===wallet)?"listed wallet":"other");
  }
  const roleOf=(address:string)=>roles.get(getAddress(address))??"other";
  const blockTimes=new Map<number,number>();
  const timestamp=async(block:number)=>{if(!blockTimes.has(block))blockTimes.set(block,(await provider.getBlock(block))!.timestamp);return blockTimes.get(block)!;};
  const senders=new Map<string,string>();
  const sender=async(hash:string)=>{if(!senders.has(hash))senders.set(hash,getAddress((await provider.getTransaction(hash))!.from));return senders.get(hash)!;};

  // Where each epoch and request was settled, so a later attempt on the same work can be recognized as a duplicate.
  const position=(log:Log)=>({block:log.blockNumber,index:log.transactionIndex,txHash:log.transactionHash});
  type Position=ReturnType<typeof position>;
  const before=(a:Position,b:Position)=>a.block<b.block||(a.block===b.block&&a.index<b.index);
  const publishedAt=new Map<string,Position>(),fulfilledAt=new Map<string,Position>();
  for(const {log,event} of parsedRegistry)if(event!.name==="EpochCommitted")publishedAt.set((event!.args.epochId as bigint).toString(),position(log));
  for(const {log,event} of parsedCoordinator)if(event!.name==="RandomnessFulfilled")fulfilledAt.set((event!.args.requestId as bigint).toString(),position(log));
  const epochs=await mapLimit(parsedRegistry.filter(entry=>entry.event!.name==="EpochCommitted"),async({log,event})=>{
    const epochId=event!.args.epochId as bigint,publisher=await sender(log.transactionHash);
    const [record,start,catalog]=await Promise.all([registry.getEpoch(epochId,{blockTag:toBlock}),registry.epochStart(epochId,{blockTag:toBlock}),
      registry.catalogAt(epochId,{blockTag:toBlock}).catch(()=>undefined)]);
    const source=Number(record.source);
    return {epochId:epochId.toString(),block:log.blockNumber,timestamp:await timestamp(log.blockNumber),txHash:log.transactionHash,publisher,publisherRole:roleOf(publisher),
      source,recipe:catalog?Number(catalog.recipes[source]):undefined,blocksIntoEpoch:log.blockNumber-Number(start)};
  });
  const requests=new Map<string,{requestId:string;consumer?:string;requestBlock?:number;requestTimestamp?:number;deadline?:number;
    fulfilled?:{block:number;timestamp:number;txHash:string;submitter:string;submitterRole:Role;latencySeconds?:number;keeperShareTo?:string};
    refunded?:{block:number;txHash:string;refundAddress:string;amount:string;paid:boolean};skipped:Array<{txHash:string;from:string;fromRole:Role;reason:string}>}>();
  const keeperShares=new Map<string,string>();
  const entry=(id:bigint)=>{const key=id.toString();if(!requests.has(key))requests.set(key,{requestId:key,skipped:[]});return requests.get(key)!;};
  for(const {log,event} of parsedCoordinator){
    const e=event!,r=entry(e.args.requestId as bigint);
    if(e.name==="RandomnessRequested"){r.consumer=getAddress(e.args.consumer);r.requestBlock=log.blockNumber;r.requestTimestamp=await timestamp(log.blockNumber);r.deadline=Number(e.args.deadline);}
    else if(e.name==="RandomnessFulfilled"){const from=await sender(log.transactionHash),at=await timestamp(log.blockNumber);
      r.fulfilled={block:log.blockNumber,timestamp:at,txHash:log.transactionHash,submitter:from,submitterRole:roleOf(from)};}
    else if(e.name==="KeeperFeePaid")keeperShares.set(r.requestId,getAddress(e.args.keeper));
    else if(e.name==="FulfillmentSkipped"){const from=await sender(log.transactionHash);r.skipped.push({txHash:log.transactionHash,from,fromRole:roleOf(from),reason:SKIP_REASONS[Number(e.args.reason)]??String(e.args.reason)});}
    else if(e.name==="RequestRefundedTo")r.refunded={block:log.blockNumber,txHash:log.transactionHash,refundAddress:getAddress(e.args.refundAddress),amount:(e.args.amount as bigint).toString(),paid:e.args.paid};
  }
  for(const r of requests.values()){
    if(!r.fulfilled)continue;
    if(r.requestTimestamp!==undefined)r.fulfilled.latencySeconds=r.fulfilled.timestamp-r.requestTimestamp;
    r.fulfilled.keeperShareTo=keeperShares.get(r.requestId);
  }

  // Keeper transactions without success evidence in logs: every block is scanned for the wallets' transactions.
  const blockNumbers=Array.from({length:toBlock-fromBlock+1},(_,i)=>fromBlock+i);
  const walletTxs=(await mapLimit(blockNumbers,async number=>{
    const block=await provider.getBlock(number,true);
    blockTimes.set(number,block!.timestamp);
    return block!.prefetchedTransactions.filter(tx=>wallets.has(getAddress(tx.from))).map(tx=>({tx,block:number}));
  })).flat();
  const outcomes=await mapLimit(walletTxs,async({tx,block})=>{
    const receipt=await provider.getTransactionReceipt(tx.hash);
    const from=getAddress(tx.from),to=tx.to?getAddress(tx.to):null;
    const parsed=to===registryAddress?REGISTRY.parseTransaction(tx):to===coordinatorAddress?COORDINATOR.parseTransaction(tx):null;
    const method=parsed?.name??(to===from&&tx.value===0n?"nonce cancellation":undefined);
    // Work this transaction acted on, and where it was settled first: an attempt whose whole work was already
    // settled elsewhere is a duplicate, whether the chain skipped its members or the transaction reverted.
    const at={block,index:receipt!.index,txHash:tx.hash};
    const work=parsed?.name==="fulfillRandomness"?[String(parsed.args[0])]
      :parsed?.name==="fulfillRandomnessBatch"?(parsed.args[0] as bigint[]).map(String)
      :parsed?.name==="commitEpoch"||parsed?.name==="commitEpochFallback"?[String(parsed.args[0])]:[];
    const settled=parsed?.name.startsWith("commitEpoch")?publishedAt:fulfilledAt;
    const duplicate=work.length>0&&work.every(id=>{const first=settled.get(id);return !!first&&first.txHash!==tx.hash&&before(first,at);});
    return {txHash:tx.hash,block,from,role:roleOf(from),to,method:method??"other",status:receipt!.status,gasUsed:receipt!.gasUsed.toString(),
      ...(work.length>0&&{work}),...(duplicate&&{duplicate:true as const})};
  });
  const reverted=outcomes.filter(o=>o.status===0),cancellations=outcomes.filter(o=>o.status===1&&o.method==="nonce cancellation");
  const duplicates=outcomes.filter(o=>o.duplicate);
  const count=(values:string[])=>values.reduce<Record<string,number>>((out,value)=>({...out,[value]:(out[value]??0)+1}),{});
  const requestList=[...requests.values()].sort((a,b)=>Number(BigInt(a.requestId)-BigInt(b.requestId)));
  return {fromBlock,toBlock,registry:registryAddress,coordinator:coordinatorAddress,
    wallets:[...roles].map(([address,role])=>({address,role})),
    summary:{epochsPublishedBy:count(epochs.map(e=>e.publisher)),requestsFulfilledBy:count(requestList.flatMap(r=>r.fulfilled?[r.fulfilled.submitter]:[])),
      requests:requestList.length,unserved:requestList.filter(r=>!r.fulfilled&&!r.refunded).map(r=>r.requestId),refunds:requestList.filter(r=>r.refunded).length,
      skippedDuplicates:requestList.reduce((sum,r)=>sum+r.skipped.length,0),revertedKeeperTransactions:reverted.length,nonceCancellations:cancellations.length,
      duplicateAttempts:duplicates.length,duplicateAttemptsBy:count(duplicates.map(o=>o.from)),
      transactionsBy:count(outcomes.map(o=>o.from)),gasUsedBy:outcomes.reduce<Record<string,string>>((out,o)=>({...out,[o.from]:String(BigInt(out[o.from]??"0")+BigInt(o.gasUsed))}),{})},
    epochs,requests:requestList,revertedKeeperTransactions:reverted,nonceCancellations:cancellations,duplicateAttempts:duplicates};
}
