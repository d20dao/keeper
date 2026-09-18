// Catalog preparation only. This queue is never used on the per-game request path.
/// body is a recipe's registered gateway body, posted byte for byte.
type Profile={endpoint:string;body:string};
type Options={concurrency?:number;intervalMs?:number;timeoutMs?:number;maxWaitMs?:number;fetcher?:typeof fetch};
type Success={ok:true;envelope:any;httpStatus:number;attempts:number;queuedAt:string;startedAt:string;receivedAt:string};
type Failure={ok:false;error:string};
const sleep=(ms:number)=>new Promise(r=>setTimeout(r,ms));
export async function collectEpochHttp<T extends Profile>(profiles:readonly T[],options:Options={}):Promise<Array<{profile:T;result:Success|Failure}>>{
  const concurrency=options.concurrency??2,interval=options.intervalMs??500,timeout=options.timeoutMs??8000,maxWait=options.maxWaitMs??5000;
  if(!Number.isInteger(concurrency)||concurrency<1||concurrency>2||![interval,timeout,maxWait].every(Number.isFinite)||interval<0||timeout<=0||maxWait<0)
    throw new Error("Invalid bounded epoch HTTP queue settings");
  const fetcher=options.fetcher??fetch,queuedAt=new Date().toISOString();
  let cursor=0,nextStart=0,gate=Promise.resolve();
  const results:Array<{profile:T;result:Success|Failure}>=new Array(profiles.length);
  function permit(){
    const turn=gate.catch(()=>{}).then(async()=>{
      for(;;){const wait=nextStart-Date.now();if(wait<=0)break;
        if(wait>maxWait)throw new Error("Retry-After exceeds collection wait budget; retry catalog collection later");await sleep(wait);}
      nextStart=Date.now()+interval;
    });gate=turn;return turn;
  }
  async function worker(){
    while(cursor<profiles.length){const index=cursor++,profile=profiles[index];
      try{
        const body=profile.body;let startedAt="";let done=false;
        for(let attempt=1;attempt<=2;attempt++){
          await permit();if(!startedAt)startedAt=new Date().toISOString();
          const response=await fetcher(profile.endpoint,{method:"POST",headers:{"Content-Type":"application/json"},body,
            redirect:"error",signal:AbortSignal.timeout(timeout)});
          if(response.status===429){
            const header=response.headers.get("retry-after");
            const retryMs=header===null?1000:/^\d+$/.test(header)?Number(header)*1000:Math.max(0,Date.parse(header)-Date.now());
            const wait=Number.isFinite(retryMs)?retryMs:1000;
            nextStart=Math.max(nextStart,Date.now()+wait);await response.body?.cancel();
            if(attempt===2)throw new Error("HTTP 429 after two same-query attempts");
            continue;
          }
          if(!response.ok){await response.body?.cancel();throw new Error(`HTTP ${response.status}; no paid retry or source fallback`);}
          const reader=response.body!.getReader();const chunks:Uint8Array[]=[];let size=0;
          for(;;){const r=await reader.read();if(r.done)break;size+=r.value.length;
            if(size>16384){await reader.cancel();throw new Error("Envelope exceeds 16 KiB");}chunks.push(r.value);}
          const bytes=new Uint8Array(size);let offset=0;for(const chunk of chunks){bytes.set(chunk,offset);offset+=chunk.length;}
          const envelope=JSON.parse(new TextDecoder("utf-8",{fatal:true}).decode(bytes));
          results[index]={profile,result:{ok:true,envelope,httpStatus:response.status,attempts:attempt,queuedAt,startedAt,receivedAt:new Date().toISOString()}};
          done=true;break;
        }
        if(!done)throw new Error("Snapshot collection attempts exhausted");
      }catch(e){results[index]={profile,result:{ok:false,error:e instanceof Error?e.message:String(e)}};}
    }
  }
  await Promise.all(Array.from({length:Math.min(concurrency,profiles.length)},()=>worker()));return results;
}
