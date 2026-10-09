# Registry and coordinator upgrades: recipe registry, backup committers, keeper-share payment and drand beacon

This runbook covers two upgrades of the live registries. The recipe-registry upgrade, with backup committers and the coordinator's keeper-share payment, is complete on both networks; the sections up to and including "Restart and record" describe it. The [beacon upgrade](#beacon-upgrade), which lets a registry serve epochs from a drand beacon, followed it and is complete on both networks too.

The recipe-registry upgrade moves the live registries to the implementation with the owner-managed recipe registry and backup committers, and the coordinators to the implementation that pays each request's keeper share to the authorized wallet that submitted its proof. Both networks completed it on 2026-09-17; each deployment manifest records the previous and current implementations under `implementationUpgrades`. The proxies are registry `0xd20Da048C1A68fa3Bc0B5f5Bc454D1530062C82D` and coordinator `0xd20da057469C45928912d983F45790C41e290571` on Arc Mainnet, and registry `0xD20Da00B47A7cD2211dC4683E306913b05903756` and coordinator `0xd20DA0FF9087d053f0291524Eac12abA1ADBd945` on Arc Testnet. Proxy addresses never change; only the implementation behind each one does. Every command prints its plan and sends nothing without `--apply`; mainnet owner commands only print Safe transactions.

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

Before any owner transaction, build or pull the keeper image of this revision. Setting each host's `keeper.env` `APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH` and `APPROVED_NEXT_IMPLEMENTATION_CODE_HASH` to the printed runtime hashes and recreating the keeper on the new image before the upgrade avoids a manual restart at execution time: the setting is read only at startup, a keeper started this way keeps sending on the current implementation, and the moment the upgrade executes and its proxy moves to the approved hash it exits at once with status 75 for its supervisor to start it again on the new code (keeper/README.md, "Configuration and identity"). Without that preparation, keepers on the previous image stop sending the moment the upgrade executes and need a manual restart on the new image with `EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH` and `EXPECTED_IMPLEMENTATION_CODE_HASH` set to the printed hashes; the service gap then lasts from execution until that restart, and requests that expire in the gap stay refundable.

The keeper release of that upgrade mapped each catalog signer to a gateway, with defaults for the four built-in Airnodes and `EPOCH_API_ENDPOINTS` for others. Keeper 0.4.1 and later read no gateway and no signed record: they prepare beacon epochs only, and a keeper environment that still sets `EPOCH_API_ENDPOINTS` only logs a warning that names it.

## 3. Owner transactions

### Arc Testnet (owner key)

```sh
node scripts/admin.ts upgrade-registry --manifest deployments/arc-testnet.json --implementation <registry implementation> --apply --env /secure/testnet-owner.env
node scripts/admin.ts upgrade-coordinator --manifest deployments/arc-testnet.json --implementation <coordinator implementation> --apply --env /secure/testnet-owner.env
node scripts/admin.ts schedule-catalog --manifest deployments/arc-testnet.json --recipes 0,1,2,4,5 --allow-signed-recipes --apply --env /secure/testnet-owner.env
```

Send the registry upgrade first: the coordinator's keeper-share path reads `isAuthorizedCommitter` on the registry, and until that view exists every share falls back to `committer()`. On testnet each call is its own transaction, so only the order matters; schedule the catalog after the upgrades, when it can simulate. `upgrade-coordinator` applies the same checks as `upgrade-registry` to the freshly compiled D20VRFCoordinator and prints the `EXPECTED_IMPLEMENTATION_CODE_HASH` pin.

`upgrade-registry` recompiles, requires the implementation's onchain runtime code to equal the freshly compiled EpochEntropy at that address, checks the current implementation against the manifest, encodes the `initializeRecipeRegistry` call, simulates the upgrade from the owner and prints the keeper pin and the manifest fields to update. `schedule-catalog` assumes no catalog: `--recipes` is required on every network (a testnet defaults only the first epoch, to the current epoch + 2). The rollout catalog `[0, 1, 2, 4, 5]` of this upgrade, which `config/service.json` still holds for the replay tests, is a signed catalog that keeper 0.4.1 and later do not serve, so scheduling it today takes `--allow-signed-recipes` (see [Scheduling a catalog](#scheduling-a-catalog)) and keepers on 0.4.0.

