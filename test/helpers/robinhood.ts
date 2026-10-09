import {readFileSync} from "node:fs";
import {AbiCoder,getBytes,id,keccak256,sha256} from "ethers";
import {signTestRound,testBeacon} from "./beacon.ts";
import {makeProof,publicKey} from "./proof.ts";

// The Arbitrum system contracts the Robinhood contracts read, at their real addresses.
export const ARB_SYS="0x0000000000000000000000000000000000000064";
export const NODE_INTERFACE="0x00000000000000000000000000000000000000c8";
/// Under the mocks an L2 block number is the local one plus this, so a read of block.number is off by a million blocks.
export const L2_OFFSET=1_000_000n;
/// EIP-170: the largest runtime code a contract may have on a node with the default limit.
export const CODE_SIZE_LIMIT=24_576;
/// EIP-3860: the largest init code a creation may carry on such a node.
export const INIT_CODE_SIZE_LIMIT=49_152;
/// The block values the Robinhood contracts must not read, as opcodes. NUMBER and BLOCKHASH are the parent chain's on an Arbitrum chain;
/// COINBASE, PREVRANDAO and GASLIMIT are chain constants or values the sequencer sets, never an input of a randomness proof. Their solc names
/// are block.number, blockhash(), block.coinbase, block.prevrandao (block.difficulty) and block.gaslimit.
export const BLOCKHASH=0x40,COINBASE=0x41,NUMBER=0x43,PREVRANDAO=0x44,GASLIMIT=0x45;
export const BLOCK_OPCODES={NUMBER,BLOCKHASH,COINBASE,PREVRANDAO,GASLIMIT};
/// Byte strings the Robinhood runtime code must not contain anywhere: the selector of ArbSys.arbBlockHash(uint256) and the address of the
/// EIP-2935 block hash history contract. Nothing on Robinhood reads a block hash.
export const FORBIDDEN_BYTES={arbBlockHash:"2b407a82",blockHashHistory:"0000f90827f1c53a10cb7a02335b175320002935"};

/// The round coordinator's pricing for the tests and the deployment helper: 0.000025 ETH minimum fee, multiplier 2, 360,000 gas
/// fulfillment overhead, 80% keeper share.
export const ROUND_PRICING={minFee:25_000_000_000_000n,feeMultiplier:2,fulfillGasOverhead:360_000,keeperBps:8000};
export const ROUND_LEAD=3n;
export const SEED_DOMAIN=id("D20_VRF_ROUND_SEED");
export const TRANSCRIPT_DOMAIN=id("D20_VRF_ROUND_TRANSCRIPT");
export const CONFIG_DOMAIN=id("D20_VRF_ROUND_CONFIG");
export const ROUND_BEACON_DOMAIN=id("D20_ROUND_BEACON_V1");
const abi=AbiCoder.defaultAbiCoder();

type Artifact={abi:any[];bytecode:string;deployedBytecode:string;immutableReferences:Record<string,Array<{start:number;length:number}>>;
  linkReferences:Record<string,Record<string,Array<{start:number;length:number}>>>;buildInfoId:string;inputSourceName:string};
