// The owner's calls on a round coordinator: each action's arguments, checked before any network access; its plan, checked against the chain
// and simulated from the owner; the Safe Transaction Builder file a contract owner signs instead; and the sending of a plan by an owner key.
// scripts/admin-robinhood.ts is its command line; the manifest it reads is the one scripts/lib/robinhood-deploy.ts writes.
//
// On a production chain the owner is the Safe its profile names, checked strictly (scripts/lib/safe.ts) before every plan, or, until the
// Safe takes the set over, the profile's interim owner, an account without code that sends with its key. The ownership transfer is three
// steps: transfer-ownership <Safe> from the interim owner's key, then accept-ownership and set-fee-recipient <Safe> as Safe transactions;
// status shows each step and the next one.
//
// A Safe Transaction Builder file (the Safe app's "Transaction Builder", Load) is
//   {"version":"1.0","chainId":"<id>","createdAt":<ms>,"meta":{"name","description","txBuilderVersion","createdFromSafeAddress","createdFromOwnerAddress"},
//    "transactions":[{"to","value":"0","data":"0x…","contractMethod":null,"contractInputsValues":null}]}
// with the call as raw calldata, which the Safe app decodes against the coordinator's verified ABI.
import {open} from "node:fs/promises";
import {join} from "node:path";
import {Contract,Interface,getAddress,getBytes,hexlify,keccak256,type Block,type JsonRpcApiProvider,type Wallet} from "ethers";
import type {OperableChain} from "./chains.ts";
import {currentFee,tipBounds} from "./gas.ts";
import {Stop} from "./deployment.ts";
import {IMPLEMENTATION_SLOT,LIMITS,beaconIdentity,checkNetwork,compiled,coordinatorInterface,failureReason,libraryRuntime,link,loadBeaconFile,
  sampleTime,storageLayoutHash,withImmutables,type BeaconFile} from "./robinhood-deploy.ts";
import {accountKind,checkProfileSafe,requireProfileSafe} from "./safe.ts";

export const ADMIN_ACTIONS=["status","set-keeper","set-backup-keeper","set-pricing","set-fee-recipient","set-keeper-bps","set-refund-bps","register-beacon",
  "schedule-beacon","cancel-beacon-schedule","transfer-ownership","accept-ownership","upgrade"] as const;
export type AdminAction=typeof ADMIN_ACTIONS[number];
export type AdminRequest=
  |{action:"status";address?:string}
  |{action:"set-keeper"|"set-fee-recipient"|"transfer-ownership";address:string}
  |{action:"set-backup-keeper";address:string;allowed:boolean}
  |{action:"set-pricing";minFee:bigint;multiplier:number;overhead:number}
  |{action:"set-keeper-bps"|"set-refund-bps";bps:number}
  |{action:"register-beacon";file:string;beacon:BeaconFile}
  |{action:"schedule-beacon";beaconId:number;fromTime:bigint}
  |{action:"cancel-beacon-schedule"|"accept-ownership"}
  |{action:"upgrade";implementation:string;approvedCodeHash:string;approvedLayoutHash?:string};
/** Each action's positional arguments, for the usage line and the refusals. */
export const ACTION_ARGUMENTS:Record<AdminAction,string>={"status":"","set-keeper":"<address>","set-backup-keeper":"<address> <true|false>",
  "set-pricing":"<minFeeWei> <multiplier> <fulfillGasOverhead>","set-fee-recipient":"<address>","set-keeper-bps":"<bps>","set-refund-bps":"<bps>",
  "register-beacon":"<beacon file>","schedule-beacon":"<beaconId> <fromTime>","cancel-beacon-schedule":"","transfer-ownership":"<address>",
  "accept-ownership":"","upgrade":"<implementation> --approve-code-hash <hash> [--approve-layout-hash <hash>]"};
const ARITY:Record<AdminAction,number>={"status":0,"set-keeper":1,"set-backup-keeper":2,"set-pricing":3,"set-fee-recipient":1,"set-keeper-bps":1,"set-refund-bps":1,
  "register-beacon":1,"schedule-beacon":2,"cancel-beacon-schedule":0,"transfer-ownership":1,"accept-ownership":0,"upgrade":1};
/** Gas caps for sending: a beacon registration stores the key and verifies the sample round (about 620,000 gas); the rest are small. */
const GAS_CAP:Partial<Record<AdminAction,bigint>>={"register-beacon":1_200_000n};
const DEFAULT_GAS_CAP=300_000n;
/** A schedule sent from an owner key must leave this much beyond MIN_SCHEDULE_LEAD for the transaction to be mined. */
export const SCHEDULE_SEND_MARGIN=60n;
const ZERO="0x0000000000000000000000000000000000000000";

