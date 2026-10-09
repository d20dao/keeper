import {test} from "node:test";
import assert from "node:assert/strict";
import {mkdtemp,mkdir,readFile,writeFile} from "node:fs/promises";
import {spawnSync} from "node:child_process";
import {tmpdir} from "node:os";
import {join} from "node:path";
import {getAddress} from "ethers";
import {loadChain,parseChain,type Chain} from "../lib/chains.ts";
import {renderKeeperEnv,type DeploymentRecord,type KeeperRole,type KeeperSecrets} from "../lib/keeper-env.ts";

const hash=(n:number)=>"0x"+n.toString(16).padStart(64,"0");
/** A round coordinator's deployment record: the coordinator's own pins and nothing else. */
const roundRecord=(chainId:number):DeploymentRecord=>({chainId,coordinator:"0xd20da0ff9087d053f0291524eac12aba1adbd945",coordinatorCodeHash:hash(1),protocolConfigurationHash:hash(2),
  coordinatorImplementationCodeHash:hash(3)});
const ROUND_CHAIN_IDS=new Set([4663,46630]);
/** The deployment record of a chain: a Robinhood chain's is a round coordinator's; any other also pins its registry implementation. */
const record=(chainId:number):DeploymentRecord=>ROUND_CHAIN_IDS.has(chainId)?roundRecord(chainId):{...roundRecord(chainId),epochImplementationCodeHash:hash(4)};
const NEON="postgresql://user:secret-password@host/db?sslmode=require&channel_binding=require";
const snapshot=(name:string)=>readFile(new URL(`./fixtures/keeper-env/${name}.txt`,import.meta.url),"utf8");
/** A keeper.env as name to value, quotes removed. */
const settingsOf=(text:string)=>Object.fromEntries(text.split("\n").filter(line=>/^[A-Z]/.test(line)).map(line=>{
  const at=line.indexOf("="),value=line.slice(at+1);
  return [line.slice(0,at),value.startsWith("'")?value.slice(1,-1):value];
}));
const ROLES:KeeperRole[]=["primary","follower"];

test("Arc's keeper.env is byte-identical to what the generator wrote before the Robinhood settings: both chains, both roles, with and without secrets",async()=>{
  // The snapshots were rendered by the generator of keeper 0.4.1 from these inputs.
  for(const key of ["arc-testnet","arc-mainnet"]){
    const chain=await loadChain(key),url=`https://${key}.example/v2/secret-key`,others={telegramBotToken:"123:secret-token",telegramChatId:"-1001234",neonDb:NEON};
    for(const role of ROLES){
      assert.equal(renderKeeperEnv(chain,record(chain.chainId),{},role),await snapshot(`${key}.${role}.bare`),`${key} ${role} without secrets`);
      const full=await snapshot(`${key}.${role}.full`);
      assert.equal(renderKeeperEnv(chain,record(chain.chainId),{...others,privateRpcUrls:[url]},role),full,`${key} ${role} with secrets`);
      assert.equal(renderKeeperEnv(chain,record(chain.chainId),{...others,privateRpcUrl:url},role),full,`${key} ${role} with the single-URL form`);
    }
  }
});

// The Robinhood values of the design's keeper table, written out per network and role: the rows that differ between the networks
// are the paid endpoints' places, the explorer, the WebSocket silence limit and the low balance threshold.
const NETWORKS={
  "robinhood-testnet":{chainId:46630,explorer:"https://explorer.testnet.chain.robinhood.com",publicRpc:"https://rpc.testnet.chain.robinhood.com",silence:"60",lowBalance:"2000000000000000"},
  "robinhood-mainnet":{chainId:4663,explorer:"https://robinhoodchain.blockscout.com",publicRpc:"https://rpc.mainnet.chain.robinhood.com",silence:"20",lowBalance:"200000000000000"},
};
const inputs=(key:string):Required<Pick<KeeperSecrets,"privateRpcUrls"|"wsUrls"|"healthApiUrl"|"healthApiKey"|"discordPublicExplorerUrl">>&KeeperSecrets=>({
  privateRpcUrls:[`https://${key}-paid-one.example/v2/secret-key-one`,`https://${key}-paid-two.example/v2/secret-key-two`],
  wsUrls:[`wss://${key}-socket.example/v2/secret-socket-key`],telegramBotToken:"123:secret-token",telegramChatId:"-1001234",neonDb:NEON,
  healthApiUrl:`https://health.example/v1/health/${key}`,healthApiKey:"secret-health-key",
  discordBotToken:"secret-discord-token-0123456789",discordChannelId:"1234567890123456789",discordPublicExplorerUrl:`https://${key}.site.example`});
