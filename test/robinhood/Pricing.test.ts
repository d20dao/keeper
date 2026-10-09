import {expect} from "chai";
import {network} from "hardhat";
import {AbiCoder,keccak256} from "ethers";
import {publicKey} from "../helpers/proof.ts";
import {CONFIG_DOMAIN,ROUND_PRICING,deployRoundCoordinator,eventsOf,nextFee,registrationOf,request,serve,solvency,type RoundFixture} from "../helpers/robinhood.ts";

const {ethers,networkHelpers}=await network.create();
const abi=AbiCoder.defaultAbiCoder();

async function fixture(){
  const c=await deployRoundCoordinator(ethers,networkHelpers);
  const hostile=await ethers.deployContract("HostileRoundConsumer",[await c.coordinator.getAddress()]);
  const payee=await ethers.deployContract("RoundPayee",[await c.coordinator.getAddress()]);
  return {...c,hostile,payee};
}
type Fixture=Awaited<ReturnType<typeof fixture>>;
const MAX_UINT96=2n**96n-1n;
/// A fixed gas price for a transaction whose block's base fee a test sets, so that the sender's fee cap never falls below it.
const priced=(extra:object={})=>({maxFeePerGas:10n**11n,maxPriorityFeePerGas:0n,...extra});
/// initialize's parameters with some replaced, in InitParams order.
function initParams(c:RoundFixture,patch:Partial<Record<"publicKey"|"owner"|"feeRecipient"|"keeper"|"keeperBps"|"minFee"|"feeMultiplier"|"fulfillGasOverhead",unknown>>){
  const p={publicKey:publicKey(),owner:c.owner.address,feeRecipient:c.feeRecipient.address,keeper:c.keeper.address,keeperBps:ROUND_PRICING.keeperBps,
    minFee:ROUND_PRICING.minFee,feeMultiplier:ROUND_PRICING.feeMultiplier,fulfillGasOverhead:ROUND_PRICING.fulfillGasOverhead,...patch};
  return [p.publicKey,p.owner,p.feeRecipient,p.keeper,p.keeperBps,p.minFee,p.feeMultiplier,p.fulfillGasOverhead];
}
/// A new proxy of the fixture's implementation, initialized with these parameters and beacon.
const deployProxy=(c:RoundFixture,params:unknown[],beacon=c.beacon)=>
  ethers.deployContract("D20Proxy",[c.implementation.target,c.implementation.interface.encodeFunctionData("initialize",[params,registrationOf(beacon)])]);

