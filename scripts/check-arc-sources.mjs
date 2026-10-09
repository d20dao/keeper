// Arc's deployed contracts stay exactly what is live. Part 1 pins the source of every Arc-deployed contract and of everything it imports
// (SHA-256 of the LF-normalised text) and the OpenZeppelin version. Part 2 reads the compiled artifacts and compares their runtime code
// with the live code hashes. A failure in either part means an Arc artifact moved: change a pin only with a reviewed Arc upgrade.
import {readFileSync} from "node:fs";
import {createHash} from "node:crypto";
import {posix} from "node:path";
import {fileURLToPath} from "node:url";
import {getAddress,getBytes,hexlify,keccak256,zeroPadValue} from "ethers";

const root=fileURLToPath(new URL("../",import.meta.url));
const read=file=>readFileSync(root+file,"utf8").replace(/\r\n/g,"\n");
const failures=[];
const fail=message=>failures.push(message);

const DEPLOYED=["contracts/D20VRFCoordinator.sol","contracts/EpochEntropy.sol","contracts/D20BeaconVerifier.sol","contracts/D20Proxy.sol","contracts/examples/D20CostClient.sol"];
// The deployed sources and their import closure inside this repository.
const SOURCES={
  "contracts/D20VRFCoordinator.sol":"fa3ea7d7995c5dd8f6a9cd50140f0cb8d35e441a6ecda513f6e82fbbacea9eaf",
  "contracts/EpochEntropy.sol":"100785392042af99e710af2e13855fd44d9c1018e33220d2601f75a3cb72f441",
  "contracts/D20BeaconVerifier.sol":"f2eb0917c2d996f4c3e4969d37e32366163532605cb9ae8e7e97e86935c7b068",
  "contracts/D20Proxy.sol":"83dcef3b72e2a0a4809c2d8df0d083d2a0f659fe2b3b01fadb1b2eaf8afb7286",
  "contracts/examples/D20CostClient.sol":"06212bb65b0073adc7de7cbc70f80f1ffc27f5e2045a32cf9dbf3978ad62ebff",
  "contracts/interfaces/ID20VRF.sol":"0361aee07644cbf7c2bcde8ffd9188825d5be633c4a1520169a7702a38c1bdbc",
  "contracts/interfaces/IBeaconVerifier.sol":"75782d629af93a368e216272cd1ab25191907ab65e79510294e8679a0f7c6f74",
  "contracts/libraries/DataTemplate.sol":"f0d93c2ec3bcb8cfb625001506fe24f938e6e2cfbae85632f2980d23595ed961",
  "contracts/libraries/RandomnessMapping.sol":"00bce68c1a6d89903d72d1c3f9ee5e1a56392188ed29b6aa32ab7451ef180c91",
  "contracts/vendor/VRF.sol":"98e0345fd2dd42178cad4ad16dd8b18621112078d63d0c64839c8bd01c539350",
  "contracts/vendor/bls-bn254/BLS.sol":"887fd94553b9d6d0a247900bdb05523d3b9ef3b8ca049e2a0524a0b1a9d37e69",
  "contracts/vendor/bls-bn254/ModExp.sol":"fd91ae9291511668914b05fece03fb9bf6e2b57f7784d2edddc82456af12b495",
};
const OPENZEPPELIN="5.6.1";