const expectedSettings=(key:keyof typeof NETWORKS,role:KeeperRole)=>{
  const network=NETWORKS[key],given=inputs(key);
  return {
    CHAIN_ID:String(network.chainId),NATIVE_CURRENCY_SYMBOL:"ETH",EXPLORER_URL:network.explorer,
    RPC_URLS:[...given.privateRpcUrls,network.publicRpc].join(","),WS_URLS:given.wsUrls.join(","),
    COORDINATOR_ADDRESS:getAddress(record(0).coordinator),EXPECTED_CODE_HASH:hash(1),EXPECTED_PROTOCOL_HASH:hash(2),
    EXPECTED_IMPLEMENTATION_CODE_HASH:hash(3),
    FINALITY_MODE:"soft",SOFT_DEPTH_BLOCKS:"0",FINALITY_AUDIT_INTERVAL_SECONDS:"60",FINALITY_AUDIT_MAX_LAG_SECONDS:"2700",
    GAS_MODEL:"arbitrum",L1_GAS_MARGIN_BPS:"2500",COORDINATOR_KIND:"round",IDLE_HEARTBEAT_SECONDS:"25",
    MIN_PRIORITY_FEE_WEI:"0",MAX_PRIORITY_FEE_WEI:"0",MAX_FEE_PER_GAS_WEI:"3000000000",CANCEL_MAX_FEE_PER_GAS_WEI:"3500000000",
    MAX_TX_COST_WEI:"5000000000000000",MAX_GAS:"13000000",FEE_COVERAGE_BPS:"12500",FULFILL_BATCH_MAX:"16",
    POLL_MS:"250",TICK_TIMEOUT_SECONDS:"20",MAX_TICK_FAILURES:"5",SEND_MARGIN_SECONDS:"5",
    NONCE_STUCK_SECONDS:"60",PROGRESS_STUCK_SECONDS:"20",SEQUENCER_DROP_SECONDS:"6",
    WS_SILENCE_SECONDS:network.silence,WS_BACKFILL_MAX_BLOCKS:"6000",WS_BACKFILL_RANGE:"2000",
    INDEX_FINALITY:"soft",INDEX_MAX_BLOCKS:"2000",INDEX_LOOKBACK_BLOCKS:"1200",
    TELEGRAM_BOT_TOKEN:given.telegramBotToken!,TELEGRAM_CHAT_ID:given.telegramChatId!,TELEGRAM_LOW_BALANCE_WEI:network.lowBalance,
    SWEEP_MIN_RESERVE_WEI:"5000000000000000",NEON_DB:NEON,
    HEALTH_API_URL:given.healthApiUrl,HEALTH_API_KEY:given.healthApiKey,HEALTH_INTERVAL_SECONDS:"30",
    DISCORD_BOT_TOKEN:given.discordBotToken!,DISCORD_PROTOCOL_CHANNEL_ID:given.discordChannelId!,DISCORD_PUBLIC_EXPLORER_URL:given.discordPublicExplorerUrl,
    KEEPER_DB:`/var/lib/d20dao/keeper-${key}.sqlite`,TX_KEY_FILE:"/etc/d20dao/transaction.key",VRF_KEY_FILE:"/etc/d20dao/vrf.key",
    SEND_TRANSACTIONS:"false",RUST_LOG:"d20dao_keeper=info",
    ...(role==="follower"?{KEEPER_ROLE:"follower",FOLLOWER_DELAY_SECONDS:"20",FOLLOWER_QUEUE_JOIN:"150",PRIMARY_LIVENESS_SECONDS:"10"}:{}),
    KEYS_VOLUME:role==="follower"?`d20dao-${key}-follower-keys`:`d20dao-keys-${key}`,
  };
};

test("the Robinhood keeper.env holds every value of the keeper table and nothing else, for both networks and both roles",async()=>{
  for(const key of Object.keys(NETWORKS) as Array<keyof typeof NETWORKS>){
    const chain=await loadChain(key);
    for(const role of ROLES){
      const text=renderKeeperEnv(chain,record(chain.chainId),inputs(key),role),got=settingsOf(text);
      assert.deepEqual(got,expectedSettings(key,role),`${key} ${role}`);
      // Every value is single-quoted except the keys volume, which keeper.sh reads unquoted.
      for(const line of text.split("\n").filter(line=>/^[A-Z]/.test(line)&&!line.startsWith("KEYS_VOLUME=")))assert.match(line,/^[A-Z0-9_]+='[^']*'$/,line);
      for(const name of ["EPOCH_API_ENDPOINTS","EXPECTED_SOURCE_HASH","FOLLOWER_BUSY_DELAY_SECONDS","TEST_API_BASE","TEST_LOCK_DIR","DRAND_RELAYS","TELEGRAM_COMMANDS",
        "IDLE_POLL_MS","FOLLOWER_RANK","FOLLOWER_LANES","APPROVED_NEXT_IMPLEMENTATION_CODE_HASH","APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH",
        "EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH","REGISTRY_KIND","BLOCK_NUDGE","BLOCK_NUDGE_AFTER_MS"])
        assert.equal(name in got,false,`${key} ${role} must not set ${name}`);
    }
  }
});

test("a Robinhood keeper.env needs only the endpoints: no optional part is written without its input, and sending is off",async()=>{
  for(const key of Object.keys(NETWORKS) as Array<keyof typeof NETWORKS>){
    const chain=await loadChain(key),got=settingsOf(renderKeeperEnv(chain,record(chain.chainId),{privateRpcUrls:inputs(key).privateRpcUrls}));
    for(const name of ["WS_URLS","TELEGRAM_BOT_TOKEN","TELEGRAM_CHAT_ID","TELEGRAM_LOW_BALANCE_WEI","NEON_DB","HEALTH_API_URL","HEALTH_API_KEY","HEALTH_INTERVAL_SECONDS",
      "DISCORD_BOT_TOKEN","DISCORD_PROTOCOL_CHANNEL_ID","DISCORD_PUBLIC_EXPLORER_URL","KEEPER_ROLE"])assert.equal(name in got,false,name);
    assert.equal(got.SEND_TRANSACTIONS,"false");
    assert.equal(got.FINALITY_MODE,"soft");assert.equal(got.GAS_MODEL,"arbitrum");assert.equal(got.COORDINATOR_KIND,"round");assert.equal(got.FEE_COVERAGE_BPS,"12500");
    // The public endpoint is last even when it is the only one, which a private-first chain accepts only when asked.
    assert.equal(renderKeeperEnv(chain,record(chain.chainId),{allowPublicOnly:true}).includes(`RPC_URLS='${NETWORKS[key].publicRpc}'`),true);
  }
});

test("rpcOrder places the public endpoints: first after the first public one by default, last on private-first chains, and the private ones keep their order",async()=>{
  const robinhood=await loadChain("robinhood-testnet"),arc=await loadChain("arc-testnet");
  const privates=["https://one.example/k1","https://two.example/k2"],rpc=(chain:Chain,urls:string[],role:KeeperRole="primary")=>
    settingsOf(renderKeeperEnv(chain,record(chain.chainId),{privateRpcUrls:urls},role)).RPC_URLS;
  assert.equal(rpc(robinhood,privates),[...privates,...robinhood.rpcUrls].join(","));
  assert.equal(rpc(robinhood,[...privates].reverse(),"follower"),[...privates].reverse().concat(robinhood.rpcUrls).join(","));
  assert.equal(rpc({...robinhood,rpcOrder:"public-first"},privates),[robinhood.rpcUrls[0],...privates,...robinhood.rpcUrls.slice(1)].join(","));
  assert.equal(rpc(arc,privates),[arc.rpcUrls[0],...privates,...arc.rpcUrls.slice(1)].join(","));
  assert.equal(rpc({...arc,rpcOrder:"private-first"},privates),[...privates,...arc.rpcUrls].join(","));
  assert.equal(rpc({...arc,rpcOrder:"public-first"},privates),rpc(arc,privates));
  assert.throws(()=>rpc({...arc,rpcOrder:"anywhere" as never},privates),/RPC order/);
});

