import {readFileSync} from "node:fs";
import {expect} from "chai";
import {network} from "hardhat";
import {bn254} from "@noble/curves/bn254";
import {deployProxy} from "./helpers/proxy.ts";
import {EPOCH_TEST_SIGNERS} from "./helpers/epoch.ts";
import {BEACON_DST,BEACON_TEMPLATE,BUILTIN_EPOCH_RECIPES,DRAND_EVMNET,beaconCanonicalRequest,beaconRoundAt,beaconRoundMessage,beaconRoundTime,beaconSlotSigner,
  decodeBeaconRound,encodeBeaconRound,encodeDataTemplate,readEpochRecipes,replayEpochCommitment,resolveEpochCatalog,selectEpoch,validateEpochRecipe,
  verifyBeaconRound,verifyEpochAttestation,type BeaconRegistration,type EpochRecipe} from "../src/index.ts";
// The secret-key helpers stay out of the public library, so tests import them from the test beacon helpers.
import {beaconPublicKey,signBeaconRound} from "./helpers/beacon.ts";
import {loadChain} from "../scripts/lib/chains.ts";
import {compiledRuntimeCodeHash,initCode} from "../scripts/lib/deployment.ts";
import {BEACON_ABI,registerBeaconArgs} from "../scripts/lib/beacon-admin.ts";
import {BEACON_PRESETS} from "../scripts/lib/drand-relay.ts";

const {ethers,networkHelpers,provider}=await network.create();
// Real evmnet chain info, rounds and the reference library's H(round) points, fetched from four public relays.
const fixture=JSON.parse(readFileSync(new URL("./fixtures/drand-evmnet-2026-09-29.json",import.meta.url),"utf8")) as {
  info:{public_key:string;period:number;genesis_time:number;hash:string;schemeID:string};rounds:Array<{round:number;signature:string}>;
  messagePoints:{points:Array<{round:number;x:string;y:string}>}};
const P=bn254.fields.Fp.ORDER,N=bn254.fields.Fr.ORDER,KEY=DRAND_EVMNET.publicKey;
const real=fixture.rounds.map(r=>({round:BigInt(r.round),signature:`0x${r.signature}`}));
const utf8=(text:string)=>ethers.hexlify(ethers.toUtf8Bytes(text));
const words=(hex:string)=>Array.from({length:ethers.dataLength(hex)/32},(_,i)=>BigInt(ethers.dataSlice(hex,32*i,32*i+32)));
const join=(values:readonly bigint[])=>ethers.concat(values.map(value=>ethers.toBeHex(value,32)));
const zeros=(bytes:number)=>"0x"+"00".repeat(bytes);
// Runtime code of the deterministic CREATE2 factory that chains.json names for every chain: it reads 32 bytes of salt, then init code.
const CREATE2_FACTORY_CODE="0x7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe03601600081602082378035828234f58015156039578182fd5b8082525050506014600cf3";
// The gas cap that scripts/create2-deploy.ts beacon-verifier sends with, and the one scripts/admin.ts register-beacon sends with.
const DEPLOYMENT_GAS_CAP=2_400_000,REGISTRATION_GAS_CAP=950_000;
/// A point of the twist curve outside the prime-order subgroup: the first x from a fixed sequence with x³ + b' a square.
function outsideSubgroup(){
  const {Fp2}=bn254.fields,b=bn254.G2.Point.CURVE().b;
  for(let i=1n;;i++){
    const x=Fp2.fromBigTuple([i,1n]);let y;
    try{y=Fp2.sqrt(Fp2.add(Fp2.mul(Fp2.sqr(x),x),b));}catch{continue;}
    return bn254.G2.Point.fromAffine({x,y});
  }
}
type Case={name:string;key:string;round:bigint;signature:string;valid:boolean};
/// The four real rounds, then inputs that must all be refused. Signatures are x ‖ y, keys are x_im ‖ x_re ‖ y_im ‖ y_re.
function cases():Case[]{
  const a=real[1],b=real[2],[x,y]=words(a.signature),[xIm,xRe,yIm,yRe]=words(KEY),q=outsideSubgroup().toAffine();
  const bad=(name:string,patch:Partial<Case>):Case=>({name,key:KEY,round:a.round,signature:a.signature,valid:false,...patch});
  return [
    ...real.map(r=>({name:`real round ${r.round}`,key:KEY,round:r.round,signature:r.signature,valid:true})),
    bad("a flipped bit in y",{signature:ethers.toBeHex(BigInt(a.signature)^1n,64)}),
    bad("a flipped bit in x",{signature:ethers.toBeHex(BigInt(a.signature)^(1n<<300n),64)}),
    bad("the negated signature",{signature:join([x,P-y])}),
    bad("the next round",{round:a.round+1n}),
    bad("another round's signature",{signature:b.signature}),
    bad("the zero encoding of infinity",{signature:zeros(64)}),
    bad("a signature x not below the field order",{signature:join([x+P,y])}),
    bad("a 63-byte signature",{signature:a.signature.slice(0,-2)}),bad("a 65-byte signature",{signature:a.signature+"00"}),bad("an empty signature",{signature:"0x"}),
    bad("the key in BLS.sol word order",{key:join([xRe,xIm,yRe,yIm])}),
    bad("an off-curve key",{key:join([xIm,xRe,yIm,(yRe+1n)%P])}),
    bad("a key word not below the field order",{key:join([xIm+P,xRe,yIm,yRe])}),
    bad("a key on the twist outside the subgroup",{key:join([q.x.c1,q.x.c0,q.y.c1,q.y.c0])}),
    bad("the zero key",{key:zeros(128)}),
    bad("a 127-byte key",{key:KEY.slice(0,-2)}),bad("a 129-byte key",{key:KEY+"00"}),bad("an empty key",{key:"0x"}),
  ];
}
// Small, large and random rounds, and both ends of uint64.
const edgeRounds=[0n,2n,3n,4n,7n,100n,255n,256n,65535n,2n**32n-1n,2n**32n,2n**53n,2n**63n,2n**64n-1n,...Array.from({length:40},(_,i)=>BigInt.asUintN(64,BigInt(ethers.id(`round ${i}`))))];

