// The Robinhood contracts stand on their own. Part 1, the import gate: the import closure of contracts/robinhood/*.sol is exactly the
// allowlist below, chain-neutral files only, so no contract of another deployment and nothing that file imports can enter it unreviewed.
// Part 2, the wording gate: no file under contracts/robinhood/ uses a word of the epoch design. Change the allowlist only with a review.
// Part 3, both gates for the round replay library: the import closure of src/round.ts is exactly its allowlist, no file of it uses a word
// of the epoch design, and it bundles for browsers from those files and its packages alone.
// Part 4, the selector gate: the compiled round coordinator shares no function selector or event topic with EpochEntropy beyond the
// OpenZeppelin members both inherit, and no selector with IBeaconVerifier. Run it after npm run compile.
import {readFileSync,readdirSync} from "node:fs";
import {posix} from "node:path";
import {fileURLToPath} from "node:url";
import {build} from "esbuild";
import {Interface} from "ethers";

const root=fileURLToPath(new URL("../",import.meta.url));
const read=file=>readFileSync(root+(file.startsWith("@")?`node_modules/${file}`:file),"utf8");
const failures=[];

const ALLOWED=[
  "contracts/robinhood/ArbitrumBlocks.sol",
  "contracts/robinhood/BeaconBook.sol",
  "contracts/robinhood/D20VRFCoordinatorRobinhood.sol",
  "contracts/robinhood/D20VRFProofVerifier.sol",
  "contracts/robinhood/LinkedRandomnessMapping.sol",
  // Chain-neutral shared code.
  "contracts/interfaces/IBeaconVerifier.sol",
  "contracts/interfaces/ID20VRF.sol",
  "contracts/libraries/RandomnessMapping.sol",
  "contracts/vendor/VRF.sol",
  "@openzeppelin/contracts-upgradeable/access/Ownable2StepUpgradeable.sol",
  "@openzeppelin/contracts-upgradeable/access/OwnableUpgradeable.sol",
  "@openzeppelin/contracts-upgradeable/utils/ContextUpgradeable.sol",
  "@openzeppelin/contracts/interfaces/IERC1967.sol",
  "@openzeppelin/contracts/interfaces/draft-IERC1822.sol",
  "@openzeppelin/contracts/proxy/ERC1967/ERC1967Utils.sol",
  "@openzeppelin/contracts/proxy/beacon/IBeacon.sol",
  "@openzeppelin/contracts/proxy/utils/Initializable.sol",
  "@openzeppelin/contracts/proxy/utils/UUPSUpgradeable.sol",
  "@openzeppelin/contracts/utils/Address.sol",
  "@openzeppelin/contracts/utils/Errors.sol",
  "@openzeppelin/contracts/utils/LowLevelCall.sol",
  "@openzeppelin/contracts/utils/ReentrancyGuard.sol",
  "@openzeppelin/contracts/utils/StorageSlot.sol",
];
const OPENZEPPELIN="5.6.1";
const LEGACY=/epoch|airnode|recipe|committer|registry/i;