const address=(text:string|undefined,what:string)=>{
  if(text===undefined||!/^0x[0-9a-fA-F]{40}$/.test(text))throw new Stop(`${what} must be an address`);
  const value=getAddress(text.toLowerCase());
  if(value===ZERO)throw new Stop(`${what} must not be the zero address`);
  return value;
};
const whole=(text:string|undefined,what:string,min:bigint,max:bigint)=>{
  if(text===undefined||!/^\d{1,30}$/.test(text)||BigInt(text)<min||BigInt(text)>max)throw new Stop(`${what} must be a whole number from ${min} to ${max}`);
  return BigInt(text);
};
/** An action and its arguments, checked before any network access: arity, address shapes, ranges, the beacon file and the approvals. */
export async function parseAdminRequest(action:string|undefined,args:string[],options:{"approve-code-hash"?:string;"approve-layout-hash"?:string;address?:string}={}):Promise<AdminRequest>{
  if(!ADMIN_ACTIONS.includes(action as AdminAction))throw new Stop(`Unknown action ${JSON.stringify(action??"")}; the actions are ${ADMIN_ACTIONS.join(", ")}`);
  const a=action as AdminAction,arity=ARITY[a];
  if(args.length!==arity)throw new Stop(`${a} takes ${arity===0?"no arguments":ACTION_ARGUMENTS[a].replace(/ --.*$/,"")}`);
  if((options["approve-code-hash"]!==undefined||options["approve-layout-hash"]!==undefined)&&a!=="upgrade")throw new Stop("--approve-code-hash and --approve-layout-hash apply to upgrade only");
  if(options.address!==undefined&&a!=="status")throw new Stop("--address applies to status only");
  switch(a){
    case "status":return {action:a,address:options.address===undefined?undefined:address(options.address,"--address")};
    case "set-keeper":case "set-fee-recipient":case "transfer-ownership":return {action:a,address:address(args[0],"The address")};
    case "set-backup-keeper":{
      if(args[1]!=="true"&&args[1]!=="false")throw new Stop("set-backup-keeper takes true to allow the wallet or false to remove it");
      return {action:a,address:address(args[0],"The address"),allowed:args[1]==="true"};
    }
    case "set-pricing":{
      const minFee=whole(args[0],"minFeeWei",0n,LIMITS.maxMinFee),multiplier=Number(whole(args[1],"The multiplier",0n,BigInt(LIMITS.maxMultiplier)));
      const overhead=Number(whole(args[2],"The fulfillment gas overhead",BigInt(LIMITS.minOverhead),BigInt(LIMITS.maxOverhead)));
      if(minFee===0n&&multiplier===0)throw new Stop("A zero minimum fee needs a multiplier: requests are never free");
      return {action:a,minFee,multiplier,overhead};
    }
    case "set-keeper-bps":return {action:a,bps:Number(whole(args[0],"The keeper share",0n,BigInt(LIMITS.bps)))};
    case "set-refund-bps":return {action:a,bps:Number(whole(args[0],"The refund share",BigInt(LIMITS.minRefundBps),BigInt(LIMITS.bps)))};
    case "register-beacon":return {action:a,file:args[0],beacon:await loadBeaconFile(args[0])};
    case "schedule-beacon":return {action:a,beaconId:Number(whole(args[0],"The beacon id",0n,LIMITS.maxBeacons-1n)),fromTime:whole(args[1],"fromTime",1n,(1n<<40n)-1n)};
    case "cancel-beacon-schedule":case "accept-ownership":return {action:a};
    case "upgrade":{
      const hash=(text:string|undefined,flag:string)=>{if(text===undefined||!/^0x[0-9a-fA-F]{64}$/.test(text))throw new Stop(`${flag} must be a 32-byte hash`);return text.toLowerCase();};
      if(options["approve-code-hash"]===undefined)throw new Stop("upgrade needs --approve-code-hash with the reviewed runtime code hash of the new implementation");
      return {action:a,implementation:address(args[0],"The implementation"),approvedCodeHash:hash(options["approve-code-hash"],"--approve-code-hash"),
        approvedLayoutHash:options["approve-layout-hash"]===undefined?undefined:hash(options["approve-layout-hash"],"--approve-layout-hash")};
    }
  }
}

/** The manifest fields the tool relies on, checked: the coordinator's contract, its proxy and implementation, and their pins. Ownership is
 * read from the chain, never from the manifest. */
export type RoundManifest={network:string;chainId:number;coordinatorContract:"D20VRFCoordinatorRobinhood";coordinator:string;coordinatorCodeHash:string;
  coordinatorImplementation:string;coordinatorImplementationCodeHash:string;storageLayoutHash:string};
export function parseRoundManifest(json:unknown,path:string):RoundManifest{
  const m=json as Record<string,unknown>|null;
  if(m===null||typeof m!=="object"||Array.isArray(m))throw new Stop(`${path} is not a manifest`);
  if(m.coordinatorContract!=="D20VRFCoordinatorRobinhood")throw new Stop(`${path} is not a round coordinator's manifest (coordinatorContract is not D20VRFCoordinatorRobinhood)`);
  if(typeof m.network!=="string"||!Number.isSafeInteger(m.chainId))throw new Stop(`${path} does not name its network and chain id`);
  for(const field of ["coordinator","coordinatorImplementation"])if(typeof m[field]!=="string"||!/^0x[0-9a-fA-F]{40}$/.test(m[field] as string))throw new Stop(`${path}: ${field} must be an address`);
  for(const field of ["coordinatorCodeHash","coordinatorImplementationCodeHash","storageLayoutHash"])
    if(typeof m[field]!=="string"||!/^0x[0-9a-f]{64}$/.test(m[field] as string))throw new Stop(`${path}: ${field} must be a lowercase 32-byte hash`);
  return {network:m.network,chainId:m.chainId as number,coordinatorContract:"D20VRFCoordinatorRobinhood",coordinator:getAddress(m.coordinator as string),
    coordinatorCodeHash:m.coordinatorCodeHash as string,coordinatorImplementation:getAddress(m.coordinatorImplementation as string),
    coordinatorImplementationCodeHash:m.coordinatorImplementationCodeHash as string,storageLayoutHash:m.storageLayoutHash as string};
}

