import {expect} from "chai";
import {network} from "hardhat";
import {deployReadyEpochFixture} from "./helpers/epoch.ts";
import {makeProof} from "./helpers/proof.ts";
const {ethers,networkHelpers}=await network.create();
const MIN=10n**15n,GWEI=10n**9n,BASE=200n*GWEI,GAS=200_000,OVERHEAD=300_000n;
const quote=(base:bigint,gas=BigInt(GAS))=>{const dynamic=5n*base*(OVERHEAD+gas);return dynamic>MIN?dynamic:MIN;};
// Keep the initializer defaults (multiplier 5, 300k overhead) instead of the shared flat fixture.
const fixture=()=>deployReadyEpochFixture(ethers,networkHelpers,MIN,false);
type Fixture=Awaited<ReturnType<typeof fixture>>;
// Explicit gasLimit skips estimation so the request executes in a block whose base fee is exactly `base`.
async function requestAt(c:Fixture,base:bigint,value:bigint,refund=c.user.address){
  await networkHelpers.setNextBlockBaseFeePerGas(base);
  return c.consumer.request(ethers.id("pricing"),GAS,refund,{value,gasLimit:500_000,maxFeePerGas:base*2n,maxPriorityFeePerGas:0n});
}
const identity=async(c:Fixture,escrow:bigint)=>expect(await ethers.provider.getBalance(await c.rng.getAddress())).to.equal(await c.rng.earnedFees()+await c.rng.totalKeeperCredits()+await c.rng.totalRefundCredits()+escrow);

