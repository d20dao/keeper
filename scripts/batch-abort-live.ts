// Live-network batch-abort drill. In one transaction the attacker opens a losing-biased raffle draw and two
// 1,000,000-gas "saboteur" helper requests, so the real keepers see the three together and batch them. A saboteur burns
// its whole callback budget on chain, but only when tx.gasprice != 0 and its owner's draw (served earlier in the same
// batch) lost; the keeper's eth_estimateGas runs with gasprice 0 on Arc, so the estimate sees a cheap callback. Before the
// fixes a starved later member reverted the whole batch after the losing result was revealed, so the requester let its
// losing draws expire, took the refund and drew again. This script never fulfils: it drives the attacker and records what
// the live keepers do, round by round. On a patched keeper every losing draw is fulfilled and kept, none is refunded, and
// the members land in one fulfillRandomnessBatch. See contracts/test/BatchAbortConsumers.sol and scripts/batch-abort-drill.ts.
//
// Plan mode (the default) reads pricing, balances and the per-round cost and sends nothing. --apply sends, targeting
// Arc testnet. Arc mainnet (chain 5042) is refused outright, whatever flags are given. A local chain (31337) is allowed
// for exercising --apply against a stand-in keeper. The sending key is read with the shared loader (DEPLOYER_KEY /
// DEPLOYER_ADDRESS) and is never printed; sent transactions are journalled by label, hash and nonce before broadcast,
// as in create2-deploy.ts / request-smoke.ts.
//
//   plan:   node scripts/batch-abort-live.ts --chain arc-testnet --from <sender address>
//   apply:  node scripts/batch-abort-live.ts --chain arc-testnet --env /secure/operator.env --apply --rounds 5
import {parseArgs} from "node:util";
import {readFile, writeFile, mkdir, open} from "node:fs/promises";
import {resolve, join} from "node:path";
import {Contract, ContractFactory, JsonRpcProvider, formatUnits, parseUnits, getAddress, getCreateAddress, keccak256, getBytes, type TransactionReceipt} from "ethers";
import {loadDeployer} from "./lib/deployer-env.ts";
import {loadChain, type Chain} from "./lib/chains.ts";
import {currentFee} from "./lib/gas.ts";
import {Stop} from "./lib/deployment.ts";

// Transactions handed to the RPC in this run; a failure after a broadcast lists them so receipts can be checked.
const broadcast: {sent: Array<{label: string; hash: string}>} = {sent: []};
const MAINNET_CHAIN_ID = 5042;      // Arc mainnet: refused, no flag overrides it.
const LOCAL_CHAIN_ID = 31337;       // Local EDR chain: allowed for testing the apply path. Arc testnet (5042002) is the target.
const DRAW_CALLBACK_GAS = 100_000;  // BatchAbortRaffle's callback budget.
const HELPER_CALLBACK_GAS = 1_000_000; // Each BatchAbortHelper's callback budget (the saboteur burns all of it on chain).
const SABOTEURS = 2;                // drawAndArm opens the draw plus this many helper requests.
const HONEST_ENTRANTS = 7;          // Entered before the attacker, so the attacker's draw wins about 1 in 8 and usually loses.
const MEMBERS = 1 + SABOTEURS;      // draw + saboteurs, the batch the keepers should form.
const DEFAULT_ROUNDS = 5;
const HARD_SPEND_CAP = parseUnits("5", 18);      // At most 5 USDC per run; --max-spend only lowers it.
const FUND_BUFFER_BPS = 3000n;      // Fund the attacker 30% over the quoted fees; drawAndArm re-quotes at execution base fee.
const PER_ROUND_GAS_BUDGET = 5_000_000n; // Upper bound on a round's gas for the cost cap (fresh deploys + entries + drawAndArm + refunds).
const SETTLE_TIMEOUT_MS = 180_000;  // The 60 s deadline plus generous slack for publication and observation.
const POLL_MS = 1500;
const sleep = (ms: number) => new Promise(done => setTimeout(done, ms));
const bps = (value: bigint, points: bigint) => value + value * points / 10000n;

type Profile = Pick<Chain, "key" | "name" | "chainId" | "rpcUrls" | "explorerUrl" | "nativeCurrency" | "gas"> & {local: boolean};