export type SafeBatch={version:"1.0";chainId:string;createdAt:number;meta:{name:string;description:string;txBuilderVersion:string;createdFromSafeAddress:string;createdFromOwnerAddress:string};
  transactions:Array<{to:string;value:"0";data:string;contractMethod:null;contractInputsValues:null}>};
/** A Safe Transaction Builder file with one call, for the Safe that owns the coordinator to load, check, sign and execute. */
export function safeBatch(chainId:number,safe:string,name:string,description:string,call:{to:string;data:string}):SafeBatch{
  return {version:"1.0",chainId:String(chainId),createdAt:Date.now(),meta:{name,description,txBuilderVersion:"1.18.0",createdFromSafeAddress:safe,createdFromOwnerAddress:""},
    transactions:[{to:call.to,value:"0",data:call.data,contractMethod:null,contractInputsValues:null}]};
}

export type AdminPlan={action:AdminAction;network:string;chainId:number;coordinator:string;headBlock:number;headTime:number;
  sender?:{role:"owner"|"pending owner";address:string;kind:"EOA"|"Safe"|"contract"};
  call?:{from:string;to:string;value:"0";function:string;args:string[];data:string};
  checks?:string[];details?:Record<string,unknown>;note?:string;status?:Record<string,unknown>;
  /** How the call is executed: sent from the owner key with --send, or proposed in the owner's Safe with safeTransaction. */
  execution?:"owner key"|"Safe";safeTransaction?:SafeBatch;
  /** On a production chain, the sender is its interim owner, whose key sends until the Safe takes the set over. */
  interimOwner?:boolean};

const text=(value:unknown):string=>typeof value==="bigint"?value.toString():Array.isArray(value)?`[${value.map(text).join(",")}]`:String(value);
const utc=(time:bigint)=>new Date(Number(time)*1000).toISOString();
const fee=(pricing:{minFee:bigint;multiplier:bigint;overhead:bigint},baseFee:bigint,callbackGas:bigint)=>{
  const dynamic=pricing.multiplier*baseFee*(pricing.overhead+callbackGas);
  return dynamic>pricing.minFee?dynamic:pricing.minFee;
};

/** The plan of an action: every check against the chain, the exact call, its simulation from the owner (or the pending owner, for
 * accept-ownership), and for an owner that is a Safe the Safe file in place of sending. status plans no call and reports every view.
 * send asks for a plan that the owner key will send now; it is refused for an owner that is a contract, and on a production chain for any
 * owner but its interim owner. */