describe("Beacon verification in TypeScript",function(){
  it("takes the network's identity from the fixture and hashes to the reference library's points",()=>{
    expect(fixture.info.schemeID).to.equal("bls-bn254-unchained-on-g1");
    expect([`0x${fixture.info.public_key}`,`0x${fixture.info.hash}`,BigInt(fixture.info.genesis_time),BigInt(fixture.info.period)]).to.deep.equal(
      [DRAND_EVMNET.publicKey,DRAND_EVMNET.chainHash,DRAND_EVMNET.genesis,DRAND_EVMNET.period]);
    expect(ethers.dataLength(DRAND_EVMNET.publicKey)).to.equal(128);expect(ethers.dataLength(DRAND_EVMNET.chainHash)).to.equal(32);
    expect(fixture.messagePoints.points.map(p=>Number(p.round))).to.deep.equal(fixture.rounds.map(r=>r.round));
    for(const p of fixture.messagePoints.points)expect(beaconRoundMessage(BigInt(p.round)),`round ${p.round}`).to.deep.equal({x:BigInt(p.x),y:BigInt(p.y)});
  });

  it("verifies the real rounds and refuses every malformed or wrong signature and key without throwing",()=>{
    const all=cases();
    expect(all.filter(c=>c.valid).length).to.equal(4);expect(all.length).to.be.greaterThan(20);
    for(const c of all)expect(verifyBeaconRound(c.key,c.round,c.signature),c.name).to.equal(c.valid);
    expect(verifyBeaconRound(ethers.getBytes(KEY),real[1].round,ethers.getBytes(real[1].signature))).to.equal(true);
    // Not a key or round the contract could receive, and not even hex: refused rather than thrown.
    expect(verifyBeaconRound(KEY,-1n,real[0].signature)).to.equal(false);expect(verifyBeaconRound(KEY,2n**64n,real[0].signature)).to.equal(false);
    expect(verifyBeaconRound("0xzz",1n,real[0].signature)).to.equal(false);expect(verifyBeaconRound(KEY,1n,"not hex")).to.equal(false);
    // The outside-subgroup point is on the curve, and both noble's test and a direct [n]Q check say it is not in G2.
    const q=outsideSubgroup();
    expect(q.isTorsionFree()).to.equal(false);expect(q.multiplyUnsafe(N-1n).add(q).is0()).to.equal(false);
  });

  it("signs and verifies with a test key pair in drand's encoding",()=>{
    const secret=BigInt(ethers.id("test beacon key"))%N,key=beaconPublicKey(secret);
    // The public key of 1 is the G2 generator, written x_im ‖ x_re ‖ y_im ‖ y_re as EIP-197 defines it.
    expect(beaconPublicKey(1n)).to.equal(join([11559732032986387107991004021392285783925812861821192530917403151452391805634n,10857046999023057135944570762232829481370756359578518086990519993285655852781n,
      4082367875863433681332203403145435568316851327593401208105741076214120093531n,8495653923123431417604973247489272438418190587263600148770280649306958101930n]));
    for(const round of [1n,21056714n,2n**64n-1n]){
      const signature=signBeaconRound(secret,round);
      expect(ethers.dataLength(key)).to.equal(128);expect(ethers.dataLength(signature)).to.equal(64);
      expect(verifyBeaconRound(key,round,signature)).to.equal(true);
      expect(verifyBeaconRound(beaconPublicKey(secret+1n),round,signature),"another key").to.equal(false);
      expect(verifyBeaconRound(key,round-1n,signature),"another round").to.equal(false);
      expect(verifyBeaconRound(KEY,round,signature),"the evmnet key").to.equal(false);
    }
    expect(()=>signBeaconRound(0n,1n)).to.throw();expect(()=>beaconPublicKey(N)).to.throw();
  });

  it("schedules rounds, encodes them as the recipe's data and names the recipe as the registry does",()=>{
    const b=DRAND_EVMNET;
    expect(BEACON_TEMPLATE).to.equal("0x040113").and.to.equal(encodeDataTemplate([{integer:{minDigits:1,maxDigits:19}}]));
    expect(beaconRoundTime(b,1n)).to.equal(b.genesis);expect(beaconRoundTime(b,21056714n)).to.equal(1790691214n);
    for(const [time,round] of [[b.genesis-1n,0n],[b.genesis,1n],[b.genesis+2n,1n],[b.genesis+3n,2n],[1790691214n,21056714n],[1790691216n,21056714n],[1790691217n,21056715n]] as const)
      expect(beaconRoundAt(b,time),`time ${time}`).to.equal(round);
    for(const round of [1n,2n,21056714n,10n**18n])expect([beaconRoundAt(b,beaconRoundTime(b,round)),beaconRoundAt(b,beaconRoundTime(b,round)+b.period-1n)]).to.deep.equal([round,round]);
    expect(()=>beaconRoundTime(b,0n)).to.throw("Invalid beacon round");expect(()=>beaconRoundTime(b,2n**64n)).to.throw("Invalid beacon round");
    expect(encodeBeaconRound(21056714n)).to.equal(utf8("21056714"));
    for(const round of [1n,9n,10n,21056714n,10n**19n-1n])expect(decodeBeaconRound(encodeBeaconRound(round))).to.equal(round);
    for(const round of [0n,-1n,10n**19n])expect(()=>encodeBeaconRound(round),String(round)).to.throw("Invalid beacon round");
    for(const text of ["","0","01","00","+1","-1"," 1","1 ","1.0","1e3","0x1","１２","1".repeat(20)])expect(()=>decodeBeaconRound(utf8(text)),JSON.stringify(text)).to.throw("Invalid beacon round data");
    expect(()=>decodeBeaconRound("0x31ff")).to.throw();
    const request=beaconCanonicalRequest(b.chainHash);
    expect(request).to.equal('["drand","0x04f1e9062b8a81f848fded9c12306733282b2727ecced50032187751166ec8c3"]');
    expect(beaconCanonicalRequest(b.chainHash.toUpperCase().replace("0X","0x"))).to.equal(request);
    expect(()=>beaconCanonicalRequest(b.chainHash.slice(0,-2))).to.throw("32 bytes");
  });
});

