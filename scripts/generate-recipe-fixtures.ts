// Regenerates the recipe fixtures shared by EpochEntropy's tests, the replay library and the keeper:
// - test/fixtures/builtin-recipes.json: the built-in recipes; test/EpochRecipes.test.ts checks them against the contract.
// - test/fixtures/epoch-data-cases.json: data-template verdicts. Hand-written cases are kept; real signed samples,
//   mutations and random templates are rebuilt deterministically. Verdicts come from the replay library;
//   test/DataTemplate.test.ts proves they equal the contract's and, for every shape a previous implementation
//   validated in code, that implementation's verdicts.
// Usage: node scripts/generate-recipe-fixtures.ts
import {readFileSync,writeFileSync} from "node:fs";
import {decodeDataTemplate,isValidDataTemplate,matchesDataTemplate} from "../src/templates.ts";
import {BUILTIN_EPOCH_RECIPES} from "../src/epoch.ts";
import {LEGACY_RECIPE_TEMPLATE,LEGACY_TEMPLATES,mutations,prng,randomTemplates,sampleRecord} from "../test/helpers/template-corpus.ts";
import {id,toUtf8Bytes} from "ethers";

const path="test/fixtures/epoch-data-cases.json";
const json=(file:string)=>JSON.parse(readFileSync(file,"utf8"));
type Case={template:string;valid:boolean;origin:string;note:string;data:string};
const previous=json(path) as {cases:Array<Partial<Case>&{recipe?:number}>};
const cases:Case[]=[],seen=new Set<string>();
const templates:Record<string,{template:string;legacyRecipe?:number;anu?:true}>={};
for(const [name,entry] of Object.entries(LEGACY_TEMPLATES))templates[name]={...entry};
function add(template:string,origin:string,note:string,data:string){
  const key=JSON.stringify([template,data]);
  if(seen.has(key))return;
  seen.add(key);cases.push({template,valid:matchesDataTemplate(templates[template].template,toUtf8Bytes(data)),origin,note,data});
}
// Hand-written cases, converted once from the 96cc722 per-recipe fixture.
for(const c of previous.cases)if(c.recipe!==undefined||c.origin==="hand")add(c.template??LEGACY_RECIPE_TEMPLATE[c.recipe!],"hand",c.note!,c.data!);
const handVerdicts=previous.cases.filter(c=>c.recipe!==undefined||c.origin==="hand");
for(const c of handVerdicts){
  const name=c.template??LEGACY_RECIPE_TEMPLATE[c.recipe!],found=cases.find(x=>x.template===name&&x.data===c.data)!;
  if(found.valid!==c.valid)throw new Error(`Hand case changed verdict: ${name} ${c.note}`);
}
const anu=(values:string[])=>`{"success":true,"type":"hex8","length":"4","data":[${values.map(v=>`"${v}"`).join(",")}]}`;
for(const [note,data] of [["four hex8 values",anu(["0123456789abcdef","fedcba9876543210","0000000000000000","ffffffffffffffff"])],
  ["uppercase hex",anu(["0123456789ABCDEF","fedcba9876543210","0000000000000000","ffffffffffffffff"])],
  ["three values",anu(["0123456789abcdef","fedcba9876543210","0000000000000000"]).replace('"length":"4"','"length":"3"')],
  ["short value",anu(["0123456789abcde","fedcba9876543210","0000000000000000","ffffffffffffffff"])],
  ["numeric length",anu(["0123456789abcdef","fedcba9876543210","0000000000000000","ffffffffffffffff"]).replace('"length":"4"','"length":4')]] as const)
  add("anu-hex8-array","hand",note,data);

// Real signed records from the providers.
const airnode=json("test/fixtures/airnode-recipes-2026-09-17.json");
const sampleTemplate:Record<string,string>={"hyperliquid-btc-day-volume":"hyperliquid-btc-volume","drpc-ethereum-blockhash":"block-hash","drpc-base-blockhash":"block-hash",
  "tickerlayer-btcusd":"tickerlayer-btcusd","tickerlayer-ethusd":"tickerlayer-ethusd","nodary-eth-usd":"nodary-eth-usd","hyperliquid-sol-mid":"hyperliquid-sol-mid","nodary-btc-usd":"nodary-btc-usd"};
const signedText=(data:unknown)=>typeof data==="string"?data:JSON.stringify(data);
for(const recipe of airnode.recipes)for(const sample of recipe.samples)add(sampleTemplate[recipe.name],"sample",`signed ${recipe.name} response`,signedText(sample.data));
for(const row of json("test/fixtures/tickerlayer-2026-09-15.json").rows)add(`tickerlayer-${row.symbol.toLowerCase()}`,"sample","signed TickerLayer response",signedText(row.envelope.data));
for(const row of json("test/fixtures/tickerlayer-numeric-js.json").rows)add("tickerlayer-btcusd","sample","JSON.stringify numeric boundary",row.exact);
if(BUILTIN_EPOCH_RECIPES.some(recipe=>!Object.values(templates).some(entry=>entry.template===recipe.template)))throw new Error("A built-in template has no fixture shape");

