// A local chain for the round coordinator's deployment and admin tests: Hardhat's in-process EDR network under a Robinhood chain id, with
// the deterministic CREATE2 factory, Hardhat's public test accounts, an ethers provider over it, a JSON-RPC endpoint on 127.0.0.1 for the
// command lines, a real Safe 1.5.0, and the round set deployed through scripts/lib/robinhood-deploy.ts. Nothing here touches a public
// network or creates a key.
import {createServer} from "node:http";
import type {AddressInfo} from "node:net";
import {spawn} from "node:child_process";
import {fileURLToPath} from "node:url";
import {mkdir,readFile,writeFile} from "node:fs/promises";
import {join} from "node:path";
import {network} from "hardhat";
import {BrowserProvider,Contract,HDNodeWallet,Interface,SigningKey,Wallet,concat,getAddress,type TransactionReceipt} from "ethers";
import {loadChain,type Chain,type OperableChain} from "../lib/chains.ts";
import {deployRoundSet,loadBeaconFile,loadCreate2Config,loadRoundService,planRoundSet,type InterimOwnership,type Roles,type RoundPlan,type SourceRecord} from "../lib/robinhood-deploy.ts";

export const root=fileURLToPath(new URL("../..",import.meta.url));
/** Runtime code of the deterministic deployment proxy; its hash is chains.json create2.codeHash. */
export const FACTORY_CODE="0x7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe03601600081602082378035828234f58015156039578182fd5b8082525050506014600cf3";
/** Code that answers every call with the word 1, so getThreshold() is 1: what a check that only asks getThreshold() took for a Safe. The
 * Safe check (scripts/lib/safe.ts) refuses it. */
export const SAFE_STAND_IN_CODE="0x600160005260206000f3";
/** Hardhat's public test accounts (its default mnemonic), funded on the local network. Never funded or used anywhere else. */
export const account=(index:number)=>{
  const node=HDNodeWallet.fromPhrase("test test test test test test test test test test test junk",undefined,`m/44'/60'/0'/0/${index}`);
  return new Wallet(node.privateKey);
};
/** The roles of a local deployment: account 0 owns, account 1 keeps, account 2's key is the VRF key, account 3 deploys. */
export const OWNER=account(0),KEEPER=account(1),VRF=account(2),DEPLOYER=account(3),STRANGER=account(4),BACKUP=account(5);
/** The local Safe's signers, accounts 6 and 7, at threshold 2: the production Safe is a 2 of 2. */
export const SAFE_SIGNERS=[account(6),account(7)];
/** The source record of an in-process deployment: these tests run from a working tree under review, which reviewedSource would refuse. */
export const TEST_SOURCE={commit:null,unreviewed:"not checked: an in-process test"} satisfies SourceRecord;
export const publicKeyOf=(wallet:Wallet):[bigint,bigint]=>{
  const point=SigningKey.computePublicKey(wallet.privateKey,false);
  return [BigInt("0x"+point.slice(4,68)),BigInt("0x"+point.slice(68))];
};

// The tests read chains.json as committed: an operator's private owner overlay (deployments/private) is never merged into them.
process.env.D20_PRIVATE_PROFILE_DIR=fileURLToPath(new URL("./fixtures/no-private-profile/",import.meta.url));
/** A Robinhood chain profile with `owner` as its owner for the test, in place of the one chains.json holds (the testnet's deployer EOA, or the
 * production profile's Safe), and any other profile fields, such as a local Safe and interim owner, from patch. The production profile keeps
 * chains.json's Safe and interim owner unless patch or the test replaces them. */
export const profile=async(key:"robinhood-testnet"|"robinhood-mainnet",owner:string,patch:Partial<Chain>={}):Promise<OperableChain>=>({...await loadChain(key),...patch,owner});
/** The production profile's Safe fields for a local Safe: the Safe as owner, its signers and threshold, and an interim owner. */
export const safeProfile=(safe:string,o:{interimOwner?:string|null;owners?:string[]|null;threshold?:number}={}):Partial<Chain>=>
  ({safe:{address:safe,owners:o.owners===undefined?SAFE_SIGNERS.map(signer=>signer.address):o.owners,threshold:o.threshold??2},interimOwner:o.interimOwner??null});