describe("Round coordinator: construction and initialization",function(){
  it("needs a proof verifier with code, and refuses every invalid initialization",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    await expect(ethers.deployContract("D20VRFCoordinatorRobinhood",[c.stranger.address],{libraries:{LinkedRandomnessMapping:c.mapping.target}}))
      .to.be.revertedWithCustomError(c.implementation,"InvalidConfig");
    const refused:Array<[string,unknown[],string,any?]>=[
      ["a zero fee recipient",initParams(c,{feeRecipient:ethers.ZeroAddress}),"InvalidConfig"],
      ["a zero keeper",initParams(c,{keeper:ethers.ZeroAddress}),"InvalidConfig"],
      ["a keeper share above 100%",initParams(c,{keeperBps:10_001}),"InvalidConfig"],
      ["a public key off the curve",initParams(c,{publicKey:[1n,2n]}),"InvalidPublicKey"],
      ["a minimum fee above MAX_MIN_FEE",initParams(c,{minFee:10n**16n+1n}),"InvalidConfig"],
      ["a multiplier above 20",initParams(c,{feeMultiplier:21}),"InvalidConfig"],
      ["an overhead below 100,000",initParams(c,{fulfillGasOverhead:99_999}),"InvalidConfig"],
      ["an overhead above 2,000,000",initParams(c,{fulfillGasOverhead:2_000_001}),"InvalidConfig"],
      ["a free request",initParams(c,{minFee:0n,feeMultiplier:0}),"InvalidConfig"],
      ["a zero owner",initParams(c,{owner:ethers.ZeroAddress}),"OwnableInvalidOwner"],
      ["an invalid beacon",initParams(c,{}),"InvalidBeacon",{...c.beacon,period:11n}],
    ];
    for(const [name,params,error,beacon] of refused)
      await expect(deployProxy(c,params,beacon??c.beacon),name).to.be.revertedWithCustomError(c.implementation,error);
    // The bounds themselves are accepted.
    const edge=c.implementation.attach(await (await deployProxy(c,initParams(c,{keeperBps:10_000,minFee:0n,feeMultiplier:20,fulfillGasOverhead:2_000_000}))).getAddress()) as any;
    expect(Array.from(await edge.pricing())).to.deep.equal([0n,20n,2_000_000n]);
    expect(await edge.keeperFeeBps()).to.equal(10_000n);
    const flat=c.implementation.attach(await (await deployProxy(c,initParams(c,{keeperBps:0,minFee:10n**16n,feeMultiplier:0,fulfillGasOverhead:100_000}))).getAddress()) as any;
    expect(Array.from(await flat.pricing())).to.deep.equal([10n**16n,0n,100_000n]);
  });

  it("starts with the key, roles, pricing and configuration hash it was given, and nothing else",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const [x,y]=publicKey();
    expect([await c.coordinator.publicKeyX(),await c.coordinator.publicKeyY(),await c.coordinator.keyHash()]).to.deep.equal([x,y,keccak256(abi.encode(["uint256[2]"],[[x,y]]))]);
    expect([await c.coordinator.owner(),await c.coordinator.pendingOwner(),await c.coordinator.keeper(),await c.coordinator.backupKeeperCount()])
      .to.deep.equal([c.owner.address,ethers.ZeroAddress,c.keeper.address,0n]);
    expect([await c.coordinator.feeRecipient(),await c.coordinator.initialFeeRecipient(),await c.coordinator.keeperFeeBps(),await c.coordinator.refundBps()])
      .to.deep.equal([c.feeRecipient.address,c.feeRecipient.address,8000n,10_000n]);
    expect([await c.coordinator.minFee(),await c.coordinator.initialMinFee(),await c.coordinator.feeMultiplier(),await c.coordinator.fulfillGasOverhead()])
      .to.deep.equal([ROUND_PRICING.minFee,ROUND_PRICING.minFee,2n,360_000n]);
    expect([await c.coordinator.nextRequestId(),await c.coordinator.earnedFees(),await c.coordinator.lastServedRequestId(),await c.coordinator.lastServedIndex(),
      await c.coordinator.totalKeeperCredits(),await c.coordinator.totalRefundCredits()]).to.deep.equal([1n,0n,0n,0n,0n,0n]);
    expect(await c.coordinator.proofVerifier()).to.equal(c.proofVerifier.target);
    expect(await c.coordinator.protocolConfigurationHash()).to.equal(keccak256(abi.encode(["bytes32","uint256[2]","address","uint256","bytes32"],
      [CONFIG_DOMAIN,[x,y],c.feeRecipient.address,ROUND_PRICING.minFee,await c.coordinator.beaconIdentity(0)])));
    const [pending,cursor]=await c.coordinator.getPendingRequestIds(1,10);
    expect([Array.from(pending),cursor]).to.deep.equal([[],1n]);
  });
});

