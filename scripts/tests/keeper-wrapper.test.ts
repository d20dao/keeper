// deploy/docker/keeper.sh against a recording fake docker: the single keeper keeps its fixed names and several named
// instances get separate projects, configuration, volumes and images. No Docker daemon is used.
import {test} from "node:test";
import assert from "node:assert/strict";
import {spawnSync} from "node:child_process";
import {mkdtempSync,mkdirSync,copyFileSync,writeFileSync,readFileSync,existsSync,rmSync,chmodSync} from "node:fs";
import {tmpdir} from "node:os";
import {join,resolve,dirname,delimiter} from "node:path";
import {fileURLToPath} from "node:url";

const repo=resolve(dirname(fileURLToPath(import.meta.url)),"../..");
const hasSh=!spawnSync("sh",["-c","true"]).error;
const FAKE_DOCKER=`#!/bin/sh
{ printf 'docker'; for arg in "$@"; do printf ' %s' "$arg"; done
  printf ' | KEYS_VOLUME=%s KEEPER_STATE_VOLUME=%s KEEPER_IMAGE=%s TOKIO_WORKER_THREADS=%s KEEPER_ENV_FILE=%s\\n' "\${KEYS_VOLUME-}" "\${KEEPER_STATE_VOLUME-}" "\${KEEPER_IMAGE-}" "\${TOKIO_WORKER_THREADS-}" "\${KEEPER_ENV_FILE-}"; } >> "$FAKE_DOCKER_LOG"
case "$1" in
  image) for ref in $FAKE_IMAGES; do eval "last=\\\${$#}"; [ "$ref" = "$last" ] && { case "$*" in *--format*) echo sha256:previous;; esac; exit 0; }; done; exit 1 ;;
  ps) case "$*" in *--format*) [ -n "\${FAKE_PS_PROJECT-}" ] && echo "project=$FAKE_PS_PROJECT";;
    *" -q "*) for volume in \${FAKE_PS_RUNNING-}; do [ "$*" = "ps -q --filter volume=$volume" ] && echo running; done;; esac; exit 0 ;;
  inspect) case "$*" in *State.Running*) echo true;; *Health*) echo healthy;; esac; exit 0 ;;
  compose) case "$*" in *" ps -q "*) echo container;; esac; exit 0 ;;
esac
exit 0
`;
const HEX="ab".repeat(32);

function setup(){
  const root=mkdtempSync(join(tmpdir(),"d20dao-wrapper-")),docker=join(root,"deploy/docker"),bin=join(root,"bin");
  mkdirSync(docker,{recursive:true});mkdirSync(bin);
  for(const file of ["keeper.sh","compose.yaml","keeper.env.example"])copyFileSync(join(repo,"deploy/docker",file),join(docker,file));
  writeFileSync(join(bin,"docker"),FAKE_DOCKER);chmodSync(join(bin,"docker"),0o755);
  const log=join(root,"docker.log");writeFileSync(log,"");
  const run=(args:string[],env:Record<string,string|undefined>={})=>{
    writeFileSync(log,"");
    const merged:Record<string,string|undefined>={...process.env};
    const pathKey=Object.keys(merged).find(key=>key.toUpperCase()==="PATH")??"PATH",path=merged[pathKey];
    delete merged[pathKey];
    Object.assign(merged,{PATH:`${bin}${delimiter}${path}`,FAKE_DOCKER_LOG:log,FAKE_IMAGES:"",...env});
    for(const name of ["KEEPER_INSTANCE","KEYS_VOLUME","TOKIO_WORKER_THREADS","KEEPER_IMAGE","KEEPER_STATE_VOLUME","KEEPER_ENV_FILE"])if(!(name in env))delete merged[name];
    const result=spawnSync("sh",[join(docker,"keeper.sh"),...args],{encoding:"utf8",env:merged as NodeJS.ProcessEnv});
    return {code:result.status,out:result.stdout,err:result.stderr,calls:readFileSync(log,"utf8").split("\n").filter(Boolean)};
  };
  // A configured deployment: placeholders replaced so up is allowed.
  const configure=(path:string,chain:string,coordinator:string,extra="")=>writeFileSync(path,readFileSync(path,"utf8")
    .replace(/VERIFIED_RPC/g,"rpc.example").replace(/^CHAIN_ID=.*$/m,`CHAIN_ID=${chain}`).replace(/^COORDINATOR_ADDRESS=.*$/m,`COORDINATOR_ADDRESS='${coordinator}'`)
    .replace(/0x0{64}/g,"0x"+"12".repeat(32))+extra);
  // The directory as the script sees it (a POSIX path under Git Bash on Windows).
  const shDir=spawnSync("sh",["-c",'cd -- "$1" && pwd',"sh",docker],{encoding:"utf8"}).stdout.trim();
  return {root,docker,shDir,run,configure,cleanup:()=>rmSync(root,{recursive:true,force:true})};
}

