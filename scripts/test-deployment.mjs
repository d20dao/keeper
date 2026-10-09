// Runs the deployment tests: every scripts/tests/*.test.ts that the test:round, test:admin, test:docker-wrapper and test:image-workflow scripts of
// package.json do not name. A new test file joins this group by existing, so adding one does not edit package.json; a file named by one of the other scripts
// runs there and not here, so no test file is left out and none runs twice.
import {readFile,readdir} from "node:fs/promises";
import {spawnSync} from "node:child_process";
import {dirname,join} from "node:path";
import {fileURLToPath} from "node:url";

const root=dirname(dirname(fileURLToPath(import.meta.url)));
const {scripts}=JSON.parse(await readFile(join(root,"package.json"),"utf8"));
const elsewhere=new Set(["test:round","test:admin","test:docker-wrapper","test:image-workflow"].flatMap(name=>(scripts[name]??"").match(/scripts\/tests\/[\w.-]+\.test\.ts/g)??[]));
const files=(await readdir(join(root,"scripts","tests"))).filter(name=>name.endsWith(".test.ts")).map(name=>`scripts/tests/${name}`)
  .filter(file=>!elsewhere.has(file)).sort();
if(files.length===0){console.error("No deployment tests found under scripts/tests");process.exit(1);}
const run=spawnSync(process.execPath,["--test",...files],{cwd:root,stdio:"inherit"});
process.exitCode=run.status??1;