describe("Round coordinator: pricing",function(){
  it("bounds setPricing at MAX_MIN_FEE, the multiplier and the overhead, refuses free requests, and never changes the configuration hash",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    expect([await c.coordinator.MAX_MIN_FEE(),await c.coordinator.MAX_FEE_MULTIPLIER(),await c.coordinator.MIN_FULFILL_GAS_OVERHEAD(),
      await c.coordinator.MAX_FULFILL_GAS_OVERHEAD()]).to.deep.equal([10n**16n,20n,100_000n,2_000_000n]);
    const hash=await c.coordinator.protocolConfigurationHash();
    for(const [fee,multiplier,overhead] of [[10n**16n+1n,2,360_000],[1n,21,360_000],[1n,2,99_999],[1n,2,2_000_001],[0n,0,360_000],[0n,0,100_000]] as const)
      await expect(c.coordinator.setPricing(fee,multiplier,overhead),`${fee} ${multiplier} ${overhead}`).to.be.revertedWithCustomError(c.coordinator,"InvalidConfig");
    await expect(c.coordinator.connect(c.stranger).setPricing(1,1,100_000)).to.be.revertedWithCustomError(c.coordinator,"OwnableUnauthorizedAccount");
    for(const [fee,multiplier,overhead] of [[10n**16n,20,2_000_000],[0n,1,100_000],[1n,0,100_000],[10n**16n,0,2_000_000]] as const){
      await expect(c.coordinator.setPricing(fee,multiplier,overhead)).to.emit(c.coordinator,"PricingChanged").withArgs(fee,multiplier,overhead);
      expect(Array.from(await c.coordinator.pricing())).to.deep.equal([fee,BigInt(multiplier),BigInt(overhead)]);
    }
    expect(await c.coordinator.protocolConfigurationHash()).to.equal(hash);
    expect(await c.coordinator.initialMinFee()).to.equal(ROUND_PRICING.minFee);
  });

  it("quotes max(minFee, multiplier × base fee × (overhead + callback gas)), flat at multiplier 0",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const formula=(fee:bigint,multiplier:bigint,overhead:bigint,gas:bigint,base:bigint)=>{const d=multiplier*base*(overhead+gas);return d>fee?d:fee;};
    const settings:Array<[bigint,number,number]>=[[ROUND_PRICING.minFee,2,360_000],[0n,1,100_000],[10n**16n,20,2_000_000],[5n,0,250_000]];
    for(const [fee,multiplier,overhead] of settings){
      await c.coordinator.setPricing(fee,multiplier,overhead);
      for(const gas of [30_000n,100_000n,1_000_000n])for(const base of [0n,1n,20_000_000n,10n**9n,10n**12n])
        expect(await c.coordinator.quoteFeeAt(gas,base),`${fee}/${multiplier}/${overhead} gas ${gas} base ${base}`)
          .to.equal(formula(fee,BigInt(multiplier),BigInt(overhead),gas,base));
    }
    // At Robinhood's 0.02 gwei floor the deployment's minimum fee dominates up to the largest callback.
    await c.coordinator.setPricing(ROUND_PRICING.minFee,2,360_000);
    expect(await c.coordinator.quoteFeeAt(100_000,20_000_000n)).to.equal(ROUND_PRICING.minFee);
    expect(await c.coordinator.quoteFeeAt(1_000_000,20_000_000n)).to.equal(2n*20_000_000n*1_360_000n);
  });

  it("charges quoteFee of the requesting transaction's own base fee",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const base=10n**9n,expected=await c.coordinator.quoteFeeAt(250_000,base);
    expect(expected).to.be.greaterThan(ROUND_PRICING.minFee);
    await networkHelpers.setNextBlockBaseFeePerGas(base);
    const receipt=await (await c.hostile.requestAtQuote(ethers.id("quoted"),250_000,c.owner.address,priced({value:10n**16n}))).wait();
    const [requested]=eventsOf(c.coordinator,receipt,"RandomnessRequested");
    expect(requested.args.feePaid).to.equal(expected);
    expect(await c.coordinator.requestFeePaid(requested.args.requestId)).to.equal(expected);
    // Exactly the quote: no overpayment credit.
    expect(eventsOf(c.coordinator,receipt,"FeeOverpaymentCredited")).to.have.length(0);
  });

  it("refuses a quote beyond uint96 escrow with FeeOverflow, and quotes the largest uint96 fee exactly",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    // 2^96 - 1 = 3^2·5·7·13 · 17·241 · 97·257·673 · (2^48 + 1): with overhead + callback gas = 4095 × 97 = 397,215 and multiplier 1, a base fee
    // reaches the uint96 maximum exactly.
    await c.coordinator.setPricing(1,1,297_215);
    const gas=100_000n,units=397_215n,base=MAX_UINT96/units;
    expect(base*units).to.equal(MAX_UINT96);
    expect(await c.coordinator.quoteFeeAt(gas,base)).to.equal(MAX_UINT96);
    await expect(c.coordinator.quoteFeeAt(gas,base+1n)).to.be.revertedWithCustomError(c.coordinator,"FeeOverflow");
    await expect(c.coordinator.quoteFeeAt(gas,2n**128n)).to.be.revertedWithCustomError(c.coordinator,"FeeOverflow");
    // A base fee so large that the product leaves 256 bits is an arithmetic panic, not FeeOverflow (no chain has such a base fee).
    await expect(c.coordinator.quoteFeeAt(gas,2n**255n)).to.be.revertedWithPanic(0x11);
    // Multiplier 0 never overflows: the fee is flat.
    await c.coordinator.setPricing(7,0,297_215);
    expect(await c.coordinator.quoteFeeAt(gas,2n**255n)).to.equal(7n);
  });

  it("settles every open request from the fee it escrowed after a pricing change",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const before=await request(c,networkHelpers);
    await c.coordinator.setPricing(ROUND_PRICING.minFee*3n,0,360_000);
    const after=await request(c,networkHelpers);
    expect([before.fee,after.fee]).to.deep.equal([ROUND_PRICING.minFee,ROUND_PRICING.minFee*3n]);
    await c.coordinator.setPricing(10n**16n,20,2_000_000);
    const {receipt}=await serve(c,ethers,before.id);
    await expect(receipt).to.emit(c.coordinator,"KeeperFeePaid").withArgs(before.id,c.keeper.address,before.fee*8000n/10_000n,true);
    await networkHelpers.time.increase(61);
    await expect(c.coordinator.refundRequest(after.id)).to.emit(c.coordinator,"RequestRefundedTo").withArgs(after.id,c.owner.address,after.fee,true);
    expect(await c.coordinator.earnedFees()).to.equal(before.fee-before.fee*8000n/10_000n);
    const s=await solvency(c,ethers);
    expect(s.balance).to.equal(s.owed);
  });
});

