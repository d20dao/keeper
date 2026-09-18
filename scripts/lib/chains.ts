import {readFile} from "node:fs/promises";
import {getAddress} from "ethers";
export interface Chain {
  key:string;name:string;chainId:number;testnet:boolean;rpcUrls:string[];explorerUrl:string;
  nativeCurrency:{name:string;symbol:string;decimals:number};compilerEvmTarget:string;
  create2:{factory:string;codeHash:string};
  /** Owner and initial fee recipient of new deployments: a multisig on production chains. */
  owner:string;
  gas:{maxGas:number;maxFeePerGasWei:string;cancelMaxFeePerGasWei:string;maxTxCostWei:string};
  expectedBlockMs?:number;
}
export async function loadChain(key="arc-testnet"):Promise<Chain>{
  const chains=JSON.parse(await readFile(new URL("../../chains.json",import.meta.url),"utf8"));
  if(!Object.hasOwn(chains,key))throw new Error("Unknown chain in chains.json");
  const chain=chains[key] as Chain;
  if(!Number.isSafeInteger(chain.chainId)||chain.chainId<=0||!Array.isArray(chain.rpcUrls)||chain.rpcUrls.length===0||chain.rpcUrls.some(url=>new URL(url).protocol!=="https:")||!/^0x[0-9a-fA-F]{64}$/.test(chain.create2?.codeHash??""))throw new Error("Invalid chain configuration");
  getAddress(chain.create2.factory);
  if(typeof chain.owner!=="string"||getAddress(chain.owner)!==chain.owner)throw new Error("Chain owner must be a checksummed address");
  if(chain.nativeCurrency.decimals!==18)throw new Error("This deployment profile requires native18 accounting");
  return {...chain,key};
}
