import {test} from "node:test";
import assert from "node:assert/strict";
import {readFileSync,readdirSync} from "node:fs";
import {Wallet,getBytes,hexlify,id,toBeHex,toUtf8Bytes} from "ethers";
import {checkRecipeFile,RecipeFileError} from "../lib/recipe-file.ts";
import {attestationDigest} from "../../src/sources.ts";
import {BUILTIN_EPOCH_RECIPES} from "../../src/epoch.ts";

const example=JSON.parse(readFileSync("config/recipes/nodary-btc-usd.json","utf8"));
const rejects=(file:unknown,message:RegExp)=>assert.throws(()=>checkRecipeFile(JSON.stringify(file)),(error:Error)=>error instanceof RecipeFileError&&message.test(error.message),String(message));

test("the example recipe files pass every self-check with their real signed samples",()=>{
  for(const name of readdirSync("config/recipes")){
    const checked=checkRecipeFile(readFileSync(`config/recipes/${name}`,"utf8"));
    assert.equal(checked.queryHash,id(checked.recipe.canonicalRequest));
    assert.ok(!BUILTIN_EPOCH_RECIPES.some(recipe=>recipe.canonicalRequest===checked.recipe.canonicalRequest),`${name} is already built in`);
  }
  const checked=checkRecipeFile(JSON.stringify(example));
  assert.equal(checked.recipe.canonicalRequest,'["latestFeeds",[["name","BTC/USD"]]]');
  assert.equal(checked.recipe.body,'{"operation":"latestFeeds","parameters":{"name":"BTC/USD"}}');
  // A body given as JSON text is kept byte for byte.
  assert.equal(checkRecipeFile(JSON.stringify({...example,body:'{"operation":"latestFeeds","parameters":{"name":"BTC/USD"}}'})).recipe.body,checked.recipe.body);
});

test("every failed self-check names the rule it broke",()=>{
  assert.throws(()=>checkRecipeFile("{"),/not valid JSON/);
  rejects({...example,extra:1},/Unknown recipe file fields: extra/);
  rejects({...example,signer:"0x1234"},/signer is not an address/);
  rejects({...example,signer:"0x"+"00".repeat(20)},/zero address/);
  rejects({...example,body:{operation:"latestFeeds",parameters:[]}},/body is not a gateway request/);
  rejects({...example,template:[{literal:"{"}]},/template is invalid: .*at least one hex, decimal or integer/);
  rejects({...example,template:[{hex:0}]},/template is invalid: .*hex length/);
  rejects({...example,body:{operation:"latestFeeds",parameters:{name:"ETH/USD"}}},/is not the canonical request hash .* the gateway signed a different request/);
  rejects({...example,sample:{...example.sample,airnode:"0x509F4275Cbe2E2201cc5444bAc8948E3cc7c665B"}},/sample.airnode .* is not the recipe signer/);
  rejects({...example,sample:{...example.sample,timestamp:1789653418}},/sample.timestamp must be the signed decimal timestamp string/);
  rejects({...example,sample:{...example.sample,signature:example.sample.signature.slice(0,-2)}},/not a canonical 65-byte low-s signature/);
  rejects({...example,sample:{...example.sample,timestamp:"1789653419"}},/recovers to .* not the signer/);
  const text=JSON.stringify(example.sample.data).replace("76634.54000000001","76634.54000000002");
  rejects({...example,sample:{...example.sample,data:text}},/recovers to .* not the signer/);
  const oversized={...example,body:{operation:"latestFeeds",parameters:{name:"x".repeat(1100)}}};
  rejects(oversized,/exceeds the registry bounds: A recipe canonical request must be 1 to 1024 bytes/);
});

test("a correctly signed record outside the template is refused",async()=>{
  const wallet=new Wallet(toBeHex(77n,32));
  const canonical='["latestFeeds",[["name","BTC/USD"]]]',data='{"BTC/USD":{"value":1,"timestamp":178965341809,"category":"crypto"}}';
  const attestation={timestamp:1789653418n,data:hexlify(toUtf8Bytes(data)),signature:"0x"};
  const signature=await wallet.signMessage(getBytes(attestationDigest(id(canonical),attestation)));
  rejects({...example,signer:wallet.address,sample:{airnode:wallet.address,requestHash:id(canonical),timestamp:"1789653418",data,signature}},/does not match the template/);
});
