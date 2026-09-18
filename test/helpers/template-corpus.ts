// Deterministic differential corpus for data templates: the records the previous per-recipe validators accepted,
// their byte-level mutations, and random templates with records built from them. Test and fixture generation only.
import {getBytes,hexlify,toUtf8Bytes,toUtf8String} from "ethers";
import {BUILTIN_EPOCH_RECIPES} from "../../src/epoch.ts";
import {encodeDataTemplate,isValidDataTemplate,type DataTemplateSegment} from "../../src/templates.ts";

const number:DataTemplateSegment={decimal:{fraction:true,exponent:true}};
const decimalString:DataTemplateSegment={decimal:{fraction:true,exponent:false}};
const feed=(name:string):DataTemplateSegment[]=>[{literal:`{"${name}":{"value":`},number,{literal:',"timestamp":'},{integer:{minDigits:13,maxDigits:13}},{literal:',"category":"crypto"}}'}];
/// Templates of every shape a previous implementation validated in code. legacyRecipe is the 96cc722 recipe id checked by
/// LegacyEpochDataValidator; the ANU array is the 640b60c source 1 shape checked by LegacyAnuValidator.
export const LEGACY_TEMPLATES:Record<string,{template:string;legacyRecipe?:number;anu?:true}>={
  "hyperliquid-btc-volume":{template:BUILTIN_EPOCH_RECIPES[0].template,legacyRecipe:0},
  "block-hash":{template:BUILTIN_EPOCH_RECIPES[1].template,legacyRecipe:1},
  "tickerlayer-btcusd":{template:BUILTIN_EPOCH_RECIPES[2].template,legacyRecipe:2},
  "tickerlayer-ethusd":{template:BUILTIN_EPOCH_RECIPES[3].template,legacyRecipe:3},
  "nodary-eth-usd":{template:BUILTIN_EPOCH_RECIPES[4].template,legacyRecipe:4},
  "hyperliquid-sol-mid":{template:encodeDataTemplate([{literal:'{"mid":"'},decimalString,{literal:'"}'}]),legacyRecipe:5},
  "nodary-btc-usd":{template:encodeDataTemplate(feed("BTC/USD")),legacyRecipe:7},
  "anu-hex8-array":{template:encodeDataTemplate([{literal:'{"success":true,"type":"hex8","length":"4","data":["'},{hex:16},
    {literal:'","'},{hex:16},{literal:'","'},{hex:16},{literal:'","'},{hex:16},{literal:'"]}'}]),anu:true},
};
/// The 96cc722 recipe ids that shared a validator with a named template.
export const LEGACY_RECIPE_TEMPLATE:Record<number,string>={0:"hyperliquid-btc-volume",1:"block-hash",2:"tickerlayer-btcusd",3:"tickerlayer-ethusd",
  4:"nodary-eth-usd",5:"hyperliquid-sol-mid",6:"block-hash",7:"nodary-btc-usd"};