### Arc Mainnet (DAO treasury Safe)

Propose **two** Safe transactions, not one. Batch A carries the two upgrades and has no deadline. Batch B carries the calls that do: `scheduleCatalog` reverts with `InvalidEpoch` once its printed `executeBeforeBlock` passes, and putting it in one atomic MultiSend with the upgrades would make late signatures revert the upgrades too.

Batch A, executed whenever the signatures are in:

```sh
node scripts/admin.ts upgrade-registry --manifest deployments/arc-mainnet.json --implementation <registry implementation>
node scripts/admin.ts upgrade-coordinator --manifest deployments/arc-mainnet.json --implementation <coordinator implementation>
```

1. `upgradeToAndCall(<registry implementation>, initializeRecipeRegistry())` on the registry proxy `0xd20Da048C1A68fa3Bc0B5f5Bc454D1530062C82D`.
2. `upgradeToAndCall(<coordinator implementation>, 0x)` on the coordinator proxy `0xd20da057469C45928912d983F45790C41e290571`. It must follow the registry upgrade inside batch A, because its keeper-share path reads `isAuthorizedCommitter` on the registry.

Then handle the keeper restart (section 4) and check the upgraded views. Batch B follows, printed once the registry runs the new implementation, so both calls simulate:

```sh
node scripts/admin.ts schedule-catalog --manifest deployments/arc-mainnet.json --recipes 0,1,2,4,5 --allow-signed-recipes --from-epoch <epoch>
node scripts/admin.ts backup-committer --manifest deployments/arc-mainnet.json --address <follower wallet>
```

3. `scheduleCatalog([0,1,2,4,5], [Hyperliquid, dRPC, TickerLayer, Nodary, dRPC Airnodes], <fromEpoch>)`.
4. Optionally `setBackupCommitter(<follower wallet>, true)`.

Between A and B the service keeps working: epochs keep the initial catalog, recipes 0 to 3 with their existing signers, and its slot 1 yields no packet and falls back after 20 blocks exactly as it did before the rollout catalog. Batch B is not time-critical either, because nothing depends on it: if its `executeBeforeBlock` passes before the signatures are in, re-run `schedule-catalog` with a later `--from-epoch` and propose it again, as often as needed. Choose `--from-epoch` so that signing fits comfortably.

## 4. Restart and record

If the keeper was not recreated beforehand with `APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH` / `APPROVED_NEXT_IMPLEMENTATION_CODE_HASH` (section 2), restart it now with the prepared `keeper.env` (`sh keeper.sh update ghcr.io/d20dao/keeper@sha256:<digest>`); a keeper prepared that way has already exited on status 75 and restarted itself on the new code. Either way, move each hash from its `APPROVED_NEXT_*` setting into its `EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH` / `EXPECTED_IMPLEMENTATION_CODE_HASH` pin and remove the approval line. Check on chain that `recipeCount()` is 6, `getRecipe(0..5)` equals `test/fixtures/builtin-recipes.json`, `catalogAt(<fromEpoch>)` lists the rollout catalog and `isAuthorizedCommitter(committer())` is true. Then record `epochImplementation`, `coordinatorImplementation`, both code hashes and an `implementationUpgrades` entry per proxy in the deployment manifest, which `admin.ts` checks on its next run. The first fulfillment after the coordinator upgrade shows the new payment rule: its `KeeperFeePaid` names the submitting keeper wallet.

Until the rollout catalog's first epoch, epochs keep the initial catalog. Its slot 1 pairs recipe 1 with the retired provider's Airnode, so a slot 1 selection yields no packet and falls back after 20 blocks, as it did before the rollout catalog. Published epochs are unaffected: on 2026-09-17 mainnet had two published epochs, both from recipe 2, and testnet sixty from recipes 0, 2 and 3, so every epoch published so far replays with the built-in recipes.

## Beacon upgrade

