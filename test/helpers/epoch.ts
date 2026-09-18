import {deployProxy} from "./proxy.ts";
import { Wallet, toBeHex, hexlify, toUtf8Bytes, getBytes } from "ethers";
import { attestationDigest } from "../../src/sources.ts";
import { BUILTIN_EPOCH_RECIPES, type EpochProvider } from "../../src/epoch.ts";
import { publicKey } from "./proof.ts";
// Public test keys, never production signers. EPOCH_TEST_WALLETS sign the initial catalog's four slots.
export const EPOCH_TEST_WALLETS=[11n,12n,13n,14n].map(n=>new Wallet(toBeHex(n,32)));
export const EPOCH_TEST_SIGNERS=EPOCH_TEST_WALLETS.map(w=>w.address);
// One test Airnode per provider for scheduled catalogs, as in production where a provider's recipes share a signer.
export const PROVIDER_TEST_WALLETS:Record<EpochProvider,Wallet>={hyperliquid:new Wallet(toBeHex(21n,32)),drpc:new Wallet(toBeHex(22n,32)),tickerlayer:new Wallet(toBeHex(23n,32)),nodary:new Wallet(toBeHex(24n,32))};
/// The rollout catalog: neighbouring slots, including the last and the first, never share a provider.
export const DEFAULT_TEST_RECIPES=[0,1,2,4,5];
export function providerTestCatalog(recipes:readonly number[]=DEFAULT_TEST_RECIPES){
  return {recipes:[...recipes],signers:recipes.map(recipe=>PROVIDER_TEST_WALLETS[BUILTIN_EPOCH_RECIPES[recipe].provider].address)};
}
export function testWalletFor(address:string):Wallet{
  const wallet=[...EPOCH_TEST_WALLETS,...Object.values(PROVIDER_TEST_WALLETS)].find(w=>w.address.toLowerCase()===address.toLowerCase());
  if(!wallet)throw new Error("Not a test Airnode");
  return wallet;
}
/// Valid signed data for each built-in recipe, shaped like the real samples.
export function epochFixtureData(recipe:number):object{
  switch(recipe){
    case 0:return {symbol:"BTC",value:"123.45"};
    case 1:case 5:return {id:null,jsonrpc:"2.0",result:"0x"+"0123456789abcdef".repeat(4)};
    case 2:case 3:return {symbol:recipe===2?"BTCUSD":"ETHUSD",price:recipe===2?76000.1:2400.2,size:6e-8,timestamp:1789490939269};
    case 4:return {"ETH/USD":{value:2458.9,timestamp:1789653374566,category:"crypto"}};
  }
  throw new Error("Invalid fixture recipe");
}
/// Sign a body (default: the recipe's fixture data) for a selection, with the test Airnode it names.
export async function signSelection(selection:{recipe:bigint|number;airnode:string;queryHash:string},timestamp:bigint,body?:string){
  const a={timestamp,data:hexlify(toUtf8Bytes(body??JSON.stringify(epochFixtureData(Number(selection.recipe))))),signature:"0x"};
  a.signature=await testWalletFor(selection.airnode).signMessage(getBytes(attestationDigest(selection.queryHash,a)));
  return a;
}
export async function epochAttestation(registry:any,epochId:bigint,ethers:any,body?:string,attempt=0) {
  const selected=await registry.getEpochFallbackSelection(epochId,attempt);
  return signSelection(selected,BigInt((await ethers.provider.getBlock("latest")).timestamp),body);
}
export async function deployReadyEpochFixture(ethers:any,networkHelpers:any,fee=10n**15n,flat=true) {
  const [owner,user,stranger]=await ethers.getSigners();
  const registry=await deployProxy(ethers,"EpochEntropy",[EPOCH_TEST_SIGNERS,owner.address,owner.address]);
  await networkHelpers.mine(Number(await registry.firstEpochStart())-await ethers.provider.getBlockNumber());
  await registry.commitEpoch(1,await epochAttestation(registry,1n,ethers));
  const rng=await deployProxy(ethers,"D20VRFCoordinator",[publicKey(),owner.address,owner.address,fee,1,await registry.getAddress(),0]);
  // Flat pricing keeps exact-fee assertions valid; base-fee quoting is covered by test/Pricing.test.ts.
  if(flat)await rng.setPricing(fee,0,300000);
  const consumer=await ethers.deployContract("TestConsumer",[await rng.getAddress()]);
  return {registry,rng,consumer,owner,user,stranger};
}