test("WS_URLS is written after RPC_URLS when WebSocket endpoints are given, they must be wss, and the sequencer feed is refused",async()=>{
  const chain=await loadChain("robinhood-mainnet"),secrets=(wsUrls:string[])=>({privateRpcUrls:["https://one.example/k1"],wsUrls});
  const lines=renderKeeperEnv(chain,record(chain.chainId),secrets(["wss://one.example/k1","wss://two.example/k2"])).split("\n");
  // After the header come CHAIN_ID, NATIVE_CURRENCY_SYMBOL, EXPLORER_URL and RPC_URLS.
  assert.match(lines[4],/^RPC_URLS=/);
  assert.equal(lines[5],"WS_URLS='wss://one.example/k1,wss://two.example/k2'");
  assert.equal("WS_URLS" in settingsOf(renderKeeperEnv(chain,record(chain.chainId),secrets([]))),false);
  for(const url of ["ws://one.example/secret-socket","https://one.example/secret-socket","wss://feed.mainnet.chain.robinhood.com/secret-socket","wss://feed.testnet.chain.robinhood.com"])
    assert.throws(()=>renderKeeperEnv(chain,record(chain.chainId),secrets([url])),error=>error instanceof Error&&/WebSocket URLs/.test(error.message)&&!error.message.includes("secret-socket"),url);
  assert.throws(()=>renderKeeperEnv(chain,record(chain.chainId),secrets(["not a url secret-socket"])),error=>error instanceof Error&&!error.message.includes("secret-socket"));
});

/** The call must throw, and the message must name none of the values that would be secrets. */
function refuses(run:()=>unknown,message:RegExp,hidden:string[]=[]){
  assert.throws(run,(error:unknown)=>error instanceof Error&&message.test(error.message)&&hidden.every(value=>!error.message.includes(value)),String(message));
}

test("optional parts are refused when incomplete or malformed, and no refusal names a secret",async()=>{
  const chain=await loadChain("robinhood-testnet"),render=(secrets:KeeperSecrets,subject:Chain=chain)=>()=>renderKeeperEnv(subject,record(subject.chainId),{privateRpcUrls:["https://one.example/k1"],...secrets});
  const key="secret-health-key",token="secret-discord-token-0123456789";
  refuses(render({healthApiUrl:"https://health.example/h"}),/needs both the endpoint and the key/);
  refuses(render({healthApiKey:key}),/needs both the endpoint and the key/,[key]);
  refuses(render({healthApiUrl:"http://health.example/h",healthApiKey:key}),/HTTPS/,[key]);
  refuses(render({healthApiUrl:"https://user:pw@health.example/h",healthApiKey:key}),/HTTPS/,[key,"user:pw"]);
  refuses(render({healthApiUrl:"https://health.example/h?token=1",healthApiKey:key}),/HTTPS/,[key]);
  refuses(render({healthApiUrl:"https://health.example/h",healthApiKey:"two words"}),/Invalid health key/,["two words"]);
  refuses(render({healthApiUrl:"https://health.example/h",healthApiKey:key,healthIntervalSeconds:4}),/5 to 3600/,[key]);
  refuses(render({healthApiUrl:"https://health.example/h",healthApiKey:key,healthIntervalSeconds:3601}),/5 to 3600/,[key]);
  refuses(render({healthApiUrl:"https://health.example/h",healthApiKey:key,healthIntervalSeconds:Number.NaN}),/5 to 3600/,[key]);
  refuses(render({healthIntervalSeconds:30}),/needs the health endpoint/);
  assert.equal(settingsOf(renderKeeperEnv(chain,record(chain.chainId),{allowPublicOnly:true,healthApiUrl:"https://health.example/h",healthApiKey:key,healthIntervalSeconds:60})).HEALTH_INTERVAL_SECONDS,"60");
  refuses(render({discordBotToken:token}),/needs both the bot token and a channel id/,[token]);
  refuses(render({discordChannelId:"1234567890123456789"}),/needs both the bot token and a channel id/);
  refuses(render({discordBotToken:"short",discordChannelId:"1234567890123456789"}),/Invalid Discord bot token/,["short"]);
  for(const channel of ["0","012","12ab","-5","18446744073709551616",""])refuses(render({discordBotToken:token,discordChannelId:channel}),/channel id/,[token]);
  assert.doesNotThrow(render({discordBotToken:token,discordChannelId:"18446744073709551615"}));
  refuses(render({discordBotToken:token,discordChannelId:"123",discordPublicExplorerUrl:"http://site.example"}),/public explorer URL/,[token]);
  refuses(render({discordBotToken:token,discordChannelId:"123",discordPublicExplorerUrl:"https://site.example/?a=1"}),/public explorer URL/,[token]);
  refuses(render({discordPublicExplorerUrl:"https://site.example"}),/public explorer URL needs Discord/);
  refuses(render({telegramBotToken:"123:secret-token"}),/both the bot token and a chat id/,["secret-token"]);
  refuses(render({telegramBotToken:"123:secret-token",telegramChatId:"@channel"}),/numeric/,["secret-token"]);
  refuses(render({neonDb:"postgresql://user:secret-password@host/db"}),/NEON_DB/,["secret-password"]);
  refuses(()=>renderKeeperEnv(chain,record(chain.chainId),{privateRpcUrls:["http://plain.example/secret-key"]}),/HTTPS/,["secret-key"]);
  refuses(()=>renderKeeperEnv(chain,record(1),{}),/another chain/);
  refuses(()=>renderKeeperEnv(chain,record(chain.chainId),{},"backup" as never),/primary or follower/);
  // A value with a quote or a line break would end its line early.
  refuses(render({healthApiUrl:"https://health.example/h",healthApiKey:"a'b"}),/unsupported character/,["a'b"]);
});

