import {test} from "node:test";
import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";
import {loadChain} from "../lib/chains.ts";
import {Stop} from "../lib/deployment.ts";
import {ARBITRUM_ESTIMATE_MARGIN_BPS,TRANSFER_GAS,planSweep,requireSweepDestination,sweepGasLimit,type SweepNode,type SweepTransfer} from "../lib/sweep.ts";

const KEEPER="0x1111111111111111111111111111111111111111",SAFE="0xB57f656149749eff6b496dF090336491f977E744",EOA="0xcA35c280c5DF22958Adb74DD03911C8A0ec3eA03";
/** A node that answers from a table and records what it was asked; what it does not know it answers like an account without code. */
function fakeNode(answers:{code?:Record<string,string>;estimate?:bigint|Error}={}){
  const calls:Array<{method:"getCode"|"estimateGas";argument:string|SweepTransfer}>=[];
  const node:SweepNode={
    async getCode(address){calls.push({method:"getCode",argument:address});return answers.code?.[address]??"0x";},
    async estimateGas(transfer){
      calls.push({method:"estimateGas",argument:transfer});
      if(answers.estimate instanceof Error)throw answers.estimate;
      return answers.estimate??TRANSFER_GAS;
    },
  };
  return {node,calls};
}
const ceilDiv=(value:bigint,divisor:bigint)=>(value+divisor-1n)/divisor;

test("an EVM chain signs a sweep with a transfer's 21,000 gas and never asks the node, which is what Arc has always done",async()=>{
  for(const key of ["arc-testnet","arc-mainnet"]){
    const arc=await loadChain(key),{node,calls}=fakeNode({estimate:999_999n});
    assert.equal(await sweepGasLimit(node,arc,{from:KEEPER,to:SAFE,value:5n}),21000n);
    assert.equal(TRANSFER_GAS,21000n);
    // The sweep is the old arithmetic exactly: balance less the buffer less 21,000 × the fee, for balances above and below what it needs to leave.
    const gwei=10n**9n;
    for(const [balance,keep,maxFee] of [[10n**19n,3n*10n**18n,88n*gwei],[3n*10n**18n,3n*10n**18n,88n*gwei],[3n*10n**18n+21000n*88n*gwei,3n*10n**18n,88n*gwei],[10n**18n,3n*10n**18n,2000n*gwei],[0n,0n,gwei],[42n,0n,0n]]){
      const old=balance-keep-21000n*maxFee;
      assert.deepEqual(await planSweep(node,arc,{keeper:KEEPER,destination:SAFE,balance,keep,maxFee}),{gasLimit:21000n,amount:old},`${key} ${balance}/${keep}/${maxFee}`);
    }
    assert.deepEqual(calls,[],`${key} never asks the node`);
  }
});

test("an Arbitrum chain signs a sweep with the node's estimate plus 25%, rounded up and never below 21,000",async()=>{
  const robinhood=await loadChain("robinhood-testnet"),transfer={from:KEEPER,to:EOA,value:7n};
  assert.equal(ARBITRUM_ESTIMATE_MARGIN_BPS,2500n);
  for(const [estimate,limit] of [[100_000n,125_000n],[100_001n,125_002n],[100_003n,125_004n],[21_000n,26_250n],[1n,21_000n],[0n,21_000n],[20_000n,25_000n],[17_000n,21_250n]]){
    const {node,calls}=fakeNode({estimate});
    assert.equal(await sweepGasLimit(node,robinhood,transfer),limit,`estimate ${estimate}`);
    assert.deepEqual(calls,[{method:"estimateGas",argument:transfer}],"the node is asked once, about this transfer");
    if(estimate>=17_000n)assert.equal(limit,ceilDiv(estimate*5n,4n),`estimate ${estimate}: ceil(estimate × 1.25)`);
  }
  // A node that cannot estimate stops the sweep with a message of its own: nothing is signed, and what the node said is not repeated.
  const failing=fakeNode({estimate:new Error("execution reverted at https://rpc.example/v2/secret-key")});
  await assert.rejects(sweepGasLimit(failing.node,robinhood,transfer),(error:unknown)=>error instanceof Stop&&/could not estimate the gas of the sweep transaction; nothing was sent/.test(error.message)&&!error.message.includes("secret-key"));
});