// ---- Part 1: sources
const IMPORT=/^\s*import\s+(?:[^"';]*?\s+from\s+)?"([^"]+)"\s*;/gm;
const closure=new Set(),packages=new Set();
for(const queue=[...DEPLOYED];queue.length;){
  const file=queue.pop();
  if(closure.has(file))continue;
  closure.add(file);
  let source;
  try{source=read(file);}catch{fail(`${file}: missing`);continue;}
  const actual=createHash("sha256").update(source).digest("hex");
  if(SOURCES[file]&&actual!==SOURCES[file])fail(`${file}: SHA-256 ${actual}, pinned ${SOURCES[file]}`);
  for(const [,specifier] of source.matchAll(IMPORT))
    if(specifier.startsWith("."))queue.push(posix.join(posix.dirname(file),specifier));
    else packages.add(specifier.split("/").slice(0,2).join("/"));
}
for(const file of closure)if(!SOURCES[file])fail(`${file}: imported by an Arc-deployed contract but not pinned`);
for(const file of Object.keys(SOURCES))if(!closure.has(file))fail(`${file}: pinned but no longer imported by an Arc-deployed contract`);
for(const name of packages){
  if(!/^@openzeppelin\/contracts(-upgradeable)?$/.test(name)){fail(`${name}: imported by an Arc-deployed contract but not pinned`);continue;}
  const version=JSON.parse(read(`node_modules/${name}/package.json`)).version;
  if(version!==OPENZEPPELIN)fail(`${name}: version ${version}, pinned ${OPENZEPPELIN}`);
}

// ---- Part 2: compiled code
function artifact(name,directory=""){
  let found,output;
  try{
    found=JSON.parse(readFileSync(`${root}artifacts/contracts/${directory}${name}.sol/${name}.json`,"utf8"));
    const build=JSON.parse(readFileSync(`${root}artifacts/build-info/${found.buildInfoId}.output.json`,"utf8"));
    output=(build.output??build).contracts[found.inputSourceName][name];
  }catch{throw new Error(`${name}: no compiled artifact; run npm run compile first`);}
  if("0x"+output.evm.deployedBytecode.object!==found.deployedBytecode)throw new Error(`${name}: the artifact differs from its build output; run npm run compile first`);
  return found;
}
/** Runtime code of an implementation placed at `address`: UUPSUpgradeable embeds its own address as the only immutable. */
function runtimeAt(found,address){
  const code=getBytes(found.deployedBytecode),references=Object.values(found.immutableReferences);
  if(address===undefined){
    if(references.length)throw new Error("expected no immutables");
    return found.deployedBytecode;
  }
  if(references.length!==1)throw new Error("expected exactly one immutable, the UUPS self address");
  const word=getBytes(zeroPadValue(getAddress(address),32));
  for(const {start,length} of references[0]){
    if(length!==32||start+32>code.length)throw new Error("invalid immutable reference");
    code.set(word,start);
  }
  return hexlify(code);
}
/** Runtime code without the CBOR metadata tail, whose IPFS hash covers every imported source. */
function withoutMetadata(hex){
  const code=getBytes(hex),start=code.length-2-(code[code.length-2]*256+code[code.length-1]);
  if(start<=0||code[start]>>5!==5)throw new Error("runtime code has no CBOR metadata tail");
  return hexlify(code.subarray(0,start));
}

// What both public networks run, from the deployment manifests: the implementation addresses and code hashes, the code hash of the proxies and
// the address of the beacon verifier. `address` is where an implementation is placed to compile its runtime code, since UUPSUpgradeable embeds
// its own address; `pinnedAddress` is the manifest's address of a contract that has no such immutable. `manifest` names the address field of the
// manifest, if any, and every code hash field that must equal `codeHash`: the registry, coordinator and cost client proxies are all D20Proxy.
const LIVE=[
  {name:"EpochEntropy",address:"0xD20dA0853a6f894c0cdc9018fD4F8F67Eac15704",codeHash:"0xb5e125e3b0f63ffe516d781266c1cbacebe3d131148b69f8736d3ca78bcddb12",manifest:["epochImplementation",["epochImplementationCodeHash"]]},
  {name:"D20CostClient",directory:"examples/",address:"0xD20DA00A872acfDe3e4721Fc1051BD23CC84B66b",codeHash:"0xb30bd76592fc6def770bf8599a4dcfa827ce4fe1e951000d462c74d488b53e91",manifest:["clientImplementation",["clientImplementationCodeHash"]]},
  {name:"D20BeaconVerifier",pinnedAddress:"0xd20dA01Aa16AeD6b77Cd8DDb869151802599100a",codeHash:"0x4388250d26298224c4a39d030263550655390ca831ab44c4124f8b0be1f65351",manifest:["beaconVerifier",["beaconVerifierCodeHash"]]},
  {name:"D20Proxy",codeHash:"0x1e98fe55cc7d87073e415635715100988aad79cbb84e39b18f96f3727d1c716f",manifest:[undefined,["registryCodeHash","coordinatorCodeHash","clientCodeHash"]]},
];
const manifests=["arc-mainnet","arc-testnet"].map(network=>[network,JSON.parse(read(`deployments/${network}.json`))]);
try{
  for(const live of LIVE){
    const hash=keccak256(runtimeAt(artifact(live.name,live.directory),live.address));
    if(hash!==live.codeHash)fail(`${live.name}: compiled runtime code${live.address?` at ${live.address}`:""} hashes to ${hash}, live ${live.codeHash}`);
    const [addressField,hashFields]=live.manifest,address=live.address??live.pinnedAddress;
    for(const [network,manifest] of manifests){
      for(const hashField of hashFields)if(manifest[hashField]!==live.codeHash)fail(`deployments/${network}.json: ${hashField} ${manifest[hashField]}, pinned ${live.codeHash}`);
      if(addressField&&manifest[addressField]!==address)fail(`deployments/${network}.json: ${addressField} ${manifest[addressField]}, pinned ${address}`);
    }
  }
  // The coordinator's full code hash is not reproducible from the sources now: its metadata hash covers EpochEntropy.sol, which changed
  // after the live implementation was built. Its executable bytes are, against the code recorded from the chain at its address.
  const fixture=JSON.parse(read("test/fixtures/coordinator-deployed-f38aaf8.json"));
  if(keccak256(fixture.deployedBytecode)!==fixture.runtimeCodeHash)fail("test/fixtures/coordinator-deployed-f38aaf8.json: the recorded code does not hash to its recorded code hash");
  for(const [network,manifest] of manifests)
    if(manifest.coordinatorImplementation!==fixture.implementation||manifest.coordinatorImplementationCodeHash!==fixture.runtimeCodeHash)
      fail(`deployments/${network}.json: the coordinator implementation differs from the f38aaf8 fixture`);
  if(withoutMetadata(runtimeAt(artifact("D20VRFCoordinator"),fixture.implementation))!==withoutMetadata(fixture.deployedBytecode))
    fail(`D20VRFCoordinator: compiled executable code at ${fixture.implementation} differs from the f38aaf8 code live on Arc`);
}catch(error){fail(error.message);}

if(failures.length){
  console.error(`Arc artifacts moved. Arc-deployed sources and their imports must stay what is live:\n${failures.map(line=>`  ${line}`).join("\n")}`);
  process.exit(1);
}
console.log(`Arc sources match their pins (${closure.size} files, OpenZeppelin ${OPENZEPPELIN}) and compile to the live code: ${LIVE.map(live=>live.name).join(", ")}, and D20VRFCoordinator without its metadata. Both manifests record those code hashes, for the three proxies too, and the beacon verifier address.`);
