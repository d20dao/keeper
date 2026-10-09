// Preloaded with `node --import` into the scripts under test. The production Robinhood profile names its owner (the Robinhood Safe, with the
// deployer EOA as interim owner); this gives the process's read of the repository's chains.json the profile as it stood before that decision:
// an explicit null owner, and no Safe or interim owner. Every script that signs, deploys or reads a deployment must still refuse such a
// chain. Nothing else is read differently, and no file changes.
import fs from "node:fs/promises";
import {syncBuiltinESMExports} from "node:module";
import {resolve} from "node:path";
import {fileURLToPath} from "node:url";

const chains=fileURLToPath(new URL("../../../chains.json",import.meta.url));
const read=fs.readFile;
fs.readFile=async function readFile(path,...rest){
  const text=await read.call(this,path,...rest);
  const file=path instanceof URL?fileURLToPath(path):typeof path==="string"?resolve(path):undefined;
  if(file!==chains)return text;
  const profiles=JSON.parse(String(text));
  for(const [key,profile] of Object.entries(profiles)){
    if(!key.startsWith("robinhood-")||profile.testnet!==false)continue;
    profile.owner=null;
    for(const field of ["safe","interimOwner","interimOwnerNote"])delete profile[field];
  }
  const patched=JSON.stringify(profiles);
  return typeof text==="string"?patched:Buffer.from(patched);
};
syncBuiltinESMExports();
