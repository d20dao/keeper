// Airnode signers and the legacy rollout catalog from config/service.json, resolved through the replay library's built-in recipes,
// and the signer of each slot of a catalog that may list beacon recipes. The rollout catalog is a signed one, kept for the replay tests
// and fixtures: schedule-catalog never defaults to it.
import {readFile} from "node:fs/promises";
import {getAddress} from "ethers";
import {BUILTIN_EPOCH_RECIPES,INITIAL_EPOCH_RECIPES,epochCatalogRecipes,type EpochProvider} from "../../src/epoch.ts";
import {Stop} from "./deployment.ts";

/// `recipes` is the legacy signed rollout catalog, which keeper 0.4.1 and later do not serve; it is no scheduling default.
export interface ServiceCatalog {signers:Record<EpochProvider,string>;recipes:readonly number[]}
export async function loadServiceCatalog(path="config/service.json"):Promise<ServiceCatalog>{
  const profile=JSON.parse(await readFile(path,"utf8"));
  const providers=[...new Set(BUILTIN_EPOCH_RECIPES.map(recipe=>recipe.provider))];
  const signers=Object.fromEntries(providers.map(provider=>[provider,getAddress(profile.airnodeSigners?.[provider]??"")])) as Record<EpochProvider,string>;
  const recipes=profile.epochCatalog as number[];
  epochCatalogRecipes({recipes,signers:catalogSigners(recipes,signers)});
  return {signers,recipes};
}
/// Each built-in recipe's signer is its provider's Airnode. Recipes registered later have no provider label here, so
/// their signers must be given explicitly.
export function catalogSigners(recipes:readonly number[],signers:Record<EpochProvider,string>):string[]{
  return recipes.map(recipe=>{
    const entry=BUILTIN_EPOCH_RECIPES[recipe];
    if(!entry)throw new Error(`Recipe ${recipe} is not a built-in recipe, so its signer cannot be derived; list the signers explicitly`);
    return signers[entry.provider];
  });
}
/// The signer of each catalog slot. A beacon recipe's is the fixed identity its registration derives (beaconSigners, read from the
/// registry), so an explicit signer must be that one. A signed recipe's is the explicit signer, or its provider's Airnode when the
/// recipe is built in.
export function resolveSlotSigners(recipes:readonly number[],options:{explicit?:readonly string[];beaconSigners:ReadonlyMap<number,string>;providers:Record<EpochProvider,string>}):string[]{
  return recipes.map((recipe,slot)=>{
    const beacon=options.beaconSigners.get(recipe),given=options.explicit?.[slot];
    if(beacon!==undefined){
      if(given!==undefined&&getAddress(given)!==getAddress(beacon))
        throw new Error(`Slot ${slot} lists recipe ${recipe}, a beacon: its signer is ${getAddress(beacon)}, the identity its registration derives, not ${getAddress(given)}`);
      return getAddress(beacon);
    }
    if(given!==undefined)return getAddress(given);
    try{return catalogSigners([recipe],options.providers)[0];}catch(error){throw new Error(`${(error as Error).message} with --signers`);}
  });
}
/// The four signers a new registry is initialized with: recipes 0-3 of its initial catalog.
export function initialCatalogSigners(signers:Record<EpochProvider,string>):[string,string,string,string]{
  return catalogSigners(INITIAL_EPOCH_RECIPES,signers) as [string,string,string,string];
}
/// Slots whose signer equals the next slot's (cyclically): a failed provider would also take that slot's fallback.
export function adjacentSignerSlots(signers:readonly string[]):number[]{
  return signers.length<2?[]:signers.flatMap((signer,slot)=>getAddress(signer)===getAddress(signers[(slot+1)%signers.length])?[slot]:[]);
}
/// schedule-catalog states its recipes on every network and its first epoch on a production chain; only a testnet defaults the epoch. No
/// recipes are assumed: the service profile's rollout catalog is a legacy signed one, and a flag left out must never schedule recipes that
/// keeper 0.4.1 and later do not serve, which leaves every epoch that selects one unpublished until another catalog takes effect.
/// The default first epoch, the current one + 2, must execute before the next epoch starts: about a minute, and less than a Safe needs to
/// collect its signatures.
export function requireExplicitSchedule(chain:{testnet:boolean;name:string},values:{recipes?:string;"from-epoch"?:string}):void{
  if(values.recipes===undefined)
    throw new Stop(`On ${chain.name}, schedule-catalog needs an explicit --recipes. No catalog is assumed: a default could schedule signed API recipes, which 0.4.1 keepers do not serve. List the recipe ids in slot order, for example --recipes 11 for the drand beacon.`);
  if(chain.testnet)return;
  if(values["from-epoch"]===undefined)
    throw new Stop(`On ${chain.name}, schedule-catalog needs an explicit --from-epoch. The owner is a Safe: a 2-of-2 Safe needs time to collect both signatures, and the default, the current epoch + 2, leaves about a minute before the call reverts with InvalidEpoch. Choose an epoch far enough ahead for the signatures; the plan prints executeBeforeBlock, the block the transaction must execute before.`);
}
/// The recipes of a catalog that keeper 0.4.1 and later cannot serve: the registered ones without a beacon registration, which are signed API
/// recipes. A catalog that lists one leaves every epoch that selects it unpublished, and the requests that wait on it refundable after expiry,
/// until another catalog takes effect, two epochs after it is scheduled at the earliest; a mistyped id (`--recipes 1` for `--recipes 11`) is
/// enough. They are refused by name unless `allowSigned` (--allow-signed-recipes, for an emergency that has moved every keeper to 0.4.0) says
/// otherwise, and then they are returned for the plan's warning. An id the registry does not hold is not among them: the registration check
/// reports it.
export function checkBeaconRecipes(recipes:readonly number[],options:{registered:bigint;beacons:ReadonlySet<number>;allowSigned:boolean}):number[]{
  const signed=recipes.filter(recipe=>BigInt(recipe)<options.registered&&!options.beacons.has(recipe));
  if(signed.length===0||options.allowSigned)return signed;
  const many=signed.length>1;
  throw new Stop(`${many?`Recipes ${signed.join(", ")} have`:`Recipe ${signed[0]} has`} no beacon registration, and 0.4.1 keepers do not serve signed API recipes: every epoch that selects ${many?"one of them":"it"} would stay unpublished until another catalog takes effect, two epochs after it is scheduled at the earliest. List a registered beacon's id, such as --recipes 11. --allow-signed-recipes overrides this for an emergency; every keeper must then run 0.4.0 before the catalog takes effect.`);
}
