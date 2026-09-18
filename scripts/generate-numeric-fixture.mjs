// Public, deterministic test key only. Generate with Node's JSON.stringify so
// the Rust signature tests do not sign their own reconstructed representation.
import {Wallet,keccak256,toUtf8Bytes,concat,toBeHex,getBytes} from 'ethers';
import {writeFileSync} from 'node:fs';
const signer=new Wallet(toBeHex(13n,32));
const canonical='["lastTrade",[["assetClass","crypto"],["symbol","BTCUSD"]]]';
const queryHash=keccak256(toUtf8Bytes(canonical));
const rows=[];
for(const size of [0,-0,1e-7,1e-6,1e-5,6e-8,1e20,1e21,7.6349990000000005,1.6200000147420003e-8,9007199254740992,Number.MIN_VALUE]){
  const data={symbol:'BTCUSD',price:76439.99,size,timestamp:1789491295769};
  const exact=JSON.stringify(data);
  const timestamp='1789491296';
  const digest=keccak256(concat([queryHash,toBeHex(BigInt(timestamp),32),toUtf8Bytes(exact)]));
  const signature=await signer.signMessage(getBytes(digest));
  rows.push({exact,envelope:{airnode:signer.address,requestHash:queryHash,timestamp,data,signature}});
}
writeFileSync('test/fixtures/tickerlayer-numeric-js.json',JSON.stringify({testOnly:true,canonical,rows},null,2)+'\n');
