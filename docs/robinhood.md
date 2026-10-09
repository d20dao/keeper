# Robinhood Chain: the round coordinator

D20 randomness runs on Robinhood Chain mainnet (chain 4663) and its testnet (chain 46630) through `D20VRFCoordinatorRobinhood`. Robinhood has no epochs, registry or recipes: each request is bound, when it is made, to the first round of drand's evmnet beacon scheduled at least 3 seconds (`ROUND_LEAD`) after its block's timestamp. The first fulfilment that serves a request of a round verifies the round's BLS signature and caches its randomness; later requests of the round read the cache. The result is the operator's VRF output over the request's fixed fields and that round's randomness.

The consumer interface is Arc's: extend `D20VRFConsumer`, call `requestRandomness(clientSeed, callbackGasLimit, refundAddress)` or `requestMappedRandomness(...)`, read `getMappedResult(requestId)`. Fees are paid in ETH.

## Addresses

| | Robinhood Chain (4663) | Robinhood Chain Testnet (46630) |
| --- | --- | --- |
| Coordinator proxy | `0xEc8b95B168c87294c45727Bd2ac903d09316D132` | `0x2f26513DE4Ed388947f5d22FD395D5E06f472f05` |
| Manifest | [robinhood-mainnet.json](../deployments/robinhood-mainnet.json) | [robinhood-testnet.json](../deployments/robinhood-testnet.json) |

Both networks share the same CREATE2 addresses and code for the rest of the set:

| Contract | Address |
| --- | --- |
| Coordinator implementation | `0xC8Cd79B9092AEA38f3434388F291eb861b148f45` |
| VRF proof verifier | `0x1EEBe8B8f7a6A18b966C3fBe3f644B8234f93709` |
| Mapping library | `0xA57093a645C1Aed12486da50AfA95F3284826849` |
| drand evmnet beacon verifier | `0xd20dA01Aa16AeD6b77Cd8DDb869151802599100a` |

The manifests record code hashes, the VRF key hash, the configuration hash, the beacon registration and every deployment transaction. Read the coordinator's current owner and fee recipient from the chain (`owner()`, `feeRecipient()`).

## Fees, deadlines and refunds

`quoteFee(callbackGasLimit)` and `quoteFeeAt(callbackGasLimit, baseFee)` return the fee: `max(minFee, feeMultiplier × baseFee × (fulfillGasOverhead + callbackGasLimit))`. Both networks initialized with `minFee` 0.000025 ETH, `feeMultiplier` 2 and `fulfillGasOverhead` 405,000, so today

```
quoteFee(callbackGasLimit) = max(0.000025 ETH, 2 × baseFee × (405,000 + callbackGasLimit))
```

The owner can change these within fixed bounds: read `pricing()` from the chain rather than relying on these figures. Off-chain senders quote with `quoteFeeAt` and the latest header's base fee plus a buffer; any excess is credited to the refund address. An accepted proof splits the escrowed fee `keeperFeeBps` (8,000, 80%) to the keeper and the rest (20%) to the DAO's fee recipient; read `keeperFeeBps()` too.

A proof must be accepted within 60 seconds (`RESPONSE_TIMEOUT`) of the request. After that deadline without one, `refundRequest(requestId)` pays `refundBps` (100% at initialization, never below 50%) of the escrowed fee to the request's fixed refund address, or credits it there (`withdrawRefundCredit`) when the transfer fails; anyone can send it, nothing sends it automatically, and gas is not refunded. A failed callback can be retried with the same result and no second fee.

There is no x402 agent API on Robinhood Chain: that API settles in USDC, which Robinhood Chain does not have. Applications call the coordinator directly.

## Trust

**How a result is made.** When a request is made, the contract binds it to a future round of drand's evmnet beacon: the first round scheduled at least 3 seconds after the request. Until that round is published, nobody can know or choose the result: not the requester, the operator, the sequencer or drand. The keeper then delivers the operator's VRF output over the request's fixed fields and the round's randomness. The contract verifies the round's BLS signature and the VRF proof before accepting it, and anyone can replay the result from public chain data.

**Assumptions.**
- **Shared with every Robinhood Chain application:** D20DAO relies on the chain's sequencer for ordering and confirmation.
- **Shared with other verifiable-randomness services:** results assume the operator's VRF key and the sequencer act independently.
- **drand:** evmnet is run by the League of Entropy, and its signatures rest on a threshold of independent operators.

**Finality.** Results are confirmed by the sequencer within seconds. Settlement on Ethereum follows in about 15–20 minutes.

