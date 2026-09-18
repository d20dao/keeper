# Compact evidence

Randomness fulfillment is `requestId + VRF proof`: 452 bytes of calldata, including the function selector. Its non-indexed FulfillmentEvidence packet is exactly 416 bytes. Only one schema is accepted; extra/truncated bytes are rejected by the public decoder.

The registry emits the full epoch packet once in EpochCommitted: canonical query string plus the original timestamp, data bytes and Airnode signature. Hyperliquid's BTC volume samples are about 48 data bytes and its SOL mid price about 17; the Ethereum and Base block-hash JSON-RPC envelopes are always 105 bytes; Nodary feed records are about 80 bytes. TickerLayer BTCUSD and ETHUSD each contribute their complete signed trade record, including exact numeric notation, within the same 128-byte data bound. The registry rejects larger signed data and packets above 2048 bytes. It never trims signed data itself.

Stored epoch commitments bind catalog, registry/chain, preparation anchor, selected source/query and signed data. A website can reconstruct all computations from these events and trusted chain context. Keeping the full API packet out of every randomness fulfillment reduces repeated calldata without relying on an offchain archive.

## Local journal compaction

The keeper runs transactional compaction at most once every 60 seconds, clearing at most 128 eligible rows per table per pass. It does not delete rows. Terminal request jobs lose large proof/calldata bodies; committed or irreversibly expired epochs lose API/selection bodies; fully resolved transaction attempts lose raw signed bytes and payloads. Unresolved attempts protect their job/epoch payloads, and any unresolved attempt for the same nonce or job protects transaction replay bytes. Active proofs, the first validated epoch packet, replacements and cancellations remain recoverable through retries and restarts.

Small identity, state and receipt metadata remains: job IDs/deadlines, epoch key/registry/catalog/ID/start and diagnostics, and transaction hashes/nonces/times. Audit events and the pending telemetry outbox are unchanged. Committed evidence is recoverable from chain logs; an expired unpublished epoch never had a chain packet, so its discarded raw response cannot be recovered there.

Unused local snapshots retire after 50 epochs (10,000 blocks) only when demand and nonce guards permit. Published terminal payloads can be compacted sooner. No history rows are deleted; identity and receipt metadata remains. SQLite can reuse freed space; compaction does not promise an immediate smaller database file. Chain logs remain immutable, and local cleanup neither deletes onchain events nor refunds previously spent gas.
