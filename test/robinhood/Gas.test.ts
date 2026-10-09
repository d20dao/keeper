// The round coordinator's gas table (design §2.11, §2.13 item 10; review H2, M1, M2): what each fulfilment shape uses and the gas limit it
// needs, with callbacks that burn their whole limit, on EDR (Osaka schedule) with MockArbSys. From it: the bounds of scripts/lib/round-gas.ts
// that the keeper's fee gate prices with, the no-loss pricing rule recomputed from the measured gas, and the largest batch a per-transaction
// gas cap admits. Print the table with ROUND_GAS_TABLE=1 (PowerShell: $env:ROUND_GAS_TABLE=1):
//   npx hardhat test test/robinhood/Gas.test.ts
import {readFileSync} from "node:fs";
import {expect} from "chai";
import {network} from "hardhat";
import {ROUND_GAS,ROUND_L1_GAS,ROUND_PRICED_VRF_CANDIDATES,ROUND_ROBINHOOD_ROUND_EXCESS,roundFulfilmentGasBound,roundFulfilmentGasLimit,
  roundFulfilmentL1Gas,roundMaxBatch,vrfHashToCurveCandidates,type RoundFulfilmentShape} from "../../scripts/lib/round-gas.ts";
import {makeProof,publicKey} from "../helpers/proof.ts";
import {EVMNET,ROUND_LEAD,assignedRound,deployRoundCoordinator,nextFee,outcome,randomnessOf,request,roundSeed,roundTime,signRound,
  type RoundFixture} from "../helpers/robinhood.ts";

// ---- The pricing under test. FULFILL_GAS_OVERHEAD is the one value to set: the overhead the deployments initialize with.
// The deployments do (config/service.robinhood-*.json); ROUND_PRICING in test/helpers/robinhood.ts still says 360,000, which fails this rule
// (see the last test).
const FULFILL_GAS_OVERHEAD=405_000n;
const MIN_FEE=25_000_000_000_000n,FEE_MULTIPLIER=2,KEEPER_BPS=8000n;
/// The keeper's share must exceed its modelled worst cost by this much, in basis points of the cost.
const MARGIN_BPS=2000n;

const CALLBACKS=[30_000,50_000,100_000,1_000_000] as const;
const BATCH_SIZES=[2,4,8,16] as const;
/// EIP-7825's per-transaction gas cap, which EDR's default hardfork enforces, and Robinhood testnet's maxTxGasLimit (ArbGasInfo).
const TX_GAS_CAP=16_777_216n,ROBINHOOD_TESTNET_TX_GAS_LIMIT=32_000_000n;
const CALLBACK_RESERVE=140_000n,MEMBER_OVERHEAD=400_000n,ROUND_BUDGET=400_000n+400_000n/63n+5_000n;
const COSTLIEST=JSON.parse(readFileSync(new URL("../fixtures/robinhood/drand-evmnet-costliest-rounds.json",import.meta.url),"utf8")).rounds
  .map((r:any)=>({round:BigInt(r.round),signature:"0x"+r.signature})) as Array<{round:bigint;signature:string}>;
const GWEI=1_000_000_000n;

type Shape={name:string;batch:boolean;callback:number;members:number;rounds:number;cached:boolean;withdrawn:boolean};
/// A measured fulfilment: its gas used, the smallest gas limit it went through at (none when the cap was too small), the gas of its
/// calldata, the gas of D20BeaconVerifier.roundMessage of each round it verified, and each proof's VRF hash-to-curve candidates.
type Measured=Shape&{gasUsed:bigint;limit?:bigint;calldata:bigint;roundMessageGas:bigint[];candidates:number[];largestAtCap?:number};