export function prng(seed:number){
  let state=seed>>>0;
  return ()=>{state=(state+0x6d2b79f5)>>>0;let t=state;t=Math.imul(t^(t>>>15),t|1);t^=t+Math.imul(t^(t>>>7),t|61);return ((t^(t>>>14))>>>0)/4294967296;};
}
const SUBSTITUTE="019afAF.eE+-\",:{}x ", INSERT="05.e-\" ,";
/// Byte-level mutations of a record: every deletion, substitution, insertion and prefix, plus whitespace and doubling.
export function mutations(record:string):string[] {
  const out=new Set<string>();
  for(let i=0;i<record.length;i++){
    out.add(record.slice(0,i)+record.slice(i+1));
    out.add(record.slice(0,i));
    for(const c of SUBSTITUTE)if(c!==record[i])out.add(record.slice(0,i)+c+record.slice(i+1));
  }
  for(let i=0;i<=record.length;i++)for(const c of INSERT)out.add(record.slice(0,i)+c+record.slice(i));
  for(const variant of [` ${record}`,`${record} `,`${record}\n`,record+record])out.add(variant);
  out.delete(record);
  return [...out];
}
/// Random byte templates: raw noise biased toward opcodes, and single-byte edits of well-formed templates.
export function randomTemplates(count:number,seed:number,seeds:readonly string[]):string[] {
  const random=prng(seed),byte=()=>Math.floor(random()*256),out:string[]=[];
  const pick=<T>(items:readonly T[])=>items[Math.floor(random()*items.length)];
  while(out.length<count){
    const mode=random();
    if(mode<0.3){
      const length=Math.floor(random()*24);
      out.push(hexlify(Uint8Array.from({length},()=>random()<0.5?1+Math.floor(random()*5):byte())));
    } else if(mode<0.6){
      out.push(hexlify(Uint8Array.from(randomSegments(random).flatMap(segment=>[...getBytes(encodeUnchecked(segment))]))));
    } else {
      const bytes=[...getBytes(pick(seeds))],at=Math.floor(random()*(bytes.length+1)),edit=random();
      if(edit<0.33)bytes.splice(at,1);else if(edit<0.66)bytes.splice(at,0,byte());else if(at<bytes.length)bytes[at]=byte();else bytes.push(byte());
      out.push(hexlify(Uint8Array.from(bytes)));
    }
  }
  return out;
}
/// Segment encodings without the well-formedness check, so generated templates can break any rule.
function encodeUnchecked(segment:{op:number;operands:number[];text?:string}):string {
  const text=segment.text===undefined?[]:[...toUtf8Bytes(segment.text)];
  return hexlify(Uint8Array.from([segment.op,...segment.operands,...text]));
}
const LITERAL_TEXT=['{"a":','"',",",'}',':"','"}','[','","',"x","0","."];
function randomSegments(random:()=>number){
  const segments:Array<{op:number;operands:number[];text?:string}>=[],count=1+Math.floor(random()*6);
  for(let i=0;i<count;i++){
    const kind=Math.floor(random()*4),edge=()=>[0,1,2,127,128,129,255][Math.floor(random()*7)];
    if(kind===0){const text=LITERAL_TEXT[Math.floor(random()*LITERAL_TEXT.length)];segments.push({op:1,operands:[random()<0.9?toUtf8Bytes(text).length:edge()],text});}
    else if(kind===1)segments.push({op:2,operands:[random()<0.8?1+Math.floor(random()*20):edge()]});
    else if(kind===2)segments.push({op:3,operands:[random()<0.85?Math.floor(random()*4):4+Math.floor(random()*4)]});
    else {const min=1+Math.floor(random()*6),max=min+Math.floor(random()*6);segments.push({op:4,operands:random()<0.8?[min,max]:[edge(),edge()]});}
  }
  return segments;
}
/// A random record for a well-formed template, near its grammar: mostly matching values with occasional edge forms.
export function sampleRecord(template:string,random:()=>number):string {
  const t=getBytes(template);let out="";
  const digits=(n:number,first="123456789")=>first[Math.floor(random()*first.length)]+Array.from({length:n-1},()=>String(Math.floor(random()*10))).join("");
  for(let at=0;at<t.length;){
    const op=t[at];
    if(op===1){out+=toUtf8String(t.slice(at+2,at+2+t[at+1]));at+=2+t[at+1];}
    else if(op===2){out+=Array.from({length:t[at+1]},()=>"0123456789abcdef"[Math.floor(random()*16)]).join("");at+=2;}
    else if(op===3){
      const flags=t[at+1];out+=random()<0.2?"0":digits(1+Math.floor(random()*5));
      if(flags&1&&random()<0.5)out+="."+digits(1+Math.floor(random()*4),"0123456789");
      if(flags&2&&random()<0.3)out+=(random()<0.5?"e":"E")+["","+","-"][Math.floor(random()*3)]+digits(1+Math.floor(random()*2),"0123456789");
      at+=2;
    } else {const min=t[at+1],max=t[at+2];out+=digits(Math.min(max,min+Math.floor(random()*(max-min+1))));at+=3;}
  }
  return out;
}
export function wellFormed(templates:readonly string[]){return templates.filter(isValidDataTemplate);}
