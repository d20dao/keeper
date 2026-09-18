import {createReadStream} from "node:fs";
import {createInterface} from "node:readline";
import {Wallet, getAddress, type Provider} from "ethers";

// Read only the named assignments. Never import the file into process.env,
// expand expressions, log lines, or include rejected values in an exception.
async function readAssignments(path:string, names:readonly string[]):Promise<Record<string,string>> {
  const values:Record<string,string>={};
  const input=createReadStream(path,{encoding:"utf8"});
  const lines=createInterface({input,crlfDelay:Infinity});
  try {
    for await(const line of lines){
      const match=/^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*?)\s*$/.exec(line);
      if(!match||!names.includes(match[1]))continue;
      if(Object.hasOwn(values,match[1]))throw new Error("Duplicate assignment");
      let value=match[2];
      if(value.startsWith('"')||value.startsWith("'")){
        const quote=value[0],end=value.indexOf(quote,1);
        if(end<0||!/^\s*(?:#.*)?$/.test(value.slice(end+1)))throw new Error("Invalid assignment");
        value=value.slice(1,end);
      } else value=value.replace(/\s+#.*$/,"").trim();
      values[match[1]]=value;
    }
    return values;
  } finally {
    lines.close(); input.destroy();
  }
}

export async function loadDeployer(path:string, provider?:Provider):Promise<Wallet> {
  let values:Record<string,string>={};
  try {
    values=await readAssignments(path,["DEPLOYER_ADDRESS","DEPLOYER_KEY"]);
    if(!/^(?:0x)?[0-9a-fA-F]{64}$/.test(values.DEPLOYER_KEY??""))throw new Error("Invalid deployer key");
    const wallet=new Wallet(values.DEPLOYER_KEY.startsWith("0x")?values.DEPLOYER_KEY:`0x${values.DEPLOYER_KEY}`,provider);
    if(wallet.address!==getAddress((values.DEPLOYER_ADDRESS??"").toLowerCase()))throw new Error("Deployer address does not match key");
    return wallet;
  } catch {
    throw new Error("Could not load matching DEPLOYER_ADDRESS and DEPLOYER_KEY; values were not logged");
  } finally {
    values.DEPLOYER_KEY="";
  }
}

/** One required named setting (for example DAO_TREASURY or a private RPC URL). The value is never logged. */
export async function loadEnvValue(path:string, name:string):Promise<string> {
  let values:Record<string,string>;
  try {values=await readAssignments(path,[name]);}
  catch {throw new Error(`Could not read ${name}; values were not logged`);}
  if(!values[name])throw new Error(`${name} is not set in the operator env file`);
  return values[name];
}