export type LocalChain=Awaited<ReturnType<typeof localChain>>;
/** EDR under the chain id of a Robinhood profile, with the factory in place, and the profile with `owner` as its owner and patch's fields. */
export async function localChain(key:"robinhood-testnet"|"robinhood-mainnet",owner:string,patch:Partial<Chain>={}){
  const chain=await profile(key,owner,patch);
  const connection=await network.create({network:"local",override:{chainId:chain.chainId}});
  const request=(method:string,params:unknown[]=[])=>connection.provider.request({method,params});
  await request("hardhat_setCode",[chain.create2.factory,FACTORY_CODE]);
  const provider=new BrowserProvider(connection.provider,chain.chainId,{polling:true,pollingInterval:50,cacheTimeout:-1});
  return {chain,provider,request,close:async()=>{provider.destroy();await connection.close();}};
}
/** The round set's plan for a local chain, from the chain's service profile, the beacon file and the CREATE2 configuration. */
export async function localPlan(chain:OperableChain,roles:Partial<Roles>={},interim?:InterimOwnership):Promise<RoundPlan>{
  const service=await loadRoundService(chain.key);
  return planRoundSet({chain,service,beacon:await loadBeaconFile(service.beacon,true),create2:await loadCreate2Config(),
    roles:{owner:chain.owner,feeRecipient:chain.owner,keeper:KEEPER.address,publicKey:publicKeyOf(VRF),...roles},...(interim===undefined?{}:{interim})});
}
/** Deploy the whole set on a local chain from the deployer account; returns the plan, the report and the paths it wrote. */
export async function deployLocal(local:LocalChain,dir:string,roles:Partial<Roles>={},interim?:InterimOwnership){
  const plan=await localPlan(local.chain,roles,interim),journalPath=join(dir,"private","round-deployment.jsonl"),manifestPath=join(dir,`${local.chain.key}.json`);
  const report=await deployRoundSet({provider:local.provider,chain:local.chain,plan,deployer:DEPLOYER.address,wallet:DEPLOYER,send:true,journalPath,manifestPath,source:TEST_SOURCE});
  return {plan,report,journalPath,manifestPath};
}

// ---- a real Safe on the local chain
const ZERO="0x0000000000000000000000000000000000000000";
export const SAFE_ABI=["function setup(address[] owners,uint256 threshold,address to,bytes data,address fallbackHandler,address paymentToken,uint256 payment,address paymentReceiver)",
  "function nonce() view returns(uint256)","function enableModule(address module)",
  "function getTransactionHash(address to,uint256 value,bytes data,uint8 operation,uint256 safeTxGas,uint256 baseGas,uint256 gasPrice,address gasToken,address refundReceiver,uint256 nonce) view returns(bytes32)",
  "function execTransaction(address to,uint256 value,bytes data,uint8 operation,uint256 safeTxGas,uint256 baseGas,uint256 gasPrice,address gasToken,address refundReceiver,bytes signatures) payable returns(bool)",
  "event ExecutionSuccess(bytes32 indexed txHash,uint256 payment)","event ExecutionFailure(bytes32 indexed txHash,uint256 payment)"];
/** A Safe 1.5.0 on the local chain, as the Safe app creates one: the canonical SafeL2 singleton and SafeProxyFactory, their runtime code read
 * from Robinhood Chain (scripts/tests/fixtures/safe-1.5.0.json) and placed at their canonical addresses, and a SafeProxy created through
 * the factory's createProxyWithNonce with setup(owners, threshold). Its code is the canonical SafeProxy's, its singleton the canonical SafeL2. */
export async function localSafe(local:Pick<LocalChain,"provider"|"request">,o:{owners?:string[];threshold?:number;saltNonce?:bigint}={}):Promise<string>{
  const fixture=JSON.parse(await readFile(join(root,"scripts/tests/fixtures/safe-1.5.0.json"),"utf8"));
  for(const part of [fixture.singleton,fixture.factory])if(await local.provider.getCode(part.address)==="0x")await local.request("hardhat_setCode",[part.address,part.code]);
  const setup=new Interface(SAFE_ABI).encodeFunctionData("setup",[o.owners??SAFE_SIGNERS.map(signer=>signer.address),o.threshold??2,ZERO,"0x",ZERO,ZERO,0,ZERO]);
  const factory=new Contract(fixture.factory.address,["function createProxyWithNonce(address singleton,bytes initializer,uint256 saltNonce) returns(address)"],STRANGER.connect(local.provider));
  const safe=await factory.createProxyWithNonce.staticCall(fixture.singleton.address,setup,o.saltNonce??0n) as string;
  await (await factory.createProxyWithNonce(fixture.singleton.address,setup,o.saltNonce??0n)).wait();
  return getAddress(safe);
}
/** Execute a call from a local Safe with its signers' signatures, as the Safe app executes a Transaction Builder file's transaction: the
 * Safe transaction hash signed by each signer, the signatures in the order of their addresses, execTransaction sent by another account.
 * Returns the receipt; a call that reverts makes execTransaction revert. */