export async function planAdminAction(o:{provider:JsonRpcApiProvider;chain:OperableChain;manifest:RoundManifest;request:AdminRequest;send:boolean}):Promise<AdminPlan>{
  const {provider,chain,manifest,request,send}=o;
  if(manifest.chainId!==chain.chainId||manifest.network!==chain.key)throw new Stop("The manifest and the chain profile differ");
  if(send&&!chain.testnet&&!chain.interimOwner)throw new Stop(`${chain.key} is a production chain, owned by a Safe: run without --send and propose the printed Safe transaction`);
  const head=await checkNetwork(provider,chain,{factory:false});
  const face=await coordinatorInterface(),proxy=manifest.coordinator,c=new Contract(proxy,face,provider);
  const plan:AdminPlan={action:request.action,network:chain.key,chainId:chain.chainId,coordinator:proxy,headBlock:head.number,headTime:head.timestamp};
  // What runs at the proxy must be what the manifest records, or a change was made that the manifest does not know about.
  const proxyCodeHash=keccak256(await provider.getCode(proxy));
  const implementation=getAddress("0x"+(await provider.getStorage(proxy,IMPLEMENTATION_SLOT)).slice(-40));
  const implementationCodeHash=keccak256(await provider.getCode(implementation));
  const pins={proxyCodeHash:proxyCodeHash===manifest.coordinatorCodeHash,implementation:implementation===manifest.coordinatorImplementation,
    implementationCodeHash:implementationCodeHash===manifest.coordinatorImplementationCodeHash};
  if(request.action==="status"){plan.status=await status(provider,c,chain,head,manifest,{proxyCodeHash,implementation,implementationCodeHash,pins},request.address);return plan;}
  if(!pins.proxyCodeHash)throw new Stop(`The proxy's code hash ${proxyCodeHash} differs from the manifest's ${manifest.coordinatorCodeHash}`);
  if(!pins.implementation||!pins.implementationCodeHash)
    throw new Stop(`The proxy runs ${implementation} (code hash ${implementationCodeHash}), not the manifest's ${manifest.coordinatorImplementation}; update the manifest after a reviewed upgrade`);

  const owner=getAddress(await c.owner()),pendingOwner=getAddress(await c.pendingOwner());
  const role=request.action==="accept-ownership"?"pending owner":"owner",senderAddress=role==="owner"?owner:pendingOwner;
  if(senderAddress===ZERO)throw new Stop("No ownership transfer is pending");
  const kind=await accountKind(provider,senderAddress);
  plan.sender={role,address:senderAddress,kind};
  // A production chain's owner is its Safe, strictly checked, or until the Safe takes the set over its interim owner, an account without code.
  const interim=!chain.testnet&&!!chain.interimOwner&&senderAddress===chain.interimOwner;
  if(interim){
    if(kind!=="EOA")throw new Stop(`The ${role} ${senderAddress} is ${chain.key}'s interim owner but has code: an interim owner is an account without code`);
    plan.interimOwner=true;
  }else if(!chain.testnet)await requireProfileSafe(provider,chain,senderAddress,`The ${role} of the coordinator on ${chain.key}`);
  if(kind==="contract")throw new Stop(`The ${role} ${senderAddress} is a contract that is not a Safe (scripts/lib/safe.ts): neither its key nor a Safe transaction can act for it`);
  if(send&&kind!=="EOA")throw new Stop(`The ${role} ${senderAddress} is a contract: run without --send and propose the printed Safe transaction`);
  if(send&&!chain.testnet&&!interim)throw new Stop(`${chain.key} is a production chain: only its interim owner sends with --send, and its Safe proposes the printed Safe transaction`);
  plan.execution=kind==="EOA"?"owner key":"Safe";

  const checks:string[]=[];
  let fn:string,args:unknown[],details:Record<string,unknown>={},note:string|undefined;
  switch(request.action){
    case "set-keeper":{
      const current=getAddress(await c.keeper());
      if(request.address===current)throw new Stop(`${request.address} is already the keeper`);
      if(await c.isBackupKeeper(request.address))throw new Stop(`${request.address} is a backup keeper; remove it with set-backup-keeper false first`);
      checks.push("not the keeper already","not a backup keeper");
      fn="setKeeper";args=[request.address];details={previousKeeper:current,keeper:request.address};
      note="The keeper share of every request served by a wallet that is not an allowed backup keeper goes to the new keeper from the next fulfillment on. Drain and stop the previous keeper first, move its journal to the new wallet, then start it; keep the VRF key unchanged.";
      break;
    }
    case "set-backup-keeper":{
      const [keeper,current,count]=[getAddress(await c.keeper()),await c.isBackupKeeper(request.address) as boolean,await c.backupKeeperCount() as bigint];
      if(request.allowed&&request.address===keeper)throw new Stop("That wallet is the keeper; a backup keeper needs its own wallet");
      if(current===request.allowed)throw new Stop(`That wallet is ${request.allowed?"already":"not"} a backup keeper`);
      if(request.allowed&&count>=LIMITS.maxBackupKeepers)throw new Stop(`The coordinator already allows the maximum of ${LIMITS.maxBackupKeepers} backup keepers`);
      checks.push("not the keeper","state changes",`at most ${LIMITS.maxBackupKeepers} backup keepers`);
      fn="setBackupKeeper";args=[request.address,request.allowed];
      details={account:request.address,allowed:request.allowed,keeper,backupKeepers:{before:count,after:request.allowed?count+1n:count-1n,maximum:LIMITS.maxBackupKeepers}};
      note=request.allowed?"The wallet earns the keeper share of the requests it serves. Fund it for gas before its keeper starts sending."
        :"The keeper using this wallet stops earning the keeper share; the requests it serves pay the share to the keeper wallet instead.";
      break;
    }
    case "set-pricing":{
      const [minFee,multiplier,overhead]=await c.pricing() as [bigint,bigint,bigint],next={minFee:request.minFee,multiplier:BigInt(request.multiplier),overhead:BigInt(request.overhead)};
      if(minFee===next.minFee&&multiplier===next.multiplier&&overhead===next.overhead)throw new Stop("The coordinator already uses that pricing");
      checks.push(`minFeeWei at most ${LIMITS.maxMinFee}`,`multiplier at most ${LIMITS.maxMultiplier}`,`overhead ${LIMITS.minOverhead} to ${LIMITS.maxOverhead}`,"a zero minimum fee only with a multiplier");
      const baseFee=head.baseFeePerGas??0n,quotes=(p:typeof next)=>Object.fromEntries([50_000n,100_000n,1_000_000n].map(gas=>[`callback${gas}`,fee(p,baseFee,gas)]));
      fn="setPricing";args=[request.minFee,request.multiplier,request.overhead];
      details={current:{minFeeWei:minFee,feeMultiplier:multiplier,fulfillGasOverhead:overhead},next:{minFeeWei:request.minFee,feeMultiplier:request.multiplier,fulfillGasOverhead:request.overhead},
        feeWeiAtLatestBaseFee:{baseFeeWei:baseFee,current:quotes({minFee,multiplier,overhead}),next:quotes(next)}};
      note="fee = max(minFee, multiplier × base fee × (fulfillGasOverhead + callback gas)). Open requests keep the fee they escrowed.";
      break;
    }
    case "set-fee-recipient":{
      const current=getAddress(await c.feeRecipient());
      if(request.address===current)throw new Stop(`${request.address} is already the fee recipient`);
      // A production chain's fees go to its Safe, or to its interim owner until the Safe takes the set over.
      if(!chain.testnet){
        if(request.address!==chain.interimOwner)await requireProfileSafe(provider,chain,request.address,"The proposed fee recipient");
        checks.push(request.address===chain.interimOwner?"the proposed fee recipient is the interim owner":"the proposed fee recipient is the profile's Safe, strictly checked");
      }
      fn="setFeeRecipient";args=[request.address];details={previousFeeRecipient:current,feeRecipient:request.address,initialFeeRecipient:getAddress(await c.initialFeeRecipient())};
      note="withdrawFees pays the protocol share to the new recipient from now on. The initial fee recipient stays in the configuration hash.";
      break;
    }
    case "set-keeper-bps":{
      const current=await c.keeperFeeBps() as bigint;
      if(BigInt(request.bps)===current)throw new Stop(`The keeper share is already ${current} bps`);
      fn="setKeeperFeeBps";args=[request.bps];details={previousKeeperFeeBps:current,keeperFeeBps:request.bps};
      note="Applies to requests served from now on, including open ones. A keeper's fee coverage assumes its share: check FEE_COVERAGE_BPS against it.";
      break;
    }
    case "set-refund-bps":{
      const current=await c.refundBps() as bigint;
      if(BigInt(request.bps)===current)throw new Stop(`The refund share is already ${current} bps`);
      fn="setRefundBps";args=[request.bps];details={previousRefundBps:current,refundBps:request.bps};
      note="Applies to requests made from now on; open requests keep the share they escrowed under.";
      break;
    }
    case "register-beacon":{
      const b=request.beacon,count=await c.beaconCount() as bigint,verifierCode=await provider.getCode(b.verifier);
      if(count>=LIMITS.maxBeacons)throw new Stop(`The coordinator already holds the maximum of ${LIMITS.maxBeacons} beacons`);
      const expected=keccak256((await compiled("D20BeaconVerifier")).artifact.deployedBytecode);
      if(verifierCode==="0x")throw new Stop(`The verifier ${b.verifier} has no code`);
      if(keccak256(verifierCode)!==expected)throw new Stop(`The verifier's runtime code hash ${keccak256(verifierCode)} is not the compiled D20BeaconVerifier's ${expected}`);
      const sampled=sampleTime(b);
      if(sampled>BigInt(head.timestamp))throw new Stop(`The sample round ${b.sampleRound} is scheduled at ${sampled}, after the latest block time ${head.timestamp}: registerBeacon refuses it; use an earlier round`);
      const identity=beaconIdentity(b);
      for(let id=0n;id<count;id++)if(await c.beaconIdentity(id)===identity)throw new Stop(`This beacon is already registered as beacon ${id}`);
      const verifier=new Contract(b.verifier,["function isValidPublicKey(bytes) view returns(bool)","function verifyRound(bytes,uint64,bytes) view returns(bool)"],provider);
      if(!await verifier.isValidPublicKey(b.publicKey))throw new Stop("The verifier does not accept the beacon's public key");
      if(!await verifier.verifyRound(b.publicKey,b.sampleRound,b.sampleSignature))throw new Stop(`The verifier does not accept the sample: the signature of round ${b.sampleRound}`);
      checks.push("verifier code equals the compiled D20BeaconVerifier","sample scheduled before the latest block","not registered yet","verifier accepts the key","verifier accepts the sample");
      fn="registerBeacon";args=[[b.verifier,b.chainHash,b.publicKey,b.genesis,b.period,b.sampleRound,b.sampleSignature]];
      details={beaconId:count,identity,file:request.file,verifier:b.verifier,verifierCodeHash:expected,chainHash:b.chainHash,genesis:b.genesis,period:b.period,
        sample:{round:b.sampleRound,time:sampled,timeUtc:utc(sampled),secondsBeforeHead:BigInt(head.timestamp)-sampled}};
      note=`The registration never changes, and nothing binds it until schedule-beacon ${count} <fromTime> puts it in force: a separate transaction (on a production chain a separate Safe transaction), executed at least ${LIMITS.minScheduleLead} seconds before fromTime.`;
      break;
    }
    case "schedule-beacon":{
      const count=await c.beaconCount() as bigint,[inForce,since,nextBeaconId,nextFrom]=await c.beaconSchedule() as [bigint,bigint,bigint,bigint];
      if(BigInt(request.beaconId)>=count)throw new Stop(`Beacon ${request.beaconId} is not registered; the coordinator holds beacons 0 to ${count-1n}`);
      if(nextFrom===0n&&BigInt(request.beaconId)===inForce)throw new Stop(`Beacon ${request.beaconId} is already in force`);
      if(nextFrom!==0n&&BigInt(request.beaconId)===nextBeaconId&&request.fromTime===nextFrom)throw new Stop("That change is already scheduled");
      const lead=request.fromTime-BigInt(head.timestamp),needed=LIMITS.minScheduleLead+(send?SCHEDULE_SEND_MARGIN:0n);
      if(lead<needed)throw new Stop(`fromTime ${request.fromTime} is ${lead} seconds after the latest block; it must be at least ${needed} (${LIMITS.minScheduleLead} seconds of MIN_SCHEDULE_LEAD${send?` and ${SCHEDULE_SEND_MARGIN} for the transaction to be mined`:""})`);
      checks.push("registered beacon","not the beacon in force",`fromTime at least ${needed} seconds after the latest block`);
      const executeBefore=request.fromTime-LIMITS.minScheduleLead;
      fn="scheduleBeacon";args=[request.beaconId,request.fromTime];
      details={beaconId:request.beaconId,fromTime:request.fromTime,fromTimeUtc:utc(request.fromTime),inForce:{beaconId:inForce,since},
        replaces:nextFrom===0n?null:{beaconId:nextBeaconId,fromTime:nextFrom},executeBefore,executeBeforeUtc:utc(executeBefore),secondsLeftToExecute:executeBefore-BigInt(head.timestamp)};
      note=`Requests whose block timestamp is fromTime or later bind beacon ${request.beaconId}; earlier ones keep their beacon. The transaction reverts InvalidSchedule once executed later than executeBefore (fromTime minus MIN_SCHEDULE_LEAD); collect the Safe signatures well before it. A pending change is replaced.`;
      break;
    }
    case "cancel-beacon-schedule":{
      const [inForce,since,nextBeaconId,nextFrom]=await c.beaconSchedule() as [bigint,bigint,bigint,bigint];
      if(nextFrom===0n)throw new Stop("No beacon change is pending (one whose time has come is already in force and cannot be cancelled)");
      checks.push("a change is pending");
      fn="cancelBeaconSchedule";args=[];details={inForce:{beaconId:inForce,since},cancels:{beaconId:nextBeaconId,fromTime:nextFrom,fromTimeUtc:utc(nextFrom)},executeBefore:nextFrom};
      note="Reverts InvalidSchedule once the change has taken effect, at fromTime.";
      break;
    }
    case "transfer-ownership":{
      if(request.address===owner)throw new Stop(`${request.address} is already the owner`);
      const proposedKind=await accountKind(provider,request.address);
      if(!chain.testnet)await requireProfileSafe(provider,chain,request.address,"The proposed owner");
      checks.push("not the owner already",...(chain.testnet?[]:["the proposed owner is the profile's Safe: a canonical SafeProxy and singleton, VERSION(), the profile's owners and a threshold of at least 2, no module"]));
      fn="transferOwnership";args=[request.address];details={owner,proposedOwner:request.address,proposedOwnerKind:proposedKind,replacesPendingOwner:pendingOwner===ZERO?null:pendingOwner};
      note=`Ownership moves only when the proposed owner calls acceptOwnership (accept-ownership). The fee recipient does not change with it${chain.testnet?"":": set-fee-recipient to the Safe follows, as a Safe transaction"}.`;
      break;
    }
    case "accept-ownership":{
      fn="acceptOwnership";args=[];details={owner,pendingOwner};
      note=chain.testnet?"The pending owner becomes the owner. Update the chain profile's owner and the operator records to match."
        :"The Safe becomes the owner, and the interim owner's key no longer acts for the set. Next: set-fee-recipient to the Safe, as a Safe transaction.";
      break;
    }
    case "upgrade":{
      const next=request.implementation;
      if(next===implementation)throw new Stop("The coordinator already runs that implementation");
      const code=await provider.getCode(next);
      if(code==="0x")throw new Stop(`The new implementation ${next} has no code`);
      const codeHash=keccak256(code);
      if(codeHash!==request.approvedCodeHash)throw new Stop(`The new implementation's runtime code hash ${codeHash} is not the approved ${request.approvedCodeHash}`);
      const compiledCode=await expectedImplementationCode(provider,next,code);
      if(keccak256(compiledCode.code)!==codeHash)throw new Stop("The new implementation's runtime code is not the compiled D20VRFCoordinatorRobinhood at that address with its helpers");
      const uuid=await new Contract(next,face,provider).proxiableUUID().catch(()=>undefined);
      if(uuid!==IMPLEMENTATION_SLOT)throw new Stop(`The new implementation's proxiableUUID is ${uuid??"missing"}, not the ERC-1967 implementation slot`);
      const layout=await storageLayoutHash();
      if(layout!==manifest.storageLayoutHash&&layout!==request.approvedLayoutHash)
        throw new Stop(`The reviewed storage layout (hash ${layout}) differs from the one the manifest records for the running implementation (${manifest.storageLayoutHash}); once a review confirms it only appends, pass --approve-layout-hash ${layout}`);
      checks.push("code hash equals the approval","code equals the compiled D20VRFCoordinatorRobinhood with its helpers","helpers' code equals the compiled helpers","proxiableUUID is the ERC-1967 slot",
        layout===manifest.storageLayoutHash?"storage layout equals the manifest's":"storage layout change approved by its hash","compiled storage layout equals the reviewed baseline");
      fn="upgradeToAndCall";args=[next,"0x"];
      details={previousImplementation:implementation,implementation:next,runtimeCodeHash:codeHash,proofVerifier:compiledCode.proofVerifier,mappingLibrary:compiledCode.mappingLibrary,
        storageLayoutHash:layout,layoutChanged:layout!==manifest.storageLayoutHash,keeperPin:{EXPECTED_IMPLEMENTATION_CODE_HASH:codeHash},
        manifestUpdate:{coordinatorImplementation:next,coordinatorImplementationCodeHash:codeHash,storageLayoutHash:layout,
          implementationUpgrades:{previousImplementation:implementation,previousImplementationCodeHash:implementationCodeHash,implementation:next,implementationCodeHash:codeHash}}};
      note="No initializer runs: the upgrade only moves the implementation slot. A keeper started with APPROVED_NEXT_IMPLEMENTATION_CODE_HASH set to this hash keeps sending until this executes and then restarts on it; keepers pinned only to the previous implementation stop sending once it executes. Record manifestUpdate in the manifest afterwards.";
      break;
    }
  }
  const data=face.encodeFunctionData(fn,args);
  plan.call={from:senderAddress,to:proxy,value:"0",function:face.getFunction(fn)!.format("sighash"),args:args.map(text),data};
  plan.checks=checks;plan.details=JSON.parse(JSON.stringify(details,(_key,value)=>typeof value==="bigint"?value.toString():value));plan.note=note;
  // Every call is simulated from the account that must send it before it is printed, proposed or signed.
  await provider.call({from:senderAddress,to:proxy,data}).catch(error=>{throw new Stop(`${fn} reverts when simulated from the ${role}: ${failureReason(error,face)}`);});
  if(plan.execution==="Safe")plan.safeTransaction=safeBatch(chain.chainId,senderAddress,`D20 round coordinator: ${request.action}`,
    `${plan.call.function} with (${plan.call.args.join(", ")}) on the round coordinator ${proxy} on ${chain.name} (chain ${chain.chainId}), checked and simulated from the Safe ${senderAddress} at block ${head.number}. ${note??""}`.trim(),{to:proxy,data});
  return plan;
}

