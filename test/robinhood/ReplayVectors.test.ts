// Replay vectors of the round coordinator for the TypeScript replay library: requests bound to real drand evmnet rounds, fulfilled on a
// local chain, with every input and output a replay recomputes. The test builds them from the contract and compares them with the committed
// file. Regenerate after a deliberate change with ROUND_VECTORS_WRITE=1 (PowerShell: $env:ROUND_VECTORS_WRITE=1), then review the diff:
//   npx hardhat test test/robinhood/ReplayVectors.test.ts
import {mkdirSync,readFileSync,writeFileSync} from "node:fs";
import {expect} from "chai";
import {network} from "hardhat";
import {AbiCoder,keccak256} from "ethers";
import {hashMapping,mapRandomness as mapOffChain,type MappingSpec} from "../../src/mapping.ts";
import {hashProof,verifyVRFProof} from "../../src/verification.ts";
import {makeProof,proofOutput,publicKey} from "../helpers/proof.ts";
import {ROUND_BEACON_DOMAIN,CONFIG_DOMAIN,EVMNET,EVMNET_START,ROUND_LEAD,SEED_DOMAIN,TRANSCRIPT_DOMAIN,assignedRound,deployRoundCoordinator,eventsOf,nextFee,
  randomnessOf,roundSeed,roundTime,type RoundFixture} from "../helpers/robinhood.ts";

const FILE=new URL("../fixtures/robinhood/round-replay-vectors.json",import.meta.url);
const abi=AbiCoder.defaultAbiCoder();
const RAW:MappingSpec={operation:0,lower:0n,upper:0n,count:0,population:0};
type Proof=ReturnType<typeof makeProof>;
/// One planned request: the real round it binds, how many seconds before that round's scheduled time it is sent, its mapping, and how it is served.
type Plan={name:string;round:bigint;before:bigint;gas:number;mapping:MappingSpec;serve:string};
const [A,B,C]=EVMNET.rounds.slice(1).map(r=>r.round);
const PLANS:Plan[]=[
  {name:"raw, first of its round, served alone",round:A,before:5n,gas:100_000,mapping:RAW,serve:"single A1"},
  {name:"three six-sided dice, served alone from the round's cache",round:A,before:4n,gas:150_000,mapping:{operation:1,lower:0n,upper:6n,count:3,population:0},serve:"single A2"},
  {name:"shuffle of ten, the last second that binds its round, served alone from the cache",round:A,before:3n,gas:200_000,mapping:{operation:6,lower:0n,upper:0n,count:10,population:10},serve:"single A3"},
  {name:"coin flip, first member of a batch that verifies its round",round:B,before:5n,gas:100_000,mapping:{operation:2,lower:0n,upper:0n,count:1,population:0},serve:"batch B"},
  {name:"raw, second member of that batch",round:B,before:4n,gas:30_000,mapping:RAW,serve:"batch B"},
  {name:"number from 1 to 1,000,000, first of its round, served alone",round:C,before:5n,gas:100_000,mapping:{operation:3,lower:1n,upper:1_000_000n,count:1,population:0},serve:"single C1"},
  {name:"five of 52 without repetition, served alone from the cache",round:C,before:3n,gas:1_000_000,mapping:{operation:5,lower:0n,upper:0n,count:5,population:52},serve:"single C2"},
];
const text=(v:bigint|number)=>v.toString();
const mappingJson=(m:MappingSpec)=>({operation:m.operation,lower:text(m.lower),upper:text(m.upper),count:m.count,population:m.population});
const proofJson=(p:Proof)=>({pk:p.pk.map(text),gamma:p.gamma.map(text),c:text(p.c),s:text(p.s),seed:text(p.seed),uWitness:p.uWitness,
  cGammaWitness:p.cGammaWitness.map(text),sHashWitness:p.sHashWitness.map(text),zInv:text(p.zInv)});
const proofFromJson=(p:any):Proof=>({pk:[BigInt(p.pk[0]),BigInt(p.pk[1])],gamma:[BigInt(p.gamma[0]),BigInt(p.gamma[1])],c:BigInt(p.c),s:BigInt(p.s),
  seed:BigInt(p.seed),uWitness:p.uWitness,cGammaWitness:[BigInt(p.cGammaWitness[0]),BigInt(p.cGammaWitness[1])],
  sHashWitness:[BigInt(p.sHashWitness[0]),BigInt(p.sHashWitness[1])],zInv:BigInt(p.zInv)});