export async function execSafe(local:Pick<LocalChain,"provider">,safe:string,call:{to:string;data:string;value?:string},signers:Wallet[]=SAFE_SIGNERS):Promise<TransactionReceipt>{
  const contract=new Contract(safe,SAFE_ABI,STRANGER.connect(local.provider)),value=BigInt(call.value??"0");
  const hash=await contract.getTransactionHash(call.to,value,call.data,0,0,0,0,ZERO,ZERO,await contract.nonce()) as string;
  const signatures=concat([...signers].sort((a,b)=>BigInt(a.address)<BigInt(b.address)?-1:1).map(signer=>signer.signingKey.sign(hash).serialized));
  const receipt=await (await contract.execTransaction(call.to,value,call.data,0,0,0,0,ZERO,ZERO,signatures)).wait() as TransactionReceipt;
  if(!receipt.logs.some(log=>log.topics[0]===contract.interface.getEvent("ExecutionSuccess")!.topicHash))throw new Error("The Safe transaction did not succeed");
  return receipt;
}

/** A JSON-RPC endpoint on 127.0.0.1 that forwards to the local chain, for the command lines under test (--rpc-url). */
export async function rpcEndpoint(local:Pick<LocalChain,"request">){
  const server=createServer((incoming,response)=>{
    const chunks:Buffer[]=[];
    incoming.on("data",chunk=>chunks.push(chunk));
    incoming.on("end",async()=>{
      const body=JSON.parse(Buffer.concat(chunks).toString("utf8")),calls=Array.isArray(body)?body:[body];
      const answers=await Promise.all(calls.map(async(call:{id:unknown;method:string;params?:unknown[]})=>{
        try{return {jsonrpc:"2.0",id:call.id,result:await local.request(call.method,call.params??[])};}
        catch(error){
          const failure=error as {code?:unknown;message?:unknown;data?:unknown};
          const data=typeof failure.data==="string"?failure.data:(failure.data as {data?:unknown}|undefined)?.data;
          return {jsonrpc:"2.0",id:call.id,error:{code:typeof failure.code==="number"?failure.code:-32000,message:String(failure.message??error),...(data===undefined?{}:{data})}};
        }
      }));
      response.writeHead(200,{"content-type":"application/json"});
      response.end(JSON.stringify(Array.isArray(body)?answers:answers[0]));
    });
  });
  await new Promise<void>(done=>server.listen(0,"127.0.0.1",done));
  return {url:`http://127.0.0.1:${(server.address() as AddressInfo).port}`,close:()=>new Promise<void>(done=>{server.closeAllConnections();server.close(()=>done());})};
}
/** Run a script with node, without blocking this process (which serves its RPC), and collect what it prints. */
export function run(script:string,args:string[],preloads:string[]=[],env:Record<string,string>={}):Promise<{status:number|null;stdout:string;stderr:string}>{
  return new Promise((done,fail)=>{
    const child=spawn(process.execPath,[...preloads.flatMap(preload=>["--import",preload]),script,...args],{cwd:root,windowsHide:true,env:{...process.env,...env}});
    let stdout="",stderr="";
    child.stdout.on("data",chunk=>stdout+=chunk);child.stderr.on("data",chunk=>stderr+=chunk);
    child.on("error",fail);child.on("close",status=>done({status,stdout,stderr}));
  });
}
/** An operator directory for the command lines, with the local roles' public test keys: keeper account 1, VRF key account 2. */
export async function operatorDirectory(dir:string,owner:string,deployer=DEPLOYER.address){
  await mkdir(dir,{recursive:true});
  const keeperFile=join(dir,"keeper-tx.key"),vrfFile=join(dir,"vrf.key");
  await writeFile(keeperFile,KEEPER.privateKey+"\n");await writeFile(vrfFile,VRF.privateKey+"\n");
  const [x,y]=publicKeyOf(VRF);
  await writeFile(join(dir,"operator.json"),JSON.stringify({owner,feeRecipient:owner,deployer,keeper:{address:KEEPER.address,keyFile:keeperFile},vrf:{keyFile:vrfFile,publicKey:[String(x),String(y)]}},null,2));
  return dir;
}
/** An env file with a local account's public test key, as the command lines read DEPLOYER_ADDRESS and DEPLOYER_KEY. */
export async function envFile(path:string,wallet:Wallet){
  await writeFile(path,`DEPLOYER_ADDRESS=${wallet.address}\nDEPLOYER_KEY=${wallet.privateKey}\n`);
  return path;
}