test("an Arbitrum chain keeps back gas for the whole signed limit at the fee cap, and estimates the largest transfer the sweep could send",async()=>{
  const testnet=await loadChain("robinhood-testnet"),mainnet=await loadChain("robinhood-mainnet");
  // A keeper wallet with 0.03 ETH, a buffer of 0.005, twice the base fee of 0.0237 gwei as the fee, and a parent-chain share in the estimate.
  const balance=30_000_000_000_000_000n,keep=5_000_000_000_000_000n,maxFee=47_508_000n,estimate=187_654n;
  for(const chain of [testnet,mainnet]){
    const {node,calls}=fakeNode({estimate}),plan=await planSweep(node,chain,{keeper:KEEPER,destination:EOA,balance,keep,maxFee});
    const gasLimit=ceilDiv(estimate*5n,4n);
    assert.deepEqual(plan,{gasLimit,amount:balance-keep-gasLimit*maxFee},chain.key);
    assert.ok(gasLimit>TRANSFER_GAS&&gasLimit>estimate,"more than a plain transfer, and more than the estimate");
    // The wallet holds the buffer after the worst case: value plus gas at the cap is exactly what is above the buffer, so the node's balance check passes.
    assert.equal(plan.amount+plan.gasLimit*maxFee+keep,balance);
    assert.deepEqual(calls,[{method:"estimateGas",argument:{from:KEEPER,to:EOA,value:balance-keep}}],"estimated for the largest amount the sweep could send");
  }
  // Nothing above the buffer needs no estimate (a node refuses to estimate a transfer its sender cannot fund), and the plan is a shortfall.
  for(const low of [keep,keep-1n,1n,0n]){
    const {node,calls}=fakeNode({estimate}),plan=await planSweep(node,testnet,{keeper:KEEPER,destination:EOA,balance:low,keep,maxFee});
    assert.deepEqual(calls,[],`balance ${low}`);
    assert.ok(plan.amount<=0n,`balance ${low}`);
  }
  // A balance just above what the gas costs leaves a sweep of zero or less, which the script reports as nothing to sweep.
  const gasLimit=ceilDiv(estimate*5n,4n),thin=keep+gasLimit*maxFee;
  assert.equal((await planSweep(fakeNode({estimate}).node,testnet,{keeper:KEEPER,destination:EOA,balance:thin,keep,maxFee})).amount,0n);
  assert.equal((await planSweep(fakeNode({estimate}).node,testnet,{keeper:KEEPER,destination:EOA,balance:thin+1n,keep,maxFee})).amount,1n);
});

test("a sweep on a production chain stops when its destination has no code there; a testnet owner may be an account",async()=>{
  const arcMainnet=await loadChain("arc-mainnet"),arcTestnet=await loadChain("arc-testnet"),robinhoodTestnet=await loadChain("robinhood-testnet"),robinhoodMainnet=await loadChain("robinhood-mainnet");
  // The treasury Safe has code: the sweep goes on, after one read of exactly that address.
  const safe=fakeNode({code:{[SAFE]:"0x608060405260"}});
  await requireSweepDestination(safe.node,arcMainnet,SAFE);
  assert.deepEqual(safe.calls,[{method:"getCode",argument:SAFE}]);
  // The same address with no code on this chain, as Arc's Safe is on chain 4663, or a mistyped address, is refused and named.
  for(const chain of [arcMainnet,robinhoodMainnet]){
    const none=fakeNode();
    await assert.rejects(requireSweepDestination(none.node,chain,SAFE),(error:unknown)=>error instanceof Stop&&error.message.includes(SAFE)&&error.message.includes(chain.name)&&/no code/.test(error.message)&&/treasury Safe/.test(error.message),chain.key);
    assert.equal(none.calls.length,1);
  }
  // A testnet owner is an account (the design allows it), so no code is expected and the node is not asked.
  for(const chain of [arcTestnet,robinhoodTestnet]){
    const none=fakeNode();
    await requireSweepDestination(none.node,chain,EOA);
    assert.deepEqual(none.calls,[],chain.key);
  }
  // Code of any length counts; only the empty answer does not.
  await requireSweepDestination(fakeNode({code:{[SAFE]:"0x00"}}).node,arcMainnet,SAFE);
  await assert.rejects(requireSweepDestination(fakeNode({code:{[SAFE]:"0x"}}).node,arcMainnet,SAFE),Stop);
});

test("sweep-keeper.ts checks the destination before it reads the key, takes its gas from the plan, and signs no fixed 21,000",async()=>{
  const source=await readFile(new URL("../sweep-keeper.ts",import.meta.url),"utf8");
  const at=(text:string)=>{const index=source.indexOf(text);assert.ok(index>=0,`sweep-keeper.ts has ${text}`);return index;};
  assert.ok(at("requireSweepDestination(")>at("validateNetwork(provider,chain)"),"on the connected chain");
  assert.ok(at("requireSweepDestination(")<at("readFile(operator.keeper.keyFile"),"before the key is read");
  assert.ok(at("planSweep(")>at("currentFee("),"planned at the fee the transaction will pay");
  assert.doesNotMatch(source,/21000n?\b/,"no fixed gas limit or reserve is left in the script");
  assert.match(source,/sendTransaction\(\{to:destination,value:amount,gasLimit,maxFeePerGas:maxFee/);
  assert.match(source,/requireOperable\(chain,"sweep-keeper\.ts"\)/);
});
