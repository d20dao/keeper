# D20DAO integration guide for agents

D20DAO provides general-purpose verifiable randomness for consumer applications. Dice, selections and shuffles are deterministic mappings of a random word; they are examples, not the scope of the service.

## Integration resources

- [Protocol, interfaces and public replay](https://github.com/d20dao/keeper)
- [SDK source](https://github.com/d20dao/d20-sdk): install `@d20dao/vrf-sdk` from npm.
- [Agent integration skills](https://github.com/d20dao/skills)

The website's `/docs` guides cover integration, architecture, sources, verification, service rules and security. Consumer access is public; there is no keeper allowlist. This does not establish an external cryptographic audit or a public SLA.

## Networks

The service runs on two networks with the same consumer interface. They differ in where the randomness comes from and what pays the fee.

| Network | Chain | Coordinator proxy | Randomness | Fee |
| --- | --- | --- | --- | --- |
| Arc Mainnet | 5042 | `0xd20da057469C45928912d983F45790C41e290571` | epochs + drand | USDC |
| Arc Testnet | 5042002 | `0xd20DA0FF9087d053f0291524Eac12abA1ADBd945` | epochs + drand | USDC |
| Robinhood Chain | 4663 | `0xEc8b95B168c87294c45727Bd2ac903d09316D132` | round-bound drand | ETH |
| Robinhood Chain Testnet | 46630 | `0x2f26513DE4Ed388947f5d22FD395D5E06f472f05` | round-bound drand | ETH |

- **Arc** publishes a drand round once per 200-block epoch when paid demand needs it, and binds it to a future block (sections marked Arc below).
- **Robinhood Chain** has no epochs: each request is bound, when it is made, to a future round of drand's evmnet beacon (section "Robinhood Chain: round-bound drand").

The public deployment manifests in https://github.com/d20dao/keeper/tree/main/deployments (`arc-mainnet.json`, `arc-testnet.json`, `robinhood-mainnet.json`, `robinhood-testnet.json`) record proxy addresses, code hashes, deployment receipts and implementation upgrades. Read the coordinator address for the intended chain from its manifest.

## Request and receive randomness (both networks)

A consumer can extend `D20VRFConsumer`, which authenticates callbacks from its configured coordinator. `ID20VRF` exposes `quoteFee(callbackGasLimit)`, `quoteFeeAt(callbackGasLimit, baseFee)`, `requestRandomness(clientSeed, callbackGasLimit, refundAddress)`, `requestMappedRandomness(...)` and `getMappedResult(requestId)`.

The fee, in the chain's native token (USDC on Arc, ETH on Robinhood Chain), is `max(minFee, feeMultiplier × baseFee × (fulfillGasOverhead + callbackGasLimit))`; `pricing()` returns the live parameters, which the owner can change within fixed bounds. A contract that requests in the same transaction pays `quoteFee(callbackGasLimit)`, which is exact. Off-chain senders must not rely on `quoteFee` through `eth_call` (it commonly sees a base fee of 0): quote with `quoteFeeAt(callbackGasLimit, latestHeader.baseFeePerGas)`, add a buffer for base-fee movement and send at least that. Underpayment reverts with `IncorrectFee(quoted, sent)`; any excess is credited to the refund address as withdrawable refund credit (`withdrawRefundCredit`), never kept as revenue. Choose a fixed refund recipient, keep the returned request ID and correlate it with delivery. The client seed is a fixed input, not a guarantee of secrecy. Store the authenticated result in a small callback and perform other application actions separately.

## Robinhood Chain: round-bound drand

`D20VRFCoordinatorRobinhood` binds each request, when it is made, to the first round of drand's evmnet beacon scheduled at least `ROUND_LEAD` (3) seconds after its block's timestamp. There are no epochs, registry, recipes or target block. The first fulfillment that serves a request of a round verifies the round's BLS signature against the registered group key and caches its randomness; the result is the operator's VRF output over the request's fixed fields and that round's randomness. Fulfillments carry the round's signature, singly or in batches of up to 16.

- **Fee:** both networks initialized with `minFee` 0.000025 ETH, `feeMultiplier` 2 and `fulfillGasOverhead` 405,000, so today `quoteFee(callbackGasLimit) = max(0.000025 ETH, 2 × baseFee × (405,000 + callbackGasLimit))`; off-chain senders quote with `quoteFeeAt(callbackGasLimit, baseFee)` and the latest header's base fee plus a buffer. The owner can change these within fixed bounds: read `pricing()` and quote with `quoteFeeAt` on chain. An accepted proof pays 80% of the escrowed fee to the keeper and 20% to the DAO (`keeperFeeBps()` 8000; read it live).
- **Deadline and refund:** a proof must be accepted within 60 seconds of the request. After that, `refundRequest(requestId)` pays `refundBps` (100% at initialization, never below 50%) of the escrowed fee to the fixed refund address, or credits it there when the transfer fails. Refunds are not automatic. A failed callback can be retried with the same result and no second fee.
- **No x402 agent API:** that API settles in USDC, which Robinhood Chain does not have; call the coordinator directly.
- **Addresses:** the coordinator implementation `0xC8Cd79B9092AEA38f3434388F291eb861b148f45`, VRF proof verifier `0x1EEBe8B8f7a6A18b966C3fBe3f644B8234f93709`, mapping library `0xA57093a645C1Aed12486da50AfA95F3284826849` and drand evmnet beacon verifier `0xd20dA01Aa16AeD6b77Cd8DDb869151802599100a` are the same on both networks.
- **Owner:** read `owner()` and `feeRecipient()` from the coordinator.

### Trust

**How a result is made.** When a request is made, the contract binds it to a future round of drand's evmnet beacon: the first round scheduled at least 3 seconds after the request. Until that round is published, nobody can know or choose the result: not the requester, the operator, the sequencer or drand. The keeper then delivers the operator's VRF output over the request's fixed fields and the round's randomness. The contract verifies the round's BLS signature and the VRF proof before accepting it, and anyone can replay the result from public chain data.

**Assumptions.**
- **Shared with every Robinhood Chain application:** D20DAO relies on the chain's sequencer for ordering and confirmation.
- **Shared with other verifiable-randomness services:** results assume the operator's VRF key and the sequencer act independently.
- **drand:** evmnet is run by the League of Entropy, and its signatures rest on a threshold of independent operators.

**Finality.** Results are confirmed by the sequencer within seconds. Settlement on Ethereum follows in about 15–20 minutes.

**Delivery and refunds.** A typical delivery takes 5–8 seconds. If a request is not fulfilled within 60 seconds, its fee can be refunded to the request's refund address. As with any randomness service, design applications so that a party cannot cancel a request and retry for a better outcome.

**Upgrades.** The coordinator is upgradeable (UUPS) by its owner; read `owner()` from the chain. Beacon changes are scheduled on chain at least 10 minutes ahead.

Details: https://github.com/d20dao/keeper/blob/main/docs/robinhood.md.

## Arc: epoch requests

A request checkpoints its epoch's canonical source anchor and escrows its quoted fee (`requestFeePaid`) even if its epoch packet has not yet been published. Consumer, client seed, mapping, request block, epoch ID and refund recipient are fixed. The epoch hash resolves at publication. Until publication, the target and complete VRF input are unresolved.

## Arc: epoch publication and proof

Epochs span 200 blocks. The first starts 200 blocks after registry initialization. The source-selection anchor is the block immediately before the epoch starts. The keeper prepares the current epoch locally. Idle epochs need no publication transaction. Live paid demand triggers publication of the packet, which cannot be overwritten.

The randomness target is `max(requestBlock, epochCommitBlock + 1)`, strictly after publication. The target hash is unknown when the packet is committed. The fixed input binds chain, coordinator, key hash, request ID, consumer, client seed, mapping hash, request block, target block and target hash, epoch ID and epoch hash. A genuine fixed secp256k1 VRF key supplies the proof. `fulfillRandomness(requestId, proof)` carries 452 calldata bytes and 416 proof-evidence bytes. `fulfillRandomnessBatch(ids, proofs)` fulfills up to 16 prepared requests in one transaction with identical per-request events, evidence, settlement and callback delivery; members already fulfilled, refunded or past their deadline are skipped with `FulfillmentSkipped(requestId, reason)` (1 fulfilled, 2 refunded, 3 expired), while a wrong seed, invalid proof or unready member reverts the whole batch. Before serving any member, a batch requires gas for every member it will serve, 140,000 + Σ(callbackGasLimit + callbackGasLimit/63 + 400,000), and otherwise reverts with `InsufficientCallbackGas`, so no callback can make a batch revert after an earlier member's result was revealed.

There is no per-request fetch or resampling. A catalog of several sources has a deterministic fallback: when the selected source yields no valid packet, attempt n (1 to the epoch's source count minus one) is the source n slots after the selected one, committed with `commitEpochFallback(epochId, n, attestation)` no earlier than n × 20 blocks into the epoch. The drand catalog has one source and so no fallback: while no round can be fetched and verified, nothing is published, and requests wait and, after expiry, are refundable. A committed packet is never replaced, and the committed source identifies the attempt. An unused local packet retires after 50 epochs. Used epoch packets remain available through public chain events.

## Arc: exact source records

Each epoch uses a catalog of 1 to 10 sources, each a registered recipe and its signer. The catalog hash, epoch ID and start-minus-one anchor hash select one slot modulo the source count. Recipes live in an owner-managed, append-only registry, and a registered recipe is never edited or removed. `recipeCount()`, `getRecipe(id)` and `recipeRequest(id)` read them. A recipe is a drand beacon or a signed API record.

The epoch source is a drand beacon: evmnet, a threshold BLS network run by the League of Entropy that signs one round every 3 seconds. `registerBeacon(verifier, chainHash, publicKey, genesis, period, sampleRound, sampleSignature)` (owner only, events `RecipeRegistered` and `BeaconRegistered`) appends it as the next recipe id. The registration is fixed once made: the verifier contract that checks the network's signatures, its chain hash, its 128-byte group public key and its schedule, where round `r` is scheduled at `genesis + (r - 1) × period`. A sample round must verify under the key with the gas allowance a commit gets, so a malformed key or a verifier that reverts, runs out of gas or answers badly cannot register. `beaconOf(id)` returns the registration (a zero verifier marks a signed recipe), `slotSigner(id)` the signer a catalog lists for the beacon, a fixed identity derived from the registration and not a key, and `verifyBeacon(id, round, signature)` whether the registered verifier accepts a signature. The recipe's canonical request and body are `["drand","0x<chain hash>"]` and its data template is a round number.

An epoch commits one round. The data is the round number in decimal, 1 to 19 digits without a leading zero. The timestamp is the round's scheduled time, `genesis + (round - 1) × period` exactly. The signature is the round's 64-byte BLS signature, which the registered verifier checks under the registered key with a fixed gas allowance, `BEACON_VERIFY_GAS` (400,000); a revert, an exhausted allowance or any answer but a lone 32-byte true is an invalid signature, and the registry reads no more than 32 bytes of the answer. If the sender's gas cannot give the verifier its whole allowance, `commitEpoch`, `commitEpochFallback`, `registerBeacon` and `verifyBeacon` revert `BeaconGasTooLow` instead, so a valid round is never read as invalid for want of gas: send a beacon commit with `eth_estimateGas` and a margin, about 511,000 gas at the least, never with a smaller fixed limit. One round can serve up to three consecutive epochs, so `dataHash` and `attestationHash` can repeat between epochs: identify an epoch by its ID or epoch hash. No signer signs the packet. Keepers read rounds from drand relays they do not trust and the registry verifies every round, so a relay can delay an epoch and cannot change it.

The drand catalog is the beacon alone, recipe 11 on a registry that holds recipes 0 to 10: `scheduleCatalog([11], [slotSigner(11)], fromEpoch)`, and the registry refuses any other signer for a beacon slot. It has one slot and no fallback source. Both networks run it, Arc Testnet from epoch 11319 and Arc Mainnet from epoch 12448. Epochs published before it took effect used signed API recipes (recipes 0 to 10) and verify as signed API records.

A signed API recipe is a canonical AirnodeHub request (its keccak256 is the signed query hash), a data template fixing the exact signed record, and the body that named the gateway request keepers sent: a JSON object posted to the provider's gateway, or a passthrough request sent to its `/api` whose attestation arrived in `X-Airnode-*` headers over the body exactly as received. `registerRecipe` (owner only, event `RecipeRegistered`) appends one. The registry still accepts and replays them, but keeper 0.4.1 and later do not fetch signed records: a catalog that selects one is refused by the keeper, which logs an error, reports the health fault `epoch_recipe_unsupported` and publishes nothing for that source. Six are built in:

| Recipe | Provider | Signed record |
| --- | --- | --- |
| 0 | Hyperliquid | BTC symbol and dayNtlVlm from metaAndAssetCtxs |
| 1 | dRPC | Ethereum mainnet block hash: eth_call of Multicall3 getLastBlockHash() at latest |
| 2 | TickerLayer | crypto BTCUSD lastTrade |
| 3 | TickerLayer | crypto ETHUSD lastTrade |
| 4 | Nodary | ETH/USD feed value, millisecond timestamp and category |
| 5 | dRPC | Base block hash: eth_call of Multicall3 getLastBlockHash() at latest |

A registry starts with the initial catalog, recipes 0 to 3 with the signers returned by `hyperliquidSigner()`, `ethereumBlockSigner()`, `btcTradeSigner()` and `ethTradeSigner()`, bound into `catalogHash()`. The owner schedules replacements of 1 to 10 distinct registered recipes for epochs at least two ahead with `scheduleCatalog(recipes, signers, fromEpoch)` (event `CatalogScheduled`). A version still two or more epochs away is replaced by a new schedule; the version due at the next epoch and every active one are kept, so the catalogs of the current and next epoch never change. `catalogAt(epoch)` returns the hash, recipe ids and signers an epoch uses, `sourceCountAt(epoch)` its source count, and `Epoch.catalogHash` records the hash; selections report both the source slot and the recipe id. Each canonical query is AirnodeHub's canonical form of a fixed operation and parameters (objects sorted by key at every depth, arrays in order). Raw values may repeat. Distinct epoch parameters bind distinct commitments without creating entropy by hashing.

At publication the attestation cannot be future-dated or more than 240 seconds old; a beacon round's timestamp is its scheduled time. A signed record's canonical low-s EIP-191 signature binds query hash, attestation timestamp and exact UTF-8 data, and raw signed records are at most 128 bytes. Every signed recipe accepts only the exact records its data template describes: literal bytes, fixed-length lowercase hex, JSON numbers and bounded positive integers, consumed exactly, with fixed key order and no extra fields or whitespace; for example the block hashes are the JSON-RPC envelope `{"id":null,"jsonrpc":"2.0","result":"0x…"}` with 64 lowercase hex characters, and TickerLayer and Nodary records keep their exact numeric bytes. Malformed, oversized or stale data is rejected rather than cropped or silently replaced.

## Arc: acceptance, delivery and refunds

A proof must be accepted onchain at or before the request timestamp plus 60 seconds. Publication and waiting for the target block consume the same window. Crossing an epoch boundary does not shorten the request's deadline. A pending transaction is not acceptance, and the timestamp rule is not a guaranteed block SLA.

Accepted proof earns the configured keeper and treasury shares even when the consumer callback fails; both are computed from the fee that request escrowed, not the live price. The keeper share goes to the wallet that submitted the accepted proof when the registry authorizes it to publish epochs (`committer()` or an allowed backup committer, read through `isAuthorizedCommitter`), and to `committer()` for any other submitter; submission itself stays permissionless. Failed transfers become backed credits. `config/service.json` carries the initialization defaults of a 0.08 USDC minimum fee (`minFeeWei`) and a 50% keeper share (`keeperFeeBps` 5000). Read the live configuration (`pricing()`, `keeperFeeBps()`) rather than treating either as permanent pricing.

A failed callback can be retried with the same stored word and no second service fee. After expiry without accepted proof, `refundBps` of the escrowed RNG fee (owner-set, never below 50%, 100% by default) is refundable by transaction to its fixed recipient and the remainder is retained as treasury revenue; the ratio is snapshotted into each request at creation (`requestRefundBps`), so a later `setRefundBps` never changes what an open request refunds. Gas and application fees are excluded. Refunds are not automatic. Publication alone does not earn the escrowed fee. Inspect request, callback, refund and credit state separately when determining an application's next action.

## Arc: independent verification and trust

`EpochCommitted` emits the complete accepted packet once when the epoch is used: a signed API record, or a beacon round with its BLS signature (448 bytes). Accepted requests expose proof evidence. `replayEpochCommitment` validates epoch selection and the committed record against the selected recipe: a signed record against its template and signer, a beacon round against its number, scheduled time and BLS signature under the registered key, with the catalog's signer equal to the registration's `slotSigner` (built-in recipes by default; `readEpochRecipes` reads any other registered recipe and, for a beacon, its `beaconOf` registration); `replayCoordinator` checks request binding, future target, key/configuration, seed, VRF, mapping, transcript and acceptance timing. Epochs published from signed API recipes verify as signed API records. Replay needs independently trusted canonical blocks, receipts, timestamps and historical implementation context, without a private keeper database or keeper connection. It computes the beacon scheme itself and does not run the registered verifier, whose runtime code is chain context to check.

The coordinator and registry use UUPS implementations behind ERC1967 proxies. Ownership transfer is two-step; owner, fee recipient and keeper/committer roles can rotate, and the owner can allow backup committers that publish epochs under the committer's rules (a backup committer's own accepted proofs pay the keeper share to it, as described above). This implementation has no VRF-key setter, never changes a registered recipe or beacon (verifier, group key and schedule included) and schedules signer catalogs only for future epochs, but trusted upgrade authority can change behavior. Rotating the transaction wallet does not rotate the VRF key. A stable proxy address or its runtime hash alone does not prove unchanged behavior; identify the implementation and configuration active at each historical receipt.

Proofs do not force operator availability or establish source truthfulness, independence or lack of bias. The operator can withhold preparation, publication or fulfillment. drand availability is a single point of failure for publication, because the drand catalog has no fallback source. A valid round shows that the network's group key signed it, not that its nodes are honest or independent. The committer chooses among the rounds the registry accepts, up to 81 scheduled in the last 240 seconds at a 3-second period; that does not let it choose outcomes, because the request's target block hash does not exist when the round is committed. A source signature or beacon round alone does not prove chain inclusion or a complete VRF result. The website lab is illustrative; its evidence explorer checks the signature of one drand round, not an accepted onchain request.

## Public explorer

The website's `/explorer` lists only canonical indexed requests and onchain epoch publications. Filter by chain, coordinator or registry, numeric ID and request state. Request details preserve the original mapping and evidence; optional what-if mapping is explicitly simulated.

Public replay verifies the epoch packet (a signed record or a beacon round), fixed request/target context, VRF, mapping and transcript. A separate chain check compares canonical receipts, event bytes, source/target hashes and historical configuration through an independently configured RPC. Mathematical validity does not establish chain inclusion. Historical proxy and UUPS implementation runtime hashes must also match the independently configured trust allowlists; otherwise code trust remains unknown. The index database alone is not proof.

An unconfigured or empty index shows a waiting state, never substituted sample activity. Test fixtures are local CI evidence and are not a production explorer fallback.

## Optional refund notification in current source

`D20VRFConsumer` authenticates `onRefund(requestId)` and delegates to optional `_onRefund`. The coordinator settles the fixed recipient payment or backed credit before the 100,000-gas notification. Failure is isolated and `retryRefundCallback` retries only notification; request/refund/retry reentry is blocked. Verify the deployed implementation before relying on this source feature: a source or SDK update alone does not upgrade the proxy.

## Agent-assisted application setup

Install `@d20dao/vrf-sdk` and read its packaged AGENTS.md and PROTOCOL-PROVENANCE.json. Website `/docs/getting-started` leads through deployment selection, consumer setup and validation. Each guide offers Copy prompt and `/prompts/<guide-slug>.txt`; `/llms.txt`, `/llms-full.txt` and `/agents.md` expose the same integration references to readers and agents.

## Development notes

Arc Mainnet and Robinhood Chain are live, so a change to a coordinator or the registry ships as an in-place UUPS upgrade of the live proxies, never as a redeployment. It must be storage-layout compatible with every deployed layout (new state takes slots from the gap and retired slots stay declared), ABI compatible and additive (the consumer ABI of the coordinator and `D20VRFConsumer` does not break, and existing functions, events and errors keep their meaning), with new state initialized by a reinitializer inside `upgradeToAndCall`. Upgrade tests start from the implementation bytecode recorded from both live networks (`test/fixtures`), not from rebuilt sources, and cover requests pending at upgrade time and the replay of history published by the previous code; `docs/registry-upgrade.md` lists the rules and `storage-layout/` the reviewed layouts.