const mappingFromJson=(m:any):MappingSpec=>({operation:m.operation,lower:BigInt(m.lower),upper:BigInt(m.upper),count:m.count,population:m.population});

/// Build the vectors on a chain of their own, whose clock starts ten minutes before the first of the fixture's recent evmnet rounds.
async function buildVectors(){
  const {ethers,networkHelpers}=await network.create({override:{initialDate:EVMNET_START}});
  const c:RoundFixture=await deployRoundCoordinator(ethers,networkHelpers,{evmnet:true});
  const signatureOf=(round:bigint)=>EVMNET.rounds.find(r=>r.round===round)!.signature;
  type Made={plan:Plan;id:bigint;timestamp:bigint;proof?:Proof;acceptance?:bigint;verifiedHere?:boolean};
  const made:Made[]=[];
  // Requests in plan order, each in its own block at its planned second.
  for(const plan of PLANS){
    const timestamp=roundTime(EVMNET,plan.round)-plan.before;
    await networkHelpers.time.setNextBlockTimestamp(timestamp);
    const fee=await nextFee(c,networkHelpers,plan.gas);
    const seed=ethers.id(`replay vector ${made.length+1}`);
    const sent=plan.mapping.operation===0?c.consumer.request(seed,plan.gas,c.owner.address,{value:fee})
      :c.consumer.requestMapped(seed,plan.gas,c.owner.address,[plan.mapping.operation,plan.mapping.lower,plan.mapping.upper,plan.mapping.count,plan.mapping.population],{value:fee});
    await (await sent).wait();
    made.push({plan,id:await c.consumer.lastRequestId(),timestamp});
    // Serve a round's requests two seconds after its scheduled time, once all of its requests are in.
    const next=PLANS[made.length];
    if(next?.round===plan.round)continue;
    const acceptance=roundTime(EVMNET,plan.round)+2n,signature=signatureOf(plan.round);
    const group=made.filter(m=>m.plan.round===plan.round);
    for(const m of group)m.proof=makeProof(await roundSeed(c,ethers,m.id,randomnessOf(signature)));
    let at=acceptance;
    for(const label of [...new Set(group.map(m=>m.plan.serve))]){
      const members=group.filter(m=>m.plan.serve===label);
      await networkHelpers.time.setNextBlockTimestamp(at);
      const cached=await c.coordinator.roundRandomness(0,plan.round)!==ethers.ZeroHash;
      const tx=label.startsWith("batch")
        ?c.coordinator.connect(c.keeper).fulfillRandomnessBatch([[0,plan.round,signature]],members.map(m=>m.id),members.map(m=>m.proof))
        :c.coordinator.connect(c.keeper).fulfillRandomness(members[0].id,members[0].proof,cached?"0x":signature);
      const receipt=await (await tx).wait();
      const verified=eventsOf(c.coordinator,receipt,"RoundVerified").length===1;
      expect(verified,label).to.equal(!cached);
      for(const m of members){m.acceptance=at;m.verifiedHere=verified&&m===members[0];}
      at+=1n;
    }
  }
  const coordinator=await c.coordinator.getAddress();
  const chainId=(await ethers.provider.getNetwork()).chainId;
  const beacon=await c.coordinator.getBeacon(0);
  const vectors=[];
  for(const m of made){
    const q=await c.coordinator.getRoundRequest(m.id),signature=signatureOf(q.round);
    vectors.push({
      name:m.plan.name,
      requestId:text(m.id),
      consumer:q.consumer,
      clientSeed:q.clientSeed,
      callbackGasLimit:Number(q.callbackGasLimit),
      refundAddress:q.refundAddress,
      mapping:mappingJson(m.plan.mapping),
      mappingHash:q.mappingHash,
      requestBlock:text(q.requestBlock),
      requestTimestamp:text(m.timestamp),
      deadline:text(q.deadline),
      feePaid:text(q.feePaid),
      beaconId:Number(q.beaconId),
      round:text(q.round),
      roundTime:text(roundTime(EVMNET,q.round)),
      roundSignature:signature,
      roundRandomness:q.roundRandomness,
      seed:text(await c.coordinator.requestSeed(m.id)),
      proof:proofJson(m.proof!),
      evidencePacket:eventsOf(c.coordinator,{logs:await ethers.provider.getLogs({address:coordinator,fromBlock:0,
        topics:[c.coordinator.interface.getEvent("FulfillmentEvidence")!.topicHash,ethers.toBeHex(m.id,32)]})},"FulfillmentEvidence")[0].args.packet,
      randomness:q.randomness,
      proofHash:q.proofHash,
      transcriptHash:q.transcriptHash,
      mappedResult:(await c.coordinator.getMappedResult(m.id)).map(text),
      acceptanceTimestamp:text(m.acceptance!),
      roundVerifiedInAcceptance:m.verifiedHere!,
      servedIn:m.plan.serve.startsWith("batch")?"batch":"single",
    });
  }
  const [x,y]=publicKey();
  return {
    source:"Built by test/robinhood/ReplayVectors.test.ts on a local chain whose ArbSys answers the local block number plus 1,000,000. Each request "+
      "binds a real drand evmnet round of test/fixtures/drand-evmnet-2026-09-29.json and is proved with the public test VRF key of test/helpers/proof.ts.",
    chainId:text(chainId),
    coordinator,
    roundLead:Number(ROUND_LEAD),
    responseTimeout:60,
    domains:{seed:SEED_DOMAIN,transcript:TRANSCRIPT_DOMAIN,configuration:CONFIG_DOMAIN,beacon:ROUND_BEACON_DOMAIN},
    keyHash:await c.coordinator.keyHash(),
    protocolConfiguration:{publicKey:[text(x),text(y)],feeRecipient:await c.coordinator.initialFeeRecipient(),initialMinFee:text(await c.coordinator.initialMinFee()),
      beaconIdentity:await c.coordinator.beaconIdentity(0),hash:await c.coordinator.protocolConfigurationHash()},
    beacons:[{beaconId:0,verifier:beacon.verifier,chainHash:beacon.chainHash,publicKey:beacon.publicKey,genesis:text(beacon.genesis),period:text(beacon.period),
      identity:await c.coordinator.beaconIdentity(0)}],
    vectors,
  };
}

