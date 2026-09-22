// Batch-abort drill. Reproduces the batch-abort issue end to end on a local chain with the hostile
// consumer (contracts/test/BatchAbortConsumers.sol) and real secp256k1 VRF proofs, on the coordinator implementation
// live on Arc (the de5f82e runtime fixture, at its live address) and on this revision's, and shows what the keeper's
// gas sizing and its resend path do on each.
//
//   npx hardhat run scripts/batch-abort-drill.ts
//
// A raffle entrant opens, in one transaction, its own draw and two helper requests with 1,000,000-gas callbacks. The
// three share a block, a deadline and consecutive ids, so a keeper batches them as [draw, helper, helper]. A helper
// burns its whole callback budget on chain but stays cheap in eth_estimateGas, because Arc (and the local EDR chain,
// verified at startup) simulates with tx.gasprice == 0. This mirrors the keeper: it prices the batch from
// eth_estimateGas run unpriced, then sends it with one of two gas limits.
//
//   old sizing  = estimate * 1.2 + 50000            (the sizing that lets a later member's revert abort the batch)
//   new sizing  = max(estimate + max(50000, (estimate - guard) / 5), floor)                   (worker::fulfillment_gas)
//
// where guard = 140000 + Sum(callbackGasLimit + callbackGasLimit/63 + 400000) is the budget the guarded coordinator
// requires before serving any member, and floor = (guard + 73112 + 27168 per member) * 64/63 adds a bound on the
// transaction's own cost and the 64th of the gas the coordinator's proxy keeps back from the implementation.
//
// Each sizing is tried on a losing round (a winning draw disarms the helpers) in three settings:
//
//   live             estimated and mined on the live implementation;
//   guarded          estimated and mined on this revision's implementation, whose batch first budgets every member it
//                    will serve (140000 + Sum(limit + limit/63 + 400000)) and otherwise reverts before serving any;
//   across upgrade   estimated on the live implementation and mined after the proxy moved to this revision's, as a batch
//                    in flight at the upgrade.
//
// When a batch reverts, its members are resent one at a time with the keeper's single sizing, as the keeper's resend
// path does. For every case the drill reports whether the batch landed, why and where it reverted (after a callback
// ran, or at the guard before any member was served), and whether the losing draw ends fulfilled and kept. Expected:
//
//   live            old sizing aborts the batch after the draw's result was seen; the resend keeps the losing draw.
//                   New sizing lands the batch with the losing draw in it.
//   guarded         both sizings land: the guard makes the estimate itself cover every member's budget.
//   across upgrade  old sizing reverts at the guard before any member is served, and the resend keeps the losing draw.
//                   New sizing lands the batch.
import {readFile} from "node:fs/promises";
import {network} from "hardhat";
import assert from "node:assert/strict";
import {publicKey, makeProof} from "../test/helpers/proof.ts";
import {EPOCH_TEST_SIGNERS, epochAttestation} from "../test/helpers/epoch.ts";
import {deployProxy} from "../test/helpers/proxy.ts";

const oldSizing=(estimate:bigint)=>estimate*12n/10n+50_000n;
// The guarded coordinator's budget for these members, and the keeper's floor around it (worker::fulfillment_floor).
const guardBudget=(limits:bigint[])=>limits.reduce((need,limit)=>need+limit+limit/63n+400_000n,140_000n);
const floorOf=(limits:bigint[])=>{const needed=guardBudget(limits)+73_112n+27_168n*BigInt(limits.length);return needed+(needed+62n)/63n;};
const newSizing=(estimate:bigint,limits:bigint[])=>{
  const excess=estimate>guardBudget(limits)?estimate-guardBudget(limits):0n;
  const padded=estimate+(excess/5n>50_000n?excess/5n:50_000n);
  const floor=floorOf(limits);
  return padded>floor?padded:floor;
};
const LIMITS=[100_000n,1_000_000n,1_000_000n];
// A reverted batch that used less than this never reached the first member's callback: it stopped at the guard.
const GUARD_REVERT_GAS=200_000n;

const {ethers,networkHelpers,provider}=await network.create({network:"loadSim"});
const [owner,...entrants]=await ethers.getSigners();
const live=JSON.parse(await readFile(new URL("../test/fixtures/coordinator-deployed-de5f82e.json",import.meta.url),"utf8"));
// The committer publishes epochs; here the owner publishes them directly, so the drill needs no daemon.
const registry=await deployProxy(ethers,"EpochEntropy",[EPOCH_TEST_SIGNERS,owner.address,owner.address]);
// The live runtime only at its live address, where its UUPS self address is valid.
await provider.request({method:"hardhat_setCode",params:[live.implementation,live.deployedBytecode]});
assert.equal(ethers.keccak256(await ethers.provider.getCode(live.implementation)),live.runtimeCodeHash);
const guardedImplementation=await ethers.deployContract("D20VRFCoordinator");
const coordinatorInterface=(await ethers.getContractFactory("D20VRFCoordinator")).interface;
const insufficientGas=coordinatorInterface.getError("InsufficientCallbackGas")!.selector;

