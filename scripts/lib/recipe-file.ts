// Recipe files for registerRecipe: a gateway body, a readable data template, the Airnode signer and one signed
// gateway response, checked together before any calldata is printed or sent.
import {getAddress,getBytes,hexlify,id,toUtf8Bytes,verifyMessage} from "ethers";
import {canonicalRequestOfBody,validateEpochRecipe,type EpochRecipe} from "../../src/epoch.ts";
import {decodeDataTemplate,encodeDataTemplate,matchesDataTemplate,type DataTemplateSegment} from "../../src/templates.ts";
import {attestationDigest,validateApiSignatureEncoding} from "../../src/sources.ts";

/// The JSON a recipe file holds. body is the gateway request (an object, serialized with JSON.stringify, or the exact
/// JSON text); sample is a signed gateway response for that body: {airnode, requestHash, timestamp, data, signature}.
export interface RecipeFile {description?:string;signer:string;body:unknown;template:DataTemplateSegment[];sample:{airnode:string;requestHash:string;timestamp:string;data:unknown;signature:string}}
export interface CheckedRecipe {recipe:EpochRecipe;signer:string;description?:string;queryHash:string;sample:{timestamp:string;data:string}}
export class RecipeFileError extends Error {}

const fail=(message:string):never=>{throw new RecipeFileError(message);};
const attempt=<T>(what:string,run:()=>T):T=>{try{return run();}catch(error){return fail(`${what}: ${error instanceof Error?error.message:String(error)}`);}};

/// Validate a recipe file's text. Every rule the registry and the keeper apply is checked, plus the sample: its
/// request hash must be the recipe's canonical request hash, its canonical low-s signature must recover to the signer
/// and its signed bytes must match the template.
export function checkRecipeFile(text:string):CheckedRecipe {
  const file=attempt("The recipe file is not valid JSON",()=>JSON.parse(text)) as RecipeFile;
  if(file===null||typeof file!=="object"||Array.isArray(file))fail("The recipe file must be a JSON object with signer, body, template and sample");
  const unknown=Object.keys(file).filter(key=>!["description","signer","body","template","sample"].includes(key));
  if(unknown.length)fail(`Unknown recipe file fields: ${unknown.join(", ")}`);
  if(file.description!==undefined&&typeof file.description!=="string")fail("description must be a string");
  const signer=attempt("signer is not an address",()=>getAddress(String(file.signer).toLowerCase()));
  if(BigInt(signer)===0n)fail("signer must not be the zero address");
  if(file.body===undefined)fail("body is required: the JSON request body the keeper posts to the gateway");
  const body=typeof file.body==="string"?file.body:JSON.stringify(file.body);
  const canonicalRequest=attempt("body is not a gateway request",()=>canonicalRequestOfBody(body));
  const template=attempt("template is invalid",()=>encodeDataTemplate(file.template));
  const recipe:EpochRecipe={canonicalRequest,template,body};
  attempt("The recipe exceeds the registry bounds",()=>validateEpochRecipe(recipe));
  const queryHash=id(canonicalRequest),sample=file.sample;
  if(sample===null||typeof sample!=="object")fail("sample is required: one signed gateway response for this body");
  if(String(sample.requestHash).toLowerCase()!==queryHash)
    fail(`sample.requestHash ${sample.requestHash} is not the canonical request hash ${queryHash}: the gateway signed a different request (canonical request ${canonicalRequest})`);
  const airnode=attempt("sample.airnode is not an address",()=>getAddress(String(sample.airnode).toLowerCase()));
  if(airnode!==signer)fail(`sample.airnode ${airnode} is not the recipe signer ${signer}`);
  if(typeof sample.timestamp!=="string"||!/^(0|[1-9][0-9]{0,77})$/.test(sample.timestamp))fail("sample.timestamp must be the signed decimal timestamp string");
  const data=typeof sample.data==="string"?sample.data:JSON.stringify(sample.data);
  if(typeof data!=="string")fail("sample.data is missing");
  attempt("sample.signature is not a canonical 65-byte low-s signature",()=>validateApiSignatureEncoding(sample.signature));
  const attestation={timestamp:BigInt(sample.timestamp),data:hexlify(toUtf8Bytes(data)),signature:sample.signature};
  const recovered=attempt("sample.signature does not recover",()=>verifyMessage(getBytes(attestationDigest(queryHash,attestation)),sample.signature));
  if(recovered!==signer)fail(`sample.signature recovers to ${recovered}, not the signer ${signer}: the signed request, timestamp or data differ`);
  if(!matchesDataTemplate(template,attestation.data))
    fail(`The sample's signed data does not match the template (${JSON.stringify(decodeDataTemplate(template))}): ${JSON.stringify(data)}`);
  return {recipe,signer,description:file.description,queryHash,sample:{timestamp:sample.timestamp,data}};
}