const calldataGas=(data:string)=>BigInt(Buffer.from(data.slice(2),"hex").reduce((gas,byte)=>gas+(byte===0?4:16),0));
/// What a fulfilment's first guard asks for inside the implementation.
const guardNeed=(callbacks:number[],rounds:number)=>CALLBACK_RESERVE+callbacks.reduce((sum,cb)=>sum+BigInt(cb)+BigInt(cb)/63n+MEMBER_OVERHEAD,0n)+BigInt(rounds)*ROUND_BUDGET;
/// The smallest gas limit, between low (which fails) and high, at which `at` goes through; undefined when even the cap does not.
async function smallestLimit(at:(gasLimit:bigint)=>Promise<string>,low:bigint,high:bigint){
  if(high>TX_GAS_CAP)high=TX_GAS_CAP;
  if(!(await at(high)).startsWith("returns"))return undefined;
  expect(await at(low),`gas limit ${low}`).to.not.match(/^returns/);
  while(high-low>1n){const middle=(low+high)/2n;if((await at(middle)).startsWith("returns"))high=middle;else low=middle;}
  return high;
}
const shapeOf=(row:Measured):RoundFulfilmentShape=>({batch:row.batch,callbackGasLimits:Array(row.members).fill(row.callback),
  roundsToVerify:row.cached?0:row.rounds,vrfCandidates:row.candidates,feesWithdrawn:row.withdrawn});

/// The measuring tools of one chain: a consumer whose callbacks burn their whole limit, and the shapes measured so far.
async function measuring(ethers:any,networkHelpers:any,c:RoundFixture){
  const burner=await ethers.deployContract("BurningRoundConsumer",[await c.coordinator.getAddress()]);
  const out:Measured[]=[];
  const keeper=c.coordinator.connect(c.keeper);
  const now=async()=>BigInt(await networkHelpers.time.latest());
  /// n requests of the burner in one block at a timestamp: their ids.
  async function requestsAt(n:number,callback:number,at:bigint){
    await networkHelpers.time.setNextBlockTimestamp(at);
    const fee=await nextFee(c,networkHelpers,callback);
    await (await burner.requestMany(n,callback,c.owner.address,{value:fee*BigInt(n)})).wait();
    const last=await burner.lastRequestId() as bigint;
    return Array.from({length:n},(_,i)=>last-BigInt(n-1-i));
  }
  /// The first round of beacon 0 scheduled at least `ahead` seconds from now, plus one.
  const roundAhead=async(ahead=10n)=>assignedRound(c.beacon,await now()+ahead)+1n;
  const proofFor=async(id:bigint,signature:string)=>makeProof(await roundSeed(c,ethers,id,randomnessOf(signature)));
  const roundMessageGas=async(round:bigint)=>BigInt(await c.beaconVerifier.roundMessage.estimateGas(round));
  async function single(shape:Shape,id:bigint,signature:string,round:bigint){
    const proof=await proofFor(id,signature);
    const at=(gasLimit:bigint)=>outcome(c.coordinator,()=>keeper.fulfillRandomness.staticCall(id,proof,signature,{gasLimit}));
    // A round still to verify is guarded before anything runs; a cached round only before the callback, after the proof's check.
    const need=guardNeed([shape.callback],shape.cached?0:1)*64n/63n;
    const limit=await smallestLimit(at,shape.cached?100_000n:need,need+300_000n);
    const tx=await keeper.fulfillRandomness(id,proof,signature,{gasLimit:limit});
    const receipt=await tx.wait();
    out.push({...shape,gasUsed:receipt.gasUsed,limit,calldata:calldataGas(tx.data),roundMessageGas:shape.cached?[]:[await roundMessageGas(round)],
      candidates:[vrfHashToCurveCandidates(publicKey(),proof.seed)]});
  }
  /// A batch of every request of each round listed, its rounds verified in it. When even the cap is too small, the largest batch of them,
  /// over every listed round, that goes through at the cap.
  async function batch(shape:Shape,groups:Array<{round:bigint;signature:string;ids:bigint[]}>){
    const proofs=await Promise.all(groups.map(async g=>Promise.all(g.ids.map(id=>proofFor(id,g.signature)))));
    const listed=groups.map(g=>[0,g.round,g.signature]);
    /// The first `count` members, taken from the rounds in turn.
    const take=(count:number)=>{
      const ids:bigint[]=[],chosen:any[]=[];
      for(let i=0;ids.length<count;i++){const g=i%groups.length,k=Math.floor(i/groups.length);ids.push(groups[g].ids[k]);chosen.push(proofs[g][k]);}
      return {ids,chosen};
    };
    const all=take(shape.members);
    const at=(gasLimit:bigint,members=all)=>outcome(c.coordinator,()=>keeper.fulfillRandomnessBatch.staticCall(listed,members.ids,members.chosen,{gasLimit}));
    const need=guardNeed(all.ids.map(()=>shape.callback),groups.length)*64n/63n;
    const limit=await smallestLimit(at,need,need+300_000n+40_000n*BigInt(shape.members));
    const data=c.coordinator.interface.encodeFunctionData("fulfillRandomnessBatch",[listed,all.ids,all.chosen]);
    const candidates=all.chosen.map((p:any)=>vrfHashToCurveCandidates(publicKey(),p.seed));
    if(limit===undefined){
      let largestAtCap=shape.members-1;
      while(largestAtCap>groups.length&&!(await at(TX_GAS_CAP,take(largestAtCap))).startsWith("returns"))largestAtCap--;
      out.push({...shape,gasUsed:0n,calldata:calldataGas(data),roundMessageGas:[],candidates,largestAtCap});
      return;
    }
    const receipt=await (await keeper.fulfillRandomnessBatch(listed,all.ids,all.chosen,{gasLimit:limit})).wait();
    out.push({...shape,gasUsed:receipt.gasUsed,limit,calldata:calldataGas(data),roundMessageGas:await Promise.all(groups.map(g=>roundMessageGas(g.round))),candidates});
  }
  const withdraw=()=>c.coordinator.connect(c.feeRecipient).withdrawFees(c.feeRecipient.address);
  return {out,now,requestsAt,roundAhead,single,batch,withdraw,roundMessageGas};
}

