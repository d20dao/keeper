import { getBytes, hexlify, toUtf8Bytes, toUtf8String, type BytesLike } from "ethers";
/// Data templates: the exact signed-data grammar of an epoch recipe, with the verdicts of EpochEntropy's DataTemplate.
/// A template is a byte sequence of segments, each an opcode and its operands:
///   0x01 LITERAL len bytes[len]  exactly these bytes (1 <= len <= 128)
///   0x02 HEX     n               exactly n characters 0-9 or a-f (1 <= n <= 128)
///   0x03 DECIMAL flags           unsigned JSON number: 0 or a nonzero digit then digits, then an optional fraction
///                                when flags & 1 and an optional exponent when flags & 2 (flags <= 3)
///   0x04 INTEGER min max         a nonzero digit then digits, min <= digit count <= max (1 <= min <= max <= 128)
/// Variable segments are greedy and never backtrack; data matches when the segments consume it exactly. A well-formed
/// template has at most 256 bytes, at least one variable segment, and a shortest match of at most 128 bytes.
export const MAX_EPOCH_DATA_BYTES=128, MAX_DATA_TEMPLATE_BYTES=256;
const LITERAL=0x01, HEX=0x02, DECIMAL=0x03, INTEGER=0x04, FRACTION=0x01, EXPONENT=0x02;
/// The readable form of a template segment. Literal text is encoded as UTF-8.
export type DataTemplateSegment=
  {literal:string}|
  {hex:number}|
  {decimal:{fraction:boolean;exponent:boolean}}|
  {integer:{minDigits:number;maxDigits:number}};