// ---- Part 1: imports
const IMPORT=/^\s*import\s+(?:[^"';]*?\s+from\s+)?"([^"]+)"\s*;/gm;
const deployed=readdirSync(root+"contracts/robinhood").filter(file=>file.endsWith(".sol")).map(file=>`contracts/robinhood/${file}`);
const closure=new Set();
for(const queue=[...deployed];queue.length;){
  const file=queue.pop();
  if(closure.has(file))continue;
  closure.add(file);
  let source;
  try{source=read(file);}catch{failures.push(`${file}: missing`);continue;}
  for(const [,specifier] of source.matchAll(IMPORT))queue.push(specifier.startsWith(".")?posix.normalize(posix.join(posix.dirname(file),specifier)):specifier);
}
for(const file of closure)if(!ALLOWED.includes(file))failures.push(`${file}: in the import closure of contracts/robinhood/*.sol but not allowed`);
for(const file of ALLOWED)if(!closure.has(file))failures.push(`${file}: allowed but no longer imported; remove it from the allowlist`);
for(const name of ["@openzeppelin/contracts","@openzeppelin/contracts-upgradeable"]){
  const version=JSON.parse(read(`${name}/package.json`)).version;
  if(version!==OPENZEPPELIN)failures.push(`${name}: version ${version}, allowed ${OPENZEPPELIN}`);
}

// ---- Part 2: wording
const robinhood=readdirSync(root+"contracts/robinhood",{recursive:true,encoding:"utf8"}).filter(file=>file.endsWith(".sol")).map(file=>`contracts/robinhood/${file.replaceAll("\\","/")}`);
for(const file of robinhood){
  if(LEGACY.test(file))failures.push(`${file}: the path uses a word of the epoch design`);
  read(file).split("\n").forEach((line,index)=>{
    const match=line.match(LEGACY);
    if(match)failures.push(`${file}:${index+1}: "${match[0]}"`);
  });
}

// ---- Part 3: the round replay library
// The library and the chain-neutral modules it reads, exactly. No module of the epoch design (beacon.ts, templates.ts, epoch.ts, sources.ts,
// replay.ts, verification.ts) may enter, nor any package but ethers and the noble curves it verifies with.
const LIBRARY_ALLOWED=["src/round.ts","src/drand.ts","src/vrf.ts","src/mapping.ts"];
const LIBRARY_PACKAGES=/^(ethers|@noble\/curves\/(bn254|secp256k1|abstract\/modular))$/;
const TS_IMPORT=/^\s*(?:import|export)\s+(?:type\s+)?(?:[^"';]*?\s+from\s+)?"([^"]+)"\s*;/gm;
const library=new Set();
for(const queue=["src/round.ts"];queue.length;){
  const file=queue.pop();
  if(library.has(file))continue;
  library.add(file);
  let source;
  try{source=read(file);}catch{failures.push(`${file}: missing`);continue;}
  for(const [,specifier] of source.matchAll(TS_IMPORT)){
    if(specifier.startsWith("."))queue.push(posix.normalize(posix.join(posix.dirname(file),specifier)));
    else if(!LIBRARY_PACKAGES.test(specifier))failures.push(`${file}: imports ${specifier}, not an allowed package`);
  }
  source.split("\n").forEach((line,index)=>{
    const match=line.match(LEGACY);
    if(match)failures.push(`${file}:${index+1}: "${match[0]}"`);
  });
}
for(const file of library)if(!LIBRARY_ALLOWED.includes(file))failures.push(`${file}: in the import closure of src/round.ts but not allowed`);
for(const file of LIBRARY_ALLOWED)if(!library.has(file))failures.push(`${file}: allowed but no longer imported by src/round.ts; remove it from the allowlist`);
// The library alone, bundled for browsers, which refuses Node's built-in modules: no source file but the allowed ones, and its replay of
// the first replay vector works from the bundle.
let bundled=0;
try{
  const result=await build({entryPoints:[root+"src/round.ts"],bundle:true,platform:"browser",format:"esm",target:"es2022",write:false,metafile:true,
    minify:true,logLevel:"silent"});
  for(const input of Object.keys(result.metafile.inputs).map(path=>path.replaceAll("\\","/")))
    if(!input.includes("node_modules/")&&!LIBRARY_ALLOWED.some(file=>input.endsWith(file)))failures.push(`${input}: in the browser bundle of src/round.ts but not allowed`);
  bundled=result.outputFiles[0].contents.length;
  const round=await import(`data:text/javascript;base64,${Buffer.from(result.outputFiles[0].text).toString("base64")}`);
  const vectors=JSON.parse(read("test/fixtures/robinhood/round-replay-vectors.json")),v=vectors.vectors[0],b=vectors.beacons[0],p=v.proof,c=vectors.protocolConfiguration;
  const pair=values=>values.map(BigInt);
  const verdict=round.replayRoundRequest({chainId:BigInt(vectors.chainId),coordinator:vectors.coordinator,
    configuration:{publicKey:pair(c.publicKey),feeRecipient:c.feeRecipient,initialMinFee:BigInt(c.initialMinFee),beaconIdentity:c.beaconIdentity},
    configurationHash:c.hash,keyHash:vectors.keyHash,
    beacon:{verifier:b.verifier,chainHash:b.chainHash,publicKey:b.publicKey,genesis:BigInt(b.genesis),period:BigInt(b.period),identity:b.identity},
    requestId:BigInt(v.requestId),consumer:v.consumer,clientSeed:v.clientSeed,mapping:{...v.mapping,lower:BigInt(v.mapping.lower),upper:BigInt(v.mapping.upper)},
    mappingHash:v.mappingHash,requestBlock:BigInt(v.requestBlock),requestTimestamp:BigInt(v.requestTimestamp),deadline:BigInt(v.deadline),
    beaconId:v.beaconId,round:BigInt(v.round),roundSignature:v.roundSignature,roundRandomness:v.roundRandomness,
    proof:{pk:pair(p.pk),gamma:pair(p.gamma),c:BigInt(p.c),s:BigInt(p.s),seed:BigInt(p.seed),uWitness:p.uWitness,cGammaWitness:pair(p.cGammaWitness),
      sHashWitness:pair(p.sHashWitness),zInv:BigInt(p.zInv)},
    randomness:v.randomness,proofHash:v.proofHash,transcriptHash:v.transcriptHash,acceptanceTimestamp:BigInt(v.acceptanceTimestamp)});
  if(!verdict.valid||verdict.seed!==BigInt(v.seed))failures.push(`src/round.ts: the bundled replay of the first replay vector fails: ${verdict.failed.join(", ")}`);
}catch(error){failures.push(`src/round.ts: does not bundle for browsers: ${error.message}`);}

// ---- Part 4: selectors and event topics
// A keeper refuses a call to the epoch registry by its selector, so the round coordinator must not answer one that EpochEntropy answers,
// nor emit an event a reader could take for the registry's. The only members both may share are OpenZeppelin's ownership, upgrade and
// initialization members, which both inherit and which mean the same on any chain. No function may share a selector with IBeaconVerifier.
const SHARED_ALLOWED=new Set([
  "acceptOwnership()","owner()","pendingOwner()","renounceOwnership()","transferOwnership(address)",
  "proxiableUUID()","UPGRADE_INTERFACE_VERSION()","upgradeToAndCall(address,bytes)",
  "event Initialized(uint64)","event OwnershipTransferStarted(address,address)","event OwnershipTransferred(address,address)","event Upgraded(address)",
]);
let compared=0;
try{
  const abiOf=(source,name)=>{
    try{return new Interface(JSON.parse(readFileSync(`${root}artifacts/contracts/${source}/${name}.json`,"utf8")).abi);}
    catch{throw new Error(`${name}: no compiled artifact; run npm run compile first`);}
  };
  /// A contract's function selectors and event topics, each with its signature.
  const members=contract=>{
    const found=new Map();
    contract.forEachFunction(f=>found.set(f.selector,f.format("sighash")));
    contract.forEachEvent(e=>found.set(e.topicHash,`event ${e.format("sighash")}`));
    return found;
  };
  const round=members(abiOf("robinhood/D20VRFCoordinatorRobinhood.sol","D20VRFCoordinatorRobinhood"));
  const registry=members(abiOf("EpochEntropy.sol","EpochEntropy")),verifier=members(abiOf("interfaces/IBeaconVerifier.sol","IBeaconVerifier"));
  const shared=new Set();
  for(const [key,member] of round){
    if(registry.has(key)){
      shared.add(member);
      if(!SHARED_ALLOWED.has(member))failures.push(`D20VRFCoordinatorRobinhood ${member} (${key.slice(0,10)}): shared with EpochEntropy's ${registry.get(key)}`);
    }
    if(verifier.has(key))failures.push(`D20VRFCoordinatorRobinhood ${member} (${key.slice(0,10)}): shared with IBeaconVerifier's ${verifier.get(key)}`);
  }
  for(const member of SHARED_ALLOWED)if(!shared.has(member))failures.push(`${member}: allowed to be shared with EpochEntropy but no longer shared; remove it from the allowlist`);
  compared=round.size;
}catch(error){failures.push(error.message);}

if(failures.length){
  console.error(`The Robinhood contracts or the round replay library import or say what they must not:\n${failures.map(line=>`  ${line}`).join("\n")}`);
  process.exit(1);
}
console.log(`Robinhood contracts import only their ${closure.size} allowed files (OpenZeppelin ${OPENZEPPELIN}), and none of the ${robinhood.length} files under contracts/robinhood/ uses a word of the epoch design.`);
console.log(`src/round.ts imports only its ${library.size} allowed files, none of which uses a word of the epoch design, and bundles for browsers (${bundled} bytes) with a working replay.`);
console.log(`The round coordinator's ${compared} function selectors and event topics share with EpochEntropy only the ${SHARED_ALLOWED.size} OpenZeppelin members allowed, and nothing with IBeaconVerifier.`);