/// A coordinator proxy on the live or this revision's implementation, with mainnet-shaped pricing (multiplier x base
/// fee x (overhead + callback)), so a 1,000,000-gas helper escrows a fee that covers a full-budget send, as on Arc.
async function coordinator(on:"live"|"guarded"){
  const init=coordinatorInterface.encodeFunctionData("initialize",[publicKey(),owner.address,owner.address,10n**16n,1,await registry.getAddress(),5000]);
  const proxy=await ethers.deployContract("D20Proxy",[on==="live"?live.implementation:await guardedImplementation.getAddress(),init]);
  const rng:any=await ethers.getContractAt("D20VRFCoordinator",await proxy.getAddress());
  await rng.setPricing(2n*10n**16n,3,300_000);
  return rng;
}
async function ensureEpoch():Promise<void>{
  const first=Number(await registry.firstEpochStart());
  if(await ethers.provider.getBlockNumber()<first)await networkHelpers.mine(first-await ethers.provider.getBlockNumber());
  const epoch=await registry.epochForBlock(await ethers.provider.getBlockNumber());
  if((await registry.getEpoch(epoch)).epochHash===ethers.ZeroHash){
    await registry.commitEpoch(epoch,await epochAttestation(registry,epoch,ethers));
  }
}
async function estimateUnpriced(rng:any,data:string):Promise<bigint>{
  // tx.gasprice == 0 in the simulation, the property the attack relies on.
  return BigInt(await provider.request({method:"eth_estimateGas",params:[{from:owner.address,to:await rng.getAddress(),data,gasPrice:"0x0"}]}));
}
async function proofOf(rng:any,id:bigint){return makeProof(await rng.requestSeed(id));}
// One raffle round: a fresh raffle and attacker, seven honest entrants and the attacker (a 1/8 win chance for the
// attacker), a published epoch, and the drawAndArm transaction. Returns the three request ids in batch order.
async function armRound(rng:any,salt:string){
  const coordinatorAddress=await rng.getAddress();
  const raffle:any=await ethers.deployContract("BatchAbortRaffle",[coordinatorAddress]);
  const attacker:any=await ethers.deployContract("BatchAbortAttacker",[coordinatorAddress,await raffle.getAddress()]);
  await owner.sendTransaction({to:await attacker.getAddress(),value:ethers.parseEther("100")});
  for(let i=0;i<7;i++)await raffle.connect(entrants[i]).enter();
  await attacker.enter();
  await ensureEpoch();
  await networkHelpers.mine(1);
  // drawAndArm opens the draw and two helper requests with consecutive ids in one transaction.
  await (await attacker.drawAndArm(ethers.id(salt))).wait();
  const draw=await raffle.drawId();
  await networkHelpers.mine(2); // past the target block (confirmationBlocks = 1)
  return {ids:[draw,draw+1n,draw+2n],attacker,raffle};
}
/// Send and mine (automine: the transaction is alone in the latest block); a revert is reported with its reason and
/// its gas used, never thrown.
async function send(rng:any,data:string,gasLimit:bigint){
  const to=await rng.getAddress();
  try {await provider.request({method:"eth_sendTransaction",params:[{from:owner.address,to,data,gas:ethers.toQuantity(gasLimit)}]});}
  catch {/* A reverted transaction is still mined; its receipt says so. */}
  const block=await ethers.provider.getBlock("latest",true);
  const tx=block!.prefetchedTransactions.at(-1)!;
  assert.equal(tx.to?.toLowerCase(),to.toLowerCase());assert.equal(tx.data,data);
  const receipt=(await ethers.provider.getTransactionReceipt(tx.hash))!;
  if(receipt.status===1)return {ok:true,gasUsed:receipt.gasUsed,reason:""};
  // Replayed priced, as mined: the helpers burn only when tx.gasprice != 0.
  let reason="none on replay";
  try {await provider.request({method:"eth_call",params:[{from:owner.address,to,data,gas:ethers.toQuantity(gasLimit),
    gasPrice:ethers.toQuantity(tx.maxFeePerGas??tx.gasPrice)},ethers.toQuantity(receipt.blockNumber-1)]});}
  catch(error:any){const text=`${error?.data??""} ${error?.message??error}`;reason=text.includes(insufficientGas.slice(2))||text.includes("InsufficientCallbackGas")?"InsufficientCallbackGas":text.slice(0,120);}
  return {ok:false,gasUsed:receipt.gasUsed,reason};
}
/// The keeper's resend path: each member alone, in order, with the single sizing.
async function resendSingly(rng:any,ids:bigint[]){
  const out=[];
  for(const [n,id] of ids.entries()){
    if((await rng.getRequest(id)).fulfilled)continue;
    const data=rng.interface.encodeFunctionData("fulfillRandomness",[id,await proofOf(rng,id)]);
    const gas=newSizing(await estimateUnpriced(rng,data),[LIMITS[n]]);
    const result=await send(rng,data,gas);
    assert.equal(result.ok,true,`single resend of ${id} failed: ${result.reason}`);
    out.push({id:String(id),gasLimit:String(gas),gasUsed:String(result.gasUsed)});
  }
  return out;
}

