# D20 randomness keeper

A general randomness service on two networks, with one consumer interface (`requestRandomness`, `requestMappedRandomness`, `getMappedResult`, `D20VRFConsumer`) and a real secp256k1 VRF proof for every result:

- **Arc** (Arc Mainnet and Arc Testnet): epochs and drand. A drand beacon round is prepared locally for each 200-block epoch and published only when a live request needs it; randomness binds the published round to a future block and fixed request context. Requests escrow a fee in USDC quoted from the block base fee and callback gas limit (never below the configured minimum), and keepers may serve up to 16 of them in one transaction.
- **Robinhood Chain** (mainnet and testnet): round-bound drand, no epochs. Each request is bound to a future drand round when it is made, so nobody can know or choose the result in advance. The contract verifies the drand signature and the VRF proof on chain, and anyone can replay a result from public data. Fees are in ETH. See [the Robinhood coordinator](docs/robinhood.md).

The d20dao-keeper daemon (0.5.1, [changelog](keeper/CHANGELOG.md)) serves either kind of coordinator, shares one durable nonce lane across all its transactions, and receives a configured share of earned fees for operating costs. On Arc it behaves exactly as 0.4.1 did.

The service contracts use owner-authorized UUPS upgrades behind stable, atomically initialized D20Proxy addresses. Ownership transfer is two-step; keeper/fee-recipient roles are adjustable. Runtime checks pin the proxy implementations. Deployment networks live in chains.json.

