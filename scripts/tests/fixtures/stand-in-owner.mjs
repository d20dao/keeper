// Preloaded with `node --import` into the round coordinator's scripts under test. It gives the process's read of the repository's chains.json
// a stand-in owner for each Robinhood profile whose owner is null (none is today: the testnet's is the deployer EOA, the production profile's
// the Robinhood Safe), and with D20_STAND_IN_PROFILE stands a local chain's Safe and interim owner in for the production profile's, so that the
// command line can be tested against a local Safe. Nothing else is read differently, and no file changes.
import fs from "node:fs/promises";
import {syncBuiltinESMExports} from "node:module";
import {resolve} from "node:path";
import {fileURLToPath} from "node:url";

// D20_STAND_IN_OWNER names another stand-in, such as the local Safe that owns a test's deployment. D20_STAND_IN_PROFILE, a JSON object such
// as {"safe":{"address":…,"owners":[…],"threshold":2},"interimOwner":…}, is merged into the production profile, with the stand-in as its
// owner, whatever chains.json holds: a local chain's Safe and interim owner then stand in for the production ones.
export const STAND_IN_OWNER=process.env.D20_STAND_IN_OWNER??"0x00000000000000000000000000000000000000A1";
const STAND_IN_PROFILE=process.env.D20_STAND_IN_PROFILE===undefined?undefined:JSON.parse(process.env.D20_STAND_IN_PROFILE);
const chains=fileURLToPath(new URL("../../../chains.json",import.meta.url));
const read=fs.readFile;
fs.readFile=async function readFile(path,...rest){
  const text=await read.call(this,path,...rest);
  const file=path instanceof URL?fileURLToPath(path):typeof path==="string"?resolve(path):undefined;
  if(file!==chains)return text;
  const profiles=JSON.parse(String(text));
  for(const [key,profile] of Object.entries(profiles)){
    if(!key.startsWith("robinhood-"))continue;
    if(profile.owner===null)profile.owner=STAND_IN_OWNER;
    if(STAND_IN_PROFILE!==undefined&&profile.testnet===false)Object.assign(profile,STAND_IN_PROFILE,{owner:STAND_IN_OWNER});
  }
  const patched=JSON.stringify(profiles);
  return typeof text==="string"?patched:Buffer.from(patched);
};
syncBuiltinESMExports();