const fileName=(source:string)=>source.slice(source.lastIndexOf("/")+1,-".sol".length);
/// A compiled contract as Hardhat wrote it (run the tests through hardhat, or compile first): the source path below contracts/, and the
/// contract's name when it is not the file's.
export function compiled(source:string,name=fileName(source)):Artifact{
  return JSON.parse(readFileSync(new URL(`../../artifacts/contracts/${source}/${name}.json`,import.meta.url),"utf8"));
}
/// The storage layout the compiler reported for a contract.
export function storageLayout(source:string,name=fileName(source)):{storage:Array<{label:string;slot:string;offset:number;type:string}>;types:Record<string,any>}{
  const artifact=compiled(source,name);
  const build=JSON.parse(readFileSync(new URL(`../../artifacts/build-info/${artifact.buildInfoId}.output.json`,import.meta.url),"utf8"));
  return (build.output??build).contracts[artifact.inputSourceName][name].storageLayout;
}
/// Code with each unlinked library placeholder (__$…$__, the 20 bytes of a PUSH20) read as the zero address.
export const unlinked=(code:string)=>code.replace(/__\$[0-9a-f]{34}\$__/g,"00".repeat(20));
/// Runtime code without its CBOR metadata tail.
export function withoutMetadata(deployedBytecode:string):Uint8Array{
  const code=getBytes(unlinked(deployedBytecode)),start=code.length-2-(code[code.length-2]*256+code[code.length-1]);
  if(start<=0||code[start]>>5!==5)throw new Error("Runtime code has no CBOR metadata tail");
  return code.subarray(0,start);
}
/// Runtime code without its metadata tail and without the data solc appends after the code: the 32-byte constants its optimizer copies
/// with PUSH1 32, PUSH2 offset, PUSH0, CODECOPY, followed by any other data. The first offset such a copy reads, past the copy itself and
/// at an instruction boundary, is where the code ends.
export function executableCode(deployedBytecode:string):Uint8Array{
  const code=withoutMetadata(deployedBytecode),starts=new Set<number>();
  let end=code.length;
  for(let i=0;i<code.length;i++){
    starts.add(i);
    if(code[i]===0x39&&i>=6&&code[i-6]===0x60&&code[i-5]===0x20&&code[i-4]===0x61&&code[i-1]===0x5f){
      const offset=code[i-3]*256+code[i-2];
      if(offset>i&&offset<end)end=offset;
    }
    if(code[i]>=0x60&&code[i]<=0x7f)i+=code[i]-0x5f;
  }
  if(end<code.length&&!starts.has(end))throw new Error("Runtime code copies constants from inside an instruction");
  return code.subarray(0,end);
}
/// The 32-byte words solc appended after the executable code, as hex.
export function codeConstants(deployedBytecode:string):string[]{
  const all=withoutMetadata(deployedBytecode),code=executableCode(deployedBytecode);
  return Array.from({length:Math.floor((all.length-code.length)/32)},(_,k)=>"0x"+Buffer.from(all.subarray(code.length+32*k,code.length+32*k+32)).toString("hex"));
}
/// How often each opcode occurs in the executable part of runtime code: PUSH data, appended constants and the metadata tail are skipped.
export function opcodeCounts(deployedBytecode:string):Map<number,number>{
  const code=executableCode(deployedBytecode),counts=new Map<number,number>();
  for(let i=0;i<code.length;i++){
    counts.set(code[i],(counts.get(code[i])??0)+1);
    if(code[i]>=0x60&&code[i]<=0x7f)i+=code[i]-0x5f;
  }
  return counts;
}
/// How often each of the BLOCK_OPCODES occurs in runtime code, by name. A contract that reads none of them has every count 0.
export function blockOpcodes(deployedBytecode:string):Record<keyof typeof BLOCK_OPCODES,number>{
  const counts=opcodeCounts(deployedBytecode);
  return Object.fromEntries(Object.entries(BLOCK_OPCODES).map(([name,opcode])=>[name,counts.get(opcode)??0])) as Record<keyof typeof BLOCK_OPCODES,number>;
}
/// Runtime code as lowercase hex without 0x. A library placeholder is hex-free text, so it cannot hide a forbidden byte string.
export const runtimeHex=(artifact:{deployedBytecode:string})=>artifact.deployedBytecode.slice(2).toLowerCase();

/// ArbSys and NodeInterface as test doubles at their real addresses, attached to their ABIs.
export async function arbitrumMocks(ethers:any,networkHelpers:any){
  await networkHelpers.setCode(ARB_SYS,compiled("robinhood/test/ArbitrumMocks.sol","MockArbSys").deployedBytecode);
  await networkHelpers.setCode(NODE_INTERFACE,compiled("robinhood/test/ArbitrumMocks.sol","MockNodeInterface").deployedBytecode);
  return {arbSys:await ethers.getContractAt("MockArbSys",ARB_SYS),nodeInterface:await ethers.getContractAt("MockNodeInterface",NODE_INTERFACE)};
}

/// A beacon registration as BeaconBook takes it.
export type RoundBeacon={verifier:string;chainHash:string;publicKey:string;genesis:bigint;period:bigint;sampleRound:bigint;sampleSignature:string};
export const registrationOf=(b:RoundBeacon)=>[b.verifier,b.chainHash,b.publicKey,b.genesis,b.period,b.sampleRound,b.sampleSignature] as const;
/// The test beacon of test/helpers/beacon.ts (rounds every 3 seconds from 3000 seconds before now), vouched for by its signature of round 1.
export function testRoundBeacon(verifier:string,now:bigint):RoundBeacon{
  return {...testBeacon(verifier,now),sampleRound:1n,sampleSignature:signTestRound(1n)};
}
/// The test beacon's signature of a round.
export const signRound=(round:bigint)=>signTestRound(round);

