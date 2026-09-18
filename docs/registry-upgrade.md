# Registry and coordinator upgrade: recipe registry, backup committers and keeper-share payment

This runbook moves the live registries to the implementation with the owner-managed recipe registry and backup committers, and the coordinators to the implementation that pays each request's keeper share to the authorized wallet that submitted its proof. Both networks completed it on 2026-09-17; each deployment manifest records the previous and current implementations under `implementationUpgrades`. The proxies are registry `0xd20Da048C1A68fa3Bc0B5f5Bc454D1530062C82D` and coordinator `0xd20da057469C45928912d983F45790C41e290571` on Arc Mainnet, and registry `0xD20Da00B47A7cD2211dC4683E306913b05903756` and coordinator `0xd20DA0FF9087d053f0291524Eac12abA1ADBd945` on Arc Testnet. Proxy addresses never change; only the implementation behind each one does. Every command prints its plan and sends nothing without `--apply`; mainnet owner commands only print Safe transactions.

## What changes

- The six built-in recipes are registered in the upgrade transaction itself: `upgradeToAndCall(implementation, initializeRecipeRegistry())`. Ids 0 to 3 keep their canonical requests, so the initial catalog, `catalogHash()`, `protocolConfigurationHash` and every committed epoch are unchanged.
- Backup committers: the owner can allow up to four wallets besides `committer()` to publish epochs under the same rules (`setBackupCommitter(account, allowed)`). A follower keeper on another server uses this to take over publication while the primary is down.
- Keeper share: the upgraded coordinator pays each request's keeper share to the wallet that submitted its accepted proof when the registry's new `isAuthorizedCommitter(account)` view says that wallet may publish epochs, and to `committer()` otherwise, so a follower earns what it serves. Submission stays permissionless, and requests, refunds, retries, escrow and the treasury share are unchanged. The coordinator wraps the view in try/catch and falls back to `committer()`, so a registry that has not been upgraded yet keeps today's behaviour instead of failing.
- Storage: slot 10 becomes the recipe array, slot 11 the backup committer mapping and slot 12 its count; the gap shrinks from 38 to 35 slots and the layout still ends at slot 47. Slots 0 to 9 keep their values on both layouts. `test/Upgradeability.test.ts` upgrades proxies running the recorded live bytecode of both implementations and checks the raw storage, the views, legacy replay and a request served through the recorded live coordinator.
- The keeper reads recipes from the registry. It needs the new image, and its `EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH` pin must name the new implementation.
- `initializeRecipeRegistry` runs only inside this upgrade: once per proxy (reinitializer 2), owner-only, and it refuses a registry with a non-empty slot 8 or a scheduled catalog. An upgrade sent without it leaves the registry unable to select sources until the owner calls `initializeRecipeRegistry()` in a separate transaction; requests still escrow meanwhile. Both live registries have slot 8 and slot 9 empty; `upgrade-registry` checks this before printing.

## 1. Deploy the implementations

The deployer pays gas (about 4.3M for the registry and 5.0M for the coordinator); no owner authority is involved. Each implementation's bytecode is identical for both networks, so one mined salt gives the same address on each.

```sh
node scripts/create2-deploy.ts epoch-implementation --chain arc-testnet --env /secure/deployer.env
python scripts/vanity/search.py deployments/private/arc-testnet/epoch-implementation-upgrade-search.json --seconds 30 --output /secure/epoch-registry-result.json
node scripts/create2-deploy.ts epoch-implementation --chain arc-testnet --env /secure/deployer.env --epoch-implementation /secure/epoch-registry-result.json --apply
node scripts/create2-deploy.ts epoch-implementation --chain arc-mainnet --mainnet --env /secure/deployer.env --epoch-implementation /secure/epoch-registry-result.json --apply
```

```sh
node scripts/create2-deploy.ts coordinator-implementation --chain arc-testnet --env /secure/deployer.env
python scripts/vanity/search.py deployments/private/arc-testnet/coordinator-implementation-upgrade-search.json --seconds 30 --output /secure/coordinator-result.json
node scripts/create2-deploy.ts coordinator-implementation --chain arc-testnet --env /secure/deployer.env --coordinator-implementation /secure/coordinator-result.json --apply
node scripts/create2-deploy.ts coordinator-implementation --chain arc-mainnet --mainnet --env /secure/deployer.env --coordinator-implementation /secure/coordinator-result.json --apply
```

The output names each implementation address and its runtime code hash, which is the keeper pin (`EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH` for the registry, `EXPECTED_IMPLEMENTATION_CODE_HASH` for the coordinator).

## 2. Prepare the keeper

Before any owner transaction, build or pull the keeper image of this revision and prepare each host's `keeper.env` with `EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH` and `EXPECTED_IMPLEMENTATION_CODE_HASH` set to the printed runtime hashes, without restarting. Keepers on the previous image stop sending the moment the upgrade executes, and the new image refuses to start until it has, so the service gap lasts from execution until the restart. Requests that expire in that gap stay refundable.

