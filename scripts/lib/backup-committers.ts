// The backup committers a registry has, found from its own BackupCommitterSet events instead of from a list somebody remembers: the
// fork rehearsal must compare every storage word that a live backup committer occupies, and a missed one would go unchecked.
import {getAddress,id,type Log} from "ethers";
import {Stop} from "./deployment.ts";

/// The public RPCs answer eth_getLogs for at most 10,000 blocks at a time; a longer range is refused.
export const LOG_PAGE_BLOCKS=10_000;
const TOPIC=id("BackupCommitterSet(address,bool)");
const sleep=(ms:number)=>new Promise(resolve=>setTimeout(resolve,ms));

export interface LogSource {getLogs(filter:{address:string;topics:string[];fromBlock:number;toBlock:number}):Promise<Log[]>}
export interface BackupCommitterHistory {
  /// Every account the events name, in the state the last event left it: true for a backup committer, false for one removed since.
  state:Map<string,boolean>;
  /// The accounts that are backup committers at the last block read, in the order they were first named.
  active:string[];
  pages:number;events:number;
}
/// Read every BackupCommitterSet event of `registry` from `fromBlock`, its deployment block, to `toBlock`, a page of at most
/// LOG_PAGE_BLOCKS blocks at a time and one page after another, `pauseMs` apart, so that a rate-limited public node is not asked for
/// more than one request at once, nor faster than it allows. A page that fails is asked again after a pause, doubling, up to `attempts`
/// times; a persistent failure is a Stop, never a shorter history. `before` is the history of the blocks before `fromBlock`, which this
/// one continues: the range is read in two parts when the chain has moved on since the first.
export async function readBackupCommitters(source:LogSource,registry:string,fromBlock:number,toBlock:number,
  options:{pageBlocks?:number;attempts?:number;retryMs?:number;pauseMs?:number;before?:BackupCommitterHistory;progress?:(page:number,pages:number)=>void}={}):Promise<BackupCommitterHistory>{
  const {pageBlocks=LOG_PAGE_BLOCKS,attempts=5,retryMs=1000,pauseMs=0,before}=options;
  if(!Number.isSafeInteger(fromBlock)||!Number.isSafeInteger(toBlock)||fromBlock<0||toBlock<fromBlock)throw new Stop("Invalid block range for the BackupCommitterSet events");
  const address=getAddress(registry),state=new Map<string,boolean>(before?.state),pages=Math.ceil((toBlock-fromBlock+1)/pageBlocks);
  let events=before?.events??0;
  for(let page=0;page<pages;page++){
    if(page>0&&pauseMs>0)await sleep(pauseMs);
    const start=fromBlock+page*pageBlocks,end=Math.min(toBlock,start+pageBlocks-1);
    let logs:Log[]|undefined;
    for(let attempt=1;logs===undefined;attempt++){
      try{logs=await source.getLogs({address,topics:[TOPIC],fromBlock:start,toBlock:end});}
      catch(error){
        if(attempt>=attempts)throw new Stop(`Reading the BackupCommitterSet events of blocks ${start} to ${end} failed after ${attempts} attempts: ${String((error as Error)?.message??error).split("\n")[0]}`);
        await sleep(retryMs*2**(attempt-1));
      }
    }
    // Events of a block are applied in their log order, so the state after the last one is the state the registry holds.
    for(const log of [...logs].sort((a,b)=>a.blockNumber-b.blockNumber||a.index-b.index)){
      if(log.topics[0]!==TOPIC||log.topics.length!==2||log.blockNumber<start||log.blockNumber>end)throw new Stop("A BackupCommitterSet log is not what was asked for");
      state.set(getAddress(`0x${log.topics[1].slice(26)}`),BigInt(log.data)!==0n);
      events++;
    }
    options.progress?.(page+1,pages);
  }
  return {state,active:[...state].filter(([,allowed])=>allowed).map(([account])=>account),pages:(before?.pages??0)+pages,events};
}