/** The runtime code a coordinator implementation at `at` has if it is the compiled D20VRFCoordinatorRobinhood, with the proof verifier it
 * answers and the library its code links, after checking that both helpers have the compiled helpers' code. */
async function expectedImplementationCode(provider:JsonRpcApiProvider,at:string,code:string){
  const contract=await compiled("D20VRFCoordinatorRobinhood");
  const places=Object.values(contract.artifact.deployedLinkReferences).flatMap(libraries=>Object.values(libraries)).flat();
  const bytes=getBytes(code),linked=new Set(places.map(({start})=>start+20<=bytes.length?getAddress(hexlify(bytes.subarray(start,start+20))):""));
  if(places.length===0||linked.size!==1||linked.has(""))throw new Stop("The new implementation is not the compiled D20VRFCoordinatorRobinhood: it does not link one mapping library where the compiled code does");
  const mappingLibrary=[...linked][0];
  const proofVerifier=await new Contract(at,["function proofVerifier() view returns(address)"],provider).proofVerifier().then(getAddress,()=>{throw new Stop("The new implementation is not the compiled D20VRFCoordinatorRobinhood: it does not answer proofVerifier()");});
  if(keccak256(await provider.getCode(proofVerifier))!==keccak256((await compiled("D20VRFProofVerifier")).artifact.deployedBytecode))
    throw new Stop(`The new implementation's proof verifier ${proofVerifier} is not the compiled D20VRFProofVerifier`);
  if(keccak256(await provider.getCode(mappingLibrary))!==keccak256(libraryRuntime((await compiled("LinkedRandomnessMapping")).artifact.deployedBytecode,mappingLibrary)))
    throw new Stop(`The new implementation's mapping library ${mappingLibrary} is not the compiled LinkedRandomnessMapping`);
  const runtime=link(contract.artifact.deployedBytecode,contract.artifact.deployedLinkReferences,mappingLibrary);
  return {code:withImmutables(contract,runtime,{__self:at,proofVerifier}),proofVerifier,mappingLibrary};
}

