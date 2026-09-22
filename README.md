# D20 randomness keeper

A general randomness service: API3-signed snapshots are prepared locally for 200-block epochs and published only when a live request needs them. Requests escrow a fee quoted from the block base fee and callback gas limit (never below the configured minimum), and keepers may serve up to 16 of them in one transaction; randomness binds the published packet to a future block and fixed request context with a real VRF proof. The d20dao-keeper daemon shares one durable nonce lane across publication and fulfillment, and receives a configured share of earned fees for operating costs.

Both service contracts use owner-authorized UUPS upgrades behind stable, atomically initialized D20Proxy addresses. Ownership transfer is two-step; keeper/fee-recipient roles are adjustable. Runtime checks pin both proxy implementations. Deployment networks live in chains.json: Arc Mainnet and Arc Testnet.

- [Epoch protocol, evidence and trust boundaries](docs/epoch-protocol.md)
- [Keeper configuration and recovery](keeper/README.md)
- [Finalized state and recovery boundary](docs/finality.md)
- [Docker service scripts](deploy/docker/README.md)
- [Arc testnet setup and load testing](docs/arc-testnet.md)
- [Fees, deadlines and refunds](docs/service-rules.tr.md)
- [Outbound heartbeat receiver](docs/keeper-health-receiver.md)
- [Optional Telegram notifications and commands](keeper/TELEGRAM.md)
- [Optional public Discord proof feed](keeper/DISCORD.md)
- [Developer SDK](https://github.com/d20dao/d20-sdk) · [Agent skills](https://github.com/d20dao/skills)

## Epoch sources

Each epoch selects one source from its catalog using its anchor block hash; if that source yields no valid packet, the following slots take over one by one as a deterministic fallback, 20 blocks apart. A catalog lists 1 to 10 recipes with their signers. Recipes live in an owner-managed, append-only registry: each is a canonical AirnodeHub request, a data template fixing the exact signed record, and the gateway body keepers post. Registered recipes are never edited or removed. The registry registers six built-in recipes itself:

| Recipe | Provider | Signed record |
| --- | --- | --- |
| 0 | Hyperliquid | BTC daily notional volume from `metaAndAssetCtxs` |
| 1 | dRPC | Ethereum mainnet block hash: `eth_call` of Multicall3 `getLastBlockHash()` at `latest`, as the exact JSON-RPC envelope |
| 2 | TickerLayer | BTCUSD last trade: symbol, price, size, timestamp |
| 3 | TickerLayer | ETHUSD last trade: symbol, price, size, timestamp |
| 4 | Nodary | ETH/USD feed: value, millisecond timestamp, category |
| 5 | dRPC | Base block hash, as recipe 1 |

Registries start with recipes 0 to 3; the rollout catalog is 0, 1, 2, 4, 5, so neighbouring slots never share a provider. Recipes, data templates, catalogs and trust notes are in [the epoch protocol](docs/epoch-protocol.md) and [SECURITY.md](SECURITY.md); the upgrade of existing registries and recipe registration are in [the registry upgrade runbook](docs/registry-upgrade.md).

## Deployments

The service is live on Arc Mainnet: coordinator proxy `0xd20da057469C45928912d983F45790C41e290571` on chain `5042`, owned by the DAO treasury Safe (see the [mainnet manifest](deployments/arc-mainnet.json)). The Arc Testnet coordinator proxy is `0xd20DA0FF9087d053f0291524Eac12abA1ADBd945` on chain `5042002`. See the [public deployment manifest](deployments/arc-testnet.json) and the [stress run](docs/benchmarks/arc-testnet-stress-2026-09-16.json): 68 paid requests (a 48-request burst and 20 spaced requests) were all fulfilled and delivered, 50 of them in batched fulfillments, with a median of 2 and a 95th percentile of 4 chain seconds. Fifteen requests fell in two epochs whose selected slot 1 source (the retired provider since replaced by dRPC) was rate limited upstream and were served from the fallback source; the longest wait, 15 seconds, includes that 20-block fallback window. The service is open to all paid consumer-contract requests; measured timings are not a delivery SLA.

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
```

`npm run demo` uses explicitly signed test API fixtures, real Rust proofs and local proxy/EVM acceptance/replay. Set `DEMO_FIXTURE=false` explicitly for live unpaid API3 responses; production never falls back to fixtures. Traces are written under `.research/epoch-demo-*/trace.json`. `DEMO_ALL_SOURCES=true` exercises every recipe of the scheduled catalog. The mining example is one local consumer harness.

`npx hardhat run scripts/fleet-drill.ts` is a separate drill rather than part of the gates: it runs real release binaries as several keeper lanes against a local chain with 500 ms blocks and no time control, injects process, RPC and restart faults, and asserts refunds, duplicate attempts, takeover latency and per-wallet service from chain data (keeper/README.md, "Fleet drill").

`scripts/batch-abort-drill.ts` reproduces the batch-abort issue on a local chain — a later batch member reverting a batch after earlier results were revealed: one transaction opens a losing-biased raffle draw and two 1,000,000-gas saboteur requests, which a keeper batches, and it shows that reserving every member's full callback budget lands the batch and keeps the losing draw while the old sizing aborts it. `scripts/batch-abort-live.ts` (`npm run drill:batch-abort-live`) runs the same attack against a live deployment where the real keepers fulfil: it opens the draw and saboteurs together each round and records whether every losing draw is fulfilled and kept in one `fulfillRandomnessBatch` or is refunded. It defaults to a read-only plan that prints the addresses, balances and per-round cost read on chain; `--apply` sends and is bounded by a total spend cap. Arc mainnet is refused; a local chain (31337) is allowed for exercising the send path.

The integration suite runs the real daemon with local RPC/API fault injection: first-packet persistence, background fetches of registered recipe bodies, epoch publication, the fallback ladder and the provider circuit breaker, normal fulfillment, nonce ownership, restart, rate limits, slow preflight, missed epoch and refunds. Load scripts stay on local chain 31337; public testnet access is a separate bounded, funded pilot. Docker builds support amd64/arm64 with private persistent state and no inbound application ports.

The 60-second deadline is enforced onchain. Missing publication leaves an accepted request waiting; no proof can be prepared until its future target is available. After expiry, the unfulfilled fee remains refundable. Callback failure after accepted proof earns service fees and can retry only the same result. Passing local tests does not establish a public-network SLA, upgrade safety or replace external cryptographic review. Original provenance and older reviews describe development history; current code/tests define the product.
