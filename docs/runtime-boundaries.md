# Runtime boundaries

The VPS keeper opens no incoming application port. Its outbound connections are verified chain RPC, one background epoch API fetch, optional authenticated heartbeat delivery and optional Telegram send/poll requests. VRF and transaction keys remain private and separate. Only the shared durable nonce lane signs transactions.

Snapshots are prepared locally for 200-block epochs. A paid request can escrow before publication; the keeper publishes only for live paid demand, then proves a block hash strictly after commitment. Idle local snapshots are not chain availability failures. Expired requests are never retried; refund transactions remain available.

The public website imports only replay code and reads chain evidence. It does not connect to the keeper, hold signer keys or require a private API-response archive. Source attestations and accepted VRF proofs are archived in separate chain events.

Heartbeat and Telegram are observations, not settlement evidence. Telegram is optional and best-effort, uses bounded queues and a separate status observer, and accepts read-only /status and /keeper commands only from its configured chat. Stale observations are shown as unknown. Bot errors cannot block transaction processing.

Both service contracts are upgradeable proxies. Runtime identity includes the current implementation of each proxy; checking only the proxy's code hash is insufficient. Ownership/role changes and upgrades are separate from moving a VPS with unchanged keys and a consistent journal backup.