**Delivery and refunds.** A typical delivery takes 5–8 seconds. If a request is not fulfilled within 60 seconds, its fee can be refunded to the request's refund address. As with any randomness service, design applications so that a party cannot cancel a request and retry for a better outcome.

**Upgrades.** The coordinator is upgradeable (UUPS) by its owner; read `owner()` from the chain. Beacon changes are scheduled on chain at least 10 minutes ahead.

**Developer note.** Send requests as normal transactions through the sequencer. A request forced through Ethereum's delayed inbox takes an earlier timestamp and may bind a round that is already public.

The keeper acts on the sequencer's head and checks what it acted on against Ethereum finality, recovering by itself if the sequencer replaces a block ([keeper README](../keeper/README.md)).

## Gas

### How it is measured

`test/robinhood/Gas.test.ts` measures every fulfilment shape on Hardhat's simulated chain (EDR, Osaka gas schedule) with `MockArbSys`, through `D20Proxy`, with consumers whose callbacks burn their whole gas limit. It finds the smallest gas limit of each shape by binary search, then sends the fulfilment at exactly that limit. Run it with `ROUND_GAS_TABLE=1` to print the table. All figures are L2 gas; Robinhood's L1 component is separate. Robinhood's own schedule can differ from EDR's (the design review measured its drand hash-to-curve higher than a local node did), so the allowance below is sized from Robinhood testnet's own figures.

Four things move a fulfilment's gas besides its shape:

| What | Cost |
|---|---|
| The callback | its whole gas limit, when it burns it |
| The VRF proof's hash-to-curve | about 5,480 for each candidate after the first; half of the seeds need one, a quarter two, and the keeper's prover counts them (`vrfHashToCurveCandidates`) |
| The drand round's hash-to-curve | `roundMessage` costs 66,843 to 76,900 (as `eth_estimateGas`), depending on the round. The costliest path, both field elements through the second Shallue-van de Woestijne candidate with both `y` negated, is measured with real rounds (`test/fixtures/robinhood/drand-evmnet-costliest-rounds.json`) |
| `earnedFees` at zero | 17,097 more, once, for the first fulfilment after `withdrawFees` |

The first fulfilment of a deployment costs about 52,000 more, once, while it writes its counters from zero.

Requests, through a consumer contract at the exact fee: 188,037 raw, 258,532 mapped (`d20`).

### Table

Gas used / smallest gas limit, by callback gas limit, with the VRF candidates and drand rounds the run met:

| Shape | 30,000 | 50,000 | 100,000 | 1,000,000 |
|---|---|---|---|---|
| single, first in its round | 481,729 / 1,044,583 | 490,723 / 1,065,246 | 535,040 / 1,116,846 | 1,440,671 / 2,045,621 |
| single, round cached | 258,655 / 396,236 | 273,185 / 411,319 | 323,193 / 462,927 | 1,228,651 / 1,397,271 |
| single, first in its round, fees just withdrawn | 487,242 / 1,044,607 | 513,297 / 1,065,234 | 585,139 / 1,116,834 | 1,453,041 / 2,045,645 |
| single, first in the costliest round | 481,187 / 1,044,607 | 501,183 / 1,065,222 | 556,695 / 1,116,822 | 1,445,720 / 2,045,633 |
| single, first in the costliest round, fees just withdrawn | 487,346 / 1,044,583 | 523,767 / 1,065,222 | 562,824 / 1,116,846 | 1,462,820 / 2,045,621 |
| batch of 2 over 1 round | 668,239 / 1,484,896 | 706,889 / 1,526,151 | 807,527 / 1,629,363 | 2,608,256 / 3,486,960 |
| batch of 2 over 2 rounds | 883,761 / 1,904,505 | 913,882 / 1,945,820 | 1,008,169 / 2,049,044 | 2,813,581 / 3,906,605 |
| batch of 4 over 1 round | 1,066,650 / 2,372,956 | 1,190,358 / 2,455,490 | 1,334,421 / 2,661,939 | 4,940,970 / 6,377,108 |
| batch of 4 over 2 rounds | 1,240,961 / 2,792,614 | 1,347,066 / 2,875,184 | 1,559,295 / 3,081,573 | 5,163,405 / 6,796,779 |
| batch of 8 over 1 round | 1,824,087 / 4,149,109 | 1,985,367 / 4,314,213 | 2,406,045 / 4,727,027 | 9,623,471 / 12,157,343 |
| batch of 8 over 2 rounds | 2,048,618 / 4,568,817 | 2,230,509 / 4,733,837 | 2,618,141 / 5,146,687 | 9,787,315 / 12,577,074 |
| batch of 16 over 1 round | 3,414,162 / 7,701,488 | 3,739,590 / 8,031,612 | 4,550,616 / 8,857,336 | over 16,777,216 (11 fit) |
| batch of 16 over 2 rounds | 3,607,686 / 8,121,139 | 3,988,037 / 8,451,371 | 4,727,695 / 9,276,903 | over 16,777,216 (10 fit) |
| batch of 8 over 8 rounds | 3,238,757 / 7,086,804 | | | |
| batch of 16 over 16 rounds | 6,500,512 / 13,996,440 | | | |

