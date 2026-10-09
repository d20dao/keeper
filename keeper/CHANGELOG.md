# Changelog

## 0.5.2

The public Discord proof feed (`DISCORD_BOT_TOKEN`, `DISCORD_PROTOCOL_CHANNEL_ID`, `EXPLORER_URL`, `DISCORD_PUBLIC_EXPLORER_URL`) also posts a round keeper's accepted fulfillments, one message per request with its drand round. Epoch mode (Arc) is unchanged.

## 0.5.1

Documentation and profile notes; no behaviour change.

## 0.5.0

Adds Robinhood Chain. **On Arc, an epoch coordinator's keeper behaves exactly as 0.4.1 did:**
- it asks the chain for the same calls in the same order, which the golden RPC traces pin;
- its journal has the same tables, indexes and triggers;
- its settings, Telegram texts and health faults are unchanged.

Every new path below runs only with `COORDINATOR_KIND=round`, `FINALITY_MODE=soft` or `GAS_MODEL=arbitrum`, none of which Arc uses.

### Round mode (`COORDINATOR_KIND=round`)

- **Serving the round coordinator.** The keeper serves `D20VRFCoordinatorRobinhood`, which binds each request to a future round of drand's evmnet beacon. There are no epochs, registry, recipes or target block.
- **Round fetching and proving.**
  - The keeper fetches each request's round from the drand relays once the round is due, and the coordinator verifies its BLS signature.
  - It proves the request over the round and sends the fulfillment, singly or in batches of up to 16, with each round's signature.
  - A round the coordinator already holds is taken from its `RoundVerified` event, or from the relays and checked against the coordinator's randomness.
  - A request whose fields a replaced block moved is proved again.
- **Fee gate.**
  - A fulfillment is priced on the gas it can use, with every listed round counted as unverified.
  - The escrowed fees must cover `FEE_COVERAGE_BPS` of that cost (at least 12,500 on Robinhood Chain).
  - The keeper's own share, `keeperFeeBps()` read on each send, must cover the whole cost.
- **Wallet balance.** A fulfillment is signed only when the wallet holds its up-front cost. A batch shrinks to what the balance covers, and a request the balance cannot cover stays prepared for a follower. The owner is asked once, in Turkish, to fund the wallet.
- **Clock behind.** A machine clock behind the chain's does not stop rounds being fetched. It is reported as `clock_behind`.
- **Sweep reserve.** Sweeps are sized in ETH and keep `SWEEP_MIN_RESERVE_WEI`.
- **Refused settings.** Every epoch-only setting is refused.

### Soft finality (`FINALITY_MODE=soft`)

- **Acting at the sequencer's head.** The keeper acts on the sequencer's latest block. It writes down every block it acts on and audits those blocks against L1 finality later.
- **One endpoint is only a suspicion.**
  - A block one endpoint shows replaced holds the keeper's sends while every endpoint is asked.
  - Only two or more endpoints that agree confirm a change. A one-against-one split settles nothing and cools nobody.
  - A shorter replacement chain is confirmed only when no endpoint has the block, over several asks.
- **Self-healing recovery.**
  - The keeper takes the chain as it is now and puts its nonce lane right: it rebroadcasts the bytes it kept, or fills a nonce with a transfer to itself.
  - It re-reads its jobs and resumes, with no operator step.
  - The owner is told once when it is done. They are asked to act only when no two endpoints settle a suspicion, or a recovery keeps failing, for ten minutes.
- **Audit.** The audit asks every endpoint, each within its budget. It makes final only blocks every answering endpoint shows with the same hash, below a `finalized` header old enough to be final.
- **Endpoints.**
  - An endpoint without the `finalized` tag still gives its view of a block.
  - An endpoint listed twice counts once.
  - A round keeper's endpoint that does not answer at startup is probed again beside the ticks and read from again once it answers.
- **Migration.** A wallet rotation waits for the marked blocks to be posted to L1, not for L1 finality.

### Arbitrum gas model (`GAS_MODEL=arbitrum`)

- **Gas limits.** Every gas limit holds the L1 component, read from `NodeInterface.gasEstimateL1Component` with `L1_GAS_MARGIN_BPS`.
- **Fulfillment limits.** A fulfillment's limit never falls below the coordinator's guard through its proxy, with a margin of 1% and at least 5,000.
- **Tips.** No tip is paid on Robinhood Chain.

### Free RPC tiers

- **Batches.** An endpoint that refuses a batch for its size is asked again in smaller chunks, and the size it takes is kept.
- **Provider errors.** A round keeper defers, without counting it, a tick that failed only on provider errors.
- **Idle polling.** An idle round keeper subscribes only to its coordinator's request, role and upgrade events. It ticks on a pushed request or every `IDLE_HEARTBEAT_SECONDS`, and re-checks its pins and role every 120 seconds.
- **Subscription settings.** `WS_SILENCE_SECONDS` and the `WS_BACKFILL_*` window apply to a round keeper's subscription.

### Notices

- **Paging rules.** Pages to the owner are in plain Turkish and are sent once per case, over restarts.
- **Low-balance alert.** A round keeper's low-balance alert is sent once per episode.
- **Ignored settings.** Settings a keeper validates but does not act on are named at startup.