// Mutations of every valid record: every template sees a deterministic sample of them.
const random=prng(0xd20da0);
const bases=cases.filter(c=>c.valid).map(c=>c.data);
for(const name of Object.keys(LEGACY_TEMPLATES)){
  const own=cases.filter(c=>c.valid&&c.template===name).map(c=>c.data);
  for(const base of own){const all=mutations(base);for(let i=0;i<all.length;i+=Math.max(1,Math.floor(all.length/16)))add(name,"mutation",`mutation of a valid ${name} record`,all[i]);}
  for(const other of bases)if(random()<0.15)add(name,"cross",`another shape's record`,other);
}
// Random well-formed templates with records built from them and their mutations.
const validity:Array<{template:string;valid:boolean;note:string}>=[];
const seeds=Object.values(LEGACY_TEMPLATES).map(entry=>entry.template);
for(const [note,template] of [["empty","0x"],["unknown opcode","0x05"],["literal without length","0x01"],["zero-length literal","0x0100"],
  ["truncated literal","0x01027b"],["literal over 128 bytes","0x0181"+"61".repeat(129)+"0301"],["hex without length","0x02"],["zero hex length","0x0200"],
  ["hex over 128","0x0281"],["decimal without flags","0x03"],["decimal flags 4","0x0304"],["integer without bounds","0x0401"],["integer min 0","0x04000a"],
  ["integer min above max","0x040503"],["integer max over 128","0x040181"],["no variable segment","0x01017b"],["shortest match over 128","0x028002800301"],
  ["over 256 bytes","0x"+"0301".repeat(129)],["hex 128 exactly","0x0280"],["decimal only","0x0303"],["integer 1..128","0x040180"],["256 bytes","0x"+"0300".repeat(128)]] as const)
  validity.push({template,valid:isValidDataTemplate(template),note});
for(const template of randomTemplates(400,0x5eed,seeds))validity.push({template,valid:isValidDataTemplate(template),note:"random"});
const generated=randomTemplates(4000,0xc0ffee,seeds).filter(template=>{
  if(!isValidDataTemplate(template))return false;
  try{decodeDataTemplate(template);return true;}catch{return false;} // Records are text: skip literals that are not UTF-8.
}).slice(0,40);
generated.forEach((template,index)=>{
  const name=`random-${index}`;templates[name]={template};
  for(let n=0;n<4;n++){
    const record=sampleRecord(template,random);add(name,"random","record built from a random template",record);
    const all=mutations(record);for(let i=0;i<6;i++)add(name,"random-mutation","mutation of a random template record",all[Math.floor(random()*all.length)]);
  }
});
const fixture={note:"Data-template verdicts shared by EpochEntropy's DataTemplate, the replay library and the keeper. Shapes with legacyRecipe (96cc722 recipe id) or anu (640b60c source 1) are also checked against those implementations' own validators. Regenerate with node scripts/generate-recipe-fixtures.ts.",
  templates,cases,templateValidity:validity};
// One entry per line keeps the fixture reviewable in diffs.
const lines=(items:readonly unknown[])=>items.map(item=>"  "+JSON.stringify(item)).join(",\n");
writeFileSync(path,`{"note":${JSON.stringify(fixture.note)},\n"templates":{\n${Object.entries(templates).map(([name,entry])=>`  ${JSON.stringify(name)}:${JSON.stringify(entry)}`).join(",\n")}\n},\n"cases":[\n${lines(cases)}\n],\n"templateValidity":[\n${lines(validity)}\n]}\n`);
console.log(JSON.stringify({cases:cases.length,valid:cases.filter(c=>c.valid).length,templates:Object.keys(templates).length,templateValidity:validity.length,validTemplates:validity.filter(v=>v.valid).length}));
writeFileSync("test/fixtures/builtin-recipes.json",JSON.stringify({note:"The recipes EpochEntropy registers itself, with exactly these ids. Provider and description are labels, not onchain data. Regenerate with node scripts/generate-recipe-fixtures.ts.",
  recipes:BUILTIN_EPOCH_RECIPES.map(r=>({id:r.id,provider:r.provider,description:r.description,canonicalRequest:r.canonicalRequest,queryHash:id(r.canonicalRequest),template:r.template,body:r.body}))},null,2)+"\n");