The same runs taken apart, with one VRF candidate per proof and the cheapest drand hash-to-curve:

| Part | Gas |
|---|---|
| single of a cached round, less its callback (the keeper sends the signature anyway) | 223,193 |
| verifying the round in a single: BLS check, cache write, `RoundVerified` | 207,047; 217,104 with the costliest hash-to-curve |
| batch member, less its callback | 161,041 to 161,371 |
| listed round a batch verifies | about 195,300; 205,350 with the costliest hash-to-curve; and about 226 for each member and listed round |

### Gas limit

Every guard runs in the implementation, behind the proxy's `DELEGATECALL`, which forwards at most 63/64 of the gas left (EIP-150): each unit a guard asks for costs 64/63 of a unit of the transaction's limit. A round still to verify asks for `ROUND_VERIFY_GAS_NEEDED` = 411,349, which costs 417,878 of the limit.

- Single, round to verify (guarded before anything runs): `64/63 × (140,000 + cb + cb/63 + 400,000 + 411,349)` plus about 47,200 for the intrinsic gas, the calldata and the work before the guard.
- Single, round cached (guarded only before its callback, after the proof's check): `64/63 × (cb + cb/63 + 140,000)` plus about 217,500, 5,570 for each further VRF candidate and, from the same first write of `earnedFees`, about 17,100 after `withdrawFees`.
- Batch: `64/63 × (140,000 + Σ(cb + cb/63 + 400,000) + rounds × 411,349)` plus about 34,300 and the calldata's gas (about 6,650 a member and 1,700 a further listed round).

`roundFulfilmentGasLimit` in `scripts/lib/round-gas.ts` gives a limit at most 6,000 above each measured one. The keeper still sends with `eth_estimateGas`, which on Robinhood also covers the L1 component.

### Fee gate bounds

The keeper's fee gate prices a fulfilment on the gas it can use, not on `eth_estimateGas`: the estimate includes callback reserves (140,000, `cb/63`, and 400,000 a member) that are never spent. `scripts/lib/round-gas.ts` holds the bounds; the gas test fails if a measurement exceeds its bound, or if a bound is more than 3,000 plus 0.5% above it.

| Constant (`ROUND_GAS`) | Bound | Covers |
|---|---|---|
| `single` | 223,400 | a single of a cached round, less its callback |
| `singleRound` | 217,300 | its round, when the single verifies it |
| `batch` | 79,500 | a batch's fixed part |
| `batchMember` | 161,400 | each member a batch serves |
| `batchRound` | 205,600 | each listed round |
| `batchMemberRound` | 250 | each member for each listed round |
| `vrfCandidate` | 5,500 | each VRF hash-to-curve candidate after a proof's first |
| `feesWithdrawn` | 17,200 | once, when `earnedFees` may be zero |
| `ROUND_ROBINHOOD_ROUND_EXCESS` | 17,000 | each verified round: Robinhood testnet's hash-to-curve above a local node (+14,200 to +17,000 measured) |

The worst-case drand hash-to-curve is in `singleRound` and `batchRound`.

```
single:  gas = 223,400 + R × (217,300 + 17,000) + cb + 5,500 × (k − 1) + 17,200
batch:   gas = 79,500 + n × 161,400 + R × (205,600 + 17,000) + 250 × n × R + Σ cb + 5,500 × Σ (k − 1) + 17,200
cost     = base fee × (gas + L1 × (1 + margin))
```

- `R` counts every round the fulfilment lists. Count a listed round even when it is cached at the decision head: a reorganisation can uncache it, and the batch then verifies it within its budget.
- `k` is a proof's VRF candidate count, from the prover. Without it, price 10: a proof needs more with probability 2⁻¹⁰.
- Drop the 17,200 only when `earnedFees` is known to be nonzero at the block the fulfilment runs in.
- `L1` is the transaction's L1 component, read live (`NodeInterface.gasEstimateL1Component`).

`roundFulfilmentGasBound` computes the gas line for a shape.

### Pricing

The fee is `max(minFee, feeMultiplier × base fee × (fulfillGasOverhead + cb))`; the keeper earns `keeperFeeBps` (8000) of it. The rule: where the dynamic fee binds, with the same base fee at request and fulfilment, the keeper's share must exceed its worst cost by 20%. In the dynamic region the base fee cancels:

```
n × 0.8 × 2 × (O + cb) ≥ 1.2 × (gas + L1)
```

The worst case priced, for every callback limit of 30,000, 50,000, 100,000 and 1,000,000, alone or in batches of 2 to 16 over 1, 2 or 16 rounds:
- every listed round still to verify, with the costliest drand hash-to-curve and the 17,000 Robinhood allowance;
- 10 VRF candidates a proof;
- fees just withdrawn;
- L1 at 25,000 for a single (Robinhood testnet measured about 20,000 to 24,000; mainnet is priced like testnet), plus 12,000 a further batch member and 3,000 a further listed round. These are L2 gas at the floor base fee, where the L1 part is largest.

The binding case is a lone request with a 30,000-gas callback: worst gas 579,400, so `O ≥ 0.75 × 579,400 − 30,000`. The smallest overhead that passes is **404,550**. The deployments initialize with 405,000, the value `FULFILL_GAS_OVERHEAD` in the test pins. An overhead of 360,000 would fail:

| Callback | Share ÷ worst cost at 360,000 |
|---|---|
| 30,000 | 1.077 |
| 50,000 | 1.094 |
| 100,000 | 1.133 |
| 1,000,000 | 1.404 |

How the smallest passing overhead moves with what is priced:

| VRF candidates priced | With the 17,000 allowance | Without it |
|---|---|---|
| 1 | 367,425 | 354,675 |
| 2 | 371,550 | 358,800 |
| 4 | 379,800 | 367,050 |
| 8 | 396,300 | 383,550 |
| 10 | 404,550 | 391,800 |

Below the crossover base fee the fee is `minFee` and the keeper's cost falls with the base fee, so the margin there is never below the crossover's. The test checks base fees from 0.01 gwei to 1,000 gwei, and that the coordinator's `quoteFeeAt` gives the fee it prices.

### Largest batch

A batch's gas limit grows by about 1,445,000 a member with 1,000,000-gas callbacks. Under EIP-7825's 16,777,216 per-transaction cap, which EDR enforces, the largest batch is:

| Callback | Over 1 round | Over 2 rounds |
|---|---|---|
| 30,000 to 100,000 | 16 | 16 |
| 1,000,000 | 11 | 10 |

The test checks the 1,000,000 rows against the chain: the model's largest batch goes through at the cap, and one more member does not.

Robinhood testnet and Robinhood Chain both report `maxTxGasLimit` 32,000,000 (`ArbGasInfo.getGasAccountingParams`, read on 6 October 2026, with `speedLimitPerSecond` 7,000,000 and `gasPoolMax` 32,000,000), which also bounds a transaction's L2 gas. Every batch of up to 16 members fits under it, even 16 members with 1,000,000-gas callbacks, each in a round of its own (about 30,000,000). A profile can record it as `gas.maxTxGasLimit`; `maxGas` and `maxDeployGas` must then fit under it.

## Deployment tooling

`scripts/deploy-robinhood.ts` deploys a set through the CREATE2 factory, only from reviewed code whose runtime code hashes `config/robinhood-create2.json` pins, journals each transaction before it is broadcast, and verifies the set it finds (code hashes, implementation slot, configuration hash, VRF key, roles, pricing and beacon). `scripts/admin-robinhood.ts` sends owner actions with the owner's key, or writes them as Safe Transaction Builder files, including the two-step ownership transfer (`transfer-ownership`, `accept-ownership`) and `set-fee-recipient`; its `status` reads the roles from the chain.

**The Safe check.** `scripts/lib/safe.ts` takes an account for a Safe only if its runtime code is a canonical SafeProxy (1.3.0, 1.4.1 or 1.5.0), its singleton (storage slot 0) is a canonical Safe or SafeL2 singleton with that singleton's code, and it answers `VERSION()`, `getOwners()` and `getThreshold()`. A production owner must also be the profile's Safe, with exactly the profile's owners and threshold, a threshold of at least 2, and no module, since a module acts without the owners' signatures. The pinned hashes and addresses are Safe's canonical deployments, each checked on Robinhood Chain and its testnet.
