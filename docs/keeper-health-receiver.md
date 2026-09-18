# Optional outbound keeper reports

Set `HEALTH_API_URL` and `HEALTH_API_KEY` together to enable an outbound POST to your receiver. Leave both unset or blank to disable delivery; whitespace-only values count as blank, and supplying only one nonblank value is an error. The keeper exposes no incoming API or port. The existing private SQLite `health` command remains available independently.

URLs require HTTPS, with numeric loopback HTTP permitted only for local chain 31337. URL credentials, query strings and fragments are rejected. The key is sent as `Authorization: Bearer <key>`, marked sensitive in the HTTP client, and never included in reports. Redirects and environment HTTP proxies are disabled. Do not enable third-party HTTP trace logging in environments containing credentials. Endpoint/key changes take effect on restart; pending reports are delivered to the newly configured endpoint, so use only a receiver authorized to receive the retained history.

`HEALTH_INTERVAL_SECONDS` defaults to 30 and accepts 5–3600 seconds, or 1–3600 on chain 31337. Each attempt has a three-second HTTP timeout and a four-second total budget. Sending runs in a separate background task, outside randomness ticks. Errors only log a fixed delivery-state transition; they do not log URLs, headers, response bodies or payloads. The response body is discarded without reading it. Normal shutdown can cancel sending; the persisted report remains retryable. A short `run --once` does not guarantee report delivery.

## Receiver contract

1. Accept POST JSON up to 64 KiB over HTTPS. Validate Bearer authentication using constant-time comparison against a privately stored credential. Authorize the credential for its expected node identities and origins. Never put this key in the public frontend.
2. Validate `version: 1` and the expected types/limits. Require the `Idempotency-Key` header to equal `reportId`. Store a globally unique `reportId` plus the exact payload/hash transactionally with the ingested events. Repeating that ID with identical bytes must succeed without creating duplicate activity; conflicting bytes must be rejected.
3. Return any 2xx status **only after durable storage commits**, including for a duplicate already stored. The keeper treats the status as the complete acknowledgement. No response JSON is necessary. Non-2xx, connection errors, timeout, lost acknowledgement and restart cause the same stored report ID and exact payload to be retried.
4. Serve a separate sanitized public read model. Escape text, map known `kind` codes to your own labels, validate public addresses/request IDs, and never render uploaded HTML. Reports are keeper observations, not cryptographic proof of onchain fulfillment. Verify chain evidence separately before making fairness/payment claims.

The envelope has `version`, `nodeId`, `chainId`, `coordinator`, `reportId`, `observedAt`, `health`, `summary`, `events`, `nextCursor`, `droppedCount`, `droppedTotal`, and `rejectionHistoryPrunedTotal`. Timestamps are Unix seconds. `nodeId` is the Keccak-256 hash of the public `chain:coordinator:transaction-sender` scope; it is not an authentication credential. `health` includes its own observation timestamp, healthy/sendEnabled flags, fixed fault codes, `role` (`primary` or `follower`; a primary and its follower report under different `nodeId` values) and, for a follower, `primaryAlive`: whether the registry committer's confirmed nonce advanced within its liveness window. A retained retry intentionally contains its original observation timestamp: the receiver must display staleness rather than treating delivery time as current health. Missing initial observations are unhealthy (`not_observed`); in that bootstrap shape `health.observedAt` and `health.sendEnabled` are absent. The receiver must accept that shape (do not reject the first durable report for missing observation fields), and display the node as not yet observed.

`chainId`, `nextCursor`, event `cursor`, and non-null `requestId` are decimal strings to preserve integer precision in JavaScript. Timestamps, counts, and `version` are JSON numbers.

Example shape (addresses/IDs abbreviated; these are not deployment values):

