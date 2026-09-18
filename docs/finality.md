# Keeper finality boundary

The transaction keeper uses finalized contract state for durable request classification. Discovery reads one finalized block snapshot, and receipt reconciliation checks the transaction hash, finalized height and canonical receipt block hash before resolving the nonce. Finalized receipt coordinates and a monotonic block checkpoint survive restart. If a previously finalized checkpoint changes, processing stops for operator investigation; the daemon never blindly lowers its nonce floor or rerolls a request.

The current approved deployment is Arc Testnet. Arc documents deterministic BFT finality and states that its `safe` and `finalized` RPC tags resolve to committed blocks. The configured three Arc endpoints were checked for `finalized` support. See [deterministic finality](https://docs.arc.io/arc/concepts/deterministic-finality) and [infrastructure finality guidance](https://docs.arc.io/integrate/infrastructure/bridges).

This relies on honest, correctly configured RPC finality reporting and the selected chain's consensus assumptions. A reversed finalized block is a finality/RPC incident, not an ordinary retry. Preserve the journal and signed attempts, verify the canonical history with approved providers, and investigate before recovery. A database reset or nonce-floor edit is not a safe workaround.

The code uses standard EVM interfaces, but another chain is not approved merely because those interfaces exist. Its finality latency must fit the contract's 60-second acceptance window; slow probabilistic-finality networks need a reviewed operating/protocol policy before deployment. A VRF target confirmation count and settlement finality are separate requirements.

The public Explorer index is separate: it stages large request refreshes in bounded durable pages and still repairs ordinary indexed-history reorganizations. Its block-end implementation observations do not establish transaction-level code attribution when upgrades occur inside a block.
