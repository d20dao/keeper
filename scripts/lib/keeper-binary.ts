import {spawn} from "node:child_process";
import {once} from "node:events";
import {resolve} from "node:path";

/// Build the keeper daemon of this revision and return its path. The integration suite and the fleet drill both run
/// the real binary, so they build it here: a stale target directory would otherwise test older code in silence.
export async function buildKeeper(profile:"debug"|"release"="debug"):Promise<string>{
  const args=["build","--manifest-path","keeper/Cargo.toml","--locked",...(profile==="release"?["--release"]:[])];
  const child=spawn("cargo",args,{stdio:["ignore","ignore","pipe"],windowsHide:true});
  let err="";
  child.stderr.on("data",data=>{err+=data;});
  const [code]=await once(child,"exit") as [number|null];
  if(code!==0)throw new Error(`cargo ${args.join(" ")} failed with code ${code}:\n${err.split("\n").slice(-20).join("\n")}`);
  return resolve(`keeper/target/${profile}/d20dao-keeper${process.platform==="win32"?".exe":""}`);
}
