import {expect} from "chai";
import {network} from "hardhat";
import {deployReadyEpochFixture} from "./helpers/epoch.ts";
const {ethers,networkHelpers}=await network.create();
const FEE=123n;
async function fixture(){const c=await deployReadyEpochFixture(ethers,networkHelpers,FEE);const consumer=await ethers.deployContract("TestRefundConsumer",[await c.rng.getAddress()]);return {...c,refundConsumer:consumer};}
async function request(c:Awaited<ReturnType<typeof fixture>>,recipient=c.user.address){await c.refundConsumer.request(recipient,{value:FEE});return await c.refundConsumer.lastRequestId() as bigint;}
describe("Refund lifecycle notification",()=>{
 it("settles the fee before notifying the original consumer, with only the request ID",async()=>{
  const c=await networkHelpers.loadFixture(fixture),id=await request(c);await networkHelpers.time.increase(61);
  const balance=await ethers.provider.getBalance(c.user.address);
  await expect(c.rng.connect(c.stranger).refundRequest(id,{gasLimit:500000})).to.emit(c.rng,"RefundCallbackAttempted").withArgs(id,await c.refundConsumer.getAddress(),true,100000);
  expect(await ethers.provider.getBalance(c.user.address)).to.equal(balance+FEE);
  expect(await c.refundConsumer.lastRefundId()).to.equal(id);expect(await c.refundConsumer.refundCount()).to.equal(1);
  expect(await c.rng.refundCallbackDelivered(id)).to.equal(true);
  await expect(c.rng.refundRequest(id)).to.be.revertedWithCustomError(c.rng,"RefundNotAvailable");
  await expect(c.rng.retryRefundCallback(id,100000)).to.be.revertedWithCustomError(c.rng,"RefundCallbackAlreadyDelivered");
 });
 for(const mode of [0,2])it(`automatic gas estimation retains the notification budget (mode ${mode})`,async()=>{
  const c=await networkHelpers.loadFixture(fixture),id=await request(c);await c.refundConsumer.setMode(mode);await networkHelpers.time.increase(61);
  await expect(c.rng.refundRequest(id)).to.emit(c.rng,"RefundCallbackAttempted").withArgs(id,await c.refundConsumer.getAddress(),mode===0,100000);
  expect((await c.rng.getRequest(id)).refunded).to.equal(true);
 });
 for(const mode of [1,2,3,5])it(`isolates callback failure / gas / return-data attacks (mode ${mode})`,async()=>{
  const c=await networkHelpers.loadFixture(fixture),id=await request(c);await c.refundConsumer.setMode(mode);await networkHelpers.time.increase(61);
  const balance=await ethers.provider.getBalance(c.user.address);
  await expect(c.rng.refundRequest(id,{gasLimit:600000})).to.emit(c.rng,"RefundCallbackAttempted").withArgs(id,await c.refundConsumer.getAddress(),false,100000);
  expect((await c.rng.getRequest(id)).refunded).to.equal(true);expect(await c.rng.refundCallbackDelivered(id)).to.equal(false);
  expect(await ethers.provider.getBalance(c.user.address)).to.equal(balance+FEE);
  await c.refundConsumer.setMode(0);await c.rng.connect(c.stranger).retryRefundCallback(id,200000,{gasLimit:400000});
  expect(await c.rng.refundCallbackDelivered(id)).to.equal(true);expect(await c.refundConsumer.refundCount()).to.equal(1);
  expect(await ethers.provider.getBalance(c.user.address)).to.equal(balance+FEE);
 });
 it("notifies even when the original refund address receives backed credit",async()=>{
  const c=await networkHelpers.loadFixture(fixture),rejector=await ethers.deployContract("RejectingRefundRecipient");const id=await request(c,await rejector.getAddress());await networkHelpers.time.increase(61);
  await c.rng.refundRequest(id,{gasLimit:500000});expect(await c.rng.refundCredits(await rejector.getAddress())).to.equal(FEE);
  expect(await c.rng.totalRefundCredits()).to.equal(FEE);expect(await c.refundConsumer.lastRefundId()).to.equal(id);
  expect(await ethers.provider.getBalance(await c.rng.getAddress())).to.equal(FEE);
 });
 it("blocks reentry into refund, credit withdrawal and notification retry",async()=>{
  const c=await networkHelpers.loadFixture(fixture),id=await request(c);await c.refundConsumer.setMode(4);await networkHelpers.time.increase(61);
  await c.rng.refundRequest(id,{gasLimit:500000});expect(await c.refundConsumer.reentrySucceeded()).to.equal(false);
  expect(await c.refundConsumer.refundCount()).to.equal(1);expect(await c.rng.refundCallbackDelivered(id)).to.equal(true);
 });
 it("keeps old consumers refundable and authenticates callbacks",async()=>{
  const c=await networkHelpers.loadFixture(fixture),old=await ethers.deployContract("LegacyRefundConsumer",[await c.rng.getAddress()]);
  await expect(c.refundConsumer.onRefund(1)).to.be.revertedWithCustomError(c.refundConsumer,"OnlyCoordinator");
  await old.request(c.user.address,{value:FEE});const id=await old.lastRequestId();await networkHelpers.time.increase(61);
  await expect(c.rng.refundRequest(id,{gasLimit:500000})).to.emit(c.rng,"RequestRefundedTo").withArgs(id,c.user.address,FEE,true);
  expect(await c.rng.refundCallbackDelivered(id)).to.equal(false);
 });
 for(const mode of [6,7])it(`rejects a fully funded reentrant ${mode===6?"raw":"mapped"} randomness request by the guard`,async()=>{
  const c=await networkHelpers.loadFixture(fixture),id=await request(c);
  await c.refundConsumer.fund({value:FEE});await c.refundConsumer.setMode(1);await networkHelpers.time.increase(61);
  await c.rng.refundRequest(id,{gasLimit:500000});await c.refundConsumer.setMode(mode);
  const next=await c.rng.nextRequestId();
  await c.rng.retryRefundCallback(id,500000,{gasLimit:650000});
  expect(await c.refundConsumer.refundCount()).to.equal(1);
  expect(await c.refundConsumer.reentrySucceeded()).to.equal(false);
  expect(await c.refundConsumer.reentryError()).to.equal(ethers.id("ReentrancyGuardReentrantCall()").slice(0,10));
  expect(await c.rng.nextRequestId()).to.equal(next);
  expect(await ethers.provider.getBalance(await c.refundConsumer.getAddress())).to.equal(FEE);
 });
 it("also blocks a new paid randomness request from the native refund recipient's receive hook",async()=>{
  const c=await networkHelpers.loadFixture(fixture),receiver=await ethers.deployContract("ReentrantRefundReceiver",[await c.rng.getAddress()]);
  const id=await request(c,await receiver.getAddress());await networkHelpers.time.increase(61);const next=await c.rng.nextRequestId();
  await expect(c.rng.refundRequest(id,{gasLimit:600000})).to.emit(receiver,"ReentryAttempted").withArgs(false,ethers.id("ReentrancyGuardReentrantCall()").slice(0,10));
  expect(await c.rng.nextRequestId()).to.equal(next);expect(await ethers.provider.getBalance(await receiver.getAddress())).to.equal(FEE);
 });
 it("requires refund eligibility and enough callback gas without changing state",async()=>{
  const c=await networkHelpers.loadFixture(fixture),id=await request(c);
  await expect(c.rng.retryRefundCallback(id,100000)).to.be.revertedWithCustomError(c.rng,"NotRefunded");
  await expect(c.rng.refundRequest(id)).to.be.revertedWithCustomError(c.rng,"RefundNotAvailable");
  await networkHelpers.time.increase(61);
  await expect(c.rng.refundRequest(id,{gasLimit:150000})).to.be.revertedWithCustomError(c.rng,"InsufficientCallbackGas");
  expect((await c.rng.getRequest(id)).refunded).to.equal(false);expect(await c.rng.totalRefundCredits()).to.equal(0);
 });
});
