import {readFileSync} from "node:fs";
import {expect} from "chai";
import {network} from "hardhat";
import {BUILTIN_EPOCH_RECIPES,decodeDataTemplate,encodeDataTemplate,isValidDataTemplate,matchesDataTemplate,validateDataTemplate} from "../src/index.ts";
import {LEGACY_TEMPLATES,mutations,prng,randomTemplates,sampleRecord} from "./helpers/template-corpus.ts";

const {ethers}=await network.create();
type Case={template:string;valid:boolean;origin:string;note:string;data:string};
const fixture=JSON.parse(readFileSync(new URL("./fixtures/epoch-data-cases.json",import.meta.url),"utf8")) as
  {templates:Record<string,{template:string;legacyRecipe?:number;anu?:true}>;cases:Case[];templateValidity:Array<{template:string;valid:boolean;note:string}>};
const utf8=(text:string)=>ethers.hexlify(ethers.toUtf8Bytes(text));
// Records per eth_call; the previous validators spend an external call per record.
const BATCH=200,LEGACY_BATCH=200;
async function batched<T>(items:readonly T[],run:(chunk:T[])=>Promise<boolean[]>,size=BATCH):Promise<boolean[]>{
  const out:boolean[]=[];
  for(let i=0;i<items.length;i+=size)out.push(...await run(items.slice(i,i+size)));
  return out;
}
async function harnesses(){
  return {templates:await ethers.deployContract("DataTemplateHarness"),legacy:await ethers.deployContract("LegacyEpochDataValidator"),anu:await ethers.deployContract("LegacyAnuValidator")};
}
/// Onchain verdicts of the registry's interpreter and, when the template has one, of the previous implementation's validator.
async function verdicts(h:Awaited<ReturnType<typeof harnesses>>,shape:{template:string;legacyRecipe?:number;anu?:true},records:readonly string[]){
  const data=records.map(utf8);
  const onchain=await batched(data,chunk=>h.templates.verdicts(shape.template,chunk));
  const legacy=shape.legacyRecipe!==undefined?await batched(data,chunk=>h.legacy.verdicts(shape.legacyRecipe!,chunk),LEGACY_BATCH)
    :shape.anu?await batched(data,chunk=>h.anu.verdicts(chunk),LEGACY_BATCH):undefined;
  return {onchain,legacy,replay:data.map(d=>matchesDataTemplate(shape.template,d))};
}