describe("D20BeaconVerifier",function(){
  this.timeout(120_000);
  const fixtureVerifier=async()=>{const [signer]=await ethers.getSigners();return {verifier:await ethers.deployContract("D20BeaconVerifier"),signer};};

  it("uses the scheme's domain separation tag and hashes every round to the point of the TypeScript library",async()=>{
    const {verifier}=await networkHelpers.loadFixture(fixtureVerifier);
    expect(ethers.toUtf8String(await verifier.DST())).to.equal(BEACON_DST);
    const rounds=[...real.map(r=>r.round),...edgeRounds];
    expect(new Set(rounds).size).to.equal(rounds.length).and.to.be.greaterThan(50);
    for(const round of rounds){
      const [x,y]=await verifier.roundMessage(round);
      expect({x,y},`round ${round}`).to.deep.equal(beaconRoundMessage(round));
    }
    for(const p of fixture.messagePoints.points)expect(Array.from(await verifier.roundMessage(BigInt(p.round)))).to.deep.equal([BigInt(p.x),BigInt(p.y)]);
  });

  it("gives every case the verdict of the TypeScript library: the real rounds are valid and everything else is not",async()=>{
    const {verifier}=await networkHelpers.loadFixture(fixtureVerifier);
    for(const c of cases()){
      expect(await verifier.verifyRound(c.key,c.round,c.signature),c.name).to.equal(c.valid);
      expect(verifyBeaconRound(c.key,c.round,c.signature),c.name).to.equal(c.valid);
    }
  });

  it("checks a key's encoding",async()=>{
    const {verifier}=await networkHelpers.loadFixture(fixtureVerifier);
    const [xIm,xRe,yIm,yRe]=words(KEY);
    expect(await verifier.isValidPublicKey(KEY)).to.equal(true);
    for(const [name,key] of [["word-swapped",join([xRe,xIm,yRe,yIm])],["off-curve",join([xIm,xRe,yIm,(yRe+1n)%P])],["a word not below the field order",join([xIm+P,xRe,yIm,yRe])],
      ["zero",zeros(128)],["127 bytes",KEY.slice(0,-2)],["129 bytes",KEY+"00"],["empty","0x"]])
      expect(await verifier.isValidPublicKey(key),name).to.equal(false);
  });

  it("verifies a test key pair and refuses another key, round or pair",async()=>{
    const {verifier}=await networkHelpers.loadFixture(fixtureVerifier);
    const secret=BigInt(ethers.id("test beacon key"))%N,key=beaconPublicKey(secret);
    expect(await verifier.isValidPublicKey(key)).to.equal(true);
    for(const round of [1n,21056714n,2n**64n-1n]){
      const signature=signBeaconRound(secret,round);
      expect(await verifier.verifyRound(key,round,signature)).to.equal(true);
      expect(await verifier.verifyRound(beaconPublicKey(secret+1n),round,signature),"another key").to.equal(false);
      expect(await verifier.verifyRound(key,round-1n,signature),"another round").to.equal(false);
      expect(await verifier.verifyRound(KEY,round,signature),"the evmnet key").to.equal(false);
    }
  });

  it("deploys through the CREATE2 factory to the address and runtime code the deployment tooling expects, within its gas cap",async()=>{
    const chain=await loadChain("arc-mainnet"),[signer]=await ethers.getSigners();
    expect(ethers.keccak256(CREATE2_FACTORY_CODE)).to.equal(chain.create2.codeHash);
    await provider.request({method:"hardhat_setCode",params:[chain.create2.factory,CREATE2_FACTORY_CODE]});
    // No constructor arguments and no immutables: one init code and one runtime code for every chain and address.
    const code=await initCode("D20BeaconVerifier",[]),salt=ethers.id("D20 beacon verifier test salt"),address=ethers.getCreate2Address(chain.create2.factory,salt,ethers.keccak256(code));
    const receipt=(await(await signer.sendTransaction({to:chain.create2.factory,data:ethers.concat([salt,code]),gasLimit:DEPLOYMENT_GAS_CAP})).wait())!;
    console.log(`D20BeaconVerifier deployment through the CREATE2 factory: gas=${receipt.gasUsed} (script cap ${DEPLOYMENT_GAS_CAP})`);
    expect(receipt.status).to.equal(1);
    expect(ethers.keccak256(await ethers.provider.getCode(address))).to.equal(await compiledRuntimeCodeHash("D20BeaconVerifier",address));
    const verifier=await ethers.getContractAt("D20BeaconVerifier",address);
    expect(await verifier.verifyRound(KEY,real[1].round,real[1].signature)).to.equal(true);
  });

  it("spends bounded gas on every invalid input, however much gas it is given",async()=>{
    const {verifier,signer}=await networkHelpers.loadFixture(fixtureVerifier),to=await verifier.getAddress(),limit=10_000_000n;
    const send=async(c:Case)=>Number((await(await signer.sendTransaction({to,gasLimit:limit,data:verifier.interface.encodeFunctionData("verifyRound",[c.key,c.round,c.signature])})).wait())!.gasUsed);
    const estimate=async(c:Case)=>Number(await verifier.verifyRound.estimateGas(c.key,c.round,c.signature,{gasLimit:limit}));
    const measured=[];
    for(const c of cases()){
      const [used,estimated]=[await send(c),await estimate(c)];
      measured.push({...c,used,estimated});
      // A 10,000,000 limit is forwarded whole; a verifier that burned it on a bad input would use nearly all of it.
      if(!c.valid){expect(used,`${c.name} used`).to.be.lessThan(400_000);expect(estimated,`${c.name} estimated`).to.be.lessThan(400_000);}
    }
    const valid=measured.filter(c=>c.valid),invalid=measured.filter(c=>!c.valid),worst=invalid.reduce((a,c)=>c.used>a.used?c:a);
    // A valid verification fits the registry's 400,000-gas allowance with room to spare.
    for(const c of valid)expect(c.used,c.name).to.be.lessThan(250_000);
    console.log(`Beacon verifyRound gas (transaction): valid=${valid.map(c=>c.used).join("/")}, worst invalid=${worst.used} (${worst.name}), bad-length inputs=${Math.min(...invalid.map(c=>c.used))}; `+
      `estimateGas valid=${valid.map(c=>c.estimated).join("/")}, worst invalid=${Math.max(...invalid.map(c=>c.estimated))}`);
  });

  it("reverts InsufficientGas instead of calling a valid signature invalid when too little gas is left for the pairing, and still answers false to invalid input",async()=>{
    const {verifier}=await networkHelpers.loadFixture(fixtureVerifier),all=cases();
    /// What verifyRound answers at a gas limit: its answer, the name of its custom error, or `fails` when it ran out of gas.
    const outcome=async(c:Case,gasLimit:number)=>{
      try{return `returns ${await verifier.verifyRound.staticCall(c.key,c.round,c.signature,{gasLimit})}`;}
      catch(error:any){return typeof error?.data==="string"&&error.data!=="0x"?verifier.interface.parseError(error.data)?.name??"fails":"fails";}
    };
    const valid=all.filter(c=>c.valid);
    // The smallest gas limit at which a valid round is answered true. Below it the answer is InsufficientGas at every limit that reaches the
    // check, and never false: the pairing needs PAIRING_GAS × 64/63 and a margin left, on top of what the hashing before it has spent.
    const threshold=async(c:Case)=>{
      let [low,high]=[150_000,1_000_000];
      expect(await outcome(c,low),c.name).to.equal("InsufficientGas");expect(await outcome(c,high),c.name).to.equal("returns true");
      while(high-low>1){const middle=Math.floor((low+high)/2);if(await outcome(c,middle)==="InsufficientGas")low=middle;else high=middle;}
      return high;
    };
    const edges=[];
    for(const c of valid){
      const edge=await threshold(c);edges.push(edge);
      expect(edge,c.name).to.be.greaterThan(200_000+3_174+21_000).and.to.be.lessThan(300_000);
      for(const offset of [...Array.from({length:41},(_,i)=>i-20),-100_000,-50_000,-20_000,-5_000,-1_000,-100,100,1_000,5_000,50_000,500_000,9_000_000]){
        const answer=await outcome(c,edge+offset);
        // Below the threshold it is InsufficientGas, or it runs out of gas before the check; above it, true. It is never false.
        if(offset<0)expect(["InsufficientGas","fails"],`${c.name} at ${edge+offset}`).to.include(answer);
        else expect(answer,`${c.name} at ${edge+offset}`).to.equal("returns true");
      }
      expect(await outcome(c,edge-1),c.name).to.equal("InsufficientGas");
    }
    console.log(`D20BeaconVerifier verifyRound answers InsufficientGas below a gas limit of ${Math.min(...edges)} to ${Math.max(...edges)} for the real rounds`);
    // Input the checks before the pairing refuse is answered false at any gas limit that reaches them, and needs none of the pairing's gas.
    // A valid encoding that is not this round's signature is only known after the pairing, so it is InsufficientGas without the gas for it.
    const afterChecks=new Set(["the negated signature","the next round","another round's signature","a key on the twist outside the subgroup"]);
    for(const c of all.filter(c=>!c.valid)){
      if(afterChecks.has(c.name)){
        expect(await outcome(c,150_000),c.name).to.equal("InsufficientGas");
        expect(await outcome(c,1_000_000),c.name).to.equal("returns false");
      } else {
        expect(await outcome(c,100_000),c.name).to.equal("returns false");
        expect(await outcome(c,1_000_000),c.name).to.equal("returns false");
      }
    }
  });
});