// drand evmnet: the chain parameters committed for the deployment (config/beacons/drand-evmnet.json) and real rounds fetched from four public
// relays (test/fixtures/drand-evmnet-2026-09-29.json).
const EVMNET_CONFIG=JSON.parse(readFileSync(new URL("../../config/beacons/drand-evmnet.json",import.meta.url),"utf8"));
const EVMNET_FIXTURE=JSON.parse(readFileSync(new URL("../fixtures/drand-evmnet-2026-09-29.json",import.meta.url),"utf8"));
export const EVMNET={genesis:BigInt(EVMNET_CONFIG.genesis),period:BigInt(EVMNET_CONFIG.period),
  rounds:EVMNET_FIXTURE.rounds.map((r:any)=>({round:BigInt(r.round),randomness:"0x"+r.randomness,signature:"0x"+r.signature})) as Array<{round:bigint;randomness:string;signature:string}>};
/// drand evmnet with a verifier deployed locally, vouched for by its real round 1. The committed configuration's sample round is later than
/// the fixture's rounds, so a chain started before them cannot register with it.
export function evmnetBeacon(verifier:string):RoundBeacon{
  const first=EVMNET.rounds.find(r=>r.round===1n)!;
  return {verifier,chainHash:EVMNET_CONFIG.chainHash,publicKey:EVMNET_CONFIG.publicKey,genesis:EVMNET.genesis,period:EVMNET.period,sampleRound:1n,sampleSignature:first.signature};
}
/// When a beacon's round is scheduled.
export const roundTime=(b:{genesis:bigint;period:bigint},round:bigint)=>b.genesis+(round-1n)*b.period;
/// The round a request sent in a block with this timestamp binds: the first r with genesis + (r - 1) * period >= timestamp + ROUND_LEAD.
export function assignedRound(b:{genesis:bigint;period:bigint},timestamp:bigint){
  const t=timestamp+ROUND_LEAD;
  return t<=b.genesis?1n:(t-b.genesis+b.period-1n)/b.period+1n;
}
/// A chain clock that starts ten minutes before the first fixture round after round 1, so requests can bind real evmnet rounds.
export const EVMNET_START=new Date(Number(roundTime(EVMNET,EVMNET.rounds[1].round)-600n)*1000);

export type RoundFixture=Awaited<ReturnType<typeof deployRoundCoordinator>>;
/// The round coordinator behind D20Proxy with its two helpers, the Arbitrum mocks, a beacon verifier and a RoundConsumer. The beacon is the
/// test beacon unless `evmnet` is set. Pricing is ROUND_PRICING; the keeper is the second signer and the fee recipient the third.
export async function deployRoundCoordinator(ethers:any,networkHelpers:any,options:{evmnet?:boolean}={}){
  const [owner,keeper,feeRecipient,stranger,backup,extra]=await ethers.getSigners();
  const mocks=await arbitrumMocks(ethers,networkHelpers);
  const beaconVerifier=await ethers.deployContract("D20BeaconVerifier");
  const verifierAddress=await beaconVerifier.getAddress();
  const beacon=options.evmnet?evmnetBeacon(verifierAddress):testRoundBeacon(verifierAddress,BigInt(await networkHelpers.time.latest()));
  const helpers=await deployHelpers(ethers);
  const implementation=await deployImplementation(ethers,helpers);
  const params=[publicKey(),owner.address,feeRecipient.address,keeper.address,ROUND_PRICING.keeperBps,ROUND_PRICING.minFee,ROUND_PRICING.feeMultiplier,ROUND_PRICING.fulfillGasOverhead];
  const proxy=await ethers.deployContract("D20Proxy",[await implementation.getAddress(),implementation.interface.encodeFunctionData("initialize",[params,registrationOf(beacon)])]);
  const coordinator=implementation.attach(await proxy.getAddress()) as any;
  const consumer=await ethers.deployContract("RoundConsumer",[await coordinator.getAddress()]);
  return {...mocks,...helpers,coordinator,implementation,proxy,consumer,beaconVerifier,beacon,params,owner,keeper,feeRecipient,stranger,backup,extra};
}
/// The proof verifier and the linked mapping library a coordinator implementation needs. The library is deployed from its bytecode alone:
/// its ABI names the mapping's enum by its Solidity name, which ethers cannot parse, and nothing calls it but the coordinator.
export async function deployHelpers(ethers:any){
  const [deployer]=await ethers.getSigners();
  const library=compiled("robinhood/LinkedRandomnessMapping.sol");
  const mapping=await new ethers.ContractFactory([],library.bytecode,deployer).deploy();
  await mapping.waitForDeployment();
  return {proofVerifier:await ethers.deployContract("D20VRFProofVerifier"),mapping};
}
/// A coordinator implementation (or a contract derived from it) linked to the helpers.
export async function deployImplementation(ethers:any,helpers:{proofVerifier:any;mapping:any},name="D20VRFCoordinatorRobinhood"){
  return ethers.deployContract(name,[await helpers.proofVerifier.getAddress()],{libraries:{LinkedRandomnessMapping:await helpers.mapping.getAddress()}});
}