type Setting="live"|"guarded"|"across upgrade";
async function losingCase(setting:Setting,sizing:"old"|"new"){
  const rng=await coordinator(setting==="guarded"?"guarded":"live");
  for(let round=0;round<40;round++){
    const snapshot=await provider.request({method:"evm_snapshot",params:[]});
    try {
      const {ids,attacker,raffle}=await armRound(rng,`${setting}-${sizing}-${round}`);
      const data=rng.interface.encodeFunctionData("fulfillRandomnessBatch",[ids,await Promise.all(ids.map(id=>proofOf(rng,id)))]);
      const estimate=await estimateUnpriced(rng,data);
      const gasLimit=sizing==="old"?oldSizing(estimate):newSizing(estimate,LIMITS);
      if(setting==="across upgrade")await (await rng.connect(owner).upgradeToAndCall(await guardedImplementation.getAddress(),"0x")).wait();
      const guard=guardBudget(LIMITS);
      const batch=await send(rng,data,gasLimit);
      const drawAfterBatch=(await rng.getRequest(ids[0])).fulfilled as boolean;
      const resent=batch.ok?[]:await resendSingly(rng,ids);
      if(!await raffle.drawn())throw new Error(`${setting}/${sizing}: the draw was never served`);
      if(await raffle.winner()===await attacker.getAddress())continue; // A win disarms the helpers; resample.
      const requests=await Promise.all(ids.map(id=>rng.getRequest(id)));
      return {setting,sizing,round,estimate:String(estimate),gasLimit:String(gasLimit),guardNeed:setting==="live"?undefined:String(guard),
        batch:{landed:batch.ok,gasUsed:String(batch.gasUsed),revert:batch.ok?undefined:batch.reason,
          revertedAt:batch.ok?undefined:batch.gasUsed<GUARD_REVERT_GAS?"the guard, before any member was served":"a member's gas check, after earlier callbacks ran",
          drawFulfilledByBatch:drawAfterBatch},
        resentSingly:resent.length,drawFulfilled:requests[0].fulfilled as boolean,drawRefunded:requests[0].refunded as boolean,
        helpersFulfilled:requests.slice(1).every(r=>r.fulfilled),attackerWon:false};
    } finally {await provider.request({method:"evm_revert",params:[snapshot]});}
  }
  throw new Error(`${setting}/${sizing}: no losing round in 40`);
}

await networkHelpers.mine(1);
// Confirm the local chain simulates estimateGas with tx.gasprice == 0, the assumption the attack needs.
{
  const probe:any=await ethers.deployContract("BatchAbortAttacker",[owner.address,owner.address]);
  const data=probe.interface.encodeFunctionData("requireUnpricedSimulation");
  await provider.request({method:"eth_estimateGas",params:[{from:owner.address,to:await probe.getAddress(),data,gasPrice:"0x0"}]});
}
const cases:Awaited<ReturnType<typeof losingCase>>[]=[];
for(const setting of ["live","guarded","across upgrade"] as Setting[])
  for(const sizing of ["old","new"] as const)cases.push(await losingCase(setting,sizing));
const of=(setting:Setting,sizing:string)=>cases.find(c=>c.setting===setting&&c.sizing===sizing)!;
// The batch-abort issue on the live implementation, and each defense.
assert.equal(of("live","old").batch.landed,false,"old sizing served a losing draw on the live implementation");
assert.equal(of("live","old").batch.revertedAt,"a member's gas check, after earlier callbacks ran");
assert.equal(of("live","new").batch.landed,true,"new sizing must land the batch on the live implementation");
for(const sizing of ["old","new"])assert.equal(of("guarded",sizing).batch.landed,true,`${sizing} sizing must land on the guarded implementation`);
assert.equal(of("across upgrade","old").batch.landed,false);
assert.equal(of("across upgrade","old").batch.revertedAt,"the guard, before any member was served");
assert.equal(of("across upgrade","new").batch.landed,true);
for(const c of cases){
  assert.equal(c.drawFulfilled,true,`${c.setting}/${c.sizing}: the losing draw was not kept`);
  assert.equal(c.drawRefunded,false);assert.equal(c.helpersFulfilled,true);
}
console.log(JSON.stringify({batchAbortDrill:{chainId:31337,zeroGasPriceSimulation:true,callbackLimits:LIMITS.map(String),cases}},null,2));
