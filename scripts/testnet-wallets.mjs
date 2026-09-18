// Creates NEW disposable testnet identities only; never sends funds or prints secret material.
import {mkdir,mkdtemp,writeFile} from 'node:fs/promises';
import {resolve,join} from 'node:path';
import {Wallet,SigningKey} from 'ethers';
import {execFileSync} from 'node:child_process';
import {loadChain} from './lib/chains.ts';
const chain=await loadChain(process.argv[2]??'arc-testnet');
if(!chain.testnet)throw new Error('This helper creates isolated testnet identities only');
await mkdir('secrets',{recursive:true});
const dir=await mkdtemp(resolve(`secrets/${chain.key}-`));
if(process.platform==='win32'){
  const sid=execFileSync('powershell.exe',['-NoProfile','-Command','[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value'],{encoding:'utf8',windowsHide:true}).trim();
  if(!/^S-1-[0-9-]+$/.test(sid))throw new Error('Cannot establish current Windows owner');
  execFileSync('icacls.exe',[dir,'/inheritance:r','/grant:r',`*${sid}:(OI)(CI)F`],{windowsHide:true,stdio:'pipe'});
}
const roles={};
for(const role of ['deployer','requester','keeper-tx','vrf']){
  const wallet=Wallet.createRandom();const keyFile=join(dir,role+'.key');
  await writeFile(keyFile,wallet.privateKey+'\n',{mode:0o600,flag:'wx'});
  const point=SigningKey.computePublicKey(wallet.privateKey,false);
  roles[role]={address:wallet.address,keyFile,...(role==='vrf'?{publicKey:[BigInt('0x'+point.slice(4,68)).toString(),BigInt('0x'+point.slice(68)).toString()],needsFunding:false}:{needsFunding:true})};
}
const manifest={chainId:chain.chainId,purpose:'isolated testnet identities only',createdAt:new Date().toISOString(),roles};
await writeFile(join(dir,'wallets.json'),JSON.stringify(manifest,null,2)+'\n',{mode:0o600,flag:'wx'});
console.log(JSON.stringify({network:chain.key,manifest:join(dir,'wallets.json'),fundOnlyOnConfiguredTestnet:Object.fromEntries(Object.entries(roles).filter(([role])=>role!=='vrf').map(([role,r])=>[role,r.address]))},null,2));