test("the profile's keeper settings are checked by name: unknown keys, ranges and the modes they apply to",async()=>{
  const chain=await loadChain("robinhood-testnet"),keeper=chain.keeper!;
  const withKeeper=(patch:object,top:Partial<Chain>={}):Chain=>({...chain,...top,keeper:{...keeper,...patch}} as Chain);
  const render=(subject:Chain)=>()=>renderKeeperEnv(subject,record(subject.chainId),{allowPublicOnly:true});
  assert.doesNotThrow(render(withKeeper({})));
  refuses(render(withKeeper({softDpeth:1})),/Unknown keeper setting softDpeth/);
  const outOfRange:Array<[string,unknown[]]>=[["softDepthBlocks",[-1,17,1.5,"0"]],["finalityAuditIntervalSeconds",[4,301]],["finalityAuditMaxLagSeconds",[599,7201]],
    ["gasModel",["l2","Arbitrum",1]],["l1GasMarginBps",[-1,20001]],["coordinatorKind",["beacon","Round","entropy",true]],["idleHeartbeatSeconds",[0,1,31,2.5,"25"]],["nonceStuckSeconds",[0,3601]],["sequencerDropSeconds",[1,61]],
    ["feeCoverageBps",[-1,100001,12500.5,"12500",null]],["wsSilenceSeconds",[4,601]],["wsBackfillMaxBlocks",[0,-5,100001]],["wsBackfillRange",[0,2.5,10001]],
    ["indexFinality",["safe",0]],["indexMaxBlocks",[0,10001]],["indexLookbackBlocks",[0,10001]],["sweepMinReserveWei",["-1","1e18","01",5]],["telegramLowBalanceWei",["0.5",1,""]]];
  for(const [name,values] of outOfRange)for(const value of values)refuses(render(withKeeper({[name]:value})),new RegExp(`Invalid keeper setting ${name}\\b`),[]);
  // The bounds themselves are valid.
  for(const patch of [{softDepthBlocks:16},{softDepthBlocks:0},{finalityAuditIntervalSeconds:5},{finalityAuditIntervalSeconds:300},{finalityAuditMaxLagSeconds:600},{finalityAuditMaxLagSeconds:7200},
    {idleHeartbeatSeconds:2},{idleHeartbeatSeconds:30},{l1GasMarginBps:0},{l1GasMarginBps:20000},{sequencerDropSeconds:2},{sequencerDropSeconds:60},{feeCoverageBps:12500},{feeCoverageBps:100000},{wsSilenceSeconds:5},{wsSilenceSeconds:600},
    {wsBackfillMaxBlocks:100000},{wsBackfillMaxBlocks:100000,wsBackfillRange:10000},{wsBackfillMaxBlocks:1,wsBackfillRange:1},{indexMaxBlocks:1},{indexMaxBlocks:10000},{indexLookbackBlocks:1},{indexLookbackBlocks:10000}])
    assert.doesNotThrow(render(withKeeper(patch)),JSON.stringify(patch));
  // Settings that mean something only under soft finality or the Arbitrum gas model need the profile to use it.
  const finalized=(patch:object={})=>withKeeper(patch,{finality:"finalized"});
  refuses(render(finalized()),/needs soft finality/);
  refuses(render({...chain,finality:undefined,keeper:{...keeper}} as Chain),/needs soft finality/);
  refuses(render(withKeeper({gasModel:"standard"})),/needs the arbitrum gas model/);
  // The idle heartbeat is a round keeper's: an epoch keeper, or a profile that leaves the kind at the keeper's default (epoch), refuses it.
  refuses(render(withKeeper({coordinatorKind:"epoch"})),/^Keeper setting idleHeartbeatSeconds needs coordinatorKind round$/);
  const {coordinatorKind:_kind,...kindless}=keeper;
  refuses(render({...chain,keeper:kindless} as Chain),/^Keeper setting idleHeartbeatSeconds needs coordinatorKind round$/);
  const slim={...chain,finality:"finalized",keeper:{coordinatorKind:"round",feeCoverageBps:12500,indexFinality:"finalized"}} as Chain;
  assert.doesNotThrow(render(slim));
  assert.equal(settingsOf(renderKeeperEnv(slim,record(slim.chainId),{allowPublicOnly:true})).FINALITY_MODE,"finalized");
  refuses(render({...slim,keeper:{...slim.keeper,indexFinality:"soft"}} as Chain),/indexFinality needs soft finality/);
  // A profile may set only some; the rest keep the keeper's defaults and are left out of the file.
  const some=settingsOf(renderKeeperEnv({...chain,keeper:{coordinatorKind:"round",feeCoverageBps:12500}} as Chain,record(chain.chainId),{allowPublicOnly:true}));
  assert.equal(some.COORDINATOR_KIND,"round");assert.equal("SOFT_DEPTH_BLOCKS" in some,false);assert.equal("IDLE_HEARTBEAT_SECONDS" in some,false);assert.equal(some.NONCE_STUCK_SECONDS,"120");
  for(const bad of [null,[],"soft",7])refuses(render({...chain,keeper:bad} as unknown as Chain),/keeper settings must be an object/);
  refuses(render({...chain,finality:"safe"} as unknown as Chain),/finality/);
  // The same rules apply when a profile is loaded.
  const entry=JSON.parse(await readFile(new URL("../../chains.json",import.meta.url),"utf8"))["robinhood-testnet"];
  assert.equal(parseChain("x",entry).rpcOrder,"private-first");
  assert.throws(()=>parseChain("x",{...entry,rpcOrder:"anywhere"}),/RPC order/);
  for(const bad of [null,[],"soft"])assert.throws(()=>parseChain("x",{...entry,keeper:bad}),/keeper settings/);
});

