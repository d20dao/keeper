# Deployment and testnet measurements

Networks are configured in chains.json. The first entry is Arc Testnet, chain 5042002, native USDC with 18-decimal accounting. Its connection settings follow https://docs.arc.io/arc/references/connect-to-arc . ERC-20 USDC uses 6 decimals and must not be confused with native gas units.

Shared initialization settings are in config/service.json; minFeeWei is the coordinator's initial minimum fee (initialMinFee, bound into protocolConfigurationHash). It carries a 0.08 USDC minimum fee and a 5,000 bps keeper share, used by testnet and mainnet deployments alike. Keep these initial values stable when reusing CREATE2 addresses across chains. Live requests pay quoteFeeAt(callbackGasLimit, baseFee) = max(minFee, feeMultiplier × baseFee × (fulfillGasOverhead + callbackGasLimit)); the owner tunes those parameters with setPricing and pricing() reports the current values. Read keeperFeeBps() and pricing() for current state.

## Deterministic addresses

Deployments use the configured CREATE2 factory only after its runtime code hash has been verified. Every implementation and proxy must start with d20da0. The service comprises EpochEntropy and D20VRFCoordinator behind D20Proxy instances. The restricted D20CostClient and its proxy exist only for supervised cost measurement.

A stable proxy can upgrade its implementation under two-step owner governance. The original CREATE2 address depends on factory, salt and init code including initializer arguments. Reusing addresses on another configured chain requires identical inputs, including original implementation addresses and initialized settings; CREATE2 support alone is insufficient. Per-chain proxy state such as firstEpochStart and protocolConfigurationHash still differs and is read after deployment.

## Prepare without broadcasting

```sh
node scripts/network-preflight.ts arc-testnet
node scripts/create2-deploy.ts prepare --chain arc-testnet --env /secure/deployer.env --new-operator
```

The loader reads only DEPLOYER_ADDRESS and DEPLOYER_KEY, validates that they match, and never prints key values or unrelated environment fields. Protected keeper/VRF identities are stored separately under the operator's home .config/d20dao directory and reused across checkouts/chains. Only `prepare --new-operator` creates one, never over an existing operator.json; every other run needs the existing identity, so a mistyped `--operator-directory` stops instead of creating fresh keys. Missing or mismatched referenced keys fail closed; they are not silently replaced.

prepare emits three public implementation search plans. The optional GPU tool in scripts/vanity uses only factory/init-code hashes and independently validates GPU results with ethers. Install its requirements in an isolated Python environment. Use registry-plan, coordinator-plan and client-plan with the corresponding mined result paths to derive the proxy plans. Changing code or initialization inputs invalidates the old results.

```sh
python scripts/vanity/search.py /secure/epoch-implementation-search.json --seconds 30 --output /secure/epoch-implementation-result.json
```

Each search has a bounded duration and a nextStart value for an explicit continuation. Search outputs are public salt/address evidence, not wallet keys.

The deploy mode prints a plan by default. Its transactions are priced from current gas: the max fee is twice the latest base fee plus the median tip of the last 20 blocks (1–50 gwei), bounded by the chain profile's fee cap, so the deployer needs the six gas limits (19.5M gas) times that fee rather than the cap. If the base fee later rises above that price the run stops and `--resume` continues at the new price. It broadcasts only with --apply, checks chain/factory/code/nonce/budgets, and saves each exact signed transaction before sending. An implementation already deployed at its CREATE2 address (for instance unchanged code shared with an earlier deployment) is reused and journaled as `existing`; a proxy candidate with code stops the deployment. An existing deployment journal blocks blind repetition; `--apply --resume` continues the same journaled plan, skipping confirmed steps and refusing if any signed step is unconfirmed or any address or identity differs. After confirmation it checks ownership, operator roles and implementation slots, then records actual runtime hashes and generates private keeper settings. Implementation hashes must account for UUPS's embedded self address; raw artifact placeholders are not deployed runtime code.

## Arc mainnet

Every mainnet command requires `--mainnet`. `prepare` and `deploy` also require `DAO_TREASURY` in the env file to equal the `arc-mainnet` owner in chains.json and that owner to have code (the DAO treasury Safe). The Safe becomes owner and fee recipient of every proxy, and the 5,000 bps keeper share splits each fee evenly between the keeper and the treasury. Use a separate operator directory so mainnet keeper and VRF keys never mix with testnet ones, and back it up before deploying.

```sh
node scripts/create2-deploy.ts prepare --chain arc-mainnet --mainnet --env /secure/deployer.env --operator-directory ~/.config/d20dao-arc-mainnet --new-operator
# Mine the implementation salts, then registry-plan, coordinator-plan and client-plan with the same flags, without --new-operator.
node scripts/create2-deploy.ts deploy --chain arc-mainnet --mainnet --env /secure/deployer.env --operator-directory ~/.config/d20dao-arc-mainnet --epoch-implementation ... --client ...
node scripts/keeper-env.ts --chain arc-mainnet --env /secure/operator.env --rpc-var <private RPC URL setting> --telegram-token-var <bot token setting> --telegram-pairing ~/.config/d20dao-arc-mainnet/telegram-pairing.json --neon-var <index database URL setting>
node scripts/request-smoke.ts --chain arc-mainnet --mainnet --env /secure/deployer.env --apply
```