This upgrade moves a registry that already has the recipe registry to the implementation that can serve epochs from a drand beacon, registers drand's evmnet and schedules a catalog that lists it. It follows the recipe-registry upgrade above. Arc Testnet completed it on 2026-09-30 and Arc Mainnet on 2026-10-01; each deployment manifest records the previous and current implementations under `implementationUpgrades`. Every command prints its plan and sends nothing without `--apply`; mainnet owner commands only print Safe transactions.

### What the beacon upgrade changes

- The registry proxy, the coordinator, every address and every ABI a consumer uses are unchanged. The registry gets a new implementation with `upgradeToAndCall(implementation, 0x)`: no initializer, and no coordinator upgrade.
- New registry functions and events: `registerBeacon`, `beaconOf`, `slotSigner`, `verifyBeacon`, `BEACON_VERIFY_GAS` and `BeaconRegistered` (see [the epoch protocol](epoch-protocol.md#beacon-recipes)). The signed-recipe code stays in the contract, unused by a beacon catalog: every published epoch replays as before. The owner could schedule a signed catalog again, but keeper 0.4.1 and later do not prepare epochs from signed recipes, so it would publish nothing; `schedule-catalog` refuses one unless `--allow-signed-recipes` is given (see [Scheduling a catalog](#scheduling-a-catalog)).
- Storage: slot 13, a former gap slot, holds the beacon registrations by recipe id. The gap shrinks from 35 to 34 slots and the layout still ends at slot 47; slots 0 to 12 keep their values. `test/Upgradeability.test.ts` upgrades a registry running the recorded live bytecode with no call data, compares every slot and view, then registers a beacon, serves a request from a beacon epoch and replays the earlier history.
- The verifier is a separate contract, D20BeaconVerifier: stateless, with no owner, no upgrade path and no constructor arguments, so one mined salt gives the same address on every network. The registry records its address in the beacon's registration, where it never changes.
- The beacon takes the next free recipe id, 11 on a registry that holds recipes 0 to 10, and the new catalog lists it alone: one source and no fallback. While drand cannot publish, requests wait and become refundable after expiry; recovery is another catalog, scheduled at least two epochs ahead.
- Every keeper, the primary and each follower, must run 0.4.0 or later before the beacon catalog is scheduled (`schedule-catalog`, batch B on mainnet). A 0.3.0 keeper refuses beacon recipes, so its epochs block and their requests are refunded. Rolling a keeper back to 0.3.0 after the switch needs a signed catalog scheduled first, at least two epochs ahead; 0.4.0 and later keep serving the beacon. The journal stays readable by 0.3.0. The `DRAND_RELAYS` setting is optional.

### Rehearse on a fork

Rehearse the whole upgrade before deploying or proposing anything. `scripts/registry-beacon-fork.ts` forks a live network with anvil (Foundry) and sends every transaction to the fork; the network sees public RPC reads and drand relay requests only.

```sh
node scripts/registry-beacon-fork.ts --chain arc-mainnet --local-deploy
```

`--local-deploy` deploys the verifier and the implementation on the fork with plain CREATE from the manifest's deployer, so the first run needs no mined salt; `--implementation-result` and `--verifier-result` take mined results and `--implementation` and `--verifier` deployed addresses. The script checks that the proxies run what the manifest says, fetches a sample round from a relay and runs batch A. Where the owner is the Safe, batch A is one MultiSendCallOnly delegatecall approved by the threshold owners with `approveHash` and executed with `execTransaction`; the script first shows that a batch with a tampered sample signature fails as a whole and leaves the implementation, the Safe nonce and the recipes untouched. It then compares every storage word and view before and after, checks that `beaconOf`, `slotSigner` and `verifyBeacon` answer as the replay library computes, runs batch B, mines to the first block of the catalog's epoch and serves a request from a real drand round: `commitEpoch`, fulfillment inside the 60-second window and a replay from public data. Every block carries an explicit timestamp that never passes the wall clock, so the rounds of the fork's blocks exist. Where the owner is an account (Arc Testnet), each call is its own transaction from the impersonated owner. `--multisend none` rehearses a Safe without MultiSendCallOnly. The JSON report on stdout lists every check that ran; a failed check is a stop.

### Deploy the verifier and the implementation

The deployer pays gas (about 1.9M for the verifier and 5.2M for the registry implementation); no owner authority is involved. Both bytecodes are identical for every network, so one mined salt each gives the same address on each.

```sh
node scripts/create2-deploy.ts beacon-verifier --chain arc-testnet --env /secure/deployer.env
python scripts/vanity/search.py deployments/private/arc-testnet/beacon-verifier-upgrade-search.json --seconds 30 --output /secure/beacon-verifier-result.json
node scripts/create2-deploy.ts beacon-verifier --chain arc-testnet --env /secure/deployer.env --beacon-verifier /secure/beacon-verifier-result.json --apply
node scripts/create2-deploy.ts beacon-verifier --chain arc-mainnet --mainnet --env /secure/deployer.env --beacon-verifier /secure/beacon-verifier-result.json --apply
```

```sh
node scripts/create2-deploy.ts epoch-implementation --chain arc-testnet --env /secure/deployer.env
python scripts/vanity/search.py deployments/private/arc-testnet/epoch-implementation-upgrade-search.json --seconds 30 --output /secure/epoch-implementation-result.json
node scripts/create2-deploy.ts epoch-implementation --chain arc-testnet --env /secure/deployer.env --epoch-implementation /secure/epoch-implementation-result.json --apply
node scripts/create2-deploy.ts epoch-implementation --chain arc-mainnet --mainnet --env /secure/deployer.env --epoch-implementation /secure/epoch-implementation-result.json --apply
```

The output names each address and its runtime code hash. The implementation's hash is the keeper pin (`EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH`); the verifier's goes into the manifest as `beaconVerifierCodeHash`. Run the rehearsal again on the deployed contracts before proposing:

```sh
node scripts/registry-beacon-fork.ts --chain arc-mainnet --implementation-result /secure/epoch-implementation-result.json --verifier-result /secure/beacon-verifier-result.json
```

### Prepare the keepers

Before the owner transactions, run the keeper release with beacon support (0.4.0 or later) on every keeper, followers included, and keep every keeper on it until a signed catalog is scheduled again, which needs keeper 0.4.0 or earlier (0.4.1 does not prepare signed epochs). Set `APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH` in each host's `keeper.env` to the new registry implementation's runtime hash, then recreate the keeper so it reads the setting (keeper/README.md, "Configuration and identity"). The release is safe before the upgrade: it asks `beaconOf` only for recipes whose canonical request names drand, and none exists yet. When batch A executes, the keeper sees the proxy move to the approved hash and exits with status 75 for its supervisor to start it again on the new implementation. Without the approval, keepers on the previous image stop sending the moment the upgrade executes and need a manual restart with `EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH` set; requests that expire in the gap stay refundable. The coordinator pin does not change, and followers get the same image and setting. Beacon epochs read rounds over outbound HTTPS from the relays in `DRAND_RELAYS` (four public drand relays by default). The relays are untrusted, so no key or pin is involved.

### Owner transactions

#### Arc Testnet (owner key)

```sh
node scripts/admin.ts upgrade-registry --manifest deployments/arc-testnet.json --implementation <registry implementation> --apply --env /secure/testnet-owner.env
node scripts/admin.ts register-beacon --manifest deployments/arc-testnet.json --verifier <beacon verifier> --apply --env /secure/testnet-owner.env
node scripts/admin.ts schedule-catalog --manifest deployments/arc-testnet.json --recipes 11 --apply --env /secure/testnet-owner.env
```

On testnet each call is its own transaction, so only the order matters. `upgrade-registry` sends `upgradeToAndCall(<implementation>, 0x)` (the registry already has the recipe registry, so there is no initializer), requires the implementation's onchain runtime code to equal the freshly compiled EpochEntropy at that address, simulates the upgrade from the owner and prints the keeper pin and the manifest fields. `register-beacon` requires the verifier's runtime code to equal the freshly compiled D20BeaconVerifier, takes a sample round from `--relay` (default `https://api.drand.sh`; `--sample-round` with `--sample-signature` give one instead), verifies it locally and through the verifier, refuses a beacon that is already registered and prints the registration, its `slotSigner` and the manifest fields to record (`beaconVerifier`, `beaconVerifierCodeHash`). It simulates the call only once the registry runs the new implementation, and `--verifier` defaults to `beaconVerifier` in the manifest. `schedule-catalog --recipes 11` reads the beacon's `slotSigner` from the registry, which accepts no other signer for it, and defaults to the current epoch + 2. It refuses a listed recipe without a beacon registration (see [Scheduling a catalog](#scheduling-a-catalog)).

#### Arc Mainnet (DAO treasury Safe)

Propose two Safe transactions. Batch A carries the upgrade and the registration and has no deadline. Batch B carries the catalog, which does: `scheduleCatalog` reverts with `InvalidEpoch` once its printed `executeBeforeBlock` passes, and inside one atomic MultiSend late signatures would revert the upgrade with it.

Batch A, executed whenever the signatures are in:

```sh
node scripts/admin.ts upgrade-registry --manifest deployments/arc-mainnet.json --implementation <registry implementation>
node scripts/admin.ts register-beacon --manifest deployments/arc-mainnet.json --verifier <beacon verifier>
```

1. `upgradeToAndCall(<registry implementation>, 0x)` on the registry proxy.
2. `registerBeacon(<verifier>, <chain hash>, <group key>, <genesis>, <period>, <sample round>, <sample signature>)` on the same proxy, after the upgrade in the same MultiSendCallOnly batch. It needs the new code, so before the upgrade `register-beacon` prints its self-check (verifier code, sample, no duplicate) and says that it did not simulate.

The batch is atomic: a registration that reverts also reverts the upgrade, which is why the sample and the verifier are checked before the proposal. A sample round is always in the past, so signing delays cannot invalidate it.

Then restart the keepers (below) and check the upgraded views. Batch B follows, printed once the registry runs the new implementation and knows the beacon, so it simulates:

```sh
node scripts/admin.ts schedule-catalog --manifest deployments/arc-mainnet.json --recipes 11 --from-epoch <epoch>
```

3. `scheduleCatalog([11], [<slotSigner>], <fromEpoch>)`.

Between A and B the service keeps its catalog. Batch B is not time-critical: if its `executeBeforeBlock` passes before the signatures are in, run `schedule-catalog` again with a later `--from-epoch` and propose it again. From `<fromEpoch>` every epoch is served by the beacon alone, so every keeper that can publish, the primary and each follower, must already run the release with beacon support (0.4.0 or later), with a relay reachable, when batch B is proposed: an earlier release records the beacon source as failed and, with one source, publishes nothing.

### Restart, check and record

If the keepers were not recreated beforehand with `APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH`, restart them now on the image with beacon support and `EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH` set to the printed hash (`sh keeper.sh update ghcr.io/d20dao/keeper@sha256:<digest>`); a keeper prepared that way has already exited on status 75 and restarted itself. Either way, move the hash from `APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH` into `EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH` and remove the approval line.

Check on chain that `recipeCount()` has grown by one; that `beaconOf(<id>)` is the verifier and the evmnet registration that `register-beacon` printed, with the chain hash, group key, genesis and period compared against drand's published chain information; that `slotSigner(<id>)` is the printed signer; that `verifyBeacon(<id>, <round>, <signature>)` accepts a current round from a relay; that `getRecipe(0..10)` are unchanged; and, once batch B executed, that `catalogAt(<fromEpoch>)` lists the beacon alone and `sourceCountAt(<fromEpoch>)` is 1. Then record `epochImplementation`, `epochImplementationCodeHash`, `beaconVerifier`, `beaconVerifierCodeHash` and an `implementationUpgrades` entry for the registry in the deployment manifest, which `admin.ts` checks on its next run. The first beacon epoch that a paid request publishes is the first test of keepers, relays and verifier together: its `EpochCommitted` packet is 448 bytes, and its round replays from public data.

### Scheduling a catalog

`schedule-catalog` assumes no catalog. `--recipes` is required on every network and lists the recipe ids in slot order; a production network needs `--from-epoch` too, and a testnet defaults it to the current epoch + 2. It refuses any listed recipe that has no beacon registration, naming it, because keeper 0.4.1 and later do not serve signed API recipes: a catalog that lists one leaves every epoch that selects it unpublished, and the requests that wait on it refundable after expiry, until another catalog takes effect, two epochs after it is scheduled at the earliest. A mistyped id is enough (`--recipes 1` for `--recipes 11`). `config/service.json` still holds the legacy signed rollout catalog `[0, 1, 2, 4, 5]` for the replay tests; nothing defaults to it.

`--allow-signed-recipes` lifts that refusal, for an emergency such as the rollback below. **Warning:** every keeper, the primary and each follower, must then run 0.4.0, the last release that prepares signed epochs, before the catalog takes effect. A 0.4.1 keeper refuses the signed recipes, logs `signed API recipes are not supported since 0.4.1`, reports the health fault `epoch_recipe_unsupported` and publishes nothing for the epochs that select them. The plan lists such recipes under `signedRecipes` and repeats the warning in its note.

### Rollback

The beacon implementation can be replaced by the previous one (`de5f82e` on both networks until their upgrades; the manifest's `implementationUpgrades` entry for this upgrade records its address and runtime hash as `previousImplementation` and `previousImplementationCodeHash`). The order matters, because the previous code cannot publish a beacon epoch: it reads the beacon recipe as a signed one whose signer, the beacon's `slotSigner`, is an identity that no key holds, so `commitEpoch` reverts and every request of that epoch waits for its refund.

1. Schedule a catalog without the beacon: `schedule-catalog --recipes <ids> --signers <their signers> --allow-signed-recipes --from-epoch <epoch>`, on mainnet a Safe transaction, with an epoch far enough ahead for its signatures. A catalog of signed recipes is served by keeper 0.4.0 or earlier only, so the keepers go back to such a release first (see [Scheduling a catalog](#scheduling-a-catalog)). A pending catalog, one that takes effect two or more epochs ahead, is replaced by it. The catalog that takes effect at the next epoch is not: if that is the beacon's, the beacon still serves that epoch and the rollback catalog starts after it.
2. Wait until that epoch has started, so that `catalogAt(<current epoch>)` is the catalog without the beacon, and until the requests of the last beacon epoch are served or past their 60-second deadline.
3. Then downgrade with `upgradeToAndCall(<previous implementation>, 0x)` on the registry proxy, proposed in the Safe. `admin.ts upgrade-registry` cannot print it: it accepts only the freshly compiled implementation, which is the one being rolled away from. Set `APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH` on each keeper to the previous implementation's runtime hash beforehand, as in "Prepare the keepers", and move it into `EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH` afterwards.

A downgrade before batch B is harmless. While no catalog lists the beacon, no epoch depends on it: the beacon stays registered as an ordinary recipe id that no catalog uses, `beaconOf`, `slotSigner` and `verifyBeacon` stop answering, storage slot 13 keeps the registration, and upgrading again finds it.

Beacon epochs published before a rollback stay verifiable. The previous implementation keeps their records, the catalogs and every recipe, but not `beaconOf`, so `readEpochRecipes` at the latest block fails for the beacon recipe. Read the recipes as of a block that the beacon implementation served, for example the epoch's commit block: `readEpochRecipes(provider, registry, ids, {blockTag})`, through an RPC that keeps the state of past blocks. The registration is also in the `BeaconRegistered` event. `test/EpochBeacon.test.ts` rolls a registry back to `de5f82e` in this order and replays a beacon epoch that way.

## Follower keeper and failover drill

A follower keeper is a second keeper, on another host, that serves while the primary is down (keeper/README.md, "Primary and follower keepers"). Acceptance is a live drill on Arc Testnet; other chains follow the same steps later.

1. Create the follower's transaction wallet on the follower host and fund it for gas; with the upgraded coordinator it earns the keeper share of the requests it serves, which accrues to that wallet and not to `committer()`. If external monitoring classifies submitters, add its address there before the follower starts sending, so its fulfillments are not reported as a foreign submitter's.
2. Allow it: `node scripts/admin.ts backup-committer --manifest deployments/arc-testnet.json --address <follower wallet> --apply --env /secure/testnet-owner.env` (on mainnet, the printed Safe transaction).
3. Configure the follower's `keeper.env` like the primary's (same pins, RPCs and coordinator) with `KEEPER_ROLE=follower`, the defaults `FOLLOWER_DELAY_SECONDS=20`, `FOLLOWER_QUEUE_JOIN=150` and `PRIMARY_LIVENESS_SECONDS=10`, its own `TX_KEY_FILE` and journal, and the same `VRF_KEY_FILE`; `node scripts/keeper-env.ts --chain arc-testnet --role follower ...` writes exactly that. A follower host runs each follower as a named Docker instance, with images loaded or pulled by digest rather than built there (deploy/docker/README.md, "Deploying a follower instance"), and should use different RPC endpoints from the primary's host. Keep the network's Telegram bot token and chat; `TELEGRAM_COMMANDS` defaults to false for a follower. Start it with sending enabled; until its wallet is an allowed backup committer it runs without sending and reports `wallet_unauthorized`.
4. Drill: run the local fleet drill first (`npx hardhat run scripts/fleet-drill.ts`, see keeper/README.md), then repeat the same scenarios on the network. With both running under light demand, confirm the follower sends nothing and that its `/status` says the primary is alive; under a burst deeper than `FOLLOWER_QUEUE_JOIN`, or one the primary leaves older than `FOLLOWER_DELAY_SECONDS`, the follower is expected to join and work the newest end. Stop the primary, make paid requests and confirm the follower publishes and serves within the liveness window (`Follower served request N` in its log and Telegram). Restart the primary, let the queue drain and confirm the follower goes quiet again. `node scripts/failover-report.ts --manifest deployments/arc-testnet.json --from-block <n> --to-block <m>` reports from chain data which wallet published each epoch and fulfilled each request, refunds, reverted keeper transactions, gas per wallet and duplicate attempts: transactions whose whole work was already settled by an earlier one, which is the contention the join rule is there to avoid.

Removing the follower is `backup-committer --remove`. At its next authorization check the follower stops sending and reports `wallet_unauthorized`, while it keeps running and reconciling anything it had already signed, so no nonce is left in flight; stop the process once its journal has no unresolved transaction.

## Registering a recipe later

A signed listing is a recipe file: the gateway body, a readable template, the Airnode and one signed gateway response for that body. `config/recipes/` holds the Hyperliquid SOL mid and Nodary BTC/USD listings as examples, and `config/recipes/passthrough-*.json` the passthrough (`/api`) form of the rollout catalog's recipes 0, 1, 2, 4 and 5 (see [the epoch protocol](epoch-protocol.md)); their samples were collected from the live gateways while they served epochs and no tool refreshes them. Keeper 0.4.1 and later do not prepare epochs from signed recipes, so registering one makes no source a keeper can serve. A drand beacon is not a recipe file: it is registered with `register-beacon` together with its verifier (see [Beacon upgrade](#beacon-upgrade)).

```sh
node scripts/admin.ts register-recipe --manifest deployments/arc-testnet.json --file config/recipes/nodary-btc-usd.json --apply --env /secure/testnet-owner.env
```

`register-recipe` refuses to print anything unless the sample's request hash equals the body's canonical request hash, its canonical low-s signature recovers to the Airnode, its signed bytes match the template and the recipe fits the registry bounds, and it names the rule that failed otherwise. It also refuses a recipe already registered with the same definition and prints the id the recipe will receive. Recipes cannot be edited or removed; a catalog decides whether one is used.

## Development rules for live upgrades

Since the Arc Mainnet release every protocol change is an in-place upgrade of the live proxies:

- Storage stays compatible with every deployed layout. New state takes slots from the gap, retired slots stay declared, and `storage-layout/` records each reviewed layout.
- New state is initialized by a reinitializer that runs inside `upgradeToAndCall` and cannot run again or harm a registry initialized by the new code.
- Requests pending at upgrade time are still served, and history published by the previous code still replays.
- The consumer ABI (the coordinator and `D20VRFConsumer`) does not break. Registry and keeper interfaces change together with a keeper release pinned to the new implementation.
- Upgrade tests start from bytecode recorded from the live networks (`test/fixtures`), not from rebuilt sources, and serve a request through the live coordinator code.
