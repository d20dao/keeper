// The image workflow's push step against a recording fake docker: the moving :main tag follows refs/heads/main alone, and a run from any
// other ref publishes its commit tag only. No Docker daemon is used.
import {test} from "node:test";
import assert from "node:assert/strict";
import {spawnSync} from "node:child_process";
import {mkdtempSync,mkdirSync,readFileSync,writeFileSync,chmodSync,rmSync} from "node:fs";
import {tmpdir} from "node:os";
import {join,resolve,dirname,delimiter} from "node:path";
import {fileURLToPath} from "node:url";

const repo=resolve(dirname(fileURLToPath(import.meta.url)),"../..");
const hasBash=!spawnSync("bash",["-c","true"]).error;
const SHA="0123456789abcdef0123456789abcdef01234567",IMAGE="ghcr.io/d20dao/keeper",DIGEST=`${IMAGE}@sha256:${"ab".repeat(32)}`;
const FAKE_DOCKER=`#!/bin/sh
printf '%s\\n' "$*" >> "$FAKE_DOCKER_LOG"
case "$1" in inspect) echo "${DIGEST}";; esac
exit 0
`;

/** The shell body of the push step, taken from the workflow file as written. */
function pushStep():string{
  const lines=readFileSync(join(repo,".github/workflows/keeper-image.yml"),"utf8").split("\n");
  const named=lines.findIndex(line=>line.includes("name: Push the smoke-tested image"));
  const run=lines.findIndex((line,index)=>index>named&&/^\s+run: \|\s*$/.test(line));
  assert.ok(named>=0&&run>named,"the push step is in the workflow");
  const indent=(line:string)=>line.length-line.trimStart().length,body:string[]=[];
  for(const line of lines.slice(run+1)){
    if(line.trim()!==""&&indent(line)<=indent(lines[run]))break;
    body.push(line);
  }
  const base=indent(body[0]);
  return body.map(line=>line.slice(base)).join("\n")+"\n";
}

function runPushStep(ref:string){
  const root=mkdtempSync(join(tmpdir(),"d20dao-image-")),bin=join(root,"bin"),log=join(root,"docker.log"),summary=join(root,"summary.md");
  try{
    mkdirSync(bin);writeFileSync(join(bin,"docker"),FAKE_DOCKER);chmodSync(join(bin,"docker"),0o755);writeFileSync(log,"");writeFileSync(summary,"");
    const env:Record<string,string|undefined>={...process.env};
    const pathKey=Object.keys(env).find(key=>key.toUpperCase()==="PATH")??"PATH",path=env[pathKey];
    delete env[pathKey];
    Object.assign(env,{PATH:`${bin}${delimiter}${path}`,FAKE_DOCKER_LOG:log,GHCR_TOKEN:"token",GITHUB_REPOSITORY_OWNER:"D20DAO",GITHUB_ACTOR:"runner",GITHUB_SHA:SHA,GITHUB_REF:ref,GITHUB_STEP_SUMMARY:summary});
    const script=join(root,"step.sh");writeFileSync(script,pushStep());
    const result=spawnSync("bash",[script],{encoding:"utf8",env:env as NodeJS.ProcessEnv});
    assert.equal(result.status,0,result.stderr);
    const commands=readFileSync(log,"utf8").split("\n").filter(Boolean);
    return {commands,pushed:commands.filter(command=>command.startsWith("push ")).map(command=>command.slice(5)),summary:readFileSync(summary,"utf8")};
  }finally{rmSync(root,{recursive:true,force:true});}
}

test("pushes :main only from refs/heads/main",{skip:!hasBash},()=>{
  const main=runPushStep("refs/heads/main");
  assert.deepEqual(main.pushed,[`${IMAGE}:sha-${SHA}`,`${IMAGE}:main`]);
  assert.ok(main.commands.includes(`tag d20dao-keeper:local ${IMAGE}:main`));
  assert.ok(main.summary.includes(DIGEST)&&main.summary.includes(SHA));
});

test("pushes the commit tag alone from a branch, a tag or a pull request ref",{skip:!hasBash},()=>{
  for(const ref of ["refs/heads/rh/arc-pins","refs/heads/main-hotfix","refs/heads/hotfix/0.4.x","refs/tags/v0.4.1","refs/pull/7/merge","refs/heads/main/extra"]){
    const other=runPushStep(ref);
    assert.deepEqual(other.pushed,[`${IMAGE}:sha-${SHA}`],ref);
    assert.ok(!other.commands.some(command=>command.includes(`${IMAGE}:main`)),`${ref} never tags or pushes :main`);
    assert.ok(other.summary.includes(DIGEST),ref);
  }
});
