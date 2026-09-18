# D20DAO integration guide for agents

D20DAO provides general-purpose verifiable randomness for consumer applications. Dice, selections and shuffles are deterministic mappings of a random word; they are examples, not the scope of the service.

## Integration resources

- [Protocol, interfaces and public replay](https://github.com/d20dao/keeper)
- [SDK source](https://github.com/d20dao/d20-sdk): install `@d20dao/vrf-sdk` from npm.
- [Agent integration skills](https://github.com/d20dao/skills)

The website's `/docs` guides cover integration, architecture, sources, verification, service rules and security. The service is deployed on Arc Mainnet (chain 5042) and Arc Testnet (chain 5042002). The public deployment manifests at https://github.com/d20dao/keeper/blob/main/deployments/arc-mainnet.json and https://github.com/d20dao/keeper/blob/main/deployments/arc-testnet.json record proxy addresses, code hashes, deployment receipts and implementation upgrades. Consumer access is public; there is no keeper allowlist. This does not establish an external cryptographic audit or a public SLA.

## Request and receive randomness

A consumer can extend `D20VRFConsumer`, which authenticates callbacks from its configured coordinator. `ID20VRF` exposes `quoteFee(callbackGasLimit)`, `quoteFeeAt(callbackGasLimit, baseFee)`, `requestRandomness(clientSeed, callbackGasLimit, refundAddress)`, `requestMappedRandomness(...)` and `getMappedResult(requestId)`.

The fee is `max(minFee, feeMultiplier × baseFee × (fulfillGasOverhead + callbackGasLimit))`; `pricing()` returns the live parameters, which the owner can change within fixed bounds. A contract that requests in the same transaction pays `quoteFee(callbackGasLimit)`, which is exact. Off-chain senders must not rely on `quoteFee` through `eth_call` (it commonly sees a base fee of 0): quote with `quoteFeeAt(callbackGasLimit, latestHeader.baseFeePerGas)`, add a buffer for base-fee movement and send at least that. Underpayment reverts with `IncorrectFee(quoted, sent)`; any excess is credited to the refund address as withdrawable refund credit (`withdrawRefundCredit`), never kept as revenue. Choose a fixed refund recipient, keep the returned request ID and correlate it with delivery. The client seed is a fixed input, not a guarantee of secrecy. Store the authenticated result in a small callback and perform other application actions separately.

A request checkpoints its epoch's canonical source anchor and escrows its quoted fee (`requestFeePaid`) even if its API packet has not yet been published. Consumer, client seed, mapping, request block, epoch ID and refund recipient are fixed. The epoch hash resolves at publication. Until publication, the target and complete VRF input are unresolved.

## Epoch publication and proof

Epochs span 200 blocks. The first starts 200 blocks after registry initialization. The source-selection anchor is the block immediately before the epoch starts. The keeper prepares the current epoch locally and preserves its first valid snapshot. Idle epochs need no publication transaction. Live paid demand triggers publication of the packet, which cannot be overwritten.

The randomness target is `max(requestBlock, epochCommitBlock + 1)`, strictly after publication. The target hash is unknown when the packet is committed. The fixed input binds chain, coordinator, key hash, request ID, consumer, client seed, mapping hash, request block, target block and target hash, epoch ID and epoch hash. A genuine fixed secp256k1 VRF key supplies the proof. `fulfillRandomness(requestId, proof)` carries 452 calldata bytes and 416 proof-evidence bytes. `fulfillRandomnessBatch(ids, proofs)` fulfills up to 16 prepared requests in one transaction with identical per-request events, evidence, settlement and callback delivery; members already fulfilled, refunded or past their deadline are skipped with `FulfillmentSkipped(requestId, reason)` (1 fulfilled, 2 refunded, 3 expired), while a wrong seed, invalid proof or unready member reverts the whole batch.

There is no per-request API fetch or resampling. When the selected source yields no valid packet, the keeper moves to the deterministic fallback: attempt n (1 to the epoch's source count minus one) is the source n slots after the selected one, committed with `commitEpochFallback(epochId, n, attestation)` no earlier than n × 20 blocks into the epoch. A saved packet is never replaced, and the committed source identifies the attempt. An unused local packet retires after 50 epochs. Used epoch packets remain available through public chain events.

## Exact source records

Each epoch uses a catalog of 1 to 10 sources, each a registered recipe and its signer. The catalog hash, epoch ID and start-minus-one anchor hash select one slot modulo the source count. Recipes live in an owner-managed, append-only registry: a recipe is a canonical AirnodeHub request (its keccak256 is the signed query hash), a data template fixing the exact signed record, and the JSON body keepers post to the provider's gateway. `registerRecipe` (owner only, event `RecipeRegistered`) appends the next id; a registered recipe is never edited or removed. `recipeCount()`, `getRecipe(id)` and `recipeRequest(id)` read them. Six recipes are built in:

| Recipe | Provider | Signed record |
| --- | --- | --- |
| 0 | Hyperliquid | BTC symbol and dayNtlVlm from metaAndAssetCtxs |
| 1 | dRPC | Ethereum mainnet block hash: eth_call of Multicall3 getLastBlockHash() at latest |
| 2 | TickerLayer | crypto BTCUSD lastTrade |
| 3 | TickerLayer | crypto ETHUSD lastTrade |
| 4 | Nodary | ETH/USD feed value, millisecond timestamp and category |
| 5 | dRPC | Base block hash: eth_call of Multicall3 getLastBlockHash() at latest |

A registry starts with the initial catalog, recipes 0 to 3 with the signers returned by `hyperliquidSigner()`, `ethereumBlockSigner()`, `btcTradeSigner()` and `ethTradeSigner()`, bound into `catalogHash()`. The owner schedules replacements of 1 to 10 distinct registered recipes for epochs at least two ahead with `scheduleCatalog(recipes, signers, fromEpoch)` (event `CatalogScheduled`); the rollout catalog is recipes 0, 1, 2, 4, 5, so each fallback moves to another provider. `catalogAt(epoch)` returns the hash, recipe ids and signers an epoch uses, `sourceCountAt(epoch)` its source count, and `Epoch.catalogHash` records the hash; selections report both the source slot and the recipe id. Each canonical query is AirnodeHub's canonical form of a fixed operation and parameters (objects sorted by key at every depth, arrays in order). Raw values may repeat. Distinct epoch parameters bind distinct commitments without creating entropy by hashing.

Canonical low-s EIP-191 signatures bind query hash, attestation timestamp and exact UTF-8 data. Raw signed records are at most 128 bytes. At publication the attestation cannot be future-dated or more than 240 seconds old. Every recipe accepts only the exact records its data template describes: literal bytes, fixed-length lowercase hex, JSON numbers and bounded positive integers, consumed exactly, with fixed key order and no extra fields or whitespace; for example the block hashes are the JSON-RPC envelope `{"id":null,"jsonrpc":"2.0","result":"0x…"}` with 64 lowercase hex characters, and TickerLayer and Nodary records keep their exact numeric bytes. Malformed, oversized or stale data is rejected rather than cropped or silently replaced.

## Acceptance, delivery and refunds

A proof must be accepted onchain at or before the request timestamp plus 60 seconds. Publication and waiting for the target block consume the same window. Crossing an epoch boundary does not shorten the request's deadline. A pending transaction is not acceptance, and the timestamp rule is not a guaranteed block SLA.

Accepted proof earns the configured keeper and treasury shares even when the consumer callback fails; both are computed from the fee that request escrowed, not the live price. The keeper share goes to the wallet that submitted the accepted proof when the registry authorizes it to publish epochs (`committer()` or an allowed backup committer, read through `isAuthorizedCommitter`), and to `committer()` for any other submitter; submission itself stays permissionless. Failed transfers become backed credits. `config/service.json` carries the initialization defaults of a 0.08 USDC minimum fee (`minFeeWei`) and a 50% keeper share (`keeperFeeBps` 5000). Read the live configuration (`pricing()`, `keeperFeeBps()`) rather than treating either as permanent pricing.

A failed callback can be retried with the same stored word and no second service fee. After expiry without accepted proof, `refundBps` of the escrowed RNG fee (owner-set, never below 50%, 100% by default) is refundable by transaction to its fixed recipient and the remainder is retained as treasury revenue; the ratio is snapshotted into each request at creation (`requestRefundBps`), so a later `setRefundBps` never changes what an open request refunds. Gas and application fees are excluded. Refunds are not automatic. Publication alone does not earn the escrowed fee. Inspect request, callback, refund and credit state separately when determining an application's next action.

## Independent verification and trust

`EpochCommitted` emits the complete accepted API packet once when the epoch is used. Accepted requests expose proof evidence. `replayEpochCommitment` validates epoch selection and the signed record against the selected recipe's template (built-in recipes by default; `readEpochRecipes` reads any other registered recipe); `replayCoordinator` checks request binding, future target, key/configuration, seed, VRF, mapping, transcript and acceptance timing. Replay needs independently trusted canonical blocks, receipts, timestamps and historical implementation context, without a private keeper database or keeper connection.

The coordinator and registry use UUPS implementations behind ERC1967 proxies. Ownership transfer is two-step; owner, fee recipient and keeper/committer roles can rotate, and the owner can allow backup committers that publish epochs under the committer's rules (a backup committer's own accepted proofs pay the keeper share to it, as described above). This implementation has no VRF-key setter and signer catalogs can only be scheduled for future epochs, but trusted upgrade authority can change behavior. Rotating the transaction wallet does not rotate the VRF key. A stable proxy address or its runtime hash alone does not prove unchanged behavior; identify the implementation and configuration active at each historical receipt.

Proofs do not force operator availability or establish source truthfulness, independence or lack of bias. The operator can withhold preparation, publication or fulfillment. A source signature alone does not prove chain inclusion or a complete VRF result. The website lab is illustrative; its historical Hyperliquid candle inspector authenticates only that historical response, not a current epoch recipe or accepted onchain request.

## Public explorer

The website's `/explorer` lists only canonical indexed requests and onchain epoch publications. Filter by chain, coordinator or registry, numeric ID and request state. Request details preserve the original mapping and evidence; optional what-if mapping is explicitly simulated.

Public replay verifies the signed epoch packet, fixed request/target context, VRF, mapping and transcript. A separate chain check compares canonical receipts, event bytes, source/target hashes and historical configuration through an independently configured RPC. Mathematical validity does not establish chain inclusion. Historical proxy and UUPS implementation runtime hashes must also match the independently configured trust allowlists; otherwise code trust remains unknown. The index database alone is not proof.

An unconfigured or empty index shows a waiting state, never substituted sample activity. Test fixtures are local CI evidence and are not a production explorer fallback.

## Optional refund notification in current source

`D20VRFConsumer` authenticates `onRefund(requestId)` and delegates to optional `_onRefund`. The coordinator settles the fixed recipient payment or backed credit before the 100,000-gas notification. Failure is isolated and `retryRefundCallback` retries only notification; request/refund/retry reentry is blocked. Verify the deployed implementation before relying on this source feature: a source or SDK update alone does not upgrade the proxy.

## Agent-assisted application setup

Install `@d20dao/vrf-sdk` and read its packaged AGENTS.md and PROTOCOL-PROVENANCE.json. Website `/docs/getting-started` leads through deployment selection, consumer setup and validation. Each guide offers Copy prompt and `/prompts/<guide-slug>.txt`; `/llms.txt`, `/llms-full.txt` and `/agents.md` expose the same integration references to readers and agents.