const plain={members:1,rounds:1,cached:false,withdrawn:false};
/// Every shape on the test beacon, whose secret the tests know, so that any round can be signed.
async function testBeaconTable(){
  const {ethers,networkHelpers}=await network.create();
  const c=await deployRoundCoordinator(ethers,networkHelpers);
  const m=await measuring(ethers,networkHelpers,c);
  // Requests through the RoundConsumer at the exact fee, after the deployment's first request.
  await request(c,networkHelpers);
  const raw=await request(c,networkHelpers);
  const fee=await nextFee(c,networkHelpers,100_000);
  const mapped=await (await c.consumer.requestMapped(ethers.id("mapped"),100_000,c.owner.address,[1,0,20,1,0],{value:fee})).wait();
  // The deployment's first fulfilment sets counters from zero; it is not a shape the keeper meets again.
  const warmRound=await m.roundAhead(),[warm]=await m.requestsAt(1,30_000,roundTime(c.beacon,warmRound)-ROUND_LEAD);
  await m.single({...plain,name:"first fulfilment of a deployment",batch:false,callback:30_000},warm,signRound(warmRound),warmRound);
  const first=m.out.pop()!;
  for(const callback of CALLBACKS){
    const round=await m.roundAhead(),[a,b]=await m.requestsAt(2,callback,roundTime(c.beacon,round)-ROUND_LEAD),signature=signRound(round);
    await m.single({...plain,name:"single, first in its round",batch:false,callback},a,signature,round);
    await m.single({...plain,name:"single, round cached",batch:false,callback,cached:true},b,signature,round);
    await m.withdraw();
    const next=await m.roundAhead(),[w]=await m.requestsAt(1,callback,roundTime(c.beacon,next)-ROUND_LEAD);
    await m.single({...plain,name:"single, first in its round, fees just withdrawn",batch:false,callback,withdrawn:true},w,signRound(next),next);
  }
  const batchOf=async(members:number,rounds:number,callback:number)=>{
    const firstRound=await m.roundAhead(),groups=[];
    for(let k=0n;k<BigInt(rounds);k++){
      const round=firstRound+k;
      groups.push({round,signature:signRound(round),ids:await m.requestsAt(members/rounds,callback,roundTime(c.beacon,round)-ROUND_LEAD)});
    }
    await m.batch({...plain,name:`batch of ${members} over ${rounds} round${rounds>1?"s":""}`,batch:true,callback,members,rounds},groups);
  };
  for(const members of BATCH_SIZES)for(const rounds of [1,2])for(const callback of CALLBACKS)await batchOf(members,rounds,callback);
  // Every member in a round of its own, the most rounds a batch can list.
  for(const members of [8,16])await batchOf(members,members,30_000);
  return {table:m.out,first,requestRaw:raw.receipt.gasUsed as bigint,requestMapped:mapped.gasUsed as bigint};
}
/// The costliest real evmnet rounds, served alone, on a chain whose clock starts ten minutes before the first of them.
async function costliestTable(){
  const start=new Date(Number(roundTime(EVMNET,COSTLIEST[0].round)-600n)*1000);
  const {ethers,networkHelpers}=await network.create({override:{initialDate:start}});
  const c=await deployRoundCoordinator(ethers,networkHelpers,{evmnet:true});
  const m=await measuring(ethers,networkHelpers,c);
  // Warm the deployment with a real fixture round before the costliest ones.
  const soonest=assignedRound(EVMNET,await m.now()+5n),warmRound=EVMNET.rounds.find(r=>r.round>soonest&&r.round<COSTLIEST[0].round)!;
  const [warm]=await m.requestsAt(1,30_000,roundTime(EVMNET,warmRound.round)-ROUND_LEAD);
  await m.single({...plain,name:"warm-up",batch:false,callback:30_000},warm,warmRound.signature,warmRound.round);
  m.out.length=0;
  const rounds=[...COSTLIEST];
  for(const callback of CALLBACKS)for(const withdrawn of [false,true]){
    const r=rounds.shift()!;
    if(withdrawn)await m.withdraw();
    const [id]=await m.requestsAt(1,callback,roundTime(EVMNET,r.round)-ROUND_LEAD);
    await m.single({...plain,name:`single, first in the costliest round${withdrawn?", fees just withdrawn":""}`,batch:false,callback,withdrawn},id,r.signature,r.round);
  }
  // The costliest path is the costliest: no round of a span of real rounds hashes to the curve for more.
  const span=await Promise.all(Array.from({length:300},(_,i)=>m.roundMessageGas(21_057_000n+BigInt(i))));
  return {table:m.out,span,costliest:await Promise.all(COSTLIEST.map(r=>m.roundMessageGas(r.round))),cheapest:span.reduce((a,b)=>a<b?a:b),c};
}