const byteOperand=(value:unknown,name:string)=>{
  if(!Number.isInteger(value)||(value as number)<1||(value as number)>MAX_EPOCH_DATA_BYTES)throw new Error(`Data template ${name} must be an integer from 1 to ${MAX_EPOCH_DATA_BYTES}`);
  return value as number;
};
/// Encode readable segments as template bytes; throws with the rule a segment breaks.
export function encodeDataTemplate(segments:readonly DataTemplateSegment[]):string {
  if(!Array.isArray(segments))throw new Error("A data template is a list of segments");
  const bytes:number[]=[];
  segments.forEach((segment,index)=>{
    const keys=segment!==null&&typeof segment==="object"?Object.keys(segment):[];
    if(keys.length!==1)throw new Error(`Data template segment ${index} must have exactly one of literal, hex, decimal or integer`);
    if("literal" in segment){
      if(typeof segment.literal!=="string")throw new Error(`Data template segment ${index}: literal must be a string`);
      const text=toUtf8Bytes(segment.literal);
      byteOperand(text.length,`segment ${index} literal length in bytes`);
      bytes.push(LITERAL,text.length,...text);
    } else if("hex" in segment){
      bytes.push(HEX,byteOperand(segment.hex,`segment ${index} hex length`));
    } else if("decimal" in segment){
      const {fraction,exponent}=segment.decimal??{};
      if(typeof fraction!=="boolean"||typeof exponent!=="boolean"||Object.keys(segment.decimal).length!==2)
        throw new Error(`Data template segment ${index}: decimal needs boolean fraction and exponent`);
      bytes.push(DECIMAL,(fraction?FRACTION:0)|(exponent?EXPONENT:0));
    } else if("integer" in segment){
      const {minDigits,maxDigits}=segment.integer??{};
      byteOperand(minDigits,`segment ${index} integer minDigits`);byteOperand(maxDigits,`segment ${index} integer maxDigits`);
      if(minDigits>maxDigits||Object.keys(segment.integer).length!==2)throw new Error(`Data template segment ${index}: integer needs minDigits <= maxDigits`);
      bytes.push(INTEGER,minDigits,maxDigits);
    } else throw new Error(`Data template segment ${index} must have exactly one of literal, hex, decimal or integer`);
  });
  const template=hexlify(Uint8Array.from(bytes));
  const problem=templateProblem(Uint8Array.from(bytes));
  if(problem)throw new Error(`Invalid data template: ${problem}`);
  return template;
}
/// Why a template is not well-formed, or undefined when it is. Same rules as DataTemplate.isValid.
function templateProblem(t:Uint8Array):string|undefined {
  if(t.length===0||t.length>MAX_DATA_TEMPLATE_BYTES)return `it must be 1 to ${MAX_DATA_TEMPLATE_BYTES} bytes`;
  let at=0,shortest=0,variable=false;
  while(at<t.length){
    const op=t[at];
    if(op===LITERAL){
      if(at+1>=t.length)return `truncated literal at byte ${at}`;
      const n=t[at+1];
      if(n===0||n>MAX_EPOCH_DATA_BYTES||at+2+n>t.length)return `invalid literal length at byte ${at}`;
      shortest+=n;at+=2+n;
    } else if(op===HEX){
      if(at+1>=t.length)return `truncated hex segment at byte ${at}`;
      const n=t[at+1];
      if(n===0||n>MAX_EPOCH_DATA_BYTES)return `invalid hex length at byte ${at}`;
      shortest+=n;at+=2;variable=true;
    } else if(op===DECIMAL){
      if(at+1>=t.length||t[at+1]>(FRACTION|EXPONENT))return `invalid decimal flags at byte ${at}`;
      shortest+=1;at+=2;variable=true;
    } else if(op===INTEGER){
      if(at+2>=t.length)return `truncated integer segment at byte ${at}`;
      const min=t[at+1],max=t[at+2];
      if(min===0||min>max||max>MAX_EPOCH_DATA_BYTES)return `invalid integer digit bounds at byte ${at}`;
      shortest+=min;at+=3;variable=true;
    } else return `unknown segment opcode ${op} at byte ${at}`;
  }
  if(!variable)return "it needs at least one hex, decimal or integer segment";
  if(shortest>MAX_EPOCH_DATA_BYTES)return `its shortest match exceeds ${MAX_EPOCH_DATA_BYTES} bytes`;
  return undefined;
}
export function isValidDataTemplate(template:BytesLike):boolean { return templateProblem(getBytes(template))===undefined; }
/// Throws unless the template is well-formed.
export function validateDataTemplate(template:BytesLike):void {
  const problem=templateProblem(getBytes(template));
  if(problem)throw new Error(`Invalid data template: ${problem}`);
}
/// The readable segments of a well-formed template whose literals are UTF-8 text.
export function decodeDataTemplate(template:BytesLike):DataTemplateSegment[] {
  const t=getBytes(template);validateDataTemplate(t);
  const segments:DataTemplateSegment[]=[];
  for(let at=0;at<t.length;){
    const op=t[at];
    if(op===LITERAL){segments.push({literal:toUtf8String(t.slice(at+2,at+2+t[at+1]))});at+=2+t[at+1];}
    else if(op===HEX){segments.push({hex:t[at+1]});at+=2;}
    else if(op===DECIMAL){segments.push({decimal:{fraction:(t[at+1]&FRACTION)!==0,exponent:(t[at+1]&EXPONENT)!==0}});at+=2;}
    else {segments.push({integer:{minDigits:t[at+1],maxDigits:t[at+2]}});at+=3;}
  }
  return segments;
}
const isDigit=(c:number|undefined)=>c!==undefined&&c>=0x30&&c<=0x39;
function digits(d:Uint8Array,p:number):number { while(isDigit(d[p]))p++; return p; }
/// End of an unsigned JSON number starting at p, or -1.
function decimalEnd(d:Uint8Array,p:number,fraction:boolean,exponent:boolean):number {
  if(!isDigit(d[p]))return -1;
  if(d[p]===0x30){p++;if(isDigit(d[p]))return -1;}
  else p=digits(d,p);
  if(fraction&&d[p]===0x2e){const first=p+1;p=digits(d,first);if(p===first)return -1;}
  if(exponent&&(d[p]===0x65||d[p]===0x45)){
    p++;if(d[p]===0x2b||d[p]===0x2d)p++;
    const first=p;p=digits(d,first);if(p===first)return -1;
  }
  return p;
}
/// Whether data is exactly a record the template describes; false for a malformed template.
export function matchesDataTemplate(template:BytesLike,data:BytesLike):boolean {
  const t=getBytes(template),d=getBytes(data);
  if(templateProblem(t)!==undefined||d.length===0||d.length>MAX_EPOCH_DATA_BYTES)return false;
  let p=0;
  for(let at=0;at<t.length;){
    const op=t[at];
    if(op===LITERAL){
      const n=t[at+1];
      if(p+n>d.length)return false;
      for(let i=0;i<n;i++)if(d[p+i]!==t[at+2+i])return false;
      p+=n;at+=2+n;
    } else if(op===HEX){
      const end=p+t[at+1];
      if(end>d.length)return false;
      for(;p<end;p++)if(!isDigit(d[p])&&(d[p]<0x61||d[p]>0x66))return false;
      at+=2;
    } else if(op===DECIMAL){
      p=decimalEnd(d,p,(t[at+1]&FRACTION)!==0,(t[at+1]&EXPONENT)!==0);
      if(p<0)return false;
      at+=2;
    } else {
      if(!isDigit(d[p])||d[p]===0x30)return false;
      const end=digits(d,p),count=end-p;
      if(count<t[at+1]||count>t[at+2])return false;
      p=end;at+=3;
    }
  }
  return p===d.length;
}