/// The fee of a request in the next block, whose base fee this sets (default 0.02 gwei, Robinhood's floor).
export async function nextFee(c:RoundFixture,networkHelpers:any,callbackGasLimit:number,baseFee=20_000_000n){
  await networkHelpers.setNextBlockBaseFeePerGas(baseFee);
  return c.coordinator.quoteFeeAt(callbackGasLimit,baseFee) as Promise<bigint>;
}
/// A raw request through the RoundConsumer at its exact fee, optionally at a block timestamp: its id, fee, receipt and getRoundRequest.
export async function request(c:RoundFixture,networkHelpers:any,options:{seed?:string;gas?:number;at?:bigint}={}){
  const gas=options.gas??100_000;
  if(options.at!==undefined)await networkHelpers.time.setNextBlockTimestamp(options.at);
  const fee=await nextFee(c,networkHelpers,gas);
  const receipt=await (await c.consumer.request(options.seed??id("seed"),gas,c.owner.address,{value:fee})).wait();
  const requestId=await c.consumer.lastRequestId() as bigint;
  return {id:requestId,fee,receipt,request:await c.coordinator.getRoundRequest(requestId)};
}
/// The seed of a request given its round's randomness, computed off chain from getRoundRequest as a keeper or a replay does.
export async function roundSeed(c:RoundFixture,ethers:any,requestId:bigint,roundRandomness:string){
  const q=await c.coordinator.getRoundRequest(requestId);
  return BigInt(keccak256(abi.encode(
    ["bytes32","uint256","address","bytes32","uint256","address","bytes32","bytes32","uint64","uint8","uint64","bytes32"],
    [SEED_DOMAIN,(await ethers.provider.getNetwork()).chainId,await c.coordinator.getAddress(),await c.coordinator.keyHash(),requestId,
      q.consumer,q.clientSeed,q.mappingHash,q.requestBlock,q.beaconId,q.round,roundRandomness])));
}
/// drand's randomness value of a round signature.
export const randomnessOf=(signature:string)=>sha256(signature);
/// A VRF proof for a request whose round has this signature.
export async function proveRequest(c:RoundFixture,ethers:any,requestId:bigint,signature:string){
  return makeProof(await roundSeed(c,ethers,requestId,randomnessOf(signature)));
}

/// A receipt's events from one contract, parsed with its interface; `name` keeps only the events of that name.
export function eventsOf(contract:any,receipt:any,name?:string):any[]{
  const address=String(contract.target).toLowerCase();
  return receipt.logs.filter((log:any)=>log.address.toLowerCase()===address)
    .map((log:any)=>{try{return contract.interface.parseLog(log);}catch{return null;}})
    .filter((event:any)=>event&&(name===undefined||event.name===name));
}
/// What a call answers, in a word: `returns <value>`, the name of the custom error it reverted with as the contract's interface names it,
/// or `fails` when it ran out of gas or reverted with no error the interface knows.
export async function outcome(contract:any,call:()=>Promise<unknown>):Promise<string>{
  try{return `returns ${String(await call())}`;}
  catch(error:any){
    const data=error?.data??error?.error?.data;
    if(typeof data!=="string"||data==="0x")return "fails";
    try{return contract.interface.parseError(data)?.name??"fails";}catch{return "fails";}
  }
}
/// Fulfil a request alone with its round's signature, as the keeper by default: the proof and the receipt.
export async function serve(c:RoundFixture,ethers:any,requestId:bigint,options:{from?:any;signature?:string;gasLimit?:bigint}={}){
  const q=await c.coordinator.getRoundRequest(requestId),signature=signRound(q.round);
  const proof=await proveRequest(c,ethers,requestId,signature);
  const receipt=await (await c.coordinator.connect(options.from??c.keeper).fulfillRandomness(requestId,proof,options.signature??signature,
    options.gasLimit===undefined?{}:{gasLimit:options.gasLimit})).wait();
  return {proof,receipt,signature};
}
/// What the coordinator holds and what it owes: the escrow of every open request, the fees it earned, and the keeper and refund credits.
/// A solvent coordinator holds exactly what it owes.
export async function solvency(c:RoundFixture,ethers:any){
  let escrow=0n;
  for(let id=1n;id<await c.coordinator.nextRequestId();id++){
    const q=await c.coordinator.getRoundRequest(id);
    if(!q.fulfilled&&!q.refunded)escrow+=q.feePaid;
  }
  const owed=escrow+await c.coordinator.earnedFees()+await c.coordinator.totalKeeperCredits()+await c.coordinator.totalRefundCredits();
  return {balance:await ethers.provider.getBalance(await c.coordinator.getAddress()) as bigint,owed,escrow};
}