describe("Round coordinator: request validation",function(){
  it("refuses an account, a constructor, a zero refund address, callback gas out of bounds, an underpayment and an invalid mapping, creating nothing",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const fee=await nextFee(c,networkHelpers,100_000);
    await expect(c.coordinator.connect(c.stranger).requestRandomness(ethers.id("eoa"),100_000,c.stranger.address,{value:fee}))
      .to.be.revertedWithCustomError(c.coordinator,"ContractConsumerRequired");
    await expect(c.coordinator.connect(c.stranger).requestMappedRandomness(ethers.id("eoa"),100_000,c.stranger.address,[1,0,6,1,0],{value:fee}))
      .to.be.revertedWithCustomError(c.coordinator,"ContractConsumerRequired");
    await expect(ethers.deployContract("ConstructorRoundRequester",[c.coordinator.target],{value:fee}))
      .to.be.revertedWithCustomError(c.coordinator,"ContractConsumerRequired");
    await expect(c.consumer.request(ethers.id("zero"),100_000,ethers.ZeroAddress,{value:fee})).to.be.revertedWithCustomError(c.coordinator,"InvalidRefundAddress");
    for(const gas of [0,29_999,1_000_001,2**32-1])
      await expect(c.consumer.request(ethers.id("gas"),gas,c.owner.address,{value:10n**16n}),`${gas}`).to.be.revertedWithCustomError(c.coordinator,"InvalidCallbackGas");
    // An underpayment names the fee of its own block, so it is mined at the base fee set for it.
    const exact=await nextFee(c,networkHelpers,100_000);
    await expect(c.consumer.request(ethers.id("short"),100_000,c.owner.address,{value:exact-1n,gasLimit:500_000}))
      .to.be.revertedWithCustomError(c.coordinator,"IncorrectFee").withArgs(exact,exact-1n);
    await expect(c.consumer.request(ethers.id("nothing"),100_000,c.owner.address)).to.be.revertedWithCustomError(c.coordinator,"IncorrectFee");
    const invalid=[[0,1,0,0,0],[1,0,1,1,0],[1,0,6,0,0],[1,0,6,129,0],[1,1,6,1,0],[2,0,0,2,0],[3,7,6,1,0],[4,0,0,1,0],[4,0,0,2,5],[4,0,0,1,257],
      [5,0,0,6,5],[5,0,0,0,5],[6,0,0,4,5],[4,1,0,1,5]];
    for(const spec of invalid)
      await expect(c.consumer.requestMapped(ethers.id("mapping"),100_000,c.owner.address,spec,{value:10n**16n}),`${spec}`)
        .to.be.revertedWithCustomError(c.coordinator,"InvalidMapping");
    expect(await c.coordinator.nextRequestId()).to.equal(1n);
    expect(await ethers.provider.getBalance(c.coordinator.target)).to.equal(0n);
  });

  it("accepts the callback gas bounds themselves, each at its own fee",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    expect([await c.coordinator.MIN_CALLBACK_GAS(),await c.coordinator.MAX_CALLBACK_GAS()]).to.deep.equal([30_000n,1_000_000n]);
    for(const gas of [30_000,1_000_000]){
      const {fee,request:q}=await request(c,networkHelpers,{gas});
      expect(q.callbackGasLimit).to.equal(BigInt(gas));
      expect(q.feePaid).to.equal(fee);
    }
    expect(await c.coordinator.nextRequestId()).to.equal(3n);
  });

  it("credits an overpayment to the refund address, which alone withdraws it, to any recipient it names",async()=>{
    const c:Fixture=await networkHelpers.loadFixture(fixture);
    const fee=await nextFee(c,networkHelpers,100_000),excess=12_345n;
    const receipt=await (await c.consumer.request(ethers.id("over"),100_000,c.payee.target,{value:fee+excess})).wait();
    const requestId=await c.consumer.lastRequestId();
    await expect(receipt).to.emit(c.coordinator,"FeeOverpaymentCredited").withArgs(requestId,c.payee.target,excess);
    expect(await c.coordinator.requestFeePaid(requestId)).to.equal(fee);
    expect([await c.coordinator.refundCredits(c.payee.target),await c.coordinator.totalRefundCredits()]).to.deep.equal([excess,excess]);
    // Not the sender, not the consumer: only the refund address.
    await expect(c.coordinator.connect(c.owner).withdrawRefundCredit(c.owner.address)).to.be.revertedWithCustomError(c.coordinator,"NoRefundCredit");
    const withdraw=(recipient:string)=>c.payee.execute(c.coordinator.target,c.coordinator.interface.encodeFunctionData("withdrawRefundCredit",[recipient]));
    await expect(withdraw(ethers.ZeroAddress)).to.be.revertedWithCustomError(c.coordinator,"InvalidRefundAddress");
    // A recipient that refuses the payment leaves the credit where it was.
    await c.payee.setMode(1);
    await expect(withdraw(c.payee.target as string)).to.be.revertedWithCustomError(c.coordinator,"TransferFailed");
    expect(await c.coordinator.refundCredits(c.payee.target)).to.equal(excess);
    const before=await ethers.provider.getBalance(c.stranger.address);
    await expect(withdraw(c.stranger.address)).to.emit(c.coordinator,"RefundCreditWithdrawn").withArgs(c.payee.target,c.stranger.address,excess);
    expect(await ethers.provider.getBalance(c.stranger.address)-before).to.equal(excess);
    expect([await c.coordinator.refundCredits(c.payee.target),await c.coordinator.totalRefundCredits()]).to.deep.equal([0n,0n]);
    await expect(withdraw(c.stranger.address)).to.be.revertedWithCustomError(c.coordinator,"NoRefundCredit");
    // The escrow itself is untouched: the request still settles its whole fee.
    const s=await solvency(c,ethers);
    expect([s.balance,s.escrow]).to.deep.equal([fee,fee]);
  });
});