test("the single keeper keeps project d20dao, keeper.env, d20dao-state-v1 and its configured keys volume",{skip:!hasSh&&"sh is not available"},()=>{
  const w=setup();
  try {
    const init=w.run(["init"]);
    assert.equal(init.code,0);assert.equal(init.out.trim(),"Created keeper.env; configure deployment before up.");
    w.configure(join(w.docker,"keeper.env"),"5042","0xd20DA0FF9087d053f0291524Eac12abA1ADBd945");
    writeFileSync(join(w.docker,"keeper.env"),readFileSync(join(w.docker,"keeper.env"),"utf8").replace("KEYS_VOLUME=d20dao-keys-v1","KEYS_VOLUME=d20dao-keys-arc-mainnet"));
    const missing=w.run(["up"]);
    assert.equal(missing.code,1);assert.match(missing.err,/d20dao-keeper:local is missing; run build/);
    const up=w.run(["up"],{FAKE_IMAGES:"d20dao-keeper:local",FAKE_PS_PROJECT:"d20dao"});
    assert.equal(up.code,0,up.err);
    const env=` | KEYS_VOLUME=d20dao-keys-arc-mainnet KEEPER_STATE_VOLUME=d20dao-state-v1 KEEPER_IMAGE=d20dao-keeper:local TOKIO_WORKER_THREADS=4 KEEPER_ENV_FILE=${w.shDir}/keeper.env`;
    assert.ok(up.calls.includes(`docker volume create --label io.d20dao.component=keeper d20dao-state-v1${env}`));
    assert.ok(up.calls.includes(`docker volume create --label io.d20dao.component=keeper d20dao-keys-arc-mainnet${env}`));
    assert.equal(up.calls.at(-1),`docker compose --project-name d20dao --env-file ${w.shDir}/keeper.env --file ${w.shDir}/compose.yaml up --detach --no-build keeper${env}`);
    // The build and the update rollback tag are unchanged.
    assert.match(w.run(["build"]).calls[0],/^docker build --network default -f .*Dockerfile -t d20dao-keeper:local /);
    const update=w.run(["update",`ghcr.io/d20dao/keeper@sha256:${HEX}`],{FAKE_IMAGES:"d20dao-keeper:local",FAKE_PS_PROJECT:"d20dao"});
    assert.equal(update.code,0,update.err);
    assert.deepEqual(update.calls.filter(c=>/^docker (pull|tag)/.test(c)).map(c=>c.split(" | ")[0]),
      [`docker pull ghcr.io/d20dao/keeper@sha256:${HEX}`,"docker tag sha256:previous d20dao-keeper:rollback",`docker tag ghcr.io/d20dao/keeper@sha256:${HEX} d20dao-keeper:local`]);
    // A process override still selects the keys volume for the single keeper.
    assert.match(w.run(["config"],{KEYS_VOLUME:"d20dao-keys-rotated"}).calls[0],/KEYS_VOLUME=d20dao-keys-rotated /);
    assert.equal(w.run(["update","d20dao-keeper:latest"]).code,2);
  } finally {w.cleanup();}
});