/** A production set's move to its Safe, step by step: transfer-ownership from the interim owner's key, then accept-ownership and
 * set-fee-recipient from the Safe; which are done, the next one, and the Safe's strict check, which the transfer must pass. Undefined on a
 * test network and on a chain whose profile names no Safe. */
async function ownershipTransfer(provider:JsonRpcApiProvider,chain:OperableChain,owner:string,pendingOwner:string,feeRecipient:string){
  const safe=chain.safe?.address;
  if(chain.testnet||!safe)return undefined;
  const check=await checkProfileSafe(provider,chain,safe);
  const steps=[
    {step:1,action:`transfer-ownership ${safe}`,call:"transferOwnership",by:"the interim owner's key (--send --mainnet)",done:owner===safe||pendingOwner===safe},
    {step:2,action:"accept-ownership",call:"acceptOwnership",by:"the Safe (a Safe Transaction Builder file, --safe-out)",done:owner===safe},
    {step:3,action:`set-fee-recipient ${safe}`,call:"setFeeRecipient",by:"the Safe (a Safe Transaction Builder file, --safe-out)",done:feeRecipient===safe},
  ];
  const next=steps.find(step=>!step.done);
  return {safe,interimOwner:chain.interimOwner??null,pending:next!==undefined,next:next?.action??null,steps,
    safeCheck:{passes:check.problems.length===0,problems:check.problems,version:check.version??null,owners:check.owners??null,threshold:check.threshold??null}};
}