- [Epoch protocol, evidence and trust boundaries (Arc)](docs/epoch-protocol.md)
- [Round coordinator, pricing and trust (Robinhood Chain)](docs/robinhood.md)
- [Keeper configuration and recovery](keeper/README.md)
- [Finalized state and recovery boundary](docs/finality.md)
- [Docker service scripts](deploy/docker/README.md)
- [Arc testnet setup and load testing](docs/arc-testnet.md)
- [Fees, deadlines and refunds](docs/service-rules.tr.md)
- [Outbound heartbeat receiver](docs/keeper-health-receiver.md)
- [Optional Telegram notifications and commands](keeper/TELEGRAM.md)
- [Optional public Discord proof feed](keeper/DISCORD.md)
- [Developer SDK](https://github.com/d20dao/d20-sdk) · [Agent skills](https://github.com/d20dao/skills)

## Epoch sources (Arc)

Each epoch selects one source from its catalog using its anchor block hash. A catalog lists 1 to 10 recipes with their signers; when the selected source yields no valid packet, the following slots take over one by one as a deterministic fallback, 20 blocks apart. Recipes live in an owner-managed, append-only registry and are never edited or removed. A recipe is one of two kinds:

- A beacon: a drand network, registered with `registerBeacon`. The registration fixes the network's group key and schedule and the verifier contract that checks its signatures. An epoch commits one round: its number, its scheduled time and its 64-byte BLS signature. Keepers read rounds from relays they do not trust, and the registered verifier checks every signature onchain.
- A signed API recipe: a canonical AirnodeHub request, a data template fixing the exact signed record, and the gateway body keepers once posted. These recipes served the epochs before the beacon: the epochs before Arc Testnet epoch 11319 and Arc Mainnet epoch 12448 were published from signed API records (recipes 0 to 10), and replay still verifies every one of them. The recipes stay registered and the registry still accepts them, but keeper 0.4.1 no longer fetches signed records: a catalog that selects one is refused by the keeper, which reports it and goes on with the epochs after it ([keeper/README.md](keeper/README.md)).

The registry registers six signed recipes itself. The beacon is registered by the owner, not built in:

| Recipe | Source | Record |
| --- | --- | --- |
| 0 | Hyperliquid | BTC daily notional volume from `metaAndAssetCtxs` |
| 1 | dRPC | Ethereum mainnet block hash: `eth_call` of Multicall3 `getLastBlockHash()` at `latest`, as the exact JSON-RPC envelope |
| 2 | TickerLayer | BTCUSD last trade: symbol, price, size, timestamp |
| 3 | TickerLayer | ETHUSD last trade: symbol, price, size, timestamp |
| 4 | Nodary | ETH/USD feed: value, millisecond timestamp, category |
| 5 | dRPC | Base block hash, as recipe 1 |
| next free id | [drand evmnet](https://docs.drand.love/developer/) | Round number in decimal, its scheduled time and the round's BLS signature |

The beacon takes the next free id: 11 on a registry that holds recipes 0 to 10. Both networks run the drand catalog, the beacon alone: `catalogAt` lists recipe 11 and nothing else from Arc Testnet epoch 11319 and Arc Mainnet epoch 12448 on, one slot with no fallback source. If drand is unavailable, no epoch is published, and requests wait and, after expiry, are refundable; recovery is a new catalog that the owner schedules at least two epochs ahead. Registries start with recipes 0 to 3 and the four Airnode signers that `config/service.json` lists beside the initial fee settings; the same file keeps the legacy signed rollout catalog 0, 1, 2, 4, 5 for the replay tests; no live network runs it any more and no `schedule-catalog` defaults to it. `schedule-catalog` needs `--recipes` on every network and refuses a recipe that is not a beacon, which keeper 0.4.1 and later cannot serve, unless `--allow-signed-recipes` is given. Recipes, data templates, catalogs and trust notes are in [the epoch protocol](docs/epoch-protocol.md) and [SECURITY.md](SECURITY.md); the upgrade of existing registries, beacon registration and recipe registration are in [the registry upgrade runbook](docs/registry-upgrade.md).

## Deployments

| Network | Chain | Coordinator proxy | Manifest |
| --- | --- | --- | --- |
| Arc Mainnet | 5042 | `0xd20da057469C45928912d983F45790C41e290571` | [arc-mainnet.json](deployments/arc-mainnet.json) |
| Arc Testnet | 5042002 | `0xd20DA0FF9087d053f0291524Eac12abA1ADBd945` | [arc-testnet.json](deployments/arc-testnet.json) |
| Robinhood Chain | 4663 | `0xEc8b95B168c87294c45727Bd2ac903d09316D132` | [robinhood-mainnet.json](deployments/robinhood-mainnet.json) |
| Robinhood Chain Testnet | 46630 | `0x2f26513DE4Ed388947f5d22FD395D5E06f472f05` | [robinhood-testnet.json](deployments/robinhood-testnet.json) |

The manifests record proxy addresses, code hashes, deployment receipts and implementation upgrades. Arc Mainnet is owned by the DAO treasury Safe. Read each Robinhood coordinator's owner from the chain (`owner()`); see [the Robinhood coordinator](docs/robinhood.md#addresses).

The Arc Testnet [stress run](docs/benchmarks/arc-testnet-stress-2026-09-16.json) was measured before the drand catalog, on signed API epochs: 68 paid requests (a 48-request burst and 20 spaced requests) were all fulfilled and delivered, 50 of them in batched fulfillments, with a median of 2 and a 95th percentile of 4 chain seconds. Fifteen requests fell in two epochs whose selected slot 1 source (the retired provider since replaced by dRPC) was rate limited upstream and were served from the fallback source; the longest wait, 15 seconds, includes that 20-block fallback window. The service is open to all paid consumer-contract requests; measured timings are not a delivery SLA.

## Run locally

Use Node 24 and Rust 1.96.1. No private environments or funded-wallet credentials are included.

```sh
npm ci
cargo build --manifest-path keeper/Cargo.toml --locked
npm run check
npm run demo
cargo fmt --manifest-path keeper/Cargo.toml -- --check
cargo clippy --manifest-path keeper/Cargo.toml --locked --all-targets -- -D warnings
cargo test --manifest-path keeper/Cargo.toml --locked
npx hardhat run scripts/keeper-integration.ts
npx hardhat run scripts/keeper-beacon-integration.ts
```

`npm run demo` runs the whole epoch flow on an isolated chain with real Rust proofs and local proxy/EVM acceptance and replay. Epoch 1 uses the initial catalog and is published from a signed CI fixture, which replay still verifies; the epochs after it come from a catalog of drand test networks whose rounds the real BLS verifier checks onchain. Traces are written under `.research/epoch-demo-*/trace.json`. `DEMO_ALL_SOURCES=true` exercises every network of the scheduled catalog. The mining example is one local consumer harness. `scripts/registry-beacon-fork.ts` rehearses the beacon upgrade of a live registry on a fork of its network, Safe batches included ([runbook](docs/registry-upgrade.md#beacon-upgrade)).

`npx hardhat run scripts/fleet-drill.ts` is a separate drill rather than part of the gates: it runs real release binaries as several keeper lanes against a local chain with 500 ms blocks and no time control, injects process, RPC and restart faults, and asserts refunds, duplicate attempts, takeover latency and per-wallet service from chain data (keeper/README.md, "Fleet drill").

`scripts/batch-abort-drill.ts` reproduces the batch-abort issue on a local chain — a later batch member reverting a batch after earlier results were revealed: one transaction opens a losing-biased raffle draw and two 1,000,000-gas saboteur requests, which a keeper batches, and it shows that reserving every member's full callback budget lands the batch and keeps the losing draw while the old sizing aborts it. `scripts/batch-abort-live.ts` (`npm run drill:batch-abort-live`) runs the same attack against a live deployment where the real keepers fulfil: it opens the draw and saboteurs together each round and records whether every losing draw is fulfilled and kept in one `fulfillRandomnessBatch` or is refunded. It defaults to a read-only plan that prints the addresses, balances and per-round cost read on chain; `--apply` sends and is bounded by a total spend cap. Arc mainnet is refused; a local chain (31337) is allowed for exercising the send path.

`scripts/keeper-integration.ts` runs the real daemon against a five-network catalog of drand test beacons, whose rounds a fake relay serves and the real BLS verifier checks, with local RPC and relay fault injection: first-packet persistence, background relay fetches, epoch publication, a lost acknowledgment, the fallback ladder and its exhaustion, a catalog switch without a restart, normal fulfillment, batching, nonce ownership, restart, rate limits, slow preflight, missed epochs, refunds, the operator sweep and the primary and follower keepers. `scripts/keeper-beacon-integration.ts` runs the daemon against fake drand relays: an initial catalog of signed API recipes that the keeper refuses (an error, a health fault, the published epoch still served, a removed setting only a warning) before the owner schedules the beacon catalog without a restart, a dropping and a lying relay beside a good one, every relay down, a packet that ages out before demand arrives, a relay that answers 404 for every round, a valid relay that always loses the race and one that holds requests for three seconds. Load scripts stay on local chain 31337; public testnet access is a separate bounded, funded pilot. Docker builds support amd64/arm64 with private persistent state and no inbound application ports.

The 60-second deadline is enforced onchain on both networks. On Arc, missing publication leaves an accepted request waiting; no proof can be prepared until its future target is available. After expiry, the unfulfilled fee remains refundable. Callback failure after accepted proof earns service fees and can retry only the same result. Passing local tests does not establish a public-network SLA, upgrade safety or replace external cryptographic review. Original provenance and older reviews describe development history; current code/tests define the product.
