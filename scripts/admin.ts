// Generate an owner/multisig transaction. Broadcasting is explicit and journaled.
import {parseArgs} from "node:util";
import {readFile,open} from "node:fs/promises";
import {resolve,dirname,join} from "node:path";
import {Contract,Interface,JsonRpcProvider,getAddress,getBytes,keccak256} from "ethers";
import {loadDeployer} from "./lib/deployer-env.ts";
import {validateNetwork,privateDirectory,compileContracts,compiledRuntimeCodeHash,Stop} from "./lib/deployment.ts";
import {loadChain,requireOperable} from "./lib/chains.ts";
import {currentFee,tipBounds} from "./lib/gas.ts";
import {BUILTIN_EPOCH_RECIPES,epochCatalogHash,epochCatalogRecipes} from "../src/epoch.ts";
import {BEACON_TEMPLATE,beaconCanonicalRequest,beaconSlotSigner,type BeaconRegistration} from "../src/beacon.ts";
import {decodeDataTemplate} from "../src/templates.ts";
import {adjacentSignerSlots,checkBeaconRecipes,loadServiceCatalog,requireExplicitSchedule,resolveSlotSigners} from "./lib/catalog.ts";
import {BEACON_ABI,answerOrMissing,findDuplicateBeacon,parseRegisterBeaconOptions,registerBeaconArgs} from "./lib/beacon-admin.ts";
import {checkBeaconSample,fetchBeaconSample} from "./lib/drand-relay.ts";
import {checkRecipeFile,RecipeFileError} from "./lib/recipe-file.ts";

const ACTIONS=["keeper","backup-committer","fee-recipient","keeper-share","transfer-owner","accept-owner","upgrade-registry","upgrade-coordinator","register-recipe","register-beacon","schedule-catalog"] as const;
type Action=typeof ACTIONS[number];
const IMPLEMENTATION_SLOT="0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";
// OpenZeppelin Initializable storage: the low 64 bits hold the initialized version.
const INITIALIZABLE_SLOT="0xf0c57e16840df040f15088dc2f81fe391c3923bec73e23a9662efc9c229c6a00";
// Gas limits for sending: storing recipe strings costs far more than a role change. A beacon registration measures about
// 620,000 gas, most of it storing the recipe and the registration and one verification of the sample round.
const GAS_CAP:Partial<Record<Action,bigint>>={"upgrade-registry":3_000_000n,"upgrade-coordinator":300_000n,"register-recipe":1_500_000n,"register-beacon":950_000n,"schedule-catalog":1_000_000n};
const ADMIN_ABI=new Interface([
  "function owner() view returns(address)","function pendingOwner() view returns(address)","function transferOwnership(address)","function acceptOwnership()",
  "function setCommitter(address)","function setFeeRecipient(address)","function setKeeperFeeBps(uint16)","function upgradeToAndCall(address,bytes) payable",
  "function epochForBlock(uint256) view returns(uint64)","function epochStart(uint64) view returns(uint64)","function sourceCountAt(uint64) view returns(uint256)",
  "function catalogAt(uint64) view returns(bytes32 hash,uint8[] recipes,address[] signers)","function scheduleCatalog(uint8[],address[],uint64)",
  "function recipeCount() view returns(uint256)","function getRecipe(uint8) view returns(bytes32 queryHash,string canonicalRequest,bytes template,string body)",
  "function registerRecipe(string,bytes,string) returns(uint8)","function initializeRecipeRegistry()",
  "function committer() view returns(address)","function setBackupCommitter(address,bool)","function isBackupCommitter(address) view returns(bool)","function backupCommitterCount() view returns(uint256)",
  "function isAuthorizedCommitter(address) view returns(bool)",
  ...BEACON_ABI,
  "error InvalidConfig()","error InvalidEpoch()","error InvalidRecipe()","error InvalidTemplate()","error InvalidInitialization()","error RenounceDisabled()",
  "error OwnableUnauthorizedAccount(address)","error ERC1967InvalidImplementation(address)","error UUPSUnsupportedProxiableUUID(bytes32)","error InvalidFallback()",
  "error BeaconGasTooLow()","error InsufficientGas()",
]);
const USAGE=`Usage: node scripts/admin.ts <${ACTIONS.join("|")}> --manifest <deployments/network.json> [options] [--apply --env <owner key file>]`;
/// The revert reason of a failed simulation, decoded from the registry's errors when possible.
function revertReason(error:unknown):string {
  const failure=error as {data?:unknown;info?:{error?:{data?:unknown}}};
  const data=failure?.data??failure?.info?.error?.data;
  if(typeof data==="string"&&data.length>=10){const parsed=ADMIN_ABI.parseError(data);if(parsed)return `${parsed.name}(${parsed.args.join(",")})`;}
  return error instanceof Error?error.message.split("\n")[0]:String(error);
}
function list(value:string|undefined,pattern:RegExp,what:string):string[]|undefined {
  if(value===undefined)return undefined;
  const items=value.split(",").map(item=>item.trim());
  if(items.some(item=>!pattern.test(item)))throw new Stop(`Invalid --${what}: ${value}`);
  return items;
}