// The keeper's own rules (keeper/src/config.rs, ChainSettings::resolve): the generator must not write a file the keeper refuses to start with.
test("the backfill range never exceeds the backfill cap, counting a setting the profile leaves out as the keeper's default",async()=>{
  const chain=await loadChain("robinhood-testnet"),keeper=chain.keeper!;
  const render=(patch:object,drop:string[]=[])=>()=>{
    const kept:Record<string,unknown>={...keeper,...patch};
    for(const name of drop)delete kept[name];
    return renderKeeperEnv({...chain,keeper:kept} as Chain,record(chain.chainId),{allowPublicOnly:true});
  };
  const refusal=/Keeper setting wsBackfillRange must not exceed wsBackfillMaxBlocks/;
  // The profile's own values are a range of 2,000 under a cap of 6,000.
  assert.doesNotThrow(render({}));
  refuses(render({wsBackfillRange:6001}),refusal);
  refuses(render({wsBackfillMaxBlocks:1999}),refusal);
  assert.doesNotThrow(render({wsBackfillRange:6000}));
  assert.doesNotThrow(render({wsBackfillMaxBlocks:2000}));
  assert.doesNotThrow(render({wsBackfillMaxBlocks:100000,wsBackfillRange:10000}));
  // The keeper compares the values it ends up with: a cap left out is 5,000 and a range left out is 500.
  refuses(render({wsBackfillRange:5001},["wsBackfillMaxBlocks"]),refusal);
  assert.doesNotThrow(render({wsBackfillRange:5000},["wsBackfillMaxBlocks"]));
  refuses(render({wsBackfillMaxBlocks:499},["wsBackfillRange"]),refusal);
  assert.doesNotThrow(render({wsBackfillMaxBlocks:500},["wsBackfillRange"]));
  assert.doesNotThrow(render({},["wsBackfillMaxBlocks","wsBackfillRange"]));
  // The refusal names settings, never their values.
  refuses(render({wsBackfillRange:6001}),/^[^0-9]*$/);
  // Written as they are given, and the index scan takes the keeper's upper bound of 10,000 on both settings.
  const got=settingsOf(render({wsBackfillMaxBlocks:100000,wsBackfillRange:10000,indexMaxBlocks:10000,indexLookbackBlocks:10000})());
  assert.deepEqual([got.WS_BACKFILL_MAX_BLOCKS,got.WS_BACKFILL_RANGE,got.INDEX_MAX_BLOCKS,got.INDEX_LOOKBACK_BLOCKS],["100000","10000","10000","10000"]);
});

test("a round coordinator's keeper.env writes its kind and fee coverage, and no registry pin, block nudge or registry kind",async()=>{
  const chain=await loadChain("robinhood-testnet"),keeper=chain.keeper!;
  const render=(patch:object,deployment:DeploymentRecord=roundRecord(chain.chainId))=>()=>
    renderKeeperEnv({...chain,keeper:{...keeper,...patch}} as Chain,deployment,{allowPublicOnly:true});
  const got=settingsOf(render({})());
  assert.equal(got.COORDINATOR_KIND,"round");assert.equal(got.FEE_COVERAGE_BPS,"12500");
  for(const name of ["EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH","REGISTRY_KIND","BLOCK_NUDGE","BLOCK_NUDGE_AFTER_MS"])assert.equal(name in got,false,name);
  // The coordinator's own pins stay.
  assert.deepEqual([got.EXPECTED_CODE_HASH,got.EXPECTED_PROTOCOL_HASH,got.EXPECTED_IMPLEMENTATION_CODE_HASH],[hash(1),hash(2),hash(3)]);
  // The keeper sends only when 1.25 times the cost is escrowed: the profile writes the coverage down, at least 12500, within the keeper's range.
  const coverage=/^Keeper setting feeCoverageBps must be set to at least 12500 with coordinatorKind round$/;
  for(const value of [12499,10000,0])refuses(render({feeCoverageBps:value}),coverage);
  const {feeCoverageBps:_coverage,...without}=keeper;
  refuses(()=>renderKeeperEnv({...chain,keeper:without},roundRecord(chain.chainId),{allowPublicOnly:true}),coverage);
  for(const value of [12500,20000,100000])assert.equal(settingsOf(render({feeCoverageBps:value})()).FEE_COVERAGE_BPS,String(value));
  // A record that pins a registry is not a round coordinator's, whatever the value.
  for(const value of [hash(4),"",undefined])
    refuses(render({},{...roundRecord(chain.chainId),epochImplementationCodeHash:value}),/^The deployment record pins a registry implementation, which a round coordinator does not have$/);
  // The settings of the epoch design are not profile settings any more: a profile that still carries one is refused by name.
  for(const [name,value] of [["registryKind","beacon"],["blockNudge",true],["blockNudgeAfterMs",1500]] as const)
    refuses(render({[name]:value}),new RegExp(`^Unknown keeper setting ${name} in the chain profile$`));
});

test("without a round coordinator the registry pin is required as before, and the fee coverage keeps the keeper's default unless a profile sets it",async()=>{
  const arc=await loadChain("arc-testnet"),{epochImplementationCodeHash:_pin,...unpinned}=record(arc.chainId);
  refuses(()=>renderKeeperEnv(arc,unpinned,{}),/^Invalid registry implementation hash$/);
  const got=settingsOf(renderKeeperEnv(arc,record(arc.chainId),{}));
  assert.equal(got.EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH,hash(4));assert.equal(got.FEE_COVERAGE_BPS,"10000");assert.equal("COORDINATOR_KIND" in got,false);
  // An explicit epoch kind is written as given and keeps the pin; a coverage the profile sets replaces the default in place.
  const epoch=settingsOf(renderKeeperEnv({...arc,keeper:{coordinatorKind:"epoch",feeCoverageBps:0}},record(arc.chainId),{}));
  assert.equal(epoch.COORDINATOR_KIND,"epoch");assert.equal(epoch.EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH,hash(4));assert.equal(epoch.FEE_COVERAGE_BPS,"0");
  refuses(()=>renderKeeperEnv({...arc,keeper:{coordinatorKind:"epoch"}},unpinned,{}),/^Invalid registry implementation hash$/);
});

