# Drained journal migration / transaction-wallet rotation

This is an offline ownership transition, not a live failover or a way to reset randomness. It does not fetch epoch rounds or broadcast transactions. The original DB is preserved read-only and permanently retired using a sidecar; the new DB retains complete logical source snapshots in `migration_archive_meta`, `migration_archive_jobs`, `migration_archive_txs` and `migration_archive_batch_members` (the request members of batched fulfillment attempts).

Stop new requests at the old consumer application, let all signed transactions resolve using the old keeper, then stop that keeper. Wait until the newest old-coordinator request deadline has passed. Migration conservatively checks the whole old coordinator's newest deadline, so continued public request ingress can prevent migration. Quiescing ingress is an operator prerequisite; a snapshot check cannot prevent requests arriving afterwards.

Provision the destination DB's parent directory and separate reviewed key files. Configure destination `KEEPER_DB` (a new absolute path), chain/coordinator, protocol/proxy/implementation pins, VRF key and transaction key. The destination registry must authorize that transaction wallet as its committer; a follower keeper's journal (a backup committer wallet) cannot be migrated with this command. Live epoch preparation and unresolved maintenance transactions must drain along with randomness requests. Keep the same scope-lock directory. Migration across chains, overwriting a DB, pending old/new wallet nonces, unresolved signed attempts, live saved work, mismatched source database identity and inconsistent RPC observations are rejected. All configured RPC endpoints must be available and agree during this administrative operation; use a reviewed healthy endpoint set.

```text
d20dao-keeper migrate --from /absolute/old.sqlite --prepare
d20dao-keeper migrate --from /absolute/old.sqlite --apply
```

`--prepare` validates and prints the proposed transition without creating the destination DB or changing ownership; it reserves filesystem locks for its inspection. Review the printed chain, paths, addresses, public-key/configuration pins and nonces. `--apply` revalidates the current destination configuration and source state before any ownership transition. No hidden force/bypass flag exists.

The transition uses synced, atomically published records and blocked-startup markers. It copies a consistent SQLite snapshot to a staging file, initializes and verifies the destination, revokes old ownership, installs new bindings, records completion, then permits destination startup. The original source marker remains as its retirement guard. Multiple binding writes are **not** one filesystem-atomic operation; guards and a durable recovery record keep partial transitions from starting normally.

If interrupted, keep ingress and both keepers stopped and use the identical original source path and destination configuration:

```text
d20dao-keeper migrate --from /absolute/old.sqlite --resume
```

An intact matching intent can resume intermediate steps, including repairing torn binding writes after verifying the source, both guards and initialized destination. Unrelated bindings, corrupt intent, changed chain nonces/configuration or unexpected destination state fail closed. Do not erase or manually rewrite locks, markers, staging snapshots or intent to bypass a refusal; preserve them for reviewed recovery. Immediate identical apply/resume retries are idempotent. Do not rerun a completed migration after the destination has started sending: its on-chain nonce may have advanced and the old plan will correctly refuse replay.

The source database's request, epoch and transaction history is retained in the migration archive. A new coordinator begins discovery independently. Health and old wallet-age state are archived before relevant operational state is cleared. The registry owner must rotate the committer onchain before a destination-wallet migration. Changing only a local key file does not grant authorization. These actions do not reroll a request on its original coordinator.

A soft-finality source (`FINALITY_MODE=soft`) is refused while it owes its audit anything: a mismatch on record, acknowledged or not, or marks beyond the head mark in blocks the sequencer can still replace. Changing the sender deletes the signed transactions of unaudited receipts, which the recovery from a replaced block broadcasts again. A mark the audit has not checked yet owes nothing once its block is at or below the chain's `safe` head (in a batch posted to L1) and the chain has the mark's hash for it; the migration reads both from the configured endpoints. Keep the old keeper running with its ingress stopped until that is so for every mark, or `d20dao-keeper finality --db <old-db> --status` lists no mismatch and no marks beyond the head mark (the keeper recovers from a mismatch by itself), then prepare and apply. The check is the same for `--prepare`, `--apply` and `--resume`.

So the migration waits for the chain's batches to reach L1, not for L1 to finalize them: a stall of L1 finality (`finality_audit_stalled` in `health`) does not block it. While the batches do not reach L1, the marks above the safe head stay owed and the migration is refused, for as long as that lasts; there is no override. The old keeper keeps serving meanwhile. A rotation of the transaction wallet that cannot wait for that is outside this drained-only tool.

The CLI requires same-directory hard-link support for atomic no-replace record publication and local filesystem locking. Unsupported filesystems fail closed. Use consistent backups including applicable WAL, scope metadata and migration sidecars. An instance ID proves identity, not backup freshness; an honest old backup can still have stale observations.

Docker users must keep the existing state volume and select the destination DB **inside it**. Provision a separate keys volume for rotation; the key importer will not replace existing different key bytes. Follow the Docker guide's migration steps. No image/DB/key migration is launched by this repository's tests or CI against a real service.

Validation includes every persistent transition phase across wallet/coordinator/path changes, torn/empty binding recovery with intact intent, refusal of corrupt/unrelated records, canonical-path startup guards and archive preservation. Local EVM integration also exercises unresolved/live-work refusal, actual CLI prepare/apply/resume, old-startup rejection, rotated-wallet delivery and new-coordinator delivery. Emergency takeover with unresolved transactions or a changing compromised wallet is outside this drained-only tool.