const definition=(recipe:EpochRecipe)=>[recipe.canonicalRequest,recipe.template,recipe.body] as const;
/// A registry with the evmnet beacon registered as recipe 6, on the given local network.
async function registryWithBeacon(net:any){
  const [owner]=await net.ethers.getSigners();
  const registry=await deployProxy(net.ethers,"EpochEntropy",[EPOCH_TEST_SIGNERS,owner.address,owner.address]);
  const verifier=await net.ethers.deployContract("D20BeaconVerifier");
  const beacon:BeaconRegistration={verifier:await verifier.getAddress(),...DRAND_EVMNET};
  await expect(registry.registerBeacon(beacon.verifier,beacon.chainHash,beacon.publicKey,beacon.genesis,beacon.period,real[0].round,real[0].signature))
    .to.emit(registry,"BeaconRegistered").withArgs(6,beacon.verifier,beacon.chainHash,beacon.publicKey,beacon.genesis,beacon.period);
  return {registry,beacon,address:await registry.getAddress()};
}

describe("Beacon recipes in the replay library",function(){
  this.timeout(120_000);

  it("reads the registered beacon back exactly as the registry describes it, and only for a beacon recipe",async()=>{
    const {registry,beacon,address}=await registryWithBeacon({ethers,networkHelpers});
    const request=beaconCanonicalRequest(beacon.chainHash);
    expect(Array.from(await registry.getRecipe(6))).to.deep.equal([ethers.id(request),request,BEACON_TEMPLATE,request]);
    expect(await registry.slotSigner(6)).to.equal(beaconSlotSigner(beacon));expect(await registry.slotSigner(0)).to.equal(ethers.ZeroAddress);
    const book=await readEpochRecipes(ethers.provider,address,[0,2,6]);
    expect(book[6]).to.deep.equal({canonicalRequest:request,template:BEACON_TEMPLATE,body:request,beacon});
    // Signed recipes come back as before, without a beacon key.
    expect(book[0]).to.deep.equal({canonicalRequest:BUILTIN_EPOCH_RECIPES[0].canonicalRequest,template:BUILTIN_EPOCH_RECIPES[0].template,body:BUILTIN_EPOCH_RECIPES[0].body});
    expect("beacon" in book[2]).to.equal(false);
    for(const recipe of [...Object.values(book),...BUILTIN_EPOCH_RECIPES])validateEpochRecipe(recipe);
    // A recipe that merely names drand, registered as signed, has no registration to attach.
    const lookalike:EpochRecipe={canonicalRequest:'["drand","0x00"]',template:BUILTIN_EPOCH_RECIPES[0].template,body:'["drand","0x00"]'};
    await registry.registerRecipe(...definition(lookalike));
    expect((await readEpochRecipes(ethers.provider,address,[7]))[7]).to.deep.equal(lookalike);
    for(const [round,signature,valid] of [[real[1].round,real[1].signature,true],[real[1].round+1n,real[1].signature,false],[real[1].round,real[2].signature,false]] as const){
      expect(await registry.verifyBeacon(6,round,signature)).to.equal(valid);expect(await registry.verifyBeacon(0,round,signature)).to.equal(false);
    }
  });

  it("registers the evmnet beacon from the calls the administration script sends and reads, for a bounded gas cost",async()=>{
    const [owner]=await ethers.getSigners(),registry=await deployProxy(ethers,"EpochEntropy",[EPOCH_TEST_SIGNERS,owner.address,owner.address]);
    const verifier=await ethers.deployContract("D20BeaconVerifier"),address=await registry.getAddress(),preset=BEACON_PRESETS.evmnet,sample={round:real[0].round,signature:real[0].signature};
    // The tooling's own fragments select the contracts' functions, and its arguments are in the contract's order.
    const abi=new ethers.Interface(BEACON_ABI),args=registerBeaconArgs(await verifier.getAddress(),preset,sample);
    expect(abi.getFunction("registerBeacon")!.selector).to.equal(registry.interface.getFunction("registerBeacon")!.selector);
    expect(abi.getFunction("beaconOf")!.selector).to.equal(registry.interface.getFunction("beaconOf")!.selector);
    expect(abi.getFunction("slotSigner")!.selector).to.equal(registry.interface.getFunction("slotSigner")!.selector);
    for(const name of ["isValidPublicKey","verifyRound"])expect(abi.getFunction(name)!.selector).to.equal(verifier.interface.getFunction(name)!.selector);
    const views=new ethers.Contract(await verifier.getAddress(),BEACON_ABI,ethers.provider);
    expect([await views.isValidPublicKey(preset.publicKey),await views.verifyRound(preset.publicKey,sample.round,sample.signature)]).to.deep.equal([true,true]);
    const receipt=(await(await owner.sendTransaction({to:address,data:abi.encodeFunctionData("registerBeacon",args)})).wait())!;
    console.log(`registerBeacon gas (evmnet, real sample round ${sample.round}): ${receipt.gasUsed} (script cap ${REGISTRATION_GAS_CAP})`);
    expect(receipt.gasUsed).to.be.lessThan(REGISTRATION_GAS_CAP);
    const registered=new ethers.Contract(address,BEACON_ABI,ethers.provider),{verifier:v,genesis,period,chainHash,publicKey:key}=await registered.beaconOf(6);
    expect({verifier:v,chainHash,publicKey:key,genesis,period}).to.deep.equal({verifier:await verifier.getAddress(),...DRAND_EVMNET});
    expect(await registered.slotSigner(6)).to.equal(beaconSlotSigner({verifier:v,chainHash,publicKey:key,genesis,period}));
  });

  it("refuses a beacon recipe that is not what registerBeacon appends, and leaves signed recipes alone",()=>{
    const beacon:BeaconRegistration={verifier:EPOCH_TEST_SIGNERS[0],...DRAND_EVMNET};
    const request=beaconCanonicalRequest(beacon.chainHash),other=beaconCanonicalRequest(ethers.id("another chain"));
    const good:EpochRecipe={canonicalRequest:request,template:BEACON_TEMPLATE,body:request,beacon};
    validateEpochRecipe(good);
    const registration=/Invalid beacon registration/,shape=/canonical request and body name its chain hash/;
    for(const [name,recipe,message] of [
      ["a request naming another chain",{...good,canonicalRequest:other,body:other},shape],["a body that is not the request",{...good,body:`${request} `},shape],
      ["a request with a stray space",{...good,canonicalRequest:request.replace("0x04","0x04 ")},shape],["another template",{...good,template:encodeDataTemplate([{integer:{minDigits:1,maxDigits:18}}])},shape],
      ["a zero verifier",{...good,beacon:{...beacon,verifier:ethers.ZeroAddress}},registration],["a zero chain hash",{...good,beacon:{...beacon,chainHash:ethers.ZeroHash}},registration],
      ["genesis 0",{...good,beacon:{...beacon,genesis:0n}},registration],["period 0",{...good,beacon:{...beacon,period:0n}},registration],
      ["a 127-byte key",{...good,beacon:{...beacon,publicKey:beacon.publicKey.slice(0,-2)}},registration],["a 129-byte key",{...good,beacon:{...beacon,publicKey:beacon.publicKey+"00"}},registration],
    ] as Array<[string,EpochRecipe,RegExp]>)expect(()=>validateEpochRecipe(recipe),name).to.throw(message);
    // The generic rules still apply first.
    expect(()=>validateEpochRecipe({...good,body:""})).to.throw("A recipe body must be");
    expect(()=>validateEpochRecipe({...good,template:"0x0305"})).to.throw("Invalid data template");
  });

  for(const {round,signature} of real.slice(1))it(`commits real drand round ${round} onchain and replays it`,async()=>{
    // Start the local chain before the round's time so the 240-second freshness bound holds.
    const signedAt=beaconRoundTime(DRAND_EVMNET,round),net=await network.create({override:{initialDate:new Date(Number(signedAt-3600n)*1000)}});
    try{
      const {registry,beacon,address}=await registryWithBeacon(net);
      await registry.scheduleCatalog([6],[beaconSlotSigner(beacon)],2);
      await expect(registry.scheduleCatalog([6],[EPOCH_TEST_SIGNERS[0]],2)).to.be.revertedWithCustomError(registry,"InvalidConfig");
      await net.networkHelpers.mine(Number(await registry.epochStart(2))-await net.ethers.provider.getBlockNumber());
      const [hash,recipes,signers]=await registry.catalogAt(2);
      const catalog=resolveEpochCatalog({registry:address,chainId:31337n,firstEpochStart:await registry.firstEpochStart(),recipeBook:await readEpochRecipes(net.ethers.provider,address,[6])},{hash,recipes,signers});
      const anchor=(await net.ethers.provider.getBlock(Number(await registry.epochStart(2))-1))!.hash!;
      const selected=selectEpoch(catalog,2n,anchor),onchain=await registry.getEpochSelection(2);
      expect([selected.recipe,selected.airnode,selected.queryHash,selected.template]).to.deep.equal([6,beaconSlotSigner(beacon),onchain.queryHash,BEACON_TEMPLATE]);
      expect(selected.beacon).to.deep.equal(beacon);
      const a={timestamp:signedAt,data:encodeBeaconRound(round),signature};
      // Each tampered attestation is refused by the contract and by replay, for the reason each names.
      const tampered:Array<[string,typeof a,string,string]>=[
        ["another round's signature",{...a,signature:real.find(r=>r.round!==round)!.signature},"InvalidSigner","Invalid beacon signature"],
        ["the previous round's number and time with this round's signature",{...a,timestamp:signedAt-3n,data:encodeBeaconRound(round-1n)},"InvalidSigner","Invalid beacon signature"],
        ["a signature of the wrong length",{...a,signature:`0x${"11".repeat(65)}`},"InvalidSigner","Invalid beacon signature"],
        ["a timestamp that is not the round's",{...a,timestamp:signedAt-3n},"InvalidTime","Invalid epoch attestation time"],
        ["the next round's number",{...a,data:encodeBeaconRound(round+1n)},"InvalidTime","Invalid epoch attestation time"],
        ["a timestamp after the block's",{...a,timestamp:signedAt+3n},"InvalidTime","Invalid epoch attestation time"],
        ["a leading zero",{...a,data:utf8(`0${round}`)},"InvalidData","Invalid exact epoch data"],
        ["a decimal point",{...a,data:utf8(`${round}.0`)},"InvalidData","Invalid exact epoch data"],
      ];
      await net.networkHelpers.time.setNextBlockTimestamp(signedAt+1n);
      for(const [name,bad,error,message] of tampered){
        await expect(registry.commitEpoch(2,bad),name).to.be.revertedWithCustomError(registry,error);
        expect(()=>verifyEpochAttestation(selected,bad,signedAt+1n),name).to.throw(message);
      }
      // A stale round, and a catalog signer that is not the registration's identity, are refused by replay.
      expect(()=>verifyEpochAttestation(selected,a,signedAt+241n)).to.throw("Invalid epoch attestation time");
      expect(()=>verifyEpochAttestation({...selected,airnode:EPOCH_TEST_SIGNERS[0]},a,signedAt+1n)).to.throw("Wrong epoch signer/query");
      expect(()=>verifyEpochAttestation({...selected,beacon:{...beacon,publicKey:beaconPublicKey(1n)}},a,signedAt+1n)).to.throw("Invalid beacon signature");
      await net.networkHelpers.time.setNextBlockTimestamp(signedAt+2n);
      const receipt=(await(await registry.commitEpoch(2,a)).wait())!,record=await registry.getEpoch(2);
      console.log(`Beacon epoch commit of round ${round}: gas=${receipt.gasUsed}`);
      expect([record.source,record.signedAt,record.dataHash]).to.deep.equal([0n,signedAt,ethers.keccak256(a.data)]);
      const packet=registry.interface.parseLog(receipt.logs[0])!.args.packet;
      const replayed=replayEpochCommitment({catalog,epochId:2n,record,commitTimestamp:signedAt+2n,packet});
      expect(replayed.epochHash).to.equal(record.epochHash);expect(replayed.signer).to.equal(beaconSlotSigner(beacon));
      expect(replayed.dataHash).to.equal(record.dataHash).and.to.equal(ethers.keccak256(utf8(String(round))));expect(replayed.attestationHash).to.equal(record.attestationHash);
      // Without the registration in its recipe book, replay would read the record as a signed one and refuse it.
      const {beacon:_,...signed}=catalog.recipeBook![6]!;
      expect(()=>replayEpochCommitment({catalog:{...catalog,recipeBook:{6:signed}},epochId:2n,record,commitTimestamp:signedAt+2n,packet})).to.throw("Expected canonical 65-byte low-s EIP-191 signature");
    }finally{await net.close();}
  });
});