// Local profile: not in chains.json (which is https-only and factory-pinned), so it is built here for the 31337 test path.
function localProfile(rpc: string): Profile {
  return {key: "local", name: "Local EDR", chainId: LOCAL_CHAIN_ID, rpcUrls: [rpc], explorerUrl: "",
    nativeCurrency: {name: "USDC", symbol: "USDC", decimals: 18}, local: true,
    gas: {maxGas: 30_000_000, maxFeePerGasWei: "100000000000", cancelMaxFeePerGasWei: "150000000000", maxTxCostWei: "4000000000000000000"}};
}

async function loadProfile(chainKey: string, rpc: string | undefined): Promise<Profile> {
  if (chainKey === "arc-mainnet") throw new Stop("Arc mainnet (chain 5042) is refused; this drill runs on Arc testnet or a local chain only");
  if (chainKey === "local" || chainKey === "localhost") return localProfile(rpc ?? "http://127.0.0.1:8545");
  const chain = await loadChain(chainKey);
  if (chain.chainId === MAINNET_CHAIN_ID) throw new Stop("Arc mainnet (chain 5042) is refused, whatever flags are given");
  return {...chain, rpcUrls: rpc ? [rpc] : chain.rpcUrls, local: false};
}

async function loadArtifact(relative: string): Promise<{abi: any[]; bytecode: string}> {
  try {return JSON.parse(await readFile(resolve(relative), "utf8"));}
  catch {throw new Stop(`Missing ${relative}; run "npm run compile" first`);}
}