test("a private-first chain given no private endpoint is refused unless the public endpoint alone is asked for; chains that are not private-first are not asked",async()=>{
  const robinhood=await loadChain("robinhood-testnet"),arc=await loadChain("arc-testnet");
  const refusal=/^robinhood-testnet lists its public endpoint last because it is rate limited and keeps little state: give a private endpoint \(--rpc-var\), or accept the public endpoint alone with --allow-public-only$/;
  const render=(secrets:KeeperSecrets,role:KeeperRole="primary",subject:Chain=robinhood)=>()=>renderKeeperEnv(subject,record(subject.chainId),secrets,role);
  for(const role of ROLES){
    refuses(render({},role),refusal);
    refuses(render({privateRpcUrls:[]},role),refusal);
    refuses(render({allowPublicOnly:false},role),refusal);
    // Nothing else the operator gave is named in the refusal.
    refuses(render({telegramBotToken:"123:secret-token",telegramChatId:"-1001234",neonDb:NEON,wsUrls:["wss://socket.example/secret-socket-key"]},role),refusal,["secret-token","secret-password","secret-socket-key"]);
  }
  // A private endpoint, in either form, is what the rule asks for.
  for(const secrets of [{privateRpcUrls:["https://one.example/k1"]},{privateRpcUrl:"https://one.example/k1"}])
    assert.equal(settingsOf(render(secrets)()).RPC_URLS,`https://one.example/k1,${robinhood.rpcUrls[0]}`);
  // The flag states the choice: the file lists the public endpoint alone, and is otherwise the file a private endpoint would give.
  const alone=render({allowPublicOnly:true})(),withPrivate=render({privateRpcUrls:["https://one.example/k1"],allowPublicOnly:true})();
  assert.equal(settingsOf(alone).RPC_URLS,robinhood.rpcUrls.join(","));
  assert.equal(alone.replace(/^RPC_URLS=.*\n/m,""),withPrivate.replace(/^RPC_URLS=.*\n/m,""));
  assert.equal(settingsOf(withPrivate).RPC_URLS,`https://one.example/k1,${robinhood.rpcUrls[0]}`);
  // Arc keeps its public-first order and asks for nothing; a private-first order asks wherever it is set.
  assert.doesNotThrow(render({},"primary",arc));
  refuses(render({},"primary",{...arc,rpcOrder:"private-first"}),/^arc-testnet lists its public endpoint last/);
  assert.doesNotThrow(render({},"primary",{...arc,rpcOrder:"public-first"}));
  assert.equal(settingsOf(render({allowPublicOnly:true},"primary",{...arc,rpcOrder:"private-first"})()).RPC_URLS,arc.rpcUrls.join(","));
});

test("the sequencer feed is refused whatever the spelling of its host: case, trailing dots, a port, credentials",async()=>{
  const chain=await loadChain("robinhood-mainnet"),render=(url:string)=>()=>renderKeeperEnv(chain,record(chain.chainId),{privateRpcUrls:["https://one.example/k1"],wsUrls:[url]});
  for(const url of ["wss://feed.mainnet.chain.robinhood.com/secret-socket","wss://feed.mainnet.chain.robinhood.com./secret-socket","wss://feed.mainnet.chain.robinhood.com../secret-socket",
    "wss://FEED.Mainnet.Chain.Robinhood.COM./secret-socket","wss://feed.testnet.chain.robinhood.com.:443/secret-socket","wss://user:pw@feed.testnet.chain.robinhood.com./secret-socket",
    "wss://feed.mainnet.chain.robinhood.com%2e/secret-socket","wss://feed.mainnet.chain.robinhood.com。/secret-socket"])
    refuses(render(url),/^WebSocket URLs must be JSON-RPC endpoints, not a sequencer feed$/,["secret-socket","user:pw"]);
  // Only the feed's own domain is refused: a provider's JSON-RPC endpoint, or a host that merely resembles it, is not the feed.
  for(const url of ["wss://robinhood-rpc.publicnode.com/secret-socket","wss://feed.mainnet.chain.robinhood.com.example.org/secret-socket","wss://notfeed.mainnet.chain.robinhood.com/secret-socket",
    "wss://feed.mainnet.chain.robinhood.co/secret-socket"])
    assert.doesNotThrow(render(url),url);
});

test("the Robinhood profiles carry the keeper table: soft finality, the Arbitrum gas model, the round coordinator, its fee coverage and no tips",async()=>{
  const testnet=await loadChain("robinhood-testnet"),mainnet=await loadChain("robinhood-mainnet");
  const shared={softDepthBlocks:0,finalityAuditIntervalSeconds:60,finalityAuditMaxLagSeconds:2700,gasModel:"arbitrum",l1GasMarginBps:2500,coordinatorKind:"round",idleHeartbeatSeconds:25,nonceStuckSeconds:60,
    sequencerDropSeconds:6,feeCoverageBps:12500,wsBackfillMaxBlocks:6000,wsBackfillRange:2000,indexFinality:"soft",indexMaxBlocks:2000,indexLookbackBlocks:1200,
    sweepMinReserveWei:"5000000000000000"};
  assert.deepEqual(testnet.keeper,{...shared,wsSilenceSeconds:60,telegramLowBalanceWei:"2000000000000000"});
  assert.deepEqual(mainnet.keeper,{...shared,wsSilenceSeconds:20,telegramLowBalanceWei:"200000000000000"});
  for(const chain of [testnet,mainnet]){
    assert.equal(chain.rpcOrder,"private-first");assert.equal(chain.finality,"soft");assert.equal(chain.blockSource,"arbitrum-l2");
    // The sweep reserve equals the transaction cost cap, which the keeper takes as its minimum anyway.
    assert.equal(chain.keeper!.sweepMinReserveWei,chain.gas.maxTxCostWei);
  }
  // An idle keeper must tick, and so mark its health, more often than the container's healthcheck (`health --max-age 30`) allows it to be silent.
  const dockerfile=await readFile(new URL("../../deploy/docker/Dockerfile",import.meta.url),"utf8"),maxAge=Number(/"--max-age",\s*"(\d+)"/.exec(dockerfile)?.[1]);
  assert.equal(maxAge,30);
  for(const chain of [testnet,mainnet]){
    assert.ok(chain.keeper!.idleHeartbeatSeconds!<maxAge,`${chain.key}: the idle heartbeat stays under the healthcheck's maximum age`);
    // The low balance warning sits under the wallet's first funding (0.0005 ETH a wallet on mainnet), so a newly funded keeper is not warned.
    if(chain.key==="robinhood-mainnet")assert.ok(BigInt(chain.keeper!.telegramLowBalanceWei!)<500_000_000_000_000n,chain.key);
  }
  for(const key of ["arc-testnet","arc-mainnet"]){const arc=await loadChain(key);assert.equal("keeper" in arc||"rpcOrder" in arc,false,key);}
});