describe("Round replay vectors",function(){
  let built:Awaited<ReturnType<typeof buildVectors>>;
  before(async()=>{
    built=await buildVectors();
    if(process.env.ROUND_VECTORS_WRITE==="1"){
      mkdirSync(new URL(".",FILE),{recursive:true});
      writeFileSync(FILE,JSON.stringify(built,null,2)+"\n");
    }
  });

  it("are what the contract produces now",()=>{
    const committed=JSON.parse(readFileSync(FILE,"utf8"));
    expect(built).to.deep.equal(committed);
  });

  it("replay off chain: assignment, round randomness, seed, proof, transcript, configuration hash and mapping",()=>{
    const v=JSON.parse(readFileSync(FILE,"utf8"));
    const chainId=BigInt(v.chainId),pk:[bigint,bigint]=[BigInt(v.protocolConfiguration.publicKey[0]),BigInt(v.protocolConfiguration.publicKey[1])];
    expect(v.keyHash).to.equal(keccak256(abi.encode(["uint256[2]"],[pk])));
    const b=v.beacons[0];
    expect(b.identity).to.equal(keccak256(abi.encode(["bytes32","address","bytes32","bytes32","uint64","uint64"],
      [v.domains.beacon,b.verifier,b.chainHash,keccak256(b.publicKey),BigInt(b.genesis),BigInt(b.period)])));
    expect(v.protocolConfiguration.beaconIdentity).to.equal(b.identity);
    expect(v.protocolConfiguration.hash).to.equal(keccak256(abi.encode(["bytes32","uint256[2]","address","uint256","bytes32"],
      [v.domains.configuration,pk,v.protocolConfiguration.feeRecipient,BigInt(v.protocolConfiguration.initialMinFee),b.identity])));
    expect(v.vectors).to.have.length(PLANS.length);
    const beacon={genesis:BigInt(b.genesis),period:BigInt(b.period)};
    for(const r of v.vectors){
      const round=BigInt(r.round),timestamp=BigInt(r.requestTimestamp);
      // Assignment: the first round scheduled at least ROUND_LEAD seconds after the request.
      expect(round,r.name).to.equal(assignedRound(beacon,timestamp));
      expect(BigInt(r.roundTime)).to.equal(roundTime(beacon,round));
      expect(BigInt(r.deadline)).to.equal(timestamp+60n);
      // The round: drand's own randomness is sha256 of the signature.
      const real=EVMNET.rounds.find(x=>x.round===round)!;
      expect([r.roundSignature,r.roundRandomness]).to.deep.equal([real.signature,real.randomness]);
      // The seed and the proof.
      const mapping=mappingFromJson(r.mapping);
      expect(r.mappingHash).to.equal(hashMapping(mapping));
      const seed=BigInt(keccak256(abi.encode(
        ["bytes32","uint256","address","bytes32","uint256","address","bytes32","bytes32","uint64","uint8","uint64","bytes32"],
        [v.domains.seed,chainId,v.coordinator,v.keyHash,BigInt(r.requestId),r.consumer,r.clientSeed,r.mappingHash,BigInt(r.requestBlock),r.beaconId,round,r.roundRandomness])));
      expect(BigInt(r.seed),r.name).to.equal(seed);
      const proof=proofFromJson(r.proof);
      const checked=verifyVRFProof(proof,pk,seed);
      expect(checked,r.name).to.deep.equal({valid:true,randomness:r.randomness});
      expect(r.randomness).to.equal(proofOutput(proof));
      expect(r.proofHash).to.equal(hashProof(proof));
      expect(keccak256(r.evidencePacket)).to.equal(r.proofHash);
      // The transcript and the mapped result.
      expect(r.transcriptHash,r.name).to.equal(keccak256(abi.encode(
        ["bytes32","uint256","address","uint256","bytes32","uint8","uint64","bytes32","bytes32","bytes32","bytes32"],
        [v.domains.transcript,chainId,v.coordinator,BigInt(r.requestId),v.protocolConfiguration.hash,r.beaconId,round,r.roundRandomness,r.proofHash,
          r.randomness,r.mappingHash])));
      expect(r.mappedResult.map(BigInt)).to.deep.equal(mapOffChain(r.randomness,mapping));
      // Accepted inside the request's window, after the round's scheduled time.
      expect(BigInt(r.acceptanceTimestamp)).to.be.within(BigInt(r.roundTime),BigInt(r.deadline));
    }
    // Every recent fixture round is used, first in round and cached alike, alone and in a batch.
    const used=new Set(v.vectors.map((r:any)=>r.round));
    expect([...used]).to.deep.equal([A,B,C].map(text));
    expect(new Set(v.vectors.map((r:any)=>`${r.servedIn} ${r.roundVerifiedInAcceptance}`))).to.deep.equal(new Set(["single true","single false","batch true","batch false"]));
  });

  it("change when any seed input changes by one bit",()=>{
    const v=JSON.parse(readFileSync(FILE,"utf8")),r=v.vectors[0];
    const seedOf=(o:any)=>BigInt(keccak256(abi.encode(
      ["bytes32","uint256","address","bytes32","uint256","address","bytes32","bytes32","uint64","uint8","uint64","bytes32"],
      [v.domains.seed,BigInt(o.chainId),o.coordinator,o.keyHash,BigInt(o.requestId),o.consumer,o.clientSeed,o.mappingHash,BigInt(o.requestBlock),o.beaconId,
        BigInt(o.round),o.roundRandomness])));
    const base={...r,chainId:v.chainId,coordinator:v.coordinator,keyHash:v.keyHash};
    expect(seedOf(base)).to.equal(BigInt(r.seed));
    const flip=(hex:string)=>"0x"+(BigInt(hex)^1n).toString(16).padStart(hex.length-2,"0");
    const changes:Array<[string,unknown]>=[["chainId",text(BigInt(v.chainId)+1n)],["coordinator",flip(v.coordinator)],["keyHash",flip(v.keyHash)],
      ["requestId",text(BigInt(r.requestId)+1n)],["consumer",flip(r.consumer)],["clientSeed",flip(r.clientSeed)],["mappingHash",flip(r.mappingHash)],
      ["requestBlock",text(BigInt(r.requestBlock)+1n)],["beaconId",r.beaconId+1],["round",text(BigInt(r.round)+1n)],["roundRandomness",flip(r.roundRandomness)]];
    for(const [field,value] of changes)expect(seedOf({...base,[field]:value}),field).to.not.equal(BigInt(r.seed));
  });
});