`keeper-env.ts` writes `keeper.docker.env` (mode 0600, never overwritten) next to the private deployment record: the deployed pins, the chain profile's fee caps, `SEND_TRANSACTIONS=false`, the private RPC from the named setting as the second endpoint, Telegram with the chain's own bot token and the chat id from its pairing record (use a separate bot per network: keepers sharing a bot token compete for its command updates; a follower of the same network shares it in notification-only mode), and the chain's index database. It reads only the named settings and prints only which optional parts were included. Copy it to the host as `deploy/docker/keeper.env`. With `--role follower` it writes `keeper.follower.docker.env` instead, with `KEEPER_ROLE=follower`, the default delay and the keys volume of the named instance `<chain>-follower`; copy that to `deploy/docker/instances/<chain>-follower/keeper.env` on the follower host. `request-smoke.ts` sends one to five paid requests through the cost client, priced from current gas, and waits for the running keeper to deliver them; without `--apply` it prints the quote and maximum spend. On mainnet `admin.ts` prints owner transactions for a Safe proposal and refuses `--apply`.

## Cost pilot

```sh
node scripts/cost-smoke.ts --manifest /secure/deployment.json --env /secure/deployer.env
```

The default is a read-only plan. --apply performs a bounded testnet run: limited keeper bootstrap funding, three accepted requests including a deliberately failed callback and same-result repair, then one unserved expired request/refund. It runs the real Rust keeper on the keeper wallet, so a local run also needs `--keeper-stopped` once the service keeper for that wallet is stopped. It stores receipt gasUsed, effective gas price, native gas cost and replay inputs. Epoch publication, fulfillment, requester transactions, repair and refund costs are reported separately. Request fees are separate transfers and must not be counted as network gas.

The cost client accepts requests only from its configured tester. This is not a public application starter. Provider availability and callback behavior influence latency/cost; local load tests do not establish a public-chain SLA.

For a configured Linux VPS, add `--keeper-ssh USER@HOST --keeper-path /absolute/checkout` to the cost script. It starts/stops the existing Docker service and reads bounded public readiness/transaction observations over SSH using `scripts/keeper-pilot.py`. The deployment key stays with the local controller. The same persistent server journal is used during the pilot and subsequent service operation; the pilot leaves the service stopped after its timeout/refund case. Review the report before starting continuous operation.

## Runtime and management

After reviewing deployed pins, install the keeper using deploy/docker/keeper.sh install or keeper.ps1 install with protected transaction and VRF key files. Configure CHAIN_ID, RPC_URLS, COORDINATOR_ADDRESS, KEEPER_DB and both implementation hash pins. The process exposes no incoming application port.

scripts/admin.ts emits owner/multisig calldata for keeper rotation, fee recipient/share, two-step owner transfer, recipe registration (`register-recipe`), signer catalog scheduling (`schedule-catalog`), backup committers (`backup-committer`) and implementation upgrades (`upgrade-registry`, `upgrade-coordinator`). Every refused step prints the rule it broke. Sending additionally requires --apply and a signing wallet matching the current or pending owner, and is refused on mainnet, where the treasury Safe signs. For transaction-wallet rotation, drain/stop first, change the onchain committer, migrate the drained journal, then start the replacement. The VRF key stays unchanged.

## Registry upgrades and recipes

The recipe-registry upgrade, the coordinator's keeper-share payment change, recipe registration and catalog scheduling are in [the registry upgrade runbook](registry-upgrade.md): deploy both implementations, run `upgradeToAndCall` with `initializeRecipeRegistry` on the registry, then the coordinator upgrade and `scheduleCatalog` (one Safe batch on mainnet, the owner key on testnet), then restart keepers on the matching image with both implementation hash pins.

To move keeper earnings to the treasury without stopping the keeper, run `sh keeper.sh sweep --amount <USDC>` (or `--keep <USDC>` to send everything above that balance) on the host. The command only queues the request in the journal; the running keeper sends it to the coordinator's `feeRecipient()` (the treasury Safe on mainnet) on its own nonce lane once no game or epoch transaction is unresolved, journals the signed bytes before broadcast, and always leaves at least 1 USDC (or `MAX_TX_COST_WEI` when larger) plus gas. `sh keeper.sh sweep --status` shows the queued, in-flight and last result; `--cancel` removes a request that has not been signed. See deploy/docker/README.md. scripts/sweep-keeper.ts remains for a stopped keeper: it moves surplus to the chain profile owner, prints the plan by default and requires `--apply --keeper-stopped`, because any transaction from the keeper wallet that the keeper did not journal can take a nonce it needs. The protocol share is not in the keeper wallet: it accrues in the coordinator as `earnedFees` and the fee recipient withdraws it with `withdrawFees(recipient)` (a Safe transaction on mainnet).

scripts/pair-telegram.py performs one-time pairing using a protected token file and a fresh displayed code. It does not send chat messages or expose unrelated incoming messages. Only the matched chat ID is recorded for the keeper. Keep credentials and operator-specific paths outside Git.
