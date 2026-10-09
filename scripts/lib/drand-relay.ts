// A drand HTTP relay's answers for one beacon preset. The relay is not trusted: its chain info must equal the preset and every
// signature it serves is verified under the preset's group key, so a wrong or hostile relay can only make a run stop.
import {beaconRoundTime,verifyBeaconRound,DRAND_EVMNET} from "../../src/beacon.ts";
import {Stop} from "./deployment.ts";

export interface BeaconPreset {name:string;chainHash:string;publicKey:string;genesis:bigint;period:bigint;scheme:string}
/// The beacon networks admin.ts registers. drand's evmnet is the only one D20BeaconVerifier checks.
export const BEACON_PRESETS:Readonly<Record<string,BeaconPreset>>=Object.freeze({evmnet:Object.freeze({name:"evmnet",...DRAND_EVMNET,scheme:"bls-bn254-unchained-on-g1"})});
export const DEFAULT_RELAY="https://api.drand.sh";
/// A sample round this many rounds behind the relay's latest: five minutes on evmnet, so it is scheduled before the chain's latest block.
export const SAMPLE_LAG=100n;
const TIMEOUT_MS=10_000,MAX_ANSWER_BYTES=16_384;
export interface RelayOptions {fetch?:typeof fetch;timeoutMs?:number}
export interface BeaconSample {round:bigint;signature:string}

/// The relay's base URL: https only, without credentials, query or fragment, and without a trailing slash.
export function relayBase(relay:string):string{
  let url:URL;
  try{url=new URL(relay);}catch{throw new Stop("--relay is not a URL");}
  if(url.protocol!=="https:"||url.username||url.password||url.search||url.hash)throw new Stop("--relay must be an https URL without credentials, query or fragment");
  return url.origin+url.pathname.replace(/\/+$/,"");
}
const reason=(error:unknown)=>{const cause=(error as {cause?:{code?:string;message?:string}})?.cause;return cause?.code??cause?.message??(error instanceof Error?error.message:String(error));};
const short=(value:unknown)=>{const text=JSON.stringify(value)??"nothing";return text.length>24?`${text.slice(0,21)}..."`:text;};
/// The body as UTF-8 text, refused once it passes MAX_ANSWER_BYTES: a chain's info and a round are far smaller.
async function readText(response:Response):Promise<string>{
  if(!response.body)return "";
  const reader=response.body.getReader(),chunks:Uint8Array[]=[];
  let size=0;
  for(;;){
    const {done,value}=await reader.read();
    if(done)return new TextDecoder("utf-8",{fatal:true}).decode(Buffer.concat(chunks));
    size+=value.length;
    if(size>MAX_ANSWER_BYTES){await reader.cancel();throw new Stop(`The relay's answer exceeds ${MAX_ANSWER_BYTES} bytes`);}
    chunks.push(value);
  }
}
/// GET one JSON object: bounded in time and size, and every failure a Stop that names the URL. A redirect is a failure: fetch would
/// follow one to any host, over any scheme it allows, and that would make relayBase's https-only rule apply to the first hop only.
async function getJson(url:string,{fetch:send=fetch,timeoutMs=TIMEOUT_MS}:RelayOptions):Promise<Record<string,unknown>>{
  let text:string;
  try{
    const response=await send(url,{signal:AbortSignal.timeout(timeoutMs),headers:{accept:"application/json"},redirect:"error"});
    if(!response.ok)throw new Stop(`The relay answered HTTP ${response.status} for ${url}`);
    text=await readText(response);
  }catch(error){
    if(error instanceof Stop)throw error;
    throw new Stop(`The relay request ${url} failed: ${(error as {name?:string})?.name==="TimeoutError"?`no answer within ${timeoutMs} ms`:reason(error)}`);
  }
  let json:unknown;
  try{json=JSON.parse(text);}catch{throw new Stop(`The relay's answer for ${url} is not valid JSON`);}
  if(typeof json!=="object"||json===null||Array.isArray(json))throw new Stop(`The relay's answer for ${url} is not a JSON object`);
  return json as Record<string,unknown>;
}
/// Throws unless the relay's chain info is exactly the preset's network: its hash, group key, schedule and scheme.
export function checkRelayInfo(preset:BeaconPreset,info:Record<string,unknown>):void{
  const expected:Record<string,string|number>={hash:preset.chainHash.slice(2),public_key:preset.publicKey.slice(2),genesis_time:Number(preset.genesis),period:Number(preset.period),schemeID:preset.scheme};
  for(const [key,want] of Object.entries(expected)){
    const got=info[key],same=typeof want==="string"?typeof got==="string"&&got.toLowerCase()===want:got===want;
    if(!same)throw new Stop(`The relay's ${key} is ${short(got)}, not the ${preset.name} preset's ${short(want)}; this is another network or another relay`);
  }
}
const readRound=(value:unknown,what:string)=>{
  if(typeof value!=="number"||!Number.isSafeInteger(value)||value<1)throw new Stop(`The relay's ${what} is not a round number`);
  return BigInt(value);
};
/// A round's signature from the relay, verified under the preset's group key. It reads the chain info first and stops unless it
/// is the preset's network. Without `round` it samples SAMPLE_LAG rounds behind the relay's latest.
export async function fetchBeaconSample(relay:string,preset:BeaconPreset,options:RelayOptions&{round?:bigint}={}):Promise<BeaconSample>{
  const base=`${relayBase(relay)}/${preset.chainHash.slice(2)}`;
  checkRelayInfo(preset,await getJson(`${base}/info`,options));
  let round=options.round;
  if(round===undefined){
    const latest=readRound((await getJson(`${base}/public/latest`,options)).round,"latest round");
    if(latest<=SAMPLE_LAG)throw new Stop(`The relay's latest round ${latest} is too early to sample ${SAMPLE_LAG} rounds behind it`);
    round=latest-SAMPLE_LAG;
  }
  const answer=await getJson(`${base}/public/${round}`,options),answered=readRound(answer.round,`answer for round ${round}`);
  if(answered!==round)throw new Stop(`The relay answered round ${answered} when asked for round ${round}`);
  const signature=answer.signature;
  if(typeof signature!=="string"||!/^(0x)?[0-9a-fA-F]{128}$/.test(signature))throw new Stop(`The relay's signature of round ${round} is not 64 bytes of hex`);
  const hex=`0x${signature.replace(/^0x/,"").toLowerCase()}`;
  if(!verifyBeaconRound(preset.publicKey,round,hex))throw new Stop(`The relay's signature of round ${round} does not verify under the ${preset.name} group key`);
  return {round,signature:hex};
}
/// The sample's scheduled time, after checking what registerBeacon does: the signature verifies under the group key and the
/// round is not scheduled after the chain's latest block time.
export function checkBeaconSample(preset:BeaconPreset,sample:BeaconSample,chainTime:number|bigint):bigint{
  if(!verifyBeaconRound(preset.publicKey,sample.round,sample.signature))throw new Stop(`The sample signature of round ${sample.round} does not verify under the ${preset.name} group key`);
  const time=beaconRoundTime(preset,sample.round);
  if(time>BigInt(chainTime))throw new Stop(`Round ${sample.round} is scheduled at ${time}, after the latest block time ${chainTime}: registerBeacon refuses it; use an earlier sample round`);
  return time;
}
