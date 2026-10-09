// What the owner tooling needs to register a beacon recipe: its options, the calls it sends and asks, the rule against
// registering one twice, and how it tells a registry that lacks a function from an RPC that failed.
import {getAddress,hexlify} from "ethers";
import {beaconCanonicalRequest,type BeaconRegistration} from "../../src/beacon.ts";
import {Stop} from "./deployment.ts";
import {BEACON_PRESETS,DEFAULT_RELAY,relayBase,type BeaconPreset,type BeaconSample} from "./drand-relay.ts";

/// EpochEntropy's beacon registration and views, and the two views of D20BeaconVerifier that are asked before a registration is proposed.
export const BEACON_ABI=[
  "function registerBeacon(address,bytes32,bytes,uint64,uint64,uint64,bytes) returns(uint8)",
  "function beaconOf(uint8) view returns(tuple(address verifier,uint64 genesis,uint64 period,bytes32 chainHash,bytes publicKey))",
  "function slotSigner(uint8) view returns(address)",
  "function isValidPublicKey(bytes) view returns(bool)","function verifyRound(bytes,uint64,bytes) view returns(bool)",
];
/// registerBeacon's arguments for a preset under a verifier, vouched for by a sample round.
export const registerBeaconArgs=(verifier:string,preset:BeaconPreset,sample:BeaconSample)=>
  [verifier,preset.chainHash,preset.publicKey,preset.genesis,preset.period,sample.round,sample.signature];

export interface RegisterBeaconOptions {preset:BeaconPreset;verifier:string;relay:string;
  /// Set when --sample-round and --sample-signature give the sample whole; otherwise it is fetched from the relay.
  sample?:BeaconSample;
  /// The round to fetch from the relay; without it, one about 100 behind the relay's latest.
  round?:bigint}
/// register-beacon's options, checked before any network access. The verifier defaults to the manifest's beaconVerifier.
export function parseRegisterBeaconOptions(values:{beacon?:string;verifier?:string;"sample-round"?:string;"sample-signature"?:string;relay?:string},manifest:{beaconVerifier?:unknown}):RegisterBeaconOptions{
  const name=values.beacon??"evmnet",preset=Object.hasOwn(BEACON_PRESETS,name)?BEACON_PRESETS[name]:undefined;
  if(!preset)throw new Stop(`Unknown --beacon ${JSON.stringify(name)}; the only preset is evmnet`);
  const text=values.verifier??manifest.beaconVerifier;
  if(text===undefined)throw new Stop("Provide --verifier <address>, or record beaconVerifier in the manifest");
  if(typeof text!=="string"||!/^0x[0-9a-fA-F]{40}$/.test(text))throw new Stop(`Invalid ${values.verifier===undefined?"beaconVerifier in the manifest":"--verifier"}`);
  const {"sample-round":roundText,"sample-signature":signatureText}=values;
  if(roundText!==undefined&&!/^[1-9]\d{0,18}$/.test(roundText))throw new Stop("--sample-round must be a round number of at most 19 digits");
  if(signatureText!==undefined&&!/^(0x)?[0-9a-fA-F]{128}$/.test(signatureText))throw new Stop("--sample-signature must be the round's 64-byte signature in hex");
  if(signatureText!==undefined&&roundText===undefined)throw new Stop("--sample-signature needs --sample-round");
  if(signatureText!==undefined&&values.relay!==undefined)throw new Stop("Give the sample either as --sample-round with --sample-signature or through --relay, not both");
  const relay=values.relay??DEFAULT_RELAY,round=roundText===undefined?undefined:BigInt(roundText);
  relayBase(relay);
  return {preset,verifier:getAddress(text.toLowerCase()),relay,round,sample:signatureText===undefined?undefined:{round:round!,signature:`0x${signatureText.replace(/^0x/,"").toLowerCase()}`}};
}
/// The id of a registered recipe that is exactly this beacon registration, if there is one. The same chain under another verifier,
/// key or schedule is a different registration, and a signed recipe that merely names the chain is not a beacon.
export function findDuplicateBeacon(candidate:BeaconRegistration,registered:ReadonlyArray<{id:number;canonicalRequest:string;beacon?:BeaconRegistration}>):number|undefined{
  const request=beaconCanonicalRequest(candidate.chainHash),key=hexlify(candidate.publicKey).toLowerCase();
  return registered.find(({canonicalRequest,beacon})=>canonicalRequest===request&&beacon!==undefined&&getAddress(beacon.verifier)===getAddress(candidate.verifier)&&
    hexlify(beacon.publicKey).toLowerCase()===key&&beacon.genesis===candidate.genesis&&beacon.period===candidate.period)?.id;
}

/// Whether an eth_call failed because the contract has no such function: the EVM reverted with no data at all, as an unknown selector
/// does in a contract without a fallback. A node reports that as revert data "0x" or, as Arc's public nodes do, as JSON-RPC error 3
/// "execution reverted" with no data field; a revert that carries data, such as a custom error, always has one. Anything else says
/// nothing about the contract: a rate limit, a timeout, a dropped connection, an HTTP error or a garbled answer.
export function isEmptyRevert(error:unknown):boolean{
  const failure=error as {code?:unknown;data?:unknown;info?:{error?:{code?:unknown;message?:unknown;data?:unknown}}}|null;
  if(failure?.code!=="CALL_EXCEPTION")return false;
  if(failure.data==="0x")return true;
  const rpc=failure.info?.error;
  return failure.data==null&&rpc?.code===3&&typeof rpc.message==="string"&&/^execution reverted$/i.test(rpc.message)&&rpc.data==null;
}
const failureText=(error:unknown)=>{
  const failure=error as {code?:unknown;shortMessage?:unknown;message?:unknown;info?:{error?:{message?:unknown}}}|null;
  const text=[failure?.info?.error?.message,failure?.shortMessage,failure?.message].find(part=>typeof part==="string"&&part!=="");
  const firstLine=String(text??error).split(/\r?\n/)[0];
  return `${typeof failure?.code==="string"?`${failure.code}: `:""}${firstLine}`;
};
/// The answer of a view the registry may not have, or undefined when it has no such function: the registry runs the implementation from
/// before the upgrade that adds it. Any other failure is a Stop, never undefined: taking an RPC that failed for a registry that lacks
/// the function would skip the duplicate check and the simulation without a word.
export async function answerOrMissing<T>(call:()=>Promise<T>,what:string):Promise<T|undefined>{
  try{return await call();}
  catch(error){
    if(isEmptyRevert(error))return undefined;
    throw new Stop(`Could not tell whether the registry has ${what}: ${failureText(error)}. The RPC failed, so nothing was assumed; run the command again.`);
  }
}