describe("Base-fee pricing",()=>{
  it("initializes multiplier 5 with 300k overhead and charges exactly 5 × basefee × (300k + gas) in the requesting transaction",async()=>{
    const c=await networkHelpers.loadFixture(fixture),expected=quote(BASE);
    expect(Array.from(await c.rng.pricing())).to.deep.equal([MIN,5n,300000n]);
    expect(await c.rng.initialMinFee()).to.equal(MIN);
    expect(expected).to.equal(5n*BASE*(OVERHEAD+BigInt(GAS)));
    expect(await c.rng.quoteFeeAt(GAS,BASE)).to.equal(expected);
    await requestAt(c,BASE,expected);
    const [requested]=await c.rng.queryFilter(c.rng.filters.RandomnessRequested(1n));
    expect(requested.args.feePaid).to.equal(expected);
    expect(await c.rng.requestFeePaid(1n)).to.equal(expected);
    expect(await ethers.provider.getBalance(await c.rng.getAddress())).to.equal(expected);
    // The harness requires msg.value == quoteFee(200000): the same-transaction quote equals quoteFeeAt(gas, header base fee).
    await networkHelpers.setNextBlockBaseFeePerGas(BASE);
    await c.consumer.rollD20(c.user.address,{value:expected,gasLimit:500_000,maxFeePerGas:BASE*2n,maxPriorityFeePerGas:0n});
    expect(await c.rng.requestFeePaid(2n)).to.equal(expected);
  });
  it("lets minFee dominate at low base fees, runs flat at multiplier 0 and refuses quotes beyond uint96 escrow",async()=>{
    const c=await networkHelpers.loadFixture(fixture),low=GWEI/10n;
    expect(await c.rng.quoteFeeAt(GAS,0n)).to.equal(MIN);
    expect(await c.rng.quoteFeeAt(GAS,low)).to.equal(MIN);
    await requestAt(c,low,MIN);expect(await c.rng.requestFeePaid(1n)).to.equal(MIN);
    await expect(c.rng.quoteFeeAt(1_000_000,2n**96n)).to.be.revertedWithCustomError(c.rng,"FeeOverflow");
    await expect(c.rng.setPricing(MIN,0,300000)).to.emit(c.rng,"PricingChanged").withArgs(MIN,0,300000);
    expect(await c.rng.quoteFeeAt(GAS,BASE)).to.equal(MIN);
    await requestAt(c,BASE,MIN);expect(await c.rng.requestFeePaid(2n)).to.equal(MIN);
  });
  it("credits overpayment to the refund address, not the sender, as refund credit only that address can withdraw",async()=>{
    const c=await networkHelpers.loadFixture(fixture),expected=quote(BASE),consumer=await c.consumer.getAddress();
    await expect(requestAt(c,BASE,expected+7n,c.stranger.address)).to.emit(c.rng,"FeeOverpaymentCredited").withArgs(1n,c.stranger.address,7n);
    const [requested]=await c.rng.queryFilter(c.rng.filters.RandomnessRequested(1n));
    expect(requested.args.feePaid).to.equal(expected);expect(await c.rng.requestFeePaid(1n)).to.equal(expected);
    expect(await c.rng.refundCredits(c.stranger.address)).to.equal(7n);
    expect(await c.rng.refundCredits(consumer)).to.equal(0n);expect(await c.rng.refundCredits(c.owner.address)).to.equal(0n);
    expect(await c.rng.totalRefundCredits()).to.equal(7n);await identity(c,expected);
    await expect(c.rng.connect(c.user).withdrawRefundCredit(c.user.address)).to.be.revertedWithCustomError(c.rng,"NoRefundCredit");
    const before=await ethers.provider.getBalance(c.user.address);
    await expect(c.rng.connect(c.stranger).withdrawRefundCredit(c.user.address)).to.emit(c.rng,"RefundCreditWithdrawn").withArgs(c.stranger.address,c.user.address,7n);
    expect(await ethers.provider.getBalance(c.user.address)).to.equal(before+7n);
    expect(await c.rng.totalRefundCredits()).to.equal(0n);await identity(c,expected);
  });
  it("rejects underpayment with the quoted fee and creates no request",async()=>{
    const c=await networkHelpers.loadFixture(fixture),expected=quote(BASE);
    for(const value of [expected-1n,0n])
      await expect(requestAt(c,BASE,value)).to.be.revertedWithCustomError(c.rng,"IncorrectFee").withArgs(expected,value);
    expect(await c.rng.nextRequestId()).to.equal(1n);
    expect(await ethers.provider.getBalance(await c.rng.getAddress())).to.equal(0n);
  });
  it("settles a request from the fee it escrowed after a pricing change and refunds the next from its own",async()=>{
    const c=await networkHelpers.loadFixture(fixture);
    await c.rng.setKeeperFeeBps(5000);await c.rng.setPricing(MIN,0,300000);
    const configuration=await c.rng.protocolConfigurationHash();
    await requestAt(c,BASE,MIN);const a=1n;
    await networkHelpers.mine(2);const proof=makeProof(await c.rng.requestSeed(a));
    await expect(c.rng.setPricing(MIN,5,300000)).to.emit(c.rng,"PricingChanged").withArgs(MIN,5,300000);
    expect(await c.rng.protocolConfigurationHash()).to.equal(configuration);expect(await c.rng.initialMinFee()).to.equal(MIN);
    const feeB=quote(BASE);await requestAt(c,BASE,feeB);const b=2n;
    expect(await c.rng.requestFeePaid(a)).to.equal(MIN);expect(await c.rng.requestFeePaid(b)).to.equal(feeB);
    await identity(c,MIN+feeB);
    await expect(c.rng.fulfillRandomness(a,proof,{gasLimit:2_000_000})).to.emit(c.rng,"KeeperFeePaid").withArgs(a,c.owner.address,MIN/2n,true);
    expect(await c.rng.earnedFees()).to.equal(MIN/2n);await identity(c,feeB);
    await networkHelpers.time.increase(61);
    await expect(c.rng.refundRequest(b,{gasLimit:500000})).to.emit(c.rng,"RequestRefundedTo").withArgs(b,c.user.address,feeB,true);
    expect(await c.rng.earnedFees()).to.equal(MIN/2n);expect(await c.rng.totalRefundCredits()).to.equal(0n);await identity(c,0n);
  });
  it("bounds pricing parameters, restricts the setter to the owner and never changes the protocol configuration hash",async()=>{
    const c=await networkHelpers.loadFixture(fixture),configuration=await c.rng.protocolConfigurationHash();
    await expect(c.rng.connect(c.user).setPricing(MIN,5,300000)).to.be.revertedWithCustomError(c.rng,"OwnableUnauthorizedAccount").withArgs(c.user.address);
    // A zero minimum fee with a zero multiplier would make requests free.
    for(const [fee,multiplier,overhead] of [[10n**19n+1n,5,300000],[MIN,21,300000],[MIN,5,99_999],[MIN,5,2_000_001],[0n,0,100_000],[0n,0,300_000]] as const)
      await expect(c.rng.setPricing(fee,multiplier,overhead)).to.be.revertedWithCustomError(c.rng,"InvalidConfig");
    expect(Array.from(await c.rng.pricing())).to.deep.equal([MIN,5n,300000n]);
    await expect(c.rng.setPricing(10n**19n,20,2_000_000)).to.emit(c.rng,"PricingChanged").withArgs(10n**19n,20,2_000_000);
    expect(Array.from(await c.rng.pricing())).to.deep.equal([10n**19n,20n,2000000n]);
    expect(await c.rng.quoteFeeAt(1_000_000,BASE)).to.equal(20n*BASE*3_000_000n);
    // The lowest accepted pricing: no minimum, multiplier 1 over the base fee; a flat fee needs a nonzero minimum.
    await expect(c.rng.setPricing(0n,1,100_000)).to.emit(c.rng,"PricingChanged").withArgs(0n,1,100_000);
    expect(Array.from(await c.rng.pricing())).to.deep.equal([0n,1n,100000n]);
    expect(await c.rng.quoteFeeAt(GAS,BASE)).to.equal(BASE*(100_000n+BigInt(GAS)));
    await expect(c.rng.setPricing(1n,0,100_000)).to.emit(c.rng,"PricingChanged").withArgs(1n,0,100_000);
    expect(await c.rng.quoteFeeAt(GAS,BASE)).to.equal(1n);
    expect(await c.rng.minFee()).to.equal(1n);expect(await c.rng.initialMinFee()).to.equal(MIN);
    expect(await c.rng.protocolConfigurationHash()).to.equal(configuration);
  });
});
