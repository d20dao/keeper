// The round replay library (src/round.ts) against a live round coordinator: the schedule it folds from the coordinator's own events
// answers roundAt as the contract does across replaced, folded and cancelled changes, and a request bound to a later beacon replays.
import {expect} from "chai";
import {network} from "hardhat";
import {replayRoundRequest,roundAt,roundBeaconIdentity,roundScheduleFromEvents,type RoundBeaconLog,type RoundRequestEvidence} from "../../src/round.ts";
import {publicKey} from "../helpers/proof.ts";
import {deployRoundCoordinator,eventsOf,registrationOf,request,serve,type RoundBeacon,type RoundFixture} from "../helpers/robinhood.ts";

const {ethers,networkHelpers}=await network.create();
const now=async()=>BigInt(await networkHelpers.time.latest());
const fixture=()=>deployRoundCoordinator(ethers,networkHelpers);
/// The test beacon on another network with its own period and genesis, so that its rounds differ from beacon 0's.
const otherBeacon=(c:RoundFixture,name:string,period:bigint,genesisShift:bigint):RoundBeacon=>
  ({...c.beacon,chainHash:ethers.id(name),period,genesis:c.beacon.genesis+genesisShift});
/// Every log of the coordinator from its deployment on, with its block's timestamp.
async function beaconLogs(c:RoundFixture):Promise<RoundBeaconLog[]>{
  const logs=await ethers.provider.getLogs({address:await c.coordinator.getAddress(),fromBlock:0,toBlock:"latest"});
  const times=new Map<number,bigint>();
  for(const log of logs)if(!times.has(log.blockNumber))times.set(log.blockNumber,BigInt((await ethers.provider.getBlock(log.blockNumber))!.timestamp));
  return logs.map(log=>({address:log.address,topics:log.topics,data:log.data,blockNumber:log.blockNumber,index:log.index,blockTimestamp:times.get(log.blockNumber)!}));
}

describe("Round replay library against the round coordinator",function(){
  it("folds the coordinator's events into the schedule its roundAt answers, across replaced, folded and cancelled changes",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const beacons=[c.beacon,otherBeacon(c,"library one",5n,3n),otherBeacon(c,"library two",2n,1n),otherBeacon(c,"library three",7n,4n)];
    for(const b of beacons.slice(1))await c.coordinator.registerBeacon(registrationOf(b));
    const changes:bigint[]=[];
    // 1 at a, replaced by 2 at b before a comes: a schedule with no cancellation event.
    const a=await now()+700n,b=a+700n;
    await c.coordinator.scheduleBeacon(1,a);
    await networkHelpers.time.setNextBlockTimestamp(a-1n);
    await c.coordinator.scheduleBeacon(2,b);
    // Once 2 is in force, 3 at d, scheduled at b itself: 2 becomes an era.
    const d=b+650n;
    await networkHelpers.time.setNextBlockTimestamp(b);
    await c.coordinator.scheduleBeacon(3,d);
    // After d, 0 at e, cancelled in its last second; then 2 at f, which stays pending.
    await networkHelpers.time.increaseTo(d+20n);
    const e=await now()+700n;
    await c.coordinator.scheduleBeacon(0,e);
    await networkHelpers.time.setNextBlockTimestamp(e-1n);
    await c.coordinator.cancelBeaconSchedule();
    const f=e+1_000n;
    await c.coordinator.scheduleBeacon(2,f);
    changes.push(a,b,d,e,f);

    const schedule=roundScheduleFromEvents(await c.coordinator.getAddress(),await beaconLogs(c));
    expect(schedule.beacons.map(x=>x.identity)).to.deep.equal(await Promise.all(beacons.map((_,i)=>c.coordinator.beaconIdentity(i))));
    for(const [i,beacon] of beacons.entries())expect(roundBeaconIdentity(beacon)).to.equal(schedule.beacons[i].identity);
    expect(schedule.eras.map(era=>[era.beaconId,era.since])).to.deep.equal([[0,schedule.eras[0].since],[2,b],[3,d]]);
    expect(schedule.pending).to.deep.equal({beaconId:2,fromTime:f});
    const times=[0n,c.beacon.genesis,schedule.eras[0].since-1n,schedule.eras[0].since,...changes.flatMap(t=>[t-1n,t,t+1n,t+17n]),f+100_000n];
    for(const t of times){
      const ours=roundAt(schedule,t);
      expect([BigInt(ours.beaconId),ours.round],`roundAt(${t})`).to.deep.equal(Array.from(await c.coordinator.roundAt(t)));
    }
  });

  it("replays a request bound to a later beacon, from the coordinator's own state and events, and refuses it under the wrong beacon",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    const two=otherBeacon(c,"library replay",4n,2n);
    await c.coordinator.registerBeacon(registrationOf(two));
    const from=await now()+700n;
    await c.coordinator.scheduleBeacon(1,from);
    const made=await request(c,networkHelpers,{at:from+5n,seed:ethers.id("library replay")});
    expect(made.request.beaconId).to.equal(1n);
    const {proof,receipt,signature}=await serve(c,ethers,made.id);
    const q=await c.coordinator.getRoundRequest(made.id),chainId=(await ethers.provider.getNetwork()).chainId;
    const [x,y]=publicKey(),registered=await c.coordinator.getBeacon(1);
    const evidence:RoundRequestEvidence={
      chainId,coordinator:await c.coordinator.getAddress(),
      configuration:{publicKey:[x,y],feeRecipient:await c.coordinator.initialFeeRecipient(),initialMinFee:await c.coordinator.initialMinFee(),
        beaconIdentity:await c.coordinator.beaconIdentity(0)},
      configurationHash:await c.coordinator.protocolConfigurationHash(),keyHash:await c.coordinator.keyHash(),
      beacon:{verifier:registered.verifier,chainHash:registered.chainHash,publicKey:registered.publicKey,genesis:registered.genesis,period:registered.period,
        identity:await c.coordinator.beaconIdentity(1)},
      requestId:made.id,consumer:q.consumer,clientSeed:q.clientSeed,mapping:{operation:0,lower:0n,upper:0n,count:0,population:0},mappingHash:q.mappingHash,
      requestBlock:q.requestBlock,requestTimestamp:from+5n,deadline:q.deadline,beaconId:Number(q.beaconId),round:q.round,
      roundSignature:eventsOf(c.coordinator,receipt,"RoundVerified")[0].args.signature,roundRandomness:q.roundRandomness,
      proof,randomness:q.randomness,proofHash:q.proofHash,transcriptHash:q.transcriptHash,
      evidencePacket:eventsOf(c.coordinator,receipt,"FulfillmentEvidence")[0].args.packet,
      mappedResult:Array.from(await c.coordinator.getMappedResult(made.id)) as bigint[],
      acceptanceTimestamp:BigInt((await ethers.provider.getBlock(receipt.blockNumber))!.timestamp),
    };
    expect(evidence.roundSignature).to.equal(signature);
    const schedule=roundScheduleFromEvents(evidence.coordinator,await beaconLogs(c));
    const verdict=replayRoundRequest(evidence,{schedule});
    expect(verdict.failed).to.deep.equal([]);
    expect(verdict.seed).to.equal(await c.coordinator.requestSeed(made.id));
    expect(verdict.checks.map(check=>check.name)).to.include("beaconInForce");
    // The same request read as beacon 0's: the seed, transcript and schedule disagree.
    const wrong=replayRoundRequest({...evidence,beaconId:0},{schedule});
    expect(wrong.failed).to.include.members(["beaconInForce","seed","transcript"]);
  });
});