async function main(){
  const {values,positionals}=parseArgs({allowPositionals:true,options:{manifest:{type:"string"},address:{type:"string"},bps:{type:"string"},contract:{type:"string",default:"registry"},
    recipes:{type:"string"},"allow-signed-recipes":{type:"boolean",default:false},signers:{type:"string"},"from-epoch":{type:"string"},implementation:{type:"string"},file:{type:"string"},remove:{type:"boolean",default:false},
    verifier:{type:"string"},beacon:{type:"string"},"sample-round":{type:"string"},"sample-signature":{type:"string"},relay:{type:"string"},env:{type:"string"},apply:{type:"boolean",default:false}}});
  const action=positionals[0] as Action;
  if(!ACTIONS.includes(action))throw new Stop(`Unknown action ${JSON.stringify(positionals[0]??"")}. ${USAGE}`);
  if(!values.manifest)throw new Stop(`Provide --manifest. ${USAGE}`);
  const manifestPath=resolve(values.manifest),manifest=JSON.parse(await readFile(manifestPath,"utf8"));
  const chain=await loadChain(manifest.network);
  // The actions and ABI here are EpochEntropy's; an Arbitrum chain's registry is another contract. Refused before any network access or key use.
  requireOperable(chain,"admin.ts");
  if(manifest.chainId!==chain.chainId)throw new Stop("Manifest and chain profile differ");
  // Mainnet proxies are owned by the DAO treasury Safe: print the transaction for a Safe proposal, never sign it here.
  if(!chain.testnet&&values.apply)throw new Stop("Mainnet owner transactions are proposed and signed in the DAO Safe; run without --apply and add the printed transaction to the Safe batch");
  if(values.apply&&!values.env)throw new Stop("--apply needs --env with the owner key file");
  // Checked before any network access: no catalog is ever scheduled from defaults, and a production chain states its first epoch too.
  if(action==="schedule-catalog")requireExplicitSchedule(chain,values);
  const beaconOptions=action==="register-beacon"?parseRegisterBeaconOptions(values,manifest):undefined;

  const provider=new JsonRpcProvider(chain.rpcUrls[0],undefined,{batchMaxCount:1});
  try {
    const head=await validateNetwork(provider,chain);
    const registryAction=["keeper","backup-committer","schedule-catalog","upgrade-registry","register-recipe","register-beacon"].includes(action);
    const kind=registryAction?"registry":action==="fee-recipient"||action==="keeper-share"||action==="upgrade-coordinator"?"coordinator":values.contract;
    if(kind!=="registry"&&kind!=="coordinator")throw new Stop("--contract must be registry or coordinator");
    const target=getAddress(manifest[kind]);
    if(keccak256(await provider.getCode(target))!==manifest[`${kind}CodeHash`])throw new Stop(`The ${kind} proxy code differs from the manifest`);
    const implementation=getAddress("0x"+(await provider.getStorage(target,IMPLEMENTATION_SLOT)).slice(-40));
    const implementationKind=kind==="registry"?"epochImplementation":"coordinatorImplementation";
    if(implementation!==manifest[implementationKind]||keccak256(await provider.getCode(implementation))!==manifest[`${implementationKind}CodeHash`])
      throw new Stop(`The ${kind} proxy runs ${implementation}, not the manifest's ${implementationKind}; update the manifest after a reviewed upgrade`);
    const view=new Contract(target,ADMIN_ABI,provider);
    const requiredSender=await (action==="accept-owner"?view.pendingOwner():view.owner());
    // The recipe registry exists once recipeCount answers; before the upgrade only the built-in recipe ids are known. Only a call to a function
    // the registry does not have (a revert with no data) means it is not there yet: an RPC that failed is a Stop, and never a registry
    // without the function, whose plan would skip the duplicate check and the simulation.
    const registeredRecipes:bigint|undefined=registryAction?await answerOrMissing(()=>view.recipeCount() as Promise<bigint>,"recipeCount()"):undefined;
    // Beacon recipes exist once beaconOf answers, which the upgrade to the beacon registry adds.
    const beaconsSupported=(action==="register-beacon"||action==="schedule-catalog")&&registeredRecipes!==undefined&&await answerOrMissing(()=>view.beaconOf(0),"beaconOf()")!==undefined;
    let method:string,args:unknown[],details:Record<string,unknown>|undefined,note:string|undefined,simulate=false;
    if(action==="accept-owner"){method="acceptOwnership";args=[];}
    else if(action==="keeper-share"){
      if(!/^\d{1,5}$/.test(values.bps??"")||Number(values.bps)>10000)throw new Stop("--bps must be keeper share basis points from 0 to 10000");
      method="setKeeperFeeBps";args=[Number(values.bps)];
    } else if(action==="upgrade-registry"){
      if(!values.implementation)throw new Stop("Provide --implementation <address>");
      const next=getAddress(values.implementation.toLowerCase());
      if(next===implementation)throw new Stop("The registry already uses that implementation");
      const version=BigInt(await provider.getStorage(target,INITIALIZABLE_SLOT))&((1n<<64n)-1n);
      let data="0x";
      if(version===1n){
        // A registry from before the recipe registry adopts it atomically: upgradeToAndCall runs initializeRecipeRegistry.
        if(BigInt(await provider.getStorage(target,8))!==0n)throw new Stop("Storage slot 8 (the retired four-signer catalog array) is not empty; this registry cannot adopt the recipe registry");
        if(BigInt(await provider.getStorage(target,9))!==0n)throw new Stop("A catalog is scheduled under the previous hardcoded recipe ids (slot 9 is not empty); initializeRecipeRegistry would refuse it");
        data=ADMIN_ABI.encodeFunctionData("initializeRecipeRegistry");
      } else if(registeredRecipes===undefined)throw new Stop(`Unexpected initialized version ${version} without a recipe registry`);
      // Only an implementation whose runtime code equals the freshly compiled EpochEntropy at that address is accepted.
      compileContracts();
      const runtimeCodeHash=await compiledRuntimeCodeHash("EpochEntropy",next),code=await provider.getCode(next);
      if(code==="0x")throw new Stop("The new implementation has no code");
      if(keccak256(code)!==runtimeCodeHash)throw new Stop("The new implementation's runtime code differs from the freshly compiled EpochEntropy");
      method="upgradeToAndCall";args=[next,data];simulate=true;
      details={previousImplementation:implementation,implementation:next,runtimeCodeHash,initializer:data==="0x"?null:"initializeRecipeRegistry()",
        registersRecipes:data==="0x"?[]:BUILTIN_EPOCH_RECIPES.map(r=>({id:r.id,provider:r.provider,description:r.description,canonicalRequest:r.canonicalRequest})),
        keeperPin:{EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH:runtimeCodeHash},manifestUpdate:{epochImplementation:next,epochImplementationCodeHash:runtimeCodeHash}};
      note="A keeper recreated beforehand with APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH set to this hash keeps sending until this executes, then exits with status 75 for its supervisor to restart it on the new code; move the hash into EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH afterwards. Without that preparation, keepers pinned to the previous implementation stop sending once this executes: set EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH and restart them with the matching keeper image. "+
        "On mainnet this is the first transaction of Safe batch A: calls that need the new code, such as register-beacon, follow it in the same batch. schedule-catalog goes in its own later transaction (batch B), because it expires and a late signature must not revert the upgrade.";
    } else if(action==="upgrade-coordinator"){
      if(!values.implementation)throw new Stop("Provide --implementation <address>");
      const next=getAddress(values.implementation.toLowerCase());
      if(next===implementation)throw new Stop("The coordinator already uses that implementation");
      compileContracts();
      const runtimeCodeHash=await compiledRuntimeCodeHash("D20VRFCoordinator",next),code=await provider.getCode(next);
      if(code==="0x")throw new Stop("The new implementation has no code");
      if(keccak256(code)!==runtimeCodeHash)throw new Stop("The new implementation's runtime code differs from the freshly compiled D20VRFCoordinator");
      // The new payment path reads isAuthorizedCommitter on the registry, so the registry upgrade comes first.
      const registryProxy=new Contract(getAddress(manifest.registry),ADMIN_ABI,provider);
      const registryReady=await answerOrMissing(()=>registryProxy.isAuthorizedCommitter(getAddress(manifest.registry)),"isAuthorizedCommitter()")!==undefined;
      method="upgradeToAndCall";args=[next,"0x"];simulate=true;
      details={previousImplementation:implementation,implementation:next,runtimeCodeHash,initializer:null,
        registryAuthorizationView:registryReady?"present":"missing: upgrade the registry first in this batch",
        keeperPin:{EXPECTED_IMPLEMENTATION_CODE_HASH:runtimeCodeHash},manifestUpdate:{coordinatorImplementation:next,coordinatorImplementationCodeHash:runtimeCodeHash}};
      note="Each accepted proof pays its keeper share to the submitting wallet when the registry authorizes it (the committer or an allowed backup committer) and to committer() otherwise. Requests, refunds, retries, the treasury share and the permissionless submission rule are unchanged. "+
        (registryReady?"":"Place this transaction after the registry upgradeToAndCall in the same Safe batch of the two upgrades: until the registry answers isAuthorizedCommitter, every share falls back to committer(). ")+
        "A keeper recreated beforehand with APPROVED_NEXT_IMPLEMENTATION_CODE_HASH set to this hash keeps sending until this executes, then exits with status 75 for its supervisor to restart it on the new code; move the hash into EXPECTED_IMPLEMENTATION_CODE_HASH afterwards. Without that preparation, keepers pinned to the previous implementation stop sending once this executes: set EXPECTED_IMPLEMENTATION_CODE_HASH and restart them with the matching keeper image.";
    } else if(action==="register-recipe"){
      if(!values.file)throw new Stop("Provide --file <recipe.json>");
      let checked;
      try{checked=checkRecipeFile(await readFile(resolve(values.file),"utf8"));}
      catch(error){if(error instanceof RecipeFileError)throw new Stop(`Recipe file rejected: ${error.message}`);throw error;}
      const {recipe}=checked;
      if(registeredRecipes!==undefined){
        if(registeredRecipes>=256n)throw new Stop("The registry already holds the maximum of 256 recipes");
        for(let existing=0n;existing<registeredRecipes;existing++){
          const [,canonicalRequest,template,body]=await view.getRecipe(existing);
          if(canonicalRequest===recipe.canonicalRequest&&template===recipe.template&&body===recipe.body)throw new Stop(`This exact recipe is already registered as id ${existing}`);
        }
      } else if(values.apply)throw new Stop("The registry does not have the recipe registry yet; apply upgrade-registry first");
      method="registerRecipe";args=[recipe.canonicalRequest,recipe.template,recipe.body];simulate=registeredRecipes!==undefined;
      details={recipeId:String(registeredRecipes??BigInt(BUILTIN_EPOCH_RECIPES.length)),description:checked.description,signer:checked.signer,queryHash:checked.queryHash,
        canonicalRequest:recipe.canonicalRequest,body:recipe.body,template:recipe.template,templateSegments:decodeDataTemplate(recipe.template),
        selfCheck:{canonicalHashMatchesSample:true,signatureRecoversSigner:true,sampleMatchesTemplate:true,sampleTimestamp:checked.sample.timestamp,sampleData:checked.sample.data}};
      note=(registeredRecipes===undefined?"Not simulated: the registry still runs the previous implementation. Propose this call once the upgrade has executed, in its own Safe transaction, where it simulates and receives the printed id. ":"")+
        "The recipe can never be edited or removed. It serves epochs only once a scheduled catalog lists its id with this signer.";
    } else if(action==="register-beacon"){
      const {preset,verifier,relay,round,sample:given}=beaconOptions!,verifierCode=await provider.getCode(verifier);
      if(verifierCode==="0x")throw new Stop(`The verifier ${verifier} has no code`);
      // Only a verifier whose runtime code equals the freshly compiled D20BeaconVerifier is accepted.
      compileContracts();
      const verifierCodeHash=await compiledRuntimeCodeHash("D20BeaconVerifier",verifier);
      if(keccak256(verifierCode)!==verifierCodeHash)throw new Stop("The verifier's runtime code differs from the freshly compiled D20BeaconVerifier");
      const sample=given??await fetchBeaconSample(relay,preset,{round});
      // The sample verifies here and is not scheduled after the head block, as registerBeacon requires; the verifier must then agree.
      const sampleTime=checkBeaconSample(preset,sample,head.timestamp),verifierView=new Contract(verifier,ADMIN_ABI,provider);
      if(!await verifierView.isValidPublicKey(preset.publicKey))throw new Stop("The verifier does not accept the beacon's public key");
      if(!await verifierView.verifyRound(preset.publicKey,sample.round,sample.signature))throw new Stop(`The verifier does not accept the sample: its signature of round ${sample.round}`);
      if(registeredRecipes!==undefined&&registeredRecipes>=256n)throw new Stop("The registry already holds the maximum of 256 recipes");
      const registration:BeaconRegistration={verifier,chainHash:preset.chainHash,publicKey:preset.publicKey,genesis:preset.genesis,period:preset.period},request=beaconCanonicalRequest(preset.chainHash);
      if(beaconsSupported){
        const registered:Array<{id:number;canonicalRequest:string;beacon?:BeaconRegistration}>=[];
        for(let id=0n;id<registeredRecipes!;id++){
          const [,canonicalRequest]=await view.getRecipe(id);
          if(canonicalRequest!==request)continue;
          const {verifier:registeredVerifier,genesis,period,chainHash,publicKey}=await view.beaconOf(id);
          registered.push({id:Number(id),canonicalRequest,beacon:BigInt(registeredVerifier)===0n?undefined:{verifier:registeredVerifier,chainHash,publicKey,genesis,period}});
        }
        const duplicate=findDuplicateBeacon(registration,registered);
        if(duplicate!==undefined)throw new Stop(`This beacon is already registered as recipe ${duplicate}`);
      } else if(values.apply)throw new Stop("The registry does not support beacons yet; apply upgrade-registry first");
      method="registerBeacon";args=registerBeaconArgs(verifier,preset,sample);simulate=beaconsSupported;
      details={recipeId:String(registeredRecipes??BigInt(BUILTIN_EPOCH_RECIPES.length)),canonicalRequest:request,body:request,template:BEACON_TEMPLATE,templateSegments:decodeDataTemplate(BEACON_TEMPLATE),
        verifier,verifierCodeHash,slotSigner:beaconSlotSigner(registration),preset:preset.name,chainHash:preset.chainHash,genesis:preset.genesis,period:preset.period,
        sample:{round:sample.round,time:sampleTime,timeUtc:new Date(Number(sampleTime)*1000).toISOString(),secondsBeforeHeadBlock:BigInt(head.timestamp)-sampleTime,signature:sample.signature,
          source:given?"command line":relay},
        selfCheck:{sampleVerifiesLocally:true,sampleNotAfterHeadBlock:true,verifierCodeEqualsCompiled:true,verifierAcceptsPublicKey:true,verifierAcceptsSample:true,
          notAlreadyRegistered:beaconsSupported?true:"not checked: the registry has no beacons yet"},
        manifestUpdate:{beaconVerifier:verifier,beaconVerifierCodeHash:verifierCodeHash}};
      note=(beaconsSupported?"":"Not simulated: the registry still runs the previous implementation, which has no registerBeacon. Propose this call in Safe batch A, right after the upgradeToAndCall, where it runs on the new code. The selfCheck lists what was verified here; a revert would revert the whole batch, upgrade included. ")+
        "The recipe can never be edited or removed. It serves epochs only once a scheduled catalog lists its id with its slotSigner: schedule-catalog reads that from the registry, in its own later transaction (batch B on mainnet), because it expires.";
    } else if(action==="backup-committer"){
      if(!values.address)throw new Stop("Provide --address with the follower keeper's transaction wallet");
      const account=getAddress(values.address.toLowerCase()),allowed=!values.remove;
      if(BigInt(account)===0n)throw new Stop("Zero address refused");
      if(registeredRecipes!==undefined){
        const [committer,current,count]=await Promise.all([view.committer(),view.isBackupCommitter(account),view.backupCommitterCount()]);
        if(allowed&&account===committer)throw new Stop("That wallet is the primary committer; a follower needs its own transaction wallet");
        if(current===allowed)throw new Stop(`That wallet is ${allowed?"already":"not"} a backup committer`);
        details={account,allowed,primaryCommitter:committer,backupCommitters:{before:String(count),after:String(allowed?count+1n:count-1n),maximum:4}};
      } else {
        if(values.apply)throw new Stop("The registry does not support backup committers yet; apply upgrade-registry first");
        details={account,allowed};
      }
      method="setBackupCommitter";args=[account,allowed];simulate=registeredRecipes!==undefined;
      note=(registeredRecipes!==undefined?"":"Not simulated: the registry still runs the previous implementation. Propose this call once the upgrade has executed, in its own Safe transaction: a call that reverts on its own must never be able to revert the upgrade with it. ")+
        (allowed?"The wallet may publish epochs exactly like the committer, and with the upgraded coordinator it earns the keeper share of the requests it serves. Fund it for gas and register it with any external monitoring that classifies submitters before the follower keeper starts sending."
          :"The follower keeper using this wallet stops sending at its next authorization check; it keeps running and reconciling any transaction it already signed, and reports the lost authorization as a health fault.");
    } else if(action==="schedule-catalog"){
      // No recipes are assumed (requireExplicitSchedule has seen --recipes); a built-in recipe's signer is its provider's Airnode.
      const service=await loadServiceCatalog();
      const recipes=list(values.recipes,/^\d{1,3}$/,"recipes")!.map(Number);
      const signers=list(values.signers,/^0x[0-9a-fA-F]{40}$/,"signers")?.map(signer=>getAddress(signer.toLowerCase()));
      if(signers&&signers.length!==recipes.length)throw new Stop(`--signers lists ${signers.length} addresses for ${recipes.length} recipes`);
      const known=registeredRecipes??BigInt(BUILTIN_EPOCH_RECIPES.length);
      // A beacon recipe's signer is the identity its registration derives, which the registry answers; only a registry with beacons has them.
      const beacons=new Map<number,{verifier:string;chainHash:string;genesis:bigint;period:bigint;signer:string}>();
      if(beaconsSupported)for(const recipe of new Set(recipes)){
        if(BigInt(recipe)>=known)continue;
        const {verifier,genesis,period,chainHash}=await view.beaconOf(recipe);
        if(BigInt(verifier)!==0n)beacons.set(recipe,{verifier,chainHash,genesis,period,signer:await view.slotSigner(recipe)});
      }
      // Keeper 0.4.1 and later serve beacon recipes only. A signed recipe, or an id mistyped into one, is refused by name unless the
      // emergency override says that the keepers still serve them.
      const signedRecipes=checkBeaconRecipes(recipes,{registered:known,beacons:new Set(beacons.keys()),allowSigned:values["allow-signed-recipes"]});
      let slotSigners:string[];
      try{slotSigners=resolveSlotSigners(recipes,{explicit:signers,beaconSigners:new Map([...beacons].map(([recipe,beacon])=>[recipe,beacon.signer])),providers:service.signers});}
      catch(error){throw new Stop((error as Error).message);}
      try{epochCatalogRecipes({recipes,signers:slotSigners});}catch{throw new Stop("A catalog lists 1 to 10 distinct recipe ids below 256, each with a nonzero signer");}
      const missing=recipes.filter(recipe=>BigInt(recipe)>=known);
      if(missing.length)throw new Stop(`Recipes ${missing.join(",")} are not registered (${registeredRecipes===undefined?"before the upgrade only the built-in ids 0-5 exist":`the registry holds ids 0-${known-1n}`}); register them first`);
      const current=await view.epochForBlock(head.number) as bigint;
      if(values["from-epoch"]!==undefined&&!/^\d{1,19}$/.test(values["from-epoch"]))throw new Stop("Invalid --from-epoch");
      const fromEpoch=values["from-epoch"]===undefined?current+2n:BigInt(values["from-epoch"]);
      if(fromEpoch<current+2n)throw new Stop(`--from-epoch must be at least ${current+2n}, two epochs after the current epoch ${current}`);
      const catalogHash=epochCatalogHash(slotSigners,recipes);
      // scheduleCatalog requires fromEpoch >= epoch(block)+2 when it executes, so it must land before epochStart(fromEpoch-1).
      const executeBeforeBlock=await view.epochStart(fromEpoch-1n) as bigint;
      let replaces;
      if(registeredRecipes!==undefined){
        const [hash,inForceRecipes,inForceSigners]=await view.catalogAt(fromEpoch);
        if(hash===catalogHash)throw new Stop("That catalog is already in force from fromEpoch");
        replaces={catalogHash:hash,recipes:inForceRecipes.map(Number),signers:Array.from(inForceSigners)};
      } else if(values.apply)throw new Stop("The registry does not have the recipe registry yet; apply upgrade-registry first");
      const describe=async(recipe:number)=>{
        const beacon=beacons.get(recipe);
        if(beacon)return {canonicalRequest:(await view.getRecipe(recipe)).canonicalRequest,beacon:{chainHash:beacon.chainHash,genesis:beacon.genesis,period:beacon.period,verifier:beacon.verifier}};
        return registeredRecipes!==undefined&&recipe>=BUILTIN_EPOCH_RECIPES.length
          ?{canonicalRequest:(await view.getRecipe(recipe)).canonicalRequest}:{provider:BUILTIN_EPOCH_RECIPES[recipe].provider,description:BUILTIN_EPOCH_RECIPES[recipe].description};
      };
      method="scheduleCatalog";args=[recipes,slotSigners,fromEpoch];simulate=registeredRecipes!==undefined;
      const adjacent=adjacentSignerSlots(slotSigners);
      details={currentEpoch:String(current),fromEpoch:String(fromEpoch),catalogHash,sources:await Promise.all(recipes.map(async(recipe,slot)=>({slot,recipe,...await describe(recipe),signer:slotSigners[slot]}))),
        adjacentSignerSlots:adjacent,signedRecipes:signedRecipes.length?signedRecipes:undefined,replaces,headBlock:head.number,executeBeforeBlock:String(executeBeforeBlock),
        approximateSecondsLeft:chain.expectedBlockMs?Math.floor(Number(executeBeforeBlock-BigInt(head.number))*chain.expectedBlockMs/1000):undefined};
      note=(registeredRecipes!==undefined?"":"Not simulated: the registry still runs the previous implementation. Propose this call once the upgrade has executed, in its own Safe transaction: a call that reverts on its own must never be able to revert the upgrade with it. ")+
        "It reverts with InvalidEpoch once executeBeforeBlock is reached; pass a later --from-epoch when signing needs more time. A catalog still pending, taking effect two or more epochs ahead, is replaced; the one taking effect at the next epoch is kept, and so is every active one, so the current and next epoch keep their catalog."+
        (adjacent.length?" Some neighbouring slots share a signer, so a failed provider would also fail their fallback.":"")+
        (beacons.size?" A beacon slot's signer is its slotSigner, read from the registry, which refuses any other.":"")+
        (signedRecipes.length?" WARNING, --allow-signed-recipes: this catalog lists signed API recipes (signedRecipes), which keepers 0.4.1 and later do not serve. Every keeper, the primary and each follower, must run 0.4.0 before it takes effect; on 0.4.1 the epochs that select a signed recipe stay unpublished, with the health fault epoch_recipe_unsupported, and the requests that wait on them become refundable after expiry.":"");
    } else {
      if(!values.address)throw new Stop("Provide --address with the new public address");
      const next=getAddress(values.address.toLowerCase());
      if(BigInt(next)===0n)throw new Stop("Zero role address refused");
      method=action==="keeper"?"setCommitter":action==="fee-recipient"?"setFeeRecipient":"transferOwnership";args=[next];
    }
    const data=ADMIN_ABI.encodeFunctionData(method,args);
    const transaction={chainId:chain.chainId,from:requiredSender,to:target,value:"0",data};
    // Registry writes are simulated as the owner before they are printed or signed.
    if(simulate)await provider.call(transaction).catch(error=>{throw new Stop(`${method} reverts when simulated from the owner: ${revertReason(error)}`);});
    if(action==="keeper")note="Drain and stop the previous keeper; rotate the committer, then migrate the drained journal to the new wallet before restarting. Keep the VRF key unchanged.";
    if(action==="transfer-owner")note="The proposed owner must separately accept ownership on this contract.";
    console.log(JSON.stringify({action,transaction,call:details?{method,args}:undefined,details,apply:values.apply,note},(_key,value)=>typeof value==="bigint"?value.toString():value,2));
    if(!values.apply)return;
    const signer=await loadDeployer(resolve(values.env!),provider);
    if(signer.address!==requiredSender)throw new Stop(`The signing wallet ${signer.address} is not the required sender ${requiredSender}`);
    const nonce=await signer.getNonce("latest");
    if(await signer.getNonce("pending")!==nonce)throw new Stop("The signing wallet has an unresolved pending transaction");
    const cap=GAS_CAP[action]??300_000n,gas=await provider.estimateGas({...transaction,from:signer.address});
    if(gas>cap)throw new Stop(`Estimated gas ${gas} exceeds the ${action} cap of ${cap}`);
    const gasLimit=gas*12n/10n>cap?cap:gas*12n/10n,{maxFee,tip}=await currentFee(provider,BigInt(chain.gas.maxFeePerGasWei),tipBounds(chain.gas));
    const raw=await signer.signTransaction({to:target,data,value:0,chainId:chain.chainId,type:2,nonce,gasLimit,maxFeePerGas:maxFee,maxPriorityFeePerGas:tip});
    const hash=keccak256(getBytes(raw)),journalDirectory=join(dirname(manifestPath),"administration");
    await privateDirectory(journalDirectory);
    const journal=await open(join(journalDirectory,`${signer.address}-${nonce}.json`),"ax",0o600);
    try {await journal.writeFile(JSON.stringify({action,transaction,hash,raw})+"\n");await journal.sync();}finally{await journal.close();}
    const sent=await provider.broadcastTransaction(raw);
    if(sent.hash!==hash)throw new Stop("The RPC returned a different transaction hash");
    const receipt=await sent.wait(1,60000);
    if(receipt?.status!==1)throw new Stop(`Transaction ${hash} was not confirmed successfully; inspect it and the journal before retrying`);
    console.log(JSON.stringify({confirmed:true,action,transactionHash:hash,block:receipt.blockNumber,gasUsed:String(receipt.gasUsed)}));
  } finally {provider.destroy();}
}
// Every failure prints its reason. Key files are read only by loadDeployer, whose errors never include their contents.
main().catch(error=>{
  console.error(`Arc administration stopped: ${error instanceof Error?error.message:String(error)}`);
  if(!(error instanceof Stop))console.error("Credentials were not logged; inspect the transaction plan and the local journal before retrying.");
  process.exitCode=1;
});