/** Every view of the coordinator, whether the proxy and its implementation are the manifest's, and on a production chain the ownership
 * transfer to its Safe. */
async function status(provider:JsonRpcApiProvider,c:Contract,chain:OperableChain,head:Block,manifest:RoundManifest,
  pins:{proxyCodeHash:string;implementation:string;implementationCodeHash:string;pins:Record<string,boolean>},backup?:string){
  const owner=getAddress(await c.owner()),ownerKind=await accountKind(provider,owner),count=await c.beaconCount() as bigint;
  const pendingOwner=getAddress(await c.pendingOwner()),feeRecipient=getAddress(await c.feeRecipient());
  const interim=!chain.testnet&&!!chain.interimOwner&&owner===chain.interimOwner;
  const ownerCheck=chain.testnet||interim?undefined:await checkProfileSafe(provider,chain,owner);
  const ownership=await ownershipTransfer(provider,chain,owner,pendingOwner,feeRecipient);
  const beacons=[];
  for(let id=0n;id<count;id++){
    const b=await c.getBeacon(id);
    beacons.push({id,verifier:b.verifier,chainHash:b.chainHash,genesis:b.genesis,period:b.period,identity:await c.beaconIdentity(id)});
  }
  const [beaconId,since,nextBeaconId,nextFrom]=await c.beaconSchedule() as [bigint,bigint,bigint,bigint];
  const [roundBeacon,round]=await c.roundAt(head.timestamp) as [bigint,bigint];
  const [minFee,multiplier,overhead]=await c.pricing() as [bigint,bigint,bigint];
  const warnings=[...Object.entries(pins.pins).filter(([,ok])=>!ok).map(([name])=>`${name} differs from the manifest`),
    ...(interim?[`the owner ${owner} is the interim owner: the transfer to the Safe is pending (ownership.next)`]:[]),
    ...(ownerCheck&&ownerCheck.problems.length?[`the owner ${owner} is not the Safe a production owner must be: ${ownerCheck.problems.join("; ")}`]:[]),
    ...(ownership?.pending&&!ownership.safeCheck.passes?[`the Safe ${ownership.safe} does not pass its strict check, so the transfer to it is refused: ${ownership.safeCheck.problems.join("; ")}`]:[])];
  const result={head:{block:head.number,time:head.timestamp},coordinator:c.target,proxyCodeHash:pins.proxyCodeHash,implementation:pins.implementation,
    implementationCodeHash:pins.implementationCodeHash,matchesManifest:pins.pins,
    owner:{address:owner,kind:ownerKind,...(chain.testnet?{}:{interimOwner:interim})},pendingOwner,
    ...(ownership===undefined?{}:{ownership}),
    keeper:await c.keeper(),backupKeeperCount:await c.backupKeeperCount(),
    ...(backup===undefined?{}:{backupKeeper:{address:backup,allowed:await c.isBackupKeeper(backup)}}),
    feeRecipient,initialFeeRecipient:await c.initialFeeRecipient(),keeperFeeBps:await c.keeperFeeBps(),refundBps:await c.refundBps(),
    pricing:{minFeeWei:minFee,feeMultiplier:multiplier,fulfillGasOverhead:overhead},initialMinFee:await c.initialMinFee(),maxMinFee:await c.MAX_MIN_FEE(),
    protocolConfigurationHash:await c.protocolConfigurationHash(),keyHash:await c.keyHash(),publicKey:[await c.publicKeyX(),await c.publicKeyY()],proofVerifier:await c.proofVerifier(),
    roundLead:await c.ROUND_LEAD(),minScheduleLead:await c.MIN_SCHEDULE_LEAD(),maxBeaconPeriod:await c.MAX_BEACON_PERIOD(),
    beacons,schedule:{beaconId,since,pending:nextFrom===0n?null:{beaconId:nextBeaconId,fromTime:nextFrom,fromTimeUtc:utc(nextFrom)}},roundAtHead:{beaconId:roundBeacon,round},
    requests:{nextRequestId:await c.nextRequestId(),lastServedRequestId:await c.lastServedRequestId()},
    balances:{coordinatorWei:await provider.getBalance(c.target as string),earnedFeesWei:await c.earnedFees(),totalKeeperCreditsWei:await c.totalKeeperCredits(),
      totalRefundCreditsWei:await c.totalRefundCredits()},
    warnings};
  return JSON.parse(JSON.stringify(result,(_key,value)=>typeof value==="bigint"?value.toString():value)) as Record<string,unknown>;
}

