// Real image smoke, using only public fixture keys and isolated, labelled test volumes.
import {spawnSync} from 'node:child_process';
import {mkdtempSync,writeFileSync,readFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {resolve,dirname} from 'node:path';
import {fileURLToPath} from 'node:url';
import {randomUUID} from 'node:crypto';
import assert from 'node:assert/strict';
const repo=resolve(dirname(fileURLToPath(import.meta.url)),'..');
const image=process.env.DOCKER_TEST_IMAGE??'d20dao-keeper:local';
const prefix=`d20dao-smoke-${randomUUID()}`;
const state=`${prefix}-state`,keys=`${prefix}-keys`,holder=`${prefix}-lock`;
const dir=mkdtempSync(resolve(tmpdir(),'d20dao-docker-'));
const tx=resolve(dir,'transaction.key'),vrf=resolve(dir,'vrf.key');
writeFileSync(tx,'0x'+(987654321n).toString(16).padStart(64,'0'),{mode:0o600});
writeFileSync(vrf,'0x'+(123456789n).toString(16).padStart(64,'0'),{mode:0o600});
function docker(args,expected=0,env={}){
  const r=spawnSync('docker',args,{cwd:repo,env:{...process.env,...env},encoding:'utf8',windowsHide:true,timeout:45000,maxBuffer:4*1024*1024});
  if(r.error)throw r.error;
  if(expected!==null)assert.equal(r.status,expected,`${args[0]} failed: ${r.stderr}\n${r.stdout}`);
  return r;
}
const mounts=['--mount',`type=volume,source=${state},target=/var/lib/d20dao`,
  '--mount',`type=volume,source=${keys},target=/run/keeper-keys,readonly`];
const hardening=['--network','none','--read-only','--cap-drop','ALL','--security-opt','no-new-privileges',
  '--tmpfs','/tmp:rw,noexec,nosuid,size=16m','--pids-limit','128'];
const config={CHAIN_ID:'31337',COORDINATOR_ADDRESS:'0x'+'11'.repeat(20),
  RPC_URLS:'https://127.0.0.1:1',CANCEL_MAX_FEE_PER_GAS_WEI:'150000000000',SEND_TRANSACTIONS:'false'};
const env=Object.entries(config).flatMap(([key,value])=>['--env',`${key}=${value}`]);
const run=(args,expected=0)=>docker(['run','--rm',...hardening,...mounts,...args],expected);
const inspectState=()=>run(['--entrypoint','sh',image,'-c',
  'cat /var/lib/d20dao/locks/*-wallet-*.lock']).stdout.trim();
try{
  const [metadata]=JSON.parse(docker(['image','inspect',image]).stdout);
  if(process.env.EXPECTED_DOCKER_ARCH)assert.equal(metadata.Architecture,process.env.EXPECTED_DOCKER_ARCH);
  assert.equal(metadata.Config.User,'10001:10001');
  assert(metadata.Config.Env.includes('TOKIO_WORKER_THREADS=4'));
  assert.deepEqual(metadata.Config.Healthcheck.Test,['CMD','/usr/local/bin/d20dao-keeper','health','--max-age','30']);
  assert.deepEqual(metadata.Config.ExposedPorts??{},{});
  assert.deepEqual(metadata.Config.Volumes??{},{}); // No accidental anonymous state volumes.
  for(const volume of [state,keys])docker(['volume','create','--label','io.d20dao.test=true',volume]);
  const provision=['run','--rm','--network','none','--read-only','--user','0:0',
    '--mount',`type=volume,source=${keys},target=/run/keeper-keys`,
    '--mount',`type=bind,source=${tx},target=/input/transaction.key,readonly`,
    '--mount',`type=bind,source=${vrf},target=/input/vrf.key,readonly`,
    '--entrypoint','/usr/local/bin/import-keys.sh',image];
  writeFileSync(vrf,readFileSync(tx));
  const sameKey=docker(provision,null);
  assert.notEqual(sameKey.status,0);assert.match(sameKey.stderr,/must be distinct/);
  writeFileSync(vrf,'0x'+(123456789n).toString(16).padStart(64,'0'));
  docker(provision);docker(provision); // Idempotent; existing pair never rotates silently.
  const permissions=run(['--entrypoint','sh',image,'-c',
    'id -u; stat -c "%a %u" /run/keeper-keys/transaction.key /run/keeper-keys/vrf.key']).stdout.trim();
  assert.equal(permissions,'10001\n400 10001\n400 10001');
  const keyBefore=run([image,'public-key','/run/keeper-keys/vrf.key']).stdout;
  writeFileSync(vrf,'0x'+(123456790n).toString(16).padStart(64,'0'));
  assert.notEqual(docker(provision,null).status,0,'replacement VRF key was silently imported');
  assert.equal(run([image,'public-key','/run/keeper-keys/vrf.key']).stdout,keyBefore);
  assert.notEqual(run(['--entrypoint','sh',image,'-c','touch /run/keeper-keys/forbidden'],null).status,0);
  assert.notEqual(run(['--entrypoint','sh',image,'-c','touch /usr/local/bin/forbidden'],null).status,0);
  const ephemeral=docker(['run','--rm','--network','none',image,'run','--once'],null);
  assert.notEqual(ephemeral.status,0);assert.match(ephemeral.stderr,/Persistent state volume is required/);
  run(['--entrypoint','sh',image,'-c','ln -s /tmp /var/lib/d20dao/escape']);
  for(const db of ['/tmp/keeper.sqlite','relative.sqlite','/var/lib/d20dao/../outside.sqlite','/var/lib/d20dao/escape/keeper.sqlite']){
    const rejected=run([...env,'--env',`KEEPER_DB=${db}`,image,'run','--once'],null);
    assert.notEqual(rejected.status,0);assert.match(rejected.stderr,/KEEPER_DB must/);
  }
  // Startup gets as far as RPC probing: key parsing, permissions, SQLite and scope binding all worked.
  const first=run([...env,'--env','KEEPER_DB=/var/lib/d20dao/./keeper.sqlite',image,'run','--once'],null);
  assert.equal(first.status,1);assert.match(first.stderr,/No healthy verified RPC endpoint/);
  const binding=inspectState();const parsed=JSON.parse(binding);
  assert.equal(parsed.journal,'/var/lib/d20dao/keeper.sqlite');assert.match(parsed.instance,/^[0-9a-f]{64}$/);
  const second=run([...env,image,'run','--once'],null);
  assert.match(second.stderr,/No healthy verified RPC endpoint/);assert.equal(inspectState(),binding);
  // An OS lock held by another container protects the same journal volume.
  docker(['run','--detach','--name',holder,...hardening,...mounts,'--entrypoint','sh',image,'-c',
    "exec flock /var/lib/d20dao/keeper.lock sh -c 'echo locked; sleep 30'"]);
  let locked=false;
  for(let i=0;i<30;i++){
    if(docker(['logs',holder]).stdout.includes('locked')){locked=true;break;}
    await new Promise(r=>setTimeout(r,100));
  }
  assert(locked);
  const duplicate=run([...env,image,'run','--once'],null);
  assert.equal(duplicate.status,1);assert.match(duplicate.stderr,/Another keeper holds this journal lock/);
  // Check production Compose without displaying environment contents.
  const example=resolve(repo,'deploy/docker/keeper.env.example');
  assert.match(readFileSync(example,'utf8'),/SEND_TRANSACTIONS=false/);
  // A fixture config avoids requiring (or reading) the operator's keeper.env.
  const fixtureEnv=resolve(dir,'keeper.env'),fixtureCompose=resolve(dir,'compose.yaml');
  writeFileSync(fixtureEnv,readFileSync(example,'utf8')
    .replace('KEEPER_DB=/var/lib/d20dao/keeper.sqlite','KEEPER_DB=/var/lib/d20dao/migrated.sqlite')
    .replace('KEYS_VOLUME=d20dao-keys-v1',`KEYS_VOLUME=${keys}`));
  writeFileSync(fixtureCompose,readFileSync(resolve(repo,'deploy/docker/compose.yaml'),'utf8'));
  const compose=JSON.parse(docker(['compose','--env-file',fixtureEnv,'-f',fixtureCompose,'config','--format','json']).stdout);
  const service=compose.services.keeper;
  assert.equal(service.user,'10001:10001');assert.equal(service.read_only,true);
  assert.equal(service.image,'d20dao-keeper:local');assert.equal(service.pull_policy,'never');
  assert.equal(service.environment.TOKIO_WORKER_THREADS,'4');
  assert.equal(service.environment.KEEPER_DB,'/var/lib/d20dao/migrated.sqlite');
  assert.equal(service.restart,'unless-stopped');assert.equal(service.stop_grace_period,'30s');
  assert.equal(service.ports?.length??0,0);assert.deepEqual(service.cap_drop,['ALL']);
  assert.equal(compose.volumes.state.external,true);assert.equal(compose.volumes.keys.external,true);
  assert.equal(compose.volumes.state.name,'d20dao-state-v1');
  assert.equal(compose.volumes.keys.name,process.env.KEYS_VOLUME||keys);
  const originalKeysVolume=process.env.KEYS_VOLUME;
  try {
    process.env.KEYS_VOLUME=`${keys}-override`;
    const selected=JSON.parse(docker(['compose','--env-file',fixtureEnv,'-f',fixtureCompose,'config','--format','json']).stdout);
    assert.equal(selected.volumes.keys.name,`${keys}-override`);
    assert.equal(selected.volumes.state.name,'d20dao-state-v1');
  } finally {
    if(originalKeysVolume===undefined)delete process.env.KEYS_VOLUME;else process.env.KEYS_VOLUME=originalKeysVolume;
  }
  // A named instance: keeper.sh and keeper.ps1 select its image, keeper.env, state volume and thread count.
  const instanceEnv=resolve(dir,'instance.env');
  writeFileSync(instanceEnv,readFileSync(fixtureEnv,'utf8').replace('KEEPER_DB=/var/lib/d20dao/migrated.sqlite','KEEPER_DB=/var/lib/d20dao/instance.sqlite'));
  const named=JSON.parse(docker(['compose','--project-name',`${prefix}-instance`,'--env-file',instanceEnv,'-f',fixtureCompose,'config','--format','json'],0,
    {KEEPER_IMAGE:`d20dao-keeper:${prefix}`,KEEPER_ENV_FILE:instanceEnv,KEEPER_STATE_VOLUME:`${prefix}-instance-state`,KEYS_VOLUME:`${prefix}-instance-keys`,TOKIO_WORKER_THREADS:'2'}).stdout);
  assert.equal(named.name,`${prefix}-instance`);assert.equal(named.services.keeper.image,`d20dao-keeper:${prefix}`);
  assert.equal(named.services.keeper.environment.KEEPER_DB,'/var/lib/d20dao/instance.sqlite');
  assert.equal(named.services.keeper.environment.TOKIO_WORKER_THREADS,'2');
  assert.equal(named.volumes.state.name,`${prefix}-instance-state`);assert.equal(named.volumes.keys.name,`${prefix}-instance-keys`);
  console.log(JSON.stringify({image,architecture:metadata.Architecture,nonRoot:true,readOnlyKeys:true,
    persistentIdentity:true,crossContainerLock:true,refusesEphemeralState:true,noPublishedPorts:true,fixtureDirectory:dir},null,2));
}finally{
  // Only the explicitly named, unique fixtures created above are removed. Never touch service volumes.
  docker(['rm','--force',holder],null);
  for(const volume of [state,keys])docker(['volume','rm',volume],null);
}