/// The fee of a request, as the coordinator quotes it under the pricing under test.
const feeAt=(callback:number,baseFee:bigint)=>{
  const dynamic=BigInt(FEE_MULTIPLIER)*baseFee*(FULFILL_GAS_OVERHEAD+BigInt(callback));
  return dynamic>MIN_FEE?dynamic:MIN_FEE;
};
const keeperShare=(fee:bigint)=>fee/10_000n*KEEPER_BPS+fee%10_000n*KEEPER_BPS/10_000n;
/// The shapes the pricing rule covers, each in the worst state it can meet: alone in its round or every listed round still to verify with the
/// costliest hash-to-curve, ROUND_PRICED_VRF_CANDIDATES candidates a proof, fees just withdrawn, and the L1 budget.
const PRICED=CALLBACKS.flatMap(callback=>[{members:1,rounds:1},...BATCH_SIZES.flatMap(members=>[1,2].map(rounds=>({members,rounds}))),{members:16,rounds:16}]
  .map(({members,rounds})=>({callback,members,shape:{batch:members>1,callbackGasLimits:Array(members).fill(callback),roundsToVerify:rounds} as RoundFulfilmentShape})));
/// The keeper's worst cost of a shape in gas: its Robinhood bound and its L1 budget.
const worstGas=(shape:RoundFulfilmentShape)=>roundFulfilmentGasBound(shape)+roundFulfilmentL1Gas(shape);
/// The smallest overhead at which a shape keeps the margin where the dynamic fee binds: members × share × 2 × (O + cb) ≥ (1 + margin) × gas.
function smallestOverhead(members:number,callback:number,gas:bigint){
  const need=(10_000n+MARGIN_BPS)*gas*10_000n,per=BigInt(members)*KEEPER_BPS*BigInt(FEE_MULTIPLIER)*10_000n;
  return (need+per-1n)/per-BigInt(callback);
}

