// A reviewed storage baseline prevents accidental layout drift in upgradeable implementations.
import {readFileSync,writeFileSync,mkdirSync} from "node:fs";
import {resolve} from "node:path";
import assert from "node:assert/strict";
const write=process.argv.includes("--write");
// The Robinhood round coordinator keeps BeaconBook's state in an ERC-7201 namespace, which the compiler's layout of the coordinator does
// not show: BeaconBookStorageProbe declares the namespace struct as its only state variable, so its baseline records the struct's members.
for(const [directory,name] of [["","EpochEntropy"],["","D20VRFCoordinator"],["robinhood/","D20VRFCoordinatorRobinhood"],["robinhood/test/","BeaconBookStorageProbe"]]){
  const artifact=JSON.parse(readFileSync(`artifacts/contracts/${directory}${name}.sol/${name}.json`,"utf8"));
  const build=JSON.parse(readFileSync(`artifacts/build-info/${artifact.buildInfoId}.output.json`,"utf8"));
  const output=(build.output??build).contracts[artifact.inputSourceName][name];
  assert.equal("0x"+output.evm.bytecode.object,artifact.bytecode,"Compile current artifacts first");
  const layout=output.storageLayout;
  assert(layout,"Compiler storage layout output is required");
  function type(id){
    const t=layout.types[id],result={encoding:t.encoding,label:t.label,numberOfBytes:t.numberOfBytes};
    for(const key of ["key","value","base"])if(t[key])result[key]=type(t[key]);
    if(t.members)result.members=t.members.map(field=>({label:field.label,slot:field.slot,offset:field.offset,type:type(field.type)}));
    return result;
  }
  const snapshot={contract:name,openzeppelin:"5.6.1",storage:layout.storage.map(field=>({label:field.label,slot:field.slot,offset:field.offset,type:type(field.type)}))};
  const file=resolve(`storage-layout/${name}.json`);
  if(write){mkdirSync("storage-layout",{recursive:true});writeFileSync(file,JSON.stringify(snapshot,null,2)+"\n");}
  else assert.deepEqual(snapshot,JSON.parse(readFileSync(file,"utf8")),`${name} storage changed: perform an upgrade-layout review before updating the baseline`);
}
console.log(write?"Storage baselines written for review.":"Reviewed implementation storage layouts match.");