test("the Robinhood example sets what the generator writes for Robinhood Chain Testnet, with placeholders for the deployment and the private endpoint",async()=>{
  const chain=await loadChain("robinhood-testnet"),example=await readFile(new URL("../../deploy/docker/keeper.robinhood.env.example",import.meta.url),"utf8");
  const zero=(bytes:number)=>"0x"+"00".repeat(bytes);
  assert.deepEqual(settingsOf(example),{...settingsOf(renderKeeperEnv(chain,record(chain.chainId),{allowPublicOnly:true})),
    RPC_URLS:`https://PRIVATE_RPC_ENDPOINT,${chain.rpcUrls[0]}`,COORDINATOR_ADDRESS:zero(20),EXPECTED_CODE_HASH:zero(32),EXPECTED_PROTOCOL_HASH:zero(32),EXPECTED_IMPLEMENTATION_CODE_HASH:zero(32)});
});

// The command line, end to end: named settings in, a 0600 file out, and nothing but names and counts on the terminal.
// The profiles are read as they are: Arc's, and the Robinhood testnet's, whose owner is decided (the deployer EOA). An undecided owner is
// refused (chain-guard.test.ts).
const cli=(...args:string[])=>spawnSync(process.execPath,["scripts/keeper-env.ts",...args],{encoding:"utf8"});
const SECRETS=["secret-key-one","secret-key-two","secret-socket-key","secret-token","secret-password","secret-health-key","secret-discord-token-0123456789","1234567890123456789"];

test("keeper-env.ts reads the named settings, writes the file and prints no secret",async()=>{
  const dir=await mkdtemp(join(tmpdir(),"d20-keeper-env-cli-")),settings=join(dir,"operator-settings.txt"),deployment=join(dir,"deployment.json");
  await writeFile(settings,[`PAID_ONE='https://paid-one.example/v2/secret-key-one'`,`PAID_TWO=https://paid-two.example/v2/secret-key-two`,`SOCKET=wss://socket.example/v2/secret-socket-key`,
    `TG_TOKEN=123:secret-token`,`INDEX_DB=${NEON}`,`HEALTH_KEY=secret-health-key`,`DISCORD_TOKEN=secret-discord-token-0123456789`,`UNRELATED=never-read-this-secret`].join("\n")+"\n");
  await writeFile(deployment,JSON.stringify(record(46630)));
  const out=join(dir,"out","keeper.txt");
  const all=["--chain","robinhood-testnet","--env",settings,"--deployment",deployment,"--out",out,"--rpc-var","PAID_ONE","--rpc-var","PAID_TWO","--ws-var","SOCKET",
    "--telegram-token-var","TG_TOKEN","--telegram-chat-id=-1001234","--neon-var","INDEX_DB","--health-url","https://health.example/v1/health/robinhood-testnet","--health-key-var","HEALTH_KEY",
    "--discord-token-var","DISCORD_TOKEN","--discord-channel-id","1234567890123456789","--discord-explorer-url","https://robinhood-testnet.site.example"];
  const run=cli(...all);
  assert.equal(run.status,0,run.stderr);
  for(const secret of SECRETS)assert.equal((run.stdout+run.stderr).includes(secret),false,`printed ${secret}`);
  assert.equal((run.stdout+run.stderr).includes("never-read-this-secret"),false);
  assert.deepEqual(JSON.parse(run.stdout),{written:out,chain:"robinhood-testnet",role:"primary",coordinator:record(46630).coordinator,sendTransactions:false,
    privateRpc:true,privateRpcUrls:2,wsUrls:1,telegram:true,neonIndexer:true,health:true,discord:true});
  const text=await readFile(out,"utf8"),got=settingsOf(text);
  assert.equal(got.RPC_URLS,"https://paid-one.example/v2/secret-key-one,https://paid-two.example/v2/secret-key-two,https://rpc.testnet.chain.robinhood.com");
  assert.equal(got.WS_URLS,"wss://socket.example/v2/secret-socket-key");
  assert.equal(got.HEALTH_API_KEY,"secret-health-key");assert.equal(got.DISCORD_BOT_TOKEN,"secret-discord-token-0123456789");assert.equal(got.NEON_DB,NEON);
  assert.equal(text.includes("never-read-this-secret"),false);
  // The file is never overwritten, and a refusal prints no secret either.
  const again=cli(...all);
  assert.equal(again.status,1);
  for(const secret of SECRETS)assert.equal((again.stdout+again.stderr).includes(secret),false,`printed ${secret} on a second run`);
  // A follower's file is named for its role by default and differs only in the role's settings.
  const follower=cli(...all.filter((_,i)=>all[i-1]!=="--out"&&all[i]!=="--out"),"--role","follower","--out",join(dir,"out","follower.txt"));
  assert.equal(follower.status,0,follower.stderr);
  const followerSettings=settingsOf(await readFile(join(dir,"out","follower.txt"),"utf8"));
  assert.equal(followerSettings.KEEPER_ROLE,"follower");assert.equal(followerSettings.KEYS_VOLUME,"d20dao-robinhood-testnet-follower-keys");
  // A setting that is missing or empty is named, never its neighbours.
  const missing=cli("--chain","robinhood-testnet","--env",settings,"--deployment",deployment,"--out",join(dir,"out","missing.txt"),"--rpc-var","NOT_THERE");
  assert.equal(missing.status,1);assert.match(missing.stderr,/NOT_THERE/);
  for(const secret of SECRETS)assert.equal(missing.stderr.includes(secret),false);
  // An option that makes the profile invalid stops before any file is written.
  const bad=cli("--chain","robinhood-testnet","--env",settings,"--deployment",deployment,"--out",join(dir,"out","bad.txt"),"--rpc-var","PAID_ONE","--ws-var","PAID_ONE");
  assert.equal(bad.status,1);assert.match(bad.stderr,/WebSocket URLs must use WSS/);
  assert.equal((bad.stdout+bad.stderr).includes("secret-key-one"),false);
  await assert.rejects(readFile(join(dir,"out","bad.txt"),"utf8"),/ENOENT/);
});