test("named instances get their own project, keeper.env, volumes and image, and never build",{skip:!hasSh&&"sh is not available"},()=>{
  const w=setup();
  try {
    for(const name of ["Arc","-arc","arc_testnet","local","rollback","a".repeat(41)]){
      const refused=w.run(["status"],{KEEPER_INSTANCE:name});
      assert.equal(refused.code,1,name);assert.match(refused.err,/KEEPER_INSTANCE must/);
    }
    const testnet={KEEPER_INSTANCE:"arc-testnet-follower"},mainnet={KEEPER_INSTANCE:"arc-mainnet-follower"};
    const init=w.run(["init"],testnet);
    assert.equal(init.out.trim(),"Created instances/arc-testnet-follower/keeper.env; configure deployment before up.");
    const testnetEnv=join(w.docker,"instances/arc-testnet-follower/keeper.env");
    assert.match(readFileSync(testnetEnv,"utf8"),/^KEYS_VOLUME=d20dao-arc-testnet-follower-keys$/m);
    assert.equal(existsSync(join(w.docker,"keeper.env")),false);
    w.run(["init"],mainnet);
    w.configure(testnetEnv,"5042002","0xd20da00000000000000000000000000000000001","TOKIO_WORKER_THREADS=2\n");
    w.configure(join(w.docker,"instances/arc-mainnet-follower/keeper.env"),"5042","0xd20da00000000000000000000000000000000001");
    const build=w.run(["build"],testnet);
    assert.equal(build.code,1);assert.match(build.err,/never builds on its host/);assert.equal(build.calls.length,0);
    const missing=w.run(["up"],testnet);
    assert.equal(missing.code,1);assert.match(missing.err,/image <image@sha256:digest \| sha256:image-id>/);
    // Image selection accepts only immutable references.
    assert.equal(w.run(["image","ghcr.io/d20dao/keeper:main"],testnet).code,2);
    assert.equal(w.run(["image",`sha256:${HEX.slice(2)}`],testnet).code,2);
    const notLoaded=w.run(["image",`sha256:${HEX}`],testnet);
    assert.equal(notLoaded.code,1);assert.match(notLoaded.err,/docker save <image> \| ssh <host> docker load/);
    const loaded=w.run(["image",`sha256:${HEX}`],{...testnet,FAKE_IMAGES:`sha256:${HEX}`});
    assert.equal(loaded.code,0,loaded.err);assert.equal(loaded.calls.at(-1)!.split(" | ")[0],`docker tag sha256:${HEX} d20dao-keeper:arc-testnet-follower`);
    const pulled=w.run(["image",`ghcr.io/d20dao/keeper@sha256:${HEX}`],mainnet);
    assert.deepEqual(pulled.calls.map(c=>c.split(" | ")[0]),[`docker pull ghcr.io/d20dao/keeper@sha256:${HEX}`,`docker tag ghcr.io/d20dao/keeper@sha256:${HEX} d20dao-keeper:arc-mainnet-follower`]);

    const images="d20dao-keeper:arc-testnet-follower d20dao-keeper:arc-mainnet-follower";
    const up=w.run(["up"],{...testnet,FAKE_IMAGES:images});
    assert.equal(up.code,0,up.err);
    const env=` | KEYS_VOLUME=d20dao-arc-testnet-follower-keys KEEPER_STATE_VOLUME=d20dao-arc-testnet-follower-state KEEPER_IMAGE=d20dao-keeper:arc-testnet-follower TOKIO_WORKER_THREADS=2 KEEPER_ENV_FILE=${w.shDir}/instances/arc-testnet-follower/keeper.env`;
    assert.equal(up.calls.at(-1),`docker compose --project-name d20dao-arc-testnet-follower --env-file ${w.shDir}/instances/arc-testnet-follower/keeper.env --file ${w.shDir}/compose.yaml up --detach --no-build keeper${env}`);
    assert.ok(up.calls.includes(`docker volume create --label io.d20dao.component=keeper d20dao-arc-testnet-follower-state${env}`));
    const other=w.run(["up"],{...mainnet,FAKE_IMAGES:images});
    assert.equal(other.code,0,other.err);assert.match(other.calls.at(-1)!,/--project-name d20dao-arc-mainnet-follower .*KEYS_VOLUME=d20dao-arc-mainnet-follower-keys KEEPER_STATE_VOLUME=d20dao-arc-mainnet-follower-state KEEPER_IMAGE=d20dao-keeper:arc-mainnet-follower TOKIO_WORKER_THREADS=4 /);

    const keys=join(w.root,"tx.key");writeFileSync(keys,"0x"+"01".repeat(32));
    const imported=w.run(["keys",keys,keys],{...testnet,FAKE_IMAGES:images});
    assert.equal(imported.code,0,imported.err);
    assert.match(imported.calls.at(-1)!,/^docker run --rm --pull never --network none .*source=d20dao-arc-testnet-follower-keys,target=\/run\/keeper-keys .* d20dao-keeper:arc-testnet-follower \|/);
    const update=w.run(["update",`sha256:${HEX}`],{...testnet,FAKE_IMAGES:`${images} sha256:${HEX}`,FAKE_PS_PROJECT:"d20dao-arc-testnet-follower"});
    assert.equal(update.code,0,update.err);
    assert.ok(update.calls.some(c=>c.startsWith("docker tag sha256:previous d20dao-keeper:arc-testnet-follower.rollback |")));
  } finally {w.cleanup();}
});