```json
{
  "version": 1,
  "nodeId": "0x<SCOPE_HASH>",
  "chainId": "5042002",
  "coordinator": "0x<COORDINATOR>",
  "reportId": "<STABLE_REPORT_ID>",
  "observedAt": 1789420000,
  "health": {"observedAt":1789420000,"healthy":true,"sendEnabled":true,"faults":[],"role":"primary","primaryAlive":null},
  "summary": {"completed":1,"rejected":0,"failed":0,"progress":0},
  "events": {
    "completed": [{"cursor":"101","requestId":"42","kind":"served","origin":"<CHAIN:COORDINATOR:SENDER>","observedAt":1789420000}],
    "rejected": [],
    "failed": [],
    "progress": []
  },
  "nextCursor": "102",
  "droppedCount": 0,
  "droppedTotal": 0,
  "rejectionHistoryPrunedTotal": 0
}
```

`summary` contains counts for the current batch, not cumulative request counts. `events` has four arrays:

| Group | Meaning |
| --- | --- |
| completed | `served` (proof accepted; callback success is not implied), `refunded` |
| rejected | Kept for the version-1 envelope and always empty from current keepers: consumer admission is public, so nothing produces a rejection. A journal from the allowlist pilot may still deliver queued `not_allowlisted` or `ignored` events here. |
| failed | `callback_failed_at_acceptance`, `expired`, `blocked`, `node_transaction_rejected`, `service_degraded` |
| progress | `discovered`, `prepared`, `service_recovered` |

Each event has a monotonic local `cursor`, optional public `requestId`, fixed `kind`, `observedAt`, and `origin` (the original public chain/coordinator/sender scope). Group by `origin`, not the envelope coordinator: explicit migration preserves old events and the exact pending report, while future envelopes describe the destination scope. Scope migration changes `nodeId`; authorize it at the receiver before restart, while still accepting the preserved old pending envelope. The receiver can additionally deduplicate events by `(origin, cursor)` only within a known journal lineage; report-ID deduplication is always required because a fresh journal can reset its cursor. Journal recreation with the same scope needs distinct receiver lineage handling.

`callback_failed_at_acceptance` records fulfilled-but-undelivered observed during reconciliation. Completed jobs are not continuously polled; later manual callback retries must be read from chain. Reverted blockchain transactions that never create a coordinator request cannot be discovered through the pending-request scan. Node transaction rejections are reported separately and never labeled as consumer-request rejection. Historical requests skipped by discovery's existing expired-prefix optimization are not individually reconstructed in telemetry.

`service_degraded` records an assessed unhealthy status or a change in its faults; `service_recovered` records an assessed unhealthy-to-healthy transition. Starting or clearing the preparation/settlement timers during routine successful work creates neither event. Node rejection events remain separate from service status transitions and contain no raw rejection reason. During explicit migration, `audit:suspended=1` suppresses status and node-rejection audit triggers while metadata is reset; deleting health metadata alone never emits a recovery event.

## Durability and bounds

Job transitions and their event inserts are atomic through SQLite triggers. Repeated unchanged states and blocked observations are deduplicated. Existing jobs are not retrospectively reconstructed when telemetry is added. One durable outbox holds at most 128 events and a 64-KiB payload; it is committed before network delivery. The acknowledgement transaction advances the cursor and removes only acknowledged events. No job payloads, calldata, seeds, signed responses, full proofs, keys, database paths or arbitrary error text are reported.

The audit event queue retains at most 4096 rows even when delivery is disabled. At capacity it preserves older unacknowledged rows and drops new audit events, counting every loss in `droppedTotal`; `droppedCount` is the count since the prior acknowledged report's snapshot. This bounds telemetry storage while making prolonged-outage loss visible. Overflow does not discard the pending report. `rejectionHistoryPrunedTotal` is kept for the version-1 envelope: it reports the pruning count of the allowlist pilot's ignored-request history, which current keepers no longer keep, so it stays at its last value (0 for journals created since). These counters are cumulative and survive restart/migration. This bounds telemetry records, not the existing randomness journal, SQLite file high-water allocation, or operator-managed logs.

Keep all `audit_*` tables, `telemetry_outbox`, relevant `meta` counters and job callback observation fields in explicit journal migration/backup. Never reset the acknowledgement cursor or discard the outbox to resolve delivery failures. Restore consistent whole-database backups. The receiver should alert on stale observations, dropped counts and authentication failures, without making receiver availability a dependency of randomness processing.