test("keeper-env.ts writes Arc's file exactly as the snapshot, from one --rpc-var or none",async()=>{
  const dir=await mkdtemp(join(tmpdir(),"d20-keeper-env-arc-")),settings=join(dir,"operator-settings.txt"),deployment=join(dir,"deployment.json");
  await mkdir(join(dir,"out"));
  await writeFile(settings,[`ARC_RPC=https://arc-testnet.example/v2/secret-key`,`ARC_TG=123:secret-token`,`ARC_DB=${NEON}`].join("\n")+"\n");
  await writeFile(deployment,JSON.stringify(record(5042002)));
  const full=cli("--chain","arc-testnet","--env",settings,"--deployment",deployment,"--out",join(dir,"out","full.txt"),"--rpc-var","ARC_RPC","--telegram-token-var","ARC_TG",
    "--telegram-chat-id=-1001234","--neon-var","ARC_DB");
  assert.equal(full.status,0,full.stderr);
  assert.deepEqual(JSON.parse(full.stdout),{written:join(dir,"out","full.txt"),chain:"arc-testnet",role:"primary",coordinator:record(5042002).coordinator,sendTransactions:false,
    privateRpc:true,privateRpcUrls:1,wsUrls:0,telegram:true,neonIndexer:true,health:false,discord:false});
  assert.equal(await readFile(join(dir,"out","full.txt"),"utf8"),await snapshot("arc-testnet.primary.full"));
  const bare=cli("--chain","arc-testnet","--env",settings,"--deployment",deployment,"--out",join(dir,"out","bare.txt"),"--role","follower");
  assert.equal(bare.status,0,bare.stderr);
  assert.equal(await readFile(join(dir,"out","bare.txt"),"utf8"),await snapshot("arc-testnet.follower.bare"));
  // Without --chain the command still means Arc Testnet, as before; a Robinhood record under that default is another chain's and is refused.
  const unnamed=cli("--env",settings,"--deployment",deployment,"--out",join(dir,"out","unnamed.txt"));
  assert.equal(unnamed.status,0,unnamed.stderr);
  assert.equal(await readFile(join(dir,"out","unnamed.txt"),"utf8"),await snapshot("arc-testnet.primary.bare"));
  const robinhoodRecord=join(dir,"robinhood-deployment.json");
  await writeFile(robinhoodRecord,JSON.stringify(record(46630)));
  const crossed=cli("--env",settings,"--deployment",robinhoodRecord,"--out",join(dir,"out","crossed.txt"));
  assert.equal(crossed.status,1);assert.match(crossed.stderr,/^Deployment record is for another chain$/m);
  await assert.rejects(readFile(join(dir,"out","crossed.txt"),"utf8"),/ENOENT/);
});

test("keeper-env.ts needs a --rpc-var on a private-first chain unless --allow-public-only is given, and writes nothing when it refuses",async()=>{
  const dir=await mkdtemp(join(tmpdir(),"d20-keeper-env-public-only-")),settings=join(dir,"operator-settings.txt"),deployment=join(dir,"deployment.json");
  await writeFile(settings,[`PAID_ONE=https://paid-one.example/v2/secret-key-one`,`UNRELATED=never-read-this-secret`].join("\n")+"\n");
  await writeFile(deployment,JSON.stringify(record(46630)));
  const args=(name:string,...more:string[])=>["--chain","robinhood-testnet","--env",settings,"--deployment",deployment,"--out",join(dir,"out",name),...more];
  const refused=cli(...args("refused.txt"));
  assert.equal(refused.status,1);
  assert.match(refused.stderr,/robinhood-testnet lists its public endpoint last .*\(--rpc-var\), or accept the public endpoint alone with --allow-public-only/);
  assert.equal(refused.stdout,"");
  await assert.rejects(readFile(join(dir,"out","refused.txt"),"utf8"),/ENOENT/);
  // Asked for, the file lists the public endpoint alone and the summary says there is no private endpoint.
  const allowed=cli(...args("public.txt","--allow-public-only"));
  assert.equal(allowed.status,0,allowed.stderr);
  assert.deepEqual(JSON.parse(allowed.stdout),{written:join(dir,"out","public.txt"),chain:"robinhood-testnet",role:"primary",coordinator:record(46630).coordinator,sendTransactions:false,
    privateRpc:false,privateRpcUrls:0,wsUrls:0,telegram:false,neonIndexer:false,health:false,discord:false});
  assert.equal(settingsOf(await readFile(join(dir,"out","public.txt"),"utf8")).RPC_URLS,"https://rpc.testnet.chain.robinhood.com");
  // With a private endpoint the flag changes nothing: it goes first and the public endpoint last, as without the flag.
  const plain=cli(...args("private.txt","--rpc-var","PAID_ONE")),flagged=cli(...args("flagged.txt","--rpc-var","PAID_ONE","--allow-public-only"));
  assert.equal(plain.status,0,plain.stderr);assert.equal(flagged.status,0,flagged.stderr);
  const text=await readFile(join(dir,"out","private.txt"),"utf8");
  assert.equal(await readFile(join(dir,"out","flagged.txt"),"utf8"),text);
  assert.equal(settingsOf(text).RPC_URLS,"https://paid-one.example/v2/secret-key-one,https://rpc.testnet.chain.robinhood.com");
  for(const run of [refused,allowed,plain,flagged])assert.equal((run.stdout+run.stderr).includes("secret-key-one")||(run.stdout+run.stderr).includes("never-read-this-secret"),false);
  // Arc is public-first and never needed the flag; giving it changes nothing.
  const arcSettings=join(dir,"arc-settings.txt"),arcDeployment=join(dir,"arc-deployment.json");
  await writeFile(arcSettings,"UNRELATED=never-read-this-secret\n");await writeFile(arcDeployment,JSON.stringify(record(5042002)));
  const arc=(name:string,...more:string[])=>cli("--chain","arc-testnet","--env",arcSettings,"--deployment",arcDeployment,"--out",join(dir,"out",name),...more);
  assert.equal(arc("arc.txt").status,0);assert.equal(arc("arc-flagged.txt","--allow-public-only").status,0);
  assert.equal(await readFile(join(dir,"out","arc.txt"),"utf8"),await snapshot("arc-testnet.primary.bare"));
  assert.equal(await readFile(join(dir,"out","arc-flagged.txt"),"utf8"),await snapshot("arc-testnet.primary.bare"));
});