describe("Round coordinator: gas table, fee gate bounds and pricing",function(){
  let t:Awaited<ReturnType<typeof testBeaconTable>>,w:Awaited<ReturnType<typeof costliestTable>>,rows:Measured[];
  before(async function(){
    this.timeout(900_000);
    t=await testBeaconTable();
    w=await costliestTable();
    rows=[...t.table,...w.table];
    if(process.env.ROUND_GAS_TABLE==="1"){
      console.log(`request raw ${t.requestRaw}, mapped d20 ${t.requestMapped}; first fulfilment of a deployment ${t.first.gasUsed}; `+
        `roundMessage cheapest ${w.cheapest}, costliest ${w.costliest[0]}; first fulfilment's VRF candidates ${t.first.candidates}, roundMessage ${t.first.roundMessageGas}`);
      console.log("shape | callback | gas used | used less callbacks | smallest gas limit | model limit | calldata | roundMessage | VRF candidates");
      for(const row of rows)console.log([row.name,row.callback,row.gasUsed||"-",row.gasUsed?row.gasUsed-BigInt(row.callback*row.members):"-",
        row.limit??`over the cap; ${row.largestAtCap} fit`,roundFulfilmentGasLimit(shapeOf(row)),row.calldata,row.roundMessageGas.join("+"),row.candidates.join(",")].join(" | "));
    }
  });

  it("measures every shape, the costliest real round among them, and the deployment's first fulfilment apart",()=>{
    expect(t.table).to.have.length(CALLBACKS.length*3+BATCH_SIZES.length*2*CALLBACKS.length+2);
    expect(w.table).to.have.length(CALLBACKS.length*2);
    for(const gas of w.costliest)expect(gas).to.equal(w.costliest[0]);
    for(const gas of w.span)expect(gas).to.be.at.most(w.costliest[0]);
    // The very first fulfilment of a deployment writes its counters from zero: compared with one like it, less its own VRF candidates and
    // hash-to-curve.
    const steady=rows.find(row=>row.name==="single, first in its round"&&row.callback===30_000)!;
    const alike=(row:Measured)=>row.gasUsed-BigInt(row.candidates[0]-1)*ROUND_GAS.vrfCandidate-row.roundMessageGas[0];
    expect(alike(t.first)-alike(steady)).to.be.within(45_000n,55_000n);
  });

  it("needs 64/63 of what the guard budgets as gas limit, through the proxy, as the limit model says",()=>{
    // An uncached round's guard of 411,349 costs 417,878 of the limit.
    expect(ROUND_BUDGET*64n/63n).to.equal(417_878n);
    for(const row of rows.filter(r=>!r.batch&&!r.cached))
      expect(row.limit!-(guardNeed([row.callback],1)*64n+62n)/63n,`${row.name}, ${row.callback}`).to.be.within(45_000n,48_000n);
    for(const row of rows){
      if(row.limit===undefined)continue;
      const model=roundFulfilmentGasLimit(shapeOf(row));
      expect(row.limit,`${row.name}, ${row.callback}`).to.be.at.most(model);
      expect(model-row.limit,`${row.name}, ${row.callback}`).to.be.at.most(6_000n);
    }
  });

  it("stays within the fee gate's bounds, which stay within 0.5% of the measurements",()=>{
    const costliest=w.costliest[0];
    for(const row of rows){
      if(row.gasUsed===0n)continue;
      const bound=roundFulfilmentGasBound(shapeOf(row))-BigInt(row.cached?0:row.rounds)*ROUND_ROBINHOOD_ROUND_EXCESS;
      expect(row.gasUsed,`${row.name}, ${row.callback}`).to.be.at.most(bound);
      // What the same fulfilment would use with the costliest hash-to-curve in each of its rounds.
      const worst=row.gasUsed+row.roundMessageGas.reduce((sum,gas)=>sum+costliest-gas,0n);
      expect(bound-worst,`${row.name}, ${row.callback}`).to.be.at.most(3_000n+worst/200n);
    }
  });

  it("admits the largest batches the limit model allows under a per-transaction cap",()=>{
    // At EIP-7825's cap 16 members with 1,000,000-gas callbacks do not fit; the model's largest batch does, and one more does not.
    const over=rows.filter(r=>r.limit===undefined);
    expect(over.map(r=>[r.callback,r.members,r.rounds])).to.deep.equal([[1_000_000,16,1],[1_000_000,16,2]]);
    for(const row of over)expect(row.largestAtCap,row.name).to.equal(roundMaxBatch(row.callback,row.rounds,TX_GAS_CAP));
    expect(CALLBACKS.map(cb=>[1,2].map(r=>roundMaxBatch(cb,r,TX_GAS_CAP)))).to.deep.equal([[16,16],[16,16],[16,16],[11,10]]);
    // At Robinhood testnet's 32,000,000 every batch fits, even sixteen 1,000,000-gas members each in a round of its own.
    for(const cb of CALLBACKS)for(const r of [1,2,16])expect(roundMaxBatch(cb,r,ROBINHOOD_TESTNET_TX_GAS_LIMIT)).to.equal(16);
  });

  it("keeps the keeper's share 20% above its worst cost for every callback, at every base fee where the dynamic fee binds",async()=>{
    // The coordinator's own quote is the fee this rule prices.
    await w.c.coordinator.setPricing(MIN_FEE,FEE_MULTIPLIER,FULFILL_GAS_OVERHEAD);
    for(const cb of CALLBACKS)for(const base of [1n,20_000_000n,GWEI,1_000n*GWEI])expect(await w.c.coordinator.quoteFeeAt(cb,base)).to.equal(feeAt(cb,base));
    let smallest=0n;
    for(const {callback,members,shape} of PRICED){
      const gas=worstGas(shape),floor=smallestOverhead(members,callback,gas);
      if(floor>smallest)smallest=floor;
      // From below Robinhood's 0.02 gwei floor to 1,000 gwei, across the base fee at which the dynamic fee overtakes the minimum.
      const crossover=MIN_FEE/(BigInt(FEE_MULTIPLIER)*(FULFILL_GAS_OVERHEAD+BigInt(callback)))+1n;
      for(const base of [10_000_000n,20_000_000n,crossover-1n,crossover,crossover*3n/2n,44_000_000n,370_000_000n,960_000_000n,10n*GWEI,1_000n*GWEI]){
        const revenue=BigInt(members)*keeperShare(feeAt(callback,base)),cost=base*gas;
        expect(revenue*10_000n,`${members} member(s) of ${callback} over ${shape.roundsToVerify} round(s) at base fee ${base}`)
          .to.be.at.least(cost*(10_000n+MARGIN_BPS));
      }
    }
    if(process.env.ROUND_GAS_TABLE==="1")console.log(`smallest fulfilment overhead that keeps the margin: ${smallest} (pricing under test: ${FULFILL_GAS_OVERHEAD})`);
    expect(FULFILL_GAS_OVERHEAD).to.be.at.least(smallest);
    // The binding case is a lone request with the smallest callback, and 360,000 does not cover it.
    expect(smallestOverhead(1,30_000,worstGas(PRICED[0].shape))).to.equal(smallest);
    expect(smallest).to.be.greaterThan(360_000n);
    expect([ROUND_L1_GAS.single,ROUND_PRICED_VRF_CANDIDATES]).to.deep.equal([25_000n,10]);
  });
});