The keeper maps each catalog signer to a gateway. The four built-in Airnodes have defaults; `EPOCH_API_ENDPOINTS` takes `AIRNODE_ADDRESS=URL` pairs (the previous `provider=URL` form is no longer accepted). A signer without a gateway makes its sources fail and fall back.

## 3. Owner transactions

### Arc Testnet (owner key)

```sh
node scripts/admin.ts upgrade-registry --manifest deployments/arc-testnet.json --implementation <registry implementation> --apply --env /secure/testnet-owner.env
node scripts/admin.ts upgrade-coordinator --manifest deployments/arc-testnet.json --implementation <coordinator implementation> --apply --env /secure/testnet-owner.env
node scripts/admin.ts schedule-catalog --manifest deployments/arc-testnet.json --apply --env /secure/testnet-owner.env
```

Send the registry upgrade first: the coordinator's keeper-share path reads `isAuthorizedCommitter` on the registry, and until that view exists every share falls back to `committer()`. On testnet each call is its own transaction, so only the order matters; schedule the catalog after the upgrades, when it can simulate. `upgrade-coordinator` applies the same checks as `upgrade-registry` to the freshly compiled D20VRFCoordinator and prints the `EXPECTED_IMPLEMENTATION_CODE_HASH` pin.

`upgrade-registry` recompiles, requires the implementation's onchain runtime code to equal the freshly compiled EpochEntropy at that address, checks the current implementation against the manifest, encodes the `initializeRecipeRegistry` call, simulates the upgrade from the owner and prints the keeper pin and the manifest fields to update. `schedule-catalog` defaults to the rollout catalog `[0, 1, 2, 4, 5]` from `config/service.json` with each provider's Airnode and to the current epoch + 2.

### Arc Mainnet (DAO treasury Safe)

Propose **two** Safe transactions, not one. Batch A carries the two upgrades and has no deadline. Batch B carries the calls that do: `scheduleCatalog` reverts with `InvalidEpoch` once its printed `executeBeforeBlock` passes, and putting it in one atomic MultiSend with the upgrades would make late signatures revert the upgrades too.

Batch A, executed whenever the signatures are in:

```sh
node scripts/admin.ts upgrade-registry --manifest deployments/arc-mainnet.json --implementation <registry implementation>
node scripts/admin.ts upgrade-coordinator --manifest deployments/arc-mainnet.json --implementation <coordinator implementation>
```

1. `upgradeToAndCall(<registry implementation>, initializeRecipeRegistry())` on the registry proxy `0xd20Da048C1A68fa3Bc0B5f5Bc454D1530062C82D`.
2. `upgradeToAndCall(<coordinator implementation>, 0x)` on the coordinator proxy `0xd20da057469C45928912d983F45790C41e290571`. It must follow the registry upgrade inside batch A, because its keeper-share path reads `isAuthorizedCommitter` on the registry.

Then restart the keepers (section 4) and check the upgraded views. Batch B follows, printed once the registry runs the new implementation, so both calls simulate:

```sh
node scripts/admin.ts schedule-catalog --manifest deployments/arc-mainnet.json --from-epoch <epoch>
node scripts/admin.ts backup-committer --manifest deployments/arc-mainnet.json --address <follower wallet>
```

3. `scheduleCatalog([0,1,2,4,5], [Hyperliquid, dRPC, TickerLayer, Nodary, dRPC Airnodes], <fromEpoch>)`.
4. Optionally `setBackupCommitter(<follower wallet>, true)`.

Between A and B the service keeps working: epochs keep the initial catalog, recipes 0 to 3 with their existing signers, and its slot 1 yields no packet and falls back after 20 blocks exactly as it did before the rollout catalog. Batch B is not time-critical either, because nothing depends on it: if its `executeBeforeBlock` passes before the signatures are in, re-run `schedule-catalog` with a later `--from-epoch` and propose it again, as often as needed. Choose `--from-epoch` so that signing fits comfortably.

## 4. Restart and record

Restart each keeper with the prepared `keeper.env` (`sh keeper.sh update ghcr.io/d20dao/keeper@sha256:<digest>`). Check on chain that `recipeCount()` is 6, `getRecipe(0..5)` equals `test/fixtures/builtin-recipes.json`, `catalogAt(<fromEpoch>)` lists the rollout catalog and `isAuthorizedCommitter(committer())` is true. Then record `epochImplementation`, `coordinatorImplementation`, both code hashes and an `implementationUpgrades` entry per proxy in the deployment manifest, which `admin.ts` checks on its next run. The first fulfillment after the coordinator upgrade shows the new payment rule: its `KeeperFeePaid` names the submitting keeper wallet.

Until the rollout catalog's first epoch, epochs keep the initial catalog. Its slot 1 pairs recipe 1 with the retired provider's Airnode, so a slot 1 selection yields no packet and falls back after 20 blocks, as it did before the rollout catalog. Published epochs are unaffected: on 2026-09-17 mainnet had two published epochs, both from recipe 2, and testnet sixty from recipes 0, 2 and 3, so every epoch published so far replays with the built-in recipes.

## Follower keeper and failover drill

