// Move keeper wallet surplus to the chain owner (the DAO treasury Safe on mainnet), keeping a gas buffer.
// Stop the keeper first: a transaction from its wallet while it runs can take a nonce the keeper journaled.
// Nothing is sent without --apply, and mainnet needs --mainnet.
import {parseArgs} from "node:util";
import {readFile} from "node:fs/promises";
import {resolve,join} from "node:path";
import {JsonRpcProvider,Wallet,formatUnits,parseUnits} from "ethers";
import {loadChain} from "./lib/chains.ts";
import {validateNetwork,Stop} from "./lib/deployment.ts";
import {currentFee} from "./lib/gas.ts";

async function main(){
  const {values}=parseArgs({options:{chain:{type:"string",default:"arc-testnet"},"operator-directory":{type:"string"},keep:{type:"string",default:"3"},
    apply:{type:"boolean",default:false},mainnet:{type:"boolean",default:false},"keeper-stopped":{type:"boolean",default:false}}});
  const chain=await loadChain(values.chain);
  if(!chain.testnet&&!values.mainnet)throw new Stop("Mainnet sweeps require --mainnet");
  if(!values["operator-directory"])throw new Stop("Provide --operator-directory with the chain's operator.json and keeper key");
  const operator=JSON.parse(await readFile(join(resolve(values["operator-directory"]),"operator.json"),"utf8"));
  // Sweeps go only to the chain profile owner (the treasury Safe on mainnet); there is no destination option.
  const destination=chain.owner;
  const keep=parseUnits(values.keep,chain.nativeCurrency.decimals);
  const provider=new JsonRpcProvider(chain.rpcUrls[0],undefined,{batchMaxCount:1});
  try {
    await validateNetwork(provider,chain);
    const key=(await readFile(operator.keeper.keyFile,"utf8")).trim();
    const keeper=new Wallet(key,provider);
    if(keeper.address!==operator.keeper.address)throw new Stop("Keeper key does not match operator.json");
    const [balance,latest,pending]=await Promise.all([provider.getBalance(keeper.address),keeper.getNonce("latest"),keeper.getNonce("pending")]);
    if(latest!==pending)throw new Stop("The keeper wallet has a pending transaction; wait until it settles");
    const {tip,maxFee}=await currentFee(provider,BigInt(chain.gas.maxFeePerGasWei));
    const gasReserve=21000n*maxFee,amount=balance-keep-gasReserve;
    const plan={chain:chain.key,keeper:keeper.address,to:destination,balance:formatUnits(balance,18),keep:formatUnits(keep,18),
      amount:amount>0n?formatUnits(amount,18):"0",currency:chain.nativeCurrency.symbol,apply:values.apply};
    console.log(JSON.stringify(plan,null,2));
    if(amount<=0n)throw new Stop("Nothing to sweep above the buffer");
    if(!values.apply)return;
    if(!values["keeper-stopped"])throw new Stop("Stop the keeper on its server first, then rerun with --keeper-stopped");
    const tx=await keeper.sendTransaction({to:destination,value:amount,gasLimit:21000n,maxFeePerGas:maxFee,maxPriorityFeePerGas:tip,nonce:latest});
    const receipt=await tx.wait(1,90000);
    if(receipt?.status!==1)throw new Stop("Sweep transaction failed");
    console.log(JSON.stringify({swept:formatUnits(amount,18),transaction:`${chain.explorerUrl}/tx/${tx.hash}`,keeperBalance:formatUnits(await provider.getBalance(keeper.address),18)},null,2));
  } finally {provider.destroy();}
}
// A command-line parse error quotes only the arguments; other failures can carry key-file text and are not printed.
main().catch(error=>{console.error(error instanceof Stop||/^ERR_PARSE_ARGS_/.test(error?.code)?`Sweep stopped: ${error.message}`:"Sweep stopped. Credentials were not logged; check the keeper wallet on the explorer before retrying.");process.exitCode=1;});