async function main() {
  const {values} = parseArgs({options: {
    chain: {type: "string", default: "arc-testnet"},
    rpc: {type: "string"},
    manifest: {type: "string"},
    coordinator: {type: "string"},
    env: {type: "string"},
    from: {type: "string"},
    rounds: {type: "string", default: String(DEFAULT_ROUNDS)},
    "max-spend": {type: "string"},
    reuse: {type: "string"},
    "no-refund": {type: "boolean", default: false},
    apply: {type: "boolean", default: false},
  }});

  const profile = await loadProfile(values.chain, values.rpc);
  const rounds = Number(values.rounds);
  if (!Number.isInteger(rounds) || rounds < 1 || rounds > 50) throw new Stop("--rounds must be between 1 and 50");
  const maxSpend = values["max-spend"] ? parseUnits(values["max-spend"], 18) : HARD_SPEND_CAP;
  if (maxSpend <= 0n || maxSpend > HARD_SPEND_CAP) throw new Stop(`--max-spend must be between 0 and 5 ${profile.nativeCurrency.symbol}`);

  // The coordinator proxy: from the public manifest (default deployments/<chain>.json) or an explicit --coordinator.
  const manifestPath = values.manifest ?? (profile.local ? undefined : `deployments/${profile.key}.json`);
  let coordinatorAddress = values.coordinator ? getAddress(values.coordinator) : undefined;
  let manifest: any;
  if (manifestPath) {
    manifest = JSON.parse(await readFile(resolve(manifestPath), "utf8"));
    if (manifest.chainId === MAINNET_CHAIN_ID) throw new Stop("The manifest is for Arc mainnet (chain 5042), which is refused");
    if (manifest.chainId !== profile.chainId) throw new Stop("The manifest is for another chain");
    coordinatorAddress ??= getAddress(manifest.coordinator);
  }
  if (!coordinatorAddress) throw new Stop("Provide --coordinator or --manifest to locate the coordinator");

  const provider = new JsonRpcProvider(profile.rpcUrls[0], undefined, {batchMaxCount: 1});
  const journal = {path: undefined as string | undefined};
  try {
    // Confirm the connected chain, and refuse mainnet even if a testnet key/manifest was pointed at it.
    const net = await provider.getNetwork();
    if (Number(net.chainId) === MAINNET_CHAIN_ID) throw new Stop("Connected RPC is Arc mainnet (chain 5042); refused");
    if (Number(net.chainId) !== profile.chainId) throw new Stop(`Connected RPC is chain ${net.chainId}, expected ${profile.chainId}`);
    const head = await provider.getBlock("latest");
    if (!head) throw new Stop("RPC returned no head block");
    if (!profile.local && Math.abs(Math.floor(Date.now() / 1000) - head.timestamp) > 120) throw new Stop("Stale RPC head");

    const coordinatorArtifact = await loadArtifact("artifacts/contracts/D20VRFCoordinator.sol/D20VRFCoordinator.json");
    const raffleArtifact = await loadArtifact("artifacts/contracts/test/BatchAbortConsumers.sol/BatchAbortRaffle.json");
    const attackerArtifact = await loadArtifact("artifacts/contracts/test/BatchAbortConsumers.sol/BatchAbortAttacker.json");
    const coordinator = new Contract(coordinatorAddress, coordinatorArtifact.abi, provider);
    const batchSelector = coordinator.interface.getFunction("fulfillRandomnessBatch")!.selector;
    const code = await provider.getCode(coordinatorAddress);
    if (code === "0x") throw new Stop("No contract at the coordinator address");

    // Price a round from real on-chain reads: fees scale with the callback budget, and drawAndArm re-quotes at execution
    // base fee, so the estimate uses the current base fee like the keeper does.
    const {baseFee, tip, maxFee} = await currentFee(provider, BigInt(profile.gas.maxFeePerGasWei));
    const [minFee, feeMultiplier, fulfillGasOverhead] = await coordinator.pricing() as [bigint, bigint, bigint];
    const drawFee = await coordinator.quoteFeeAt(DRAW_CALLBACK_GAS, baseFee) as bigint;
    const saboteurFee = await coordinator.quoteFeeAt(HELPER_CALLBACK_GAS, baseFee) as bigint;
    const feesPerRound = drawFee + BigInt(SABOTEURS) * saboteurFee;
    const fundingPerRound = bps(feesPerRound, FUND_BUFFER_BPS);
    const gasPerRound = PER_ROUND_GAS_BUDGET * maxFee;
    const perRoundBudget = fundingPerRound + gasPerRound;
    const estimatedTotal = BigInt(rounds) * perRoundBudget;

    // Sender: the --env wallet (needed for --apply) or a read-only --from address for the plan's balance line.
    let sender = values.from ? getAddress(values.from) : undefined;
    if (values.env && !sender) sender = (await loadDeployer(resolve(values.env), provider)).address;
    const senderBalance = sender ? await provider.getBalance(sender) : null;

    const usd = (value: bigint) => formatUnits(value, 18);
    const plan = {
      mode: values.apply ? "apply" : "plan",
      chain: profile.key, chainId: profile.chainId, coordinator: coordinatorAddress,
      sender: sender ?? null, senderBalance: senderBalance === null ? null : usd(senderBalance),
      currency: profile.nativeCurrency.symbol,
      rounds, membersPerRound: MEMBERS, honestEntrants: HONEST_ENTRANTS,
      pricing: {minFee: usd(minFee), feeMultiplier: Number(feeMultiplier), fulfillGasOverhead: Number(fulfillGasOverhead), baseFeeGwei: formatUnits(baseFee, "gwei")},
      feePerDraw: usd(drawFee), feePerSaboteur: usd(saboteurFee), feesPerRound: usd(feesPerRound),
      fundingPerRound: usd(fundingPerRound), maxGasCostPerRound: usd(gasPerRound), estimatedCostPerRound: usd(perRoundBudget),
      estimatedTotalCost: usd(estimatedTotal), maxSpend: usd(maxSpend), maxFeePerGasGwei: formatUnits(maxFee, "gwei"),
      reuse: values.reuse ?? null,
    };
    console.log(JSON.stringify(plan, null, 2));

    if (estimatedTotal > maxSpend) console.error(`Note: the estimated ${usd(estimatedTotal)} ${profile.nativeCurrency.symbol} exceeds the ${usd(maxSpend)} cap; lower --rounds or raise --max-spend (never above 5).`);
    if (!values.apply) return;

    // ---- apply ----
    if (!values.env) throw new Stop("--apply needs --env with the sending key (DEPLOYER_KEY / DEPLOYER_ADDRESS)");
    const wallet = await loadDeployer(resolve(values.env), provider);
    if (estimatedTotal > maxSpend) throw new Stop(`Estimated ${usd(estimatedTotal)} ${profile.nativeCurrency.symbol} exceeds the ${usd(maxSpend)} cap; lower --rounds or --max-spend`);
    const balance = await provider.getBalance(wallet.address);
    if (balance < estimatedTotal) throw new Stop(`Sender balance ${usd(balance)} is below the ${usd(estimatedTotal)} ${profile.nativeCurrency.symbol} budget`);
    let start = await wallet.getNonce("latest");
    if (await wallet.getNonce("pending") !== start) throw new Stop("Sender has unresolved transactions");

    await mkdir(resolve(".research"), {recursive: true});
    journal.path = resolve(join(".research", `batch-abort-live-${profile.key}-${new Date().toISOString().replace(/[:.]/g, "-")}.jsonl`));
    const journalFile = await open(journal.path, "ax", 0o600);
    const record = async (value: unknown) => {await journalFile.writeFile(JSON.stringify(value) + "\n"); await journalFile.sync();};
    await record({type: "plan", plan});

    let confirmed = 0, spent = 0n;
    // Sign raw and journal before broadcast, so a later failure lists what was handed to the RPC; funds may then have moved.
    async function send(label: string, request: {to?: string; data?: string; value?: bigint}, gasCap: bigint): Promise<{receipt: TransactionReceipt; hash: string; contractAddress: string}> {
      // Nonces are tracked locally from the verified start; the node rejects any that an external transaction took.
      const nonce = start + confirmed;
      const estimate = await provider.estimateGas({from: wallet.address, ...request});
      const gasLimit = estimate + estimate / 2n > gasCap ? gasCap : estimate + estimate / 2n;
      if (estimate > gasCap) throw new Stop(`${label} gas estimate ${estimate} exceeds its ${gasCap} cap`);
      const contractAddress = request.to ? request.to : getCreateAddress({from: wallet.address, nonce});
      const raw = await wallet.signTransaction({...request, value: request.value ?? 0n, chainId: profile.chainId, type: 2, nonce, gasLimit, maxFeePerGas: maxFee, maxPriorityFeePerGas: tip});
      const hash = keccak256(getBytes(raw));
      await record({type: "signed-before-broadcast", label, hash, nonce, to: request.to ?? null, value: String(request.value ?? 0n)});
      broadcast.sent.push({label, hash});
      const tx = await provider.broadcastTransaction(raw);
      if (tx.hash !== hash) throw new Stop("Unexpected broadcast hash");
      const receipt = await tx.wait(1, 90_000);
      if (receipt?.status !== 1) throw new Stop(`${label} transaction failed`);
      await record({type: "confirmed", label, hash, block: receipt.blockNumber, gasUsed: String(receipt.gasUsed), gasCost: String(receipt.fee)});
      confirmed++;
      spent += receipt.fee + (request.value ?? 0n);
      return {receipt, hash, contractAddress};
    }

    const raffleFactory = new ContractFactory(raffleArtifact.abi, raffleArtifact.bytecode);
    const attackerFactory = new ContractFactory(attackerArtifact.abi, attackerArtifact.bytecode);
    // Persist each deployed set so --reuse <attacker> on a later run can find its raffle (the attacker's raffle is private).
    async function deploySet(): Promise<{attacker: Contract; raffle: Contract; address: string; raffleAddress: string; fresh: true}> {
      const raffleDeploy = await send("deploy raffle", {data: (await raffleFactory.getDeployTransaction(coordinatorAddress)).data}, PER_ROUND_GAS_BUDGET);
      const raffleAddress = raffleDeploy.contractAddress;
      const attackerDeploy = await send("deploy attacker", {data: (await attackerFactory.getDeployTransaction(coordinatorAddress, raffleAddress)).data}, PER_ROUND_GAS_BUDGET);
      const address = attackerDeploy.contractAddress;
      const record = {chainId: profile.chainId, coordinator: coordinatorAddress, attacker: address, raffle: raffleAddress};
      await writeFile(resolve(join(".research", `batch-abort-live-${profile.key}-${address}.json`)), JSON.stringify(record, null, 2) + "\n", {mode: 0o600});
      return {attacker: new Contract(address, attackerArtifact.abi, wallet), raffle: new Contract(raffleAddress, raffleArtifact.abi, wallet), address, raffleAddress, fresh: true};
    }
    async function loadSet(attacker: string): Promise<{attacker: Contract; raffle: Contract; address: string; raffleAddress: string; fresh: false}> {
      const rec = JSON.parse(await readFile(resolve(join(".research", `batch-abort-live-${profile.key}-${getAddress(attacker)}.json`)), "utf8"));
      if (getAddress(rec.coordinator) !== coordinatorAddress) throw new Stop("The reused drill set was deployed against a different coordinator");
      return {attacker: new Contract(getAddress(rec.attacker), attackerArtifact.abi, wallet), raffle: new Contract(getAddress(rec.raffle), raffleArtifact.abi, wallet), address: getAddress(rec.attacker), raffleAddress: getAddress(rec.raffle), fresh: false};
    }

    // Realise the refund of an own request past its deadline, so an aborted (refunded, then redrawable) draw is observed
    // as such rather than left pending. Permissionless; the fee returns to the attacker as refund credit.
    async function realiseRefund(id: bigint): Promise<boolean> {
      const r = await coordinator.getRequest(id);
      if (r.fulfilled || r.refunded) return r.refunded;
      const receipt = await send(`refund request ${id}`, {to: coordinatorAddress, data: coordinator.interface.encodeFunctionData("refundRequest", [id])}, 500_000n);
      return receipt.receipt.status === 1;
    }

    type RoundResult = {round: number; ids: string[]; states: Record<string, string>; batched: boolean | null; fulfilmentTx: string[];
      gasLimit: string | null; gasUsed: string | null; lostDraw: boolean | null; attackSucceeded: boolean; note: string};
    const results: RoundResult[] = [];
    let set: Awaited<ReturnType<typeof deploySet>> | Awaited<ReturnType<typeof loadSet>> | null = values.reuse ? await loadSet(values.reuse) : null;

    for (let round = 0; round < rounds; round++) {
      if (spent + perRoundBudget > maxSpend) {console.error(`Stopping before round ${round}: another round would pass the ${usd(maxSpend)} ${profile.nativeCurrency.symbol} cap (spent ${usd(spent)}).`); break;}

      // Reuse the set only while its raffle can draw again (fresh, or reset by a previous refund); otherwise deploy a new one.
      const redrawable = set ? (await (set.raffle as any).drawId()) === 0n : false;
      if (!redrawable) set = await deploySet();
      const {attacker, raffle, address: attackerAddress, raffleAddress} = set!;

      // A fresh raffle has no entrants and is open; enter honest entrants (biasing the draw to lose) then the attacker.
      if (!(await (raffle as any).closed())) {
        for (let i = 0; i < HONEST_ENTRANTS; i++) await send(`enter honest ${round}.${i}`, {to: raffleAddress, data: raffle.interface.encodeFunctionData("enter", [])}, 200_000n);
        await send(`enter attacker ${round}`, {to: attackerAddress, data: attacker.interface.encodeFunctionData("enter", [])}, 300_000n);
      }

      // Fund the attacker for the exact fees it will re-quote and forward inside drawAndArm, with a base-fee buffer.
      await send(`fund attacker ${round}`, {to: attackerAddress, value: fundingPerRound}, 100_000n);

      // One transaction opens the draw and both saboteur requests, with consecutive ids the keepers should batch.
      const first = await coordinator.nextRequestId() as bigint;
      const salt = keccak256(Buffer.from(`batch-abort-live:${profile.chainId}:${attackerAddress}:${round}:${Date.now()}`));
      const armed = await send(`drawAndArm ${round}`, {to: attackerAddress, data: attacker.interface.encodeFunctionData("drawAndArm", [salt])}, 1_500_000n);
      const ids = Array.from({length: MEMBERS}, (_, i) => first + BigInt(i));
      // Confirm the three requests were opened together in this one transaction.
      const opened = armed.receipt.logs.filter(l => l.address.toLowerCase() === coordinatorAddress.toLowerCase())
        .map(l => {try {return coordinator.interface.parseLog(l);} catch {return null;}}).filter(p => p?.name === "RandomnessRequested").map(p => p!.args.requestId as bigint);
      if (opened.length !== MEMBERS || ids.some(id => !opened.includes(id))) throw new Stop(`Round ${round}: expected ${MEMBERS} requests opened in one transaction, saw ${opened.length}`);

      // Wait for the keepers to fulfil each request or for it to pass its 60 s deadline.
      const states = new Map<bigint, string>();
      for (const stopAt = Date.now() + SETTLE_TIMEOUT_MS; states.size < ids.length && Date.now() < stopAt;) {
        const now = (await provider.getBlock("latest"))!;
        for (const id of ids) {
          if (states.has(id)) continue;
          const r = await coordinator.getRequest(id);
          if (r.fulfilled) states.set(id, "fulfilled");
          else if (r.refunded) states.set(id, "refunded");
          else if (now.timestamp > Number(r.deadline)) states.set(id, "expired");
        }
        if (states.size < ids.length) await sleep(POLL_MS);
      }

      // Realise refunds for any own request that expired unfulfilled, so an aborted draw shows as refunded (and redrawable).
      if (!values["no-refund"]) for (const id of ids) if (states.get(id) === "expired" && spent + 600_000n * maxFee < maxSpend) {if (await realiseRefund(id)) states.set(id, "refunded");}

      // Which transaction(s) fulfilled the members, and whether they were served in one fulfillRandomnessBatch.
      const fulfilled = ids.filter(id => states.get(id) === "fulfilled");
      const txHashes = new Set<string>();
      for (const id of fulfilled) {
        // Bounded from the request block: public endpoints refuse log queries into pruned history.
        const logs = await coordinator.queryFilter(coordinator.filters.RandomnessFulfilled(id), armed.receipt.blockNumber);
        for (const log of logs) txHashes.add(log.transactionHash);
      }
      let batched: boolean | null = null, gasLimit: string | null = null, gasUsed: string | null = null, note = "";
      if (fulfilled.length === 0) {batched = null; note = "no member fulfilled";}
      else if (txHashes.size === 1 && fulfilled.length > 1) {
        const hash = [...txHashes][0];
        const tx = await provider.getTransaction(hash), receipt = await provider.getTransactionReceipt(hash);
        batched = tx?.data.startsWith(batchSelector) ?? false;
        gasLimit = tx ? String(tx.gasLimit) : null; gasUsed = receipt ? String(receipt.gasUsed) : null;
        note = batched ? `served in one batch of ${fulfilled.length}` : "single fulfilment selector, not a batch";
      } else {batched = false; note = `served in ${txHashes.size} separate transactions (keeper may run FULFILL_BATCH_MAX=1); not batched`;}

      // The draw is the first id; a loss is a fulfilled draw the attacker did not win. On the patched keeper it is kept.
      const drawState = states.get(ids[0]) ?? "pending";
      let lostDraw: boolean | null = null;
      if (drawState === "fulfilled") {try {lostDraw = (getAddress(await (raffle as any).winner())) !== attackerAddress;} catch {lostDraw = null;}}
      const attackSucceeded = drawState === "refunded" || drawState === "expired"; // A losing/aborted draw not kept: on a patched keeper this never happens.

      const result: RoundResult = {round, ids: ids.map(String), states: Object.fromEntries(ids.map(id => [String(id), states.get(id) ?? "pending"])),
        batched, fulfilmentTx: [...txHashes], gasLimit, gasUsed, lostDraw, attackSucceeded, note};
      results.push(result);
      console.log(`round ${round}: draw=${drawState}${lostDraw === null ? "" : lostDraw ? " (lost)" : " (won)"} saboteurs=${ids.slice(1).map(id => states.get(id) ?? "pending").join(",")} batched=${batched} gasLimit=${gasLimit ?? "-"} gasUsed=${gasUsed ?? "-"} attackSucceeded=${attackSucceeded} :: ${note}`);
    }

    await journalFile.close();

    // Verdict. A pass keeps every losing draw, refunds none and lands the batches; anything else is called out.
    const observed = results.filter(r => r.batched !== null);
    const anyRefunded = results.some(r => r.attackSucceeded);
    const anyNotBatched = observed.some(r => r.batched === false);
    const allBatched = observed.length > 0 && observed.every(r => r.batched === true);
    const verdict = anyRefunded ? "attack-succeeded" : anyNotBatched ? "not-batched" : allBatched ? "pass" : "inconclusive";
    const summary = {batchAbortLive: {chain: profile.key, chainId: profile.chainId, coordinator: coordinatorAddress,
      rounds: results.length, verdict, spent: usd(spent), maxSpend: usd(maxSpend), currency: profile.nativeCurrency.symbol,
      results, journal: journal.path!}};
    const summaryPath = resolve(join(".research", `batch-abort-live-${profile.key}-${new Date().toISOString().replace(/[:.]/g, "-")}.summary.json`));
    await writeFile(summaryPath, JSON.stringify(summary, null, 2) + "\n", {mode: 0o600});
    console.log(JSON.stringify(summary, null, 2));
    if (verdict !== "pass") process.exitCode = 1;
  } finally {provider.destroy();}
}

// Every failure prints its reason; one after a broadcast lists what was handed to the RPC. Keys and env values are read
// only by loadDeployer, whose errors never include them; a parse error is not quoted, as it can echo file text.
main().catch(error => {
  console.error(`Batch-abort live drill stopped: ${error instanceof SyntaxError ? "an input file could not be parsed" : error instanceof Error ? error.message : String(error)}`);
  if (broadcast.sent.length) console.error(`Handed to the RPC, so funds may have moved; check each receipt: ${broadcast.sent.map(({label, hash}) => `${label} ${hash}`).join(", ")}`);
  if (!(error instanceof Stop)) console.error("Credentials were not logged; inspect local configuration and the drill journal before retrying.");
  process.exitCode = 1;
});