A follower keeper is a second keeper, on another host, that serves while the primary is down (keeper/README.md, "Primary and follower keepers"). Acceptance is a live drill on Arc Testnet; other chains follow the same steps later.

1. Create the follower's transaction wallet on the follower host and fund it for gas; with the upgraded coordinator it earns the keeper share of the requests it serves, which accrues to that wallet and not to `committer()`. If external monitoring classifies submitters, add its address there before the follower starts sending, so its fulfillments are not reported as a foreign submitter's.
2. Allow it: `node scripts/admin.ts backup-committer --manifest deployments/arc-testnet.json --address <follower wallet> --apply --env /secure/testnet-owner.env` (on mainnet, the printed Safe transaction).
3. Configure the follower's `keeper.env` like the primary's (same pins, RPCs and coordinator) with `KEEPER_ROLE=follower`, the defaults `FOLLOWER_DELAY_SECONDS=20`, `FOLLOWER_QUEUE_JOIN=150` and `PRIMARY_LIVENESS_SECONDS=10`, its own `TX_KEY_FILE` and journal, and the same `VRF_KEY_FILE`; `node scripts/keeper-env.ts --chain arc-testnet --role follower ...` writes exactly that. A follower host runs each follower as a named Docker instance, with images loaded or pulled by digest rather than built there (deploy/docker/README.md, "Deploying a follower instance"), and should use different RPC endpoints from the primary's host. Keep the network's Telegram bot token and chat; `TELEGRAM_COMMANDS` defaults to false for a follower. Start it with sending enabled; until its wallet is an allowed backup committer it runs without sending and reports `wallet_unauthorized`.
4. Drill: run the local fleet drill first (`npx hardhat run scripts/fleet-drill.ts`, see keeper/README.md), then repeat the same scenarios on the network. With both running under light demand, confirm the follower sends nothing and that its `/status` says the primary is alive; under a burst deeper than `FOLLOWER_QUEUE_JOIN`, or one the primary leaves older than `FOLLOWER_DELAY_SECONDS`, the follower is expected to join and work the newest end. Stop the primary, make paid requests and confirm the follower publishes and serves within the liveness window (`Follower served request N` in its log and Telegram). Restart the primary, let the queue drain and confirm the follower goes quiet again. `node scripts/failover-report.ts --manifest deployments/arc-testnet.json --from-block <n> --to-block <m>` reports from chain data which wallet published each epoch and fulfilled each request, refunds, reverted keeper transactions, gas per wallet and duplicate attempts: transactions whose whole work was already settled by an earlier one, which is the contention the join rule is there to avoid.

Removing the follower is `backup-committer --remove`. At its next authorization check the follower stops sending and reports `wallet_unauthorized`, while it keeps running and reconciling anything it had already signed, so no nonce is left in flight; stop the process once its journal has no unresolved transaction.

## Registering a recipe later

A new listing is a recipe file: the gateway body, a readable template, the Airnode and one signed gateway response for that body. `config/recipes/` holds the Hyperliquid SOL mid and Nodary BTC/USD listings as examples.

```sh
node scripts/admin.ts register-recipe --manifest deployments/arc-testnet.json --file config/recipes/nodary-btc-usd.json --apply --env /secure/testnet-owner.env
node scripts/admin.ts schedule-catalog --manifest deployments/arc-testnet.json --recipes 0,1,2,4,5,6 --signers 0x509F4275Cbe2E2201cc5444bAc8948E3cc7c665B,0x511AcE8648D2f64260d50D036F8f8ce622d92137,0x32f5eA20F05fdADfCD50Cb8eD920acE96D5f9f2c,0xE70f1e8b22a21e4Bb5188918a3033341b281E4c0,0x511AcE8648D2f64260d50D036F8f8ce622d92137,0xE70f1e8b22a21e4Bb5188918a3033341b281E4c0 --apply --env /secure/testnet-owner.env
```

`register-recipe` refuses to print anything unless the sample's request hash equals the body's canonical request hash, its canonical low-s signature recovers to the Airnode, its signed bytes match the template and the recipe fits the registry bounds, and it names the rule that failed otherwise. It also refuses a recipe already registered with the same definition and prints the id the recipe will receive. Recipes cannot be edited or removed; a catalog decides whether one is used. Keepers need no update for a new recipe, only a gateway for its Airnode.

## Development rules for live upgrades

Since the Arc Mainnet release every protocol change is an in-place upgrade of the live proxies:

- Storage stays compatible with every deployed layout. New state takes slots from the gap, retired slots stay declared, and `storage-layout/` records each reviewed layout.
- New state is initialized by a reinitializer that runs inside `upgradeToAndCall` and cannot run again or harm a registry initialized by the new code.
- Requests pending at upgrade time are still served, and history published by the previous code still replays.
- The consumer ABI (the coordinator and `D20VRFConsumer`) does not break. Registry and keeper interfaces change together with a keeper release pinned to the new implementation.
- Upgrade tests start from bytecode recorded from the live networks (`test/fixtures`), not from rebuilt sources, and serve a request through the live coordinator code.