test("instances refuse shared volumes, shared keys overrides, a duplicate coordinator and invalid thread counts",{skip:!hasSh&&"sh is not available"},()=>{
  const w=setup();
  try {
    const follower={KEEPER_INSTANCE:"arc-testnet-follower",FAKE_IMAGES:"d20dao-keeper:arc-testnet-follower"};
    w.run(["init"],follower);
    const env=join(w.docker,"instances/arc-testnet-follower/keeper.env");
    w.configure(env,"5042002","0xD20DA00000000000000000000000000000000001");
    const inUse=w.run(["up"],{...follower,FAKE_PS_PROJECT:"d20dao"});
    assert.equal(inUse.code,1);assert.match(inUse.err,/in use by a container outside project d20dao-arc-testnet-follower/);
    const override=w.run(["status"],{...follower,KEYS_VOLUME:"d20dao-keys-v1"});
    assert.equal(override.code,1);assert.match(override.err,/Set KEYS_VOLUME in instances\/arc-testnet-follower\/keeper.env/);
    assert.equal(w.run(["status"],{...follower,KEYS_VOLUME:"d20dao-arc-testnet-follower-keys"}).code,0);
    // The same chain and coordinator in another keeper.env of this checkout, in any address case and quoting.
    w.run(["init"]);w.configure(join(w.docker,"keeper.env"),"5042002","0xd20da00000000000000000000000000000000001");
    const duplicate=w.run(["up"],follower);
    assert.equal(duplicate.code,1);assert.match(duplicate.err,/^keeper.env configures the same chain and coordinator/);
    w.configure(join(w.docker,"keeper.env"),"5042","0xd20da00000000000000000000000000000000001");
    assert.equal(w.run(["up"],follower).code,0);
    const base=readFileSync(env,"utf8");
    for(const [line,message] of [["KEYS_VOLUME=d20dao-keys-v1",/single keeper's keys volume/],["KEYS_VOLUME=d20dao-arc-mainnet-follower-state",/Keys and state must use different volumes/],
      ["TOKIO_WORKER_THREADS=0",/TOKIO_WORKER_THREADS must be/],["TOKIO_WORKER_THREADS=17",/TOKIO_WORKER_THREADS must be/],["TOKIO_WORKER_THREADS='2'",/TOKIO_WORKER_THREADS must be/]] as const){
      writeFileSync(env,base.replace(/^KEYS_VOLUME=.*$/m,line.startsWith("KEYS")?line:"KEYS_VOLUME=d20dao-arc-testnet-follower-keys")+(line.startsWith("TOKIO")?`${line}\n`:""));
      const refused=w.run(["status"],follower);
      assert.equal(refused.code,1,line);assert.match(refused.err,message);
    }
  } finally {w.cleanup();}
});

test("keys and migrate refuse while a container uses the selected keeper's volumes",{skip:!hasSh&&"sh is not available"},()=>{
  const w=setup();
  try {
    w.run(["init"]);w.configure(join(w.docker,"keeper.env"),"5042","0xd20da00000000000000000000000000000000001");
    const keys=join(w.root,"tx.key");writeFileSync(keys,"0x"+"01".repeat(32));
    const image={FAKE_IMAGES:"d20dao-keeper:local"},migrate=["migrate","/var/lib/d20dao/keeper.sqlite","prepare"];
    const keysRefused=w.run(["keys",keys,keys],{...image,FAKE_PS_RUNNING:"d20dao-keys-v1"});
    assert.equal(keysRefused.code,1);assert.match(keysRefused.err,/Stop containers using the keys volume first/);
    assert.ok(!keysRefused.calls.some(c=>c.startsWith("docker run ")));
    for(const [running,kind] of [["d20dao-state-v1","state"],["d20dao-keys-v1","keys"]]){
      const refused=w.run(migrate,{...image,FAKE_PS_RUNNING:running});
      assert.equal(refused.code,1,running);assert.match(refused.err,new RegExp(`Stop containers using the ${kind} volume first`));
      assert.ok(!refused.calls.some(c=>c.startsWith("docker compose ")),running);
    }
    // Another keeper's volumes do not block this one.
    const migrated=w.run(migrate,{...image,FAKE_PS_RUNNING:"d20dao-arc-testnet-follower-state d20dao-arc-testnet-follower-keys"});
    assert.equal(migrated.code,0,migrated.err);
    assert.equal(migrated.calls.at(-1)!.split(" | ")[0],`docker compose --project-name d20dao --env-file ${w.shDir}/keeper.env --file ${w.shDir}/compose.yaml run --rm --no-deps keeper migrate --from /var/lib/d20dao/keeper.sqlite --prepare`);
    assert.equal(w.run(["keys",keys,keys],image).code,0);
    const follower={KEEPER_INSTANCE:"arc-testnet-follower",FAKE_IMAGES:"d20dao-keeper:arc-testnet-follower"};
    w.run(["init"],follower);
    const instance=w.run(migrate,{...follower,FAKE_PS_RUNNING:"d20dao-arc-testnet-follower-state"});
    assert.equal(instance.code,1);assert.match(instance.err,/Stop containers using the state volume first/);
  } finally {w.cleanup();}
});