/** Send a planned call from the owner key, on a test network or from a production chain's interim owner: the signer must be the planned
 * sender, an account without code, with no transaction pending. The signed transaction is journaled in journalDirectory before it is broadcast. */
export async function sendAdminAction(o:{provider:JsonRpcApiProvider;chain:OperableChain;plan:AdminPlan;signer:Wallet;journalDirectory:string}){
  const {provider,chain,plan,signer}=o;
  if(!plan.call||!plan.sender)throw new Stop(`${plan.action} sends nothing`);
  if(!chain.testnet&&!(plan.interimOwner&&!!chain.interimOwner&&plan.sender.address===chain.interimOwner))
    throw new Stop(`${chain.key} is a production chain: only its interim owner sends, and its Safe proposes the printed Safe transaction`);
  if(plan.execution!=="owner key")throw new Stop("The owner is a contract: propose the Safe transaction instead");
  if(getAddress(signer.address)!==plan.sender.address)throw new Stop(`The signing wallet ${signer.address} is not the ${plan.sender.role} ${plan.sender.address}`);
  const nonce=await provider.getTransactionCount(signer.address,"latest");
  if(await provider.getTransactionCount(signer.address,"pending")!==nonce)throw new Stop("The signing wallet has a pending transaction");
  const cap=GAS_CAP[plan.action]??DEFAULT_GAS_CAP,gas=await provider.estimateGas({from:signer.address,to:plan.call.to,data:plan.call.data});
  if(gas>cap)throw new Stop(`Estimated gas ${gas} exceeds the ${plan.action} cap of ${cap}`);
  const gasLimit=gas*12n/10n>cap?cap:gas*12n/10n,{maxFee,tip}=await currentFee(provider,BigInt(chain.gas.maxFeePerGasWei),tipBounds(chain.gas));
  const raw=await signer.signTransaction({to:plan.call.to,data:plan.call.data,value:0,chainId:chain.chainId,type:2,nonce,gasLimit,maxFeePerGas:maxFee,maxPriorityFeePerGas:tip});
  const hash=keccak256(raw);
  const journal=await open(join(o.journalDirectory,`${signer.address}-${nonce}.json`),"ax",0o600);
  try{await journal.writeFile(JSON.stringify({action:plan.action,call:plan.call,hash,raw})+"\n");await journal.sync();}finally{await journal.close();}
  const sent=await provider.broadcastTransaction(raw);
  if(sent.hash!==hash)throw new Stop("The RPC returned another transaction hash");
  const receipt=await provider.waitForTransaction(hash,1,120_000);
  if(receipt?.status!==1)throw new Stop(`Transaction ${hash} did not succeed; inspect it and the journal before running again`);
  return {transactionHash:hash,block:receipt.blockNumber,gasUsed:String(receipt.gasUsed)};
}