describe("Data templates",function(){
  this.timeout(600_000);
  it("encodes the built-in record shapes as the specified segment bytes and decodes them back",()=>{
    // Hyperliquid: LITERAL(25) '{"symbol":"BTC","value":"', DECIMAL(fraction), LITERAL(2) '"}'.
    expect(BUILTIN_EPOCH_RECIPES[0].template).to.equal("0x0119"+utf8('{"symbol":"BTC","value":"').slice(2)+"0301"+"0102"+utf8('"}').slice(2));
    // dRPC: the JSON-RPC envelope around HEX(64).
    expect(BUILTIN_EPOCH_RECIPES[1].template).to.equal("0x0127"+utf8('{"id":null,"jsonrpc":"2.0","result":"0x').slice(2)+"0240"+"0102"+utf8('"}').slice(2));
    // TickerLayer: DECIMAL(fraction|exponent) price and size, INTEGER(1..16) timestamp.
    expect(BUILTIN_EPOCH_RECIPES[2].template).to.equal("0x011b"+utf8('{"symbol":"BTCUSD","price":').slice(2)+"0303"+"0108"+utf8(',"size":').slice(2)+"0303"+
      "010d"+utf8(',"timestamp":').slice(2)+"040110"+"0101"+utf8("}").slice(2));
    // Nodary: INTEGER(13..13) millisecond timestamp.
    expect(BUILTIN_EPOCH_RECIPES[4].template).to.contain("040d0d");
    for(const recipe of BUILTIN_EPOCH_RECIPES)expect(encodeDataTemplate(decodeDataTemplate(recipe.template))).to.equal(recipe.template);
    expect(BUILTIN_EPOCH_RECIPES.map(recipe=>recipe.template)).to.deep.equal([0,1,2,3,4,1].map(i=>BUILTIN_EPOCH_RECIPES[i].template));
    expect(matchesDataTemplate(LEGACY_TEMPLATES["anu-hex8-array"].template,utf8(`{"success":true,"type":"hex8","length":"4","data":["${"0123456789abcdef".repeat(1)}","${"f".repeat(16)}","${"0".repeat(16)}","${"a1".repeat(8)}"]}`))).to.equal(true);
  });

  it("names the broken rule when a readable template is invalid",()=>{
    for(const [segments,message] of [
      [[],/1 to 256 bytes/],
      [[{literal:"{"}],/at least one hex, decimal or integer/],
      [[{literal:""},{hex:1}],/literal length in bytes must be an integer from 1 to 128/],
      [[{hex:0}],/hex length must be an integer from 1 to 128/],
      [[{hex:129}],/hex length must be an integer from 1 to 128/],
      [[{integer:{minDigits:5,maxDigits:4}}],/minDigits <= maxDigits/],
      [[{decimal:{fraction:true}}],/boolean fraction and exponent/],
      [[{hex:1,literal:"x"}],/exactly one of literal, hex, decimal or integer/],
      [[{hex:128},{hex:1}],/shortest match exceeds 128 bytes/],
      [Array(129).fill({decimal:{fraction:false,exponent:false}}),/1 to 256 bytes/],
    ] as const)expect(()=>encodeDataTemplate(segments as never),JSON.stringify(segments).slice(0,60)).to.throw(message);
    expect(()=>validateDataTemplate("0x0305")).to.throw(/invalid decimal flags at byte 0/);
  });

  it("gives every fixture case the verdict of the contract, the replay library and the validator it replaces",async()=>{
    const h=await harnesses();
    expect(fixture.cases.length).to.be.greaterThan(2000);
    let compared=0,legacyCompared=0;
    for(const [name,shape] of Object.entries(fixture.templates)){
      const cases=fixture.cases.filter(c=>c.template===name);
      if(shape.legacyRecipe!==undefined||shape.anu)expect(LEGACY_TEMPLATES[name].template,name).to.equal(shape.template);
      const {onchain,legacy,replay}=await verdicts(h,shape,cases.map(c=>c.data));
      const mismatches=cases.filter((c,i)=>onchain[i]!==c.valid||replay[i]!==c.valid||(legacy!==undefined&&legacy[i]!==c.valid));
      expect(mismatches,name).to.deep.equal([]);
      compared+=cases.length;if(legacy)legacyCompared+=cases.length;
    }
    expect(compared).to.equal(fixture.cases.length);expect(legacyCompared).to.be.greaterThan(1000);
    expect(fixture.cases.filter(c=>c.origin==="sample").every(c=>c.valid)).to.equal(true);
  });

  it("agrees with the contract on every fixture template's well-formedness",async()=>{
    const h=await harnesses();
    const onchain=await batched(fixture.templateValidity.map(v=>v.template),chunk=>h.templates.validity(chunk));
    fixture.templateValidity.forEach((v,i)=>{
      expect(onchain[i],`${v.note} ${v.template.slice(0,40)}`).to.equal(v.valid);expect(isValidDataTemplate(v.template),v.note).to.equal(v.valid);
    });
    expect(fixture.templateValidity.filter(v=>v.valid).length).to.be.greaterThan(50);
    expect(fixture.templateValidity.filter(v=>!v.valid).length).to.be.greaterThan(200);
  });

  it("matches the previous validators on every single-byte mutation of every valid record",async()=>{
    const h=await harnesses();
    let total=0,accepted=0;
    for(const [name,shape] of Object.entries(LEGACY_TEMPLATES)){
      // Up to two real signed samples and two hand-written valid records per shape.
      const valid=fixture.cases.filter(c=>c.template===name&&c.valid);
      const bases=[...valid.filter(c=>c.origin==="sample").slice(0,2),...valid.filter(c=>c.origin==="hand").slice(0,2)].map(c=>c.data);
      const records=[...new Set(bases.flatMap(base=>[base,...mutations(base)]))];
      const {onchain,legacy,replay}=await verdicts(h,shape,records);
      const mismatches=records.filter((_,i)=>onchain[i]!==legacy![i]||replay[i]!==legacy![i]);
      expect(mismatches,name).to.deep.equal([]);
      total+=records.length;accepted+=legacy!.filter(Boolean).length;
    }
    // Digit substitutions keep many mutations valid, so both verdicts are exercised in bulk.
    expect(total).to.be.greaterThan(25000);expect(accepted).to.be.greaterThan(1500);
  });

  it("matches the contract on random templates and records built from them",async()=>{
    const h=await harnesses(),random=prng(0xa11ce);
    const seeds=Object.values(LEGACY_TEMPLATES).map(entry=>entry.template);
    const templates=randomTemplates(3000,0xbadc0de,seeds);
    const onchainValidity=await batched(templates,chunk=>h.templates.validity(chunk));
    templates.forEach((template,i)=>expect(onchainValidity[i],template).to.equal(isValidDataTemplate(template)));
    const valid=templates.filter(isValidDataTemplate).filter(t=>{try{decodeDataTemplate(t);return true;}catch{return false;}}).slice(0,150);
    let matched=0,compared=0;
    for(const template of valid){
      const built=Array.from({length:4},()=>sampleRecord(template,random));
      const records=[...new Set([...built,...built.flatMap(record=>{const all=mutations(record);return Array.from({length:10},()=>all[Math.floor(random()*all.length)]);})])];
      const {onchain,replay}=await verdicts(h,{template},records);
      expect(records.filter((_,i)=>onchain[i]!==replay[i]),template).to.deep.equal([]);
      matched+=onchain.filter(Boolean).length;compared+=records.length;
    }
    expect(valid.length).to.equal(150);expect(matched).to.be.greaterThan(300);expect(compared-matched).to.be.greaterThan(1000);
  });
});
