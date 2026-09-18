// Airnode signers and the default epoch catalog from config/service.json, resolved through the replay library's built-in recipes.
import {readFile} from "node:fs/promises";
import {getAddress} from "ethers";
import {BUILTIN_EPOCH_RECIPES,INITIAL_EPOCH_RECIPES,epochCatalogRecipes,type EpochProvider} from "../../src/epoch.ts";

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
/// The four signers a new registry is initialized with: recipes 0-3 of its initial catalog.
export function initialCatalogSigners(signers:Record<EpochProvider,string>):[string,string,string,string]{
  return catalogSigners(INITIAL_EPOCH_RECIPES,signers) as [string,string,string,string];
}
/// Slots whose signer equals the next slot's (cyclically): a failed provider would also take that slot's fallback.
export function adjacentSignerSlots(signers:readonly string[]):number[]{
  return signers.length<2?[]:signers.flatMap((signer,slot)=>getAddress(signer)===getAddress(signers[(slot+1)%signers.length])?[slot]:[]);
}
