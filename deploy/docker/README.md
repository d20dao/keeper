# Keeper Docker service

Native Linux container builds for **linux/amd64** and **linux/arm64**. Linux uses Docker Engine + Compose v2; Windows and macOS use Docker Desktop in **Linux container mode**. This is not a native Windows container or a Windows Service binary. Docker/its VM must be running; configure the Docker service to start on boot on a VPS, or Docker Desktop to start at login. No application port is published.

The image builds the existing Rust release binary from locked dependencies. The keeper exposes no HTTP server and uses the same protocol, journal and recovery rules as the native binary. No live credentials are included. The default config disables transaction sending; the keeper's operating rules are in [keeper/README.md](../../keeper/README.md).

## Setup

From the repository root on Windows (PowerShell):

```powershell
./deploy/docker/keeper.ps1 init
# Edit deploy/docker/keeper.env with reviewed chain/contract/key pins.
./deploy/docker/keeper.ps1 config
./deploy/docker/keeper.ps1 install 'C:\secure\transaction.key' 'C:\secure\vrf.key'
./deploy/docker/keeper.ps1 logs
```

On Linux/macOS (POSIX shell; no executable-bit dependency):

```sh
sh deploy/docker/keeper.sh init
# Edit deploy/docker/keeper.env with reviewed deployment settings.
sh deploy/docker/keeper.sh config
sh deploy/docker/keeper.sh install /secure/transaction.key /secure/vrf.key
sh deploy/docker/keeper.sh logs
```

Both scripts accept `init`, `install`, `build`, `image`, `keys`, `up`, `stop`, `down`, `restart`, `status`, `health`, `sweep`, `logs`, `config`, `migrate`; `update` is POSIX only. After configuration, `install <transaction-key> <vrf-key>` builds, imports keys and starts the service; the Linux wrapper also enables an existing Docker systemd service when permitted. Install Docker Engine and Compose first. `init` never overwrites an existing config. `config` validates Compose without printing environment values. `up` refuses template placeholders and requires an existing local image (`build`, `update` or `image` first). `restart` recreates the container so config/image changes take effect. `stop` and `down` preserve both volumes; there is deliberately no reset/delete-volume command. Without `KEEPER_INSTANCE` these commands act on the fixed Compose project `d20dao`; with it, on that named instance (see [Several keepers on one host](#several-keepers-on-one-host)).

Supply your own separate transaction and VRF key files (32-byte hexadecimal); do not use public test keys. The VRF key must match the coordinator. `CANCEL_MAX_FEE_PER_GAS_WEI` is required and must include recovery margin, per the keeper rules. Keep `SEND_TRANSACTIONS=false` until the reviewed deployment is ready for transactions. The keeper processes valid paid requests from all consumer contracts.

`KEEPER_NETWORK_MODE` selects the runtime network and defaults to `bridge`. An explicitly configured Linux host can use `host` when Docker bridge/firewall management is disabled. The process still exposes no application listener; host mode shares the host network namespace. For that installation the POSIX build wrapper accepts process environment `KEEPER_BUILD_NETWORK=host`, or an independently verified image can be loaded before running `keys` and `up`. These scripts do not configure host firewall or SSH rules.

## State, keys and ownership

- `d20dao-state-v1` (a named instance: `d20dao-<name>-state`) mounts at `/var/lib/d20dao`. It contains SQLite, WAL and **scope lock/binding metadata together**. Fixed names and fixed in-container paths prevent a checkout or Compose project rename from silently selecting a fresh journal.
- `KEYS_VOLUME` (default `d20dao-keys-v1`, or `d20dao-<name>-keys` for a named instance) mounts read-only at `/run/keeper-keys` for the keeper. The `keys` command is a separate network-disabled root provisioning container. It validates keys with the real Rust parser, refuses to replace existing different bytes, installs them with UID 10001 and mode 0400, then exits. It never prints key contents or puts them in build args, image layers or environment values.
- Import normalizes permissions inside a Docker volume rather than trusting NTFS/macOS bind-mount mode bits. On Windows, Docker Desktop shared-volume permissions can differ from Unix permissions ([Docker documentation](https://docs.docker.com/desktop/troubleshoot-and-support/troubleshoot/topics/)). Keep the original host files protected too. Docker daemon/host administrators remain trusted; these are not encrypted secret stores or HSMs.
- The runtime is UID/GID 10001, read-only root filesystem, all Linux capabilities dropped, no-new-privileges, bounded memory/PIDs/logs, and read-only key storage. The small `/tmp` tmpfs is ephemeral. Default Docker seccomp remains enabled.
- Core Tokio threads default to four, not one per host CPU, so a many-core host does not exhaust the container PID limit at startup; `TOKIO_WORKER_THREADS` in `keeper.env` (1 to 16) changes it. Proof work still uses the keeper's bounded preparation concurrency ([Tokio runtime configuration](https://docs.rs/tokio/latest/tokio/runtime/struct.Builder.html#method.worker_threads)).
- Entry point refuses `run` and `migrate` without explicit state and key mounts. No anonymous volumes are declared in the image. Run one keeper per wallet/coordinator with the same state volume; independent Docker daemons/hosts do not share locks.
- To change keys or migrate native state, stop the service and perform a reviewed migration. Import cannot silently rotate keys. Native Windows journal bindings reference Windows paths and cannot simply be copied into Linux and have their binding bypassed. There is no automatic metadata rewrite or guard bypass.

For backup, **stop the keeper first** and take a consistent backup of the entire state volume, including lock metadata and any WAL. Back up keys separately using your encrypted secret-backup policy. Restore the original complete state and keys to the same named volumes/paths while stopped. Do not restore only the `.sqlite` file, delete lock files, run `docker volume prune`, or switch to fresh volumes as a recovery shortcut. Both volumes are external to Compose, so Compose teardown does not own/delete them.

## Several keepers on one host

One host can run several keepers side by side, for example a follower for Arc Testnet and one for Arc Mainnet. Each is a named instance selected by `KEEPER_INSTANCE` in the process environment (lowercase letters, digits and hyphens, at most 40 characters, not `local` or `rollback`). Prefix every command with it so a shell never acts on the wrong keeper:

```sh
KEEPER_INSTANCE=arc-testnet-follower sh deploy/docker/keeper.sh status
```

| | Single keeper (`KEEPER_INSTANCE` unset) | Named instance `<name>` |
| --- | --- | --- |
| Compose project | `d20dao` | `d20dao-<name>` |
| Settings | `deploy/docker/keeper.env` | `deploy/docker/instances/<name>/keeper.env` |
| State volume | `d20dao-state-v1` | `d20dao-<name>-state` |
| Keys volume | `KEYS_VOLUME`, default `d20dao-keys-v1` | `KEYS_VOLUME`, default `d20dao-<name>-keys` |
| Image | `d20dao-keeper:local`, rollback `d20dao-keeper:rollback` | `d20dao-keeper:<name>`, rollback `d20dao-keeper:<name>.rollback` |
| `build` | allowed | refused |

A host that already runs a single keeper keeps working unchanged: leave `KEEPER_INSTANCE` unset and every name above stays what it was. Setting `KEEPER_INSTANCE` never adopts an existing keeper; it selects new, empty volumes and so a new journal. Moving a running keeper into a named instance is a drained migration to a new host or journal, never a rename.

Each instance has its own settings, journal, scope locks, keys, image tag, container, logs and health status. Scope locks live in each state volume, so instances cannot see each other's locks, and the scripts enforce the separation instead: `up`, `restart` and `update` refuse a state or keys volume attached to a container of another Compose project, and refuse to start when another `keeper.env` in the same checkout (the single keeper's or another instance's) configures the same `CHAIN_ID` and `COORDINATOR_ADDRESS`. Run one keeper per coordinator on a host; a primary and its follower belong on different hosts. A named instance takes `KEYS_VOLUME` only from its own `keeper.env` (a different process override is refused, because it would apply to every instance) and refuses `d20dao-keys-v1`. The scripts cannot tell whether two keys volumes hold the same key: import a separate transaction key per instance.

Named instances never build on their host. Build or publish the image elsewhere and select it by an immutable reference with `image`, which does not start or recreate the keeper:

```sh
# CI image: pulled by digest.
KEEPER_INSTANCE=<instance> sh deploy/docker/keeper.sh image ghcr.io/d20dao/keeper@sha256:<digest>
# Or copy an image built on another machine, then select it by the image ID reported on that machine.
docker save d20dao-keeper:local | ssh <user>@<host> docker load
ssh <user>@<host> docker image inspect --format '{{.Id}}' d20dao-keeper:local
KEEPER_INSTANCE=<instance> sh deploy/docker/keeper.sh image sha256:<image id>
```

Mutable tags are refused. Compare the reported ID with `docker image inspect` on the build machine; the two match when both Docker installations use the same image store. `update <digest or image ID>` switches a running instance with the same health-gated rollback as the single keeper, and `install` for a named instance requires a selected image instead of building. Images are never pulled implicitly (`pull_policy: never`, `docker run --pull never`).

Resources per instance: 512 MiB memory, 128 PIDs, a 16 MiB `/tmp`, three 10 MB log files, so n instances are bounded by n × 512 MiB and n × 128 PIDs. `TOKIO_WORKER_THREADS` in an instance's `keeper.env` sets its core runtime threads (1 to 16, default 4, unquoted). The default holds on a two-core machine: the worker threads mostly wait on RPC and gateway I/O, and VRF proofs run in a separate blocking pool of at most four at a time, about 10 ms each on a desktop CPU, so the thread count bounds the container's PIDs rather than its CPU use. Lower it when an instance shares a small machine.

Each keeper instance with its own wallet is a full service lane, and the registry allows four backup committers beside the committer, so five lanes are the contract's limit. Adding an instance is the cheapest way to add throughput: a lane serves a burst at the rate its own nonce lane allows, and the lanes divide the queue between them by the join rule rather than by coordination.

## Deploying a follower instance

A network's primary keeper runs as the single keeper of its own host, so a named instance is not needed there. Its followers run as named instances on a separate host, which may hold one follower per network side by side: each instance keeps its own Compose project, `keeper.env`, journal, keys volume and image tag, so the networks share nothing but the Docker daemon. Give the two hosts of a network different RPC endpoints, so one provider incident cannot stop both lanes, or the liveness reads they depend on, at the same time. A follower host should not build images: load or pull one by digest, as above.

```sh
export KEEPER_INSTANCE=<network>-follower   # or prefix each command
sh deploy/docker/keeper.sh init
# Replace instances/<network>-follower/keeper.env with the output of
# node scripts/keeper-env.ts --chain <network> --role follower ... (mode 0600).
sh deploy/docker/keeper.sh image ghcr.io/d20dao/keeper@sha256:<digest>
sh deploy/docker/keeper.sh config
sh deploy/docker/keeper.sh install <transaction.key> <vrf.key>
sh deploy/docker/keeper.sh logs
```

The follower's wallet must be an allowed backup committer and funded for gas; the registry owner allows it, and any external monitoring that classifies submitters should know it before it starts sending (see [the registry upgrade runbook](../../docs/registry-upgrade.md#follower-keeper-and-failover-drill)). Each further network repeats these steps with its own transaction key and that network's VRF key. Instances restart independently with the Docker service.

## Service behavior and limits

Compose uses `restart: unless-stopped` and forwards SIGTERM via its init process with a 30-second grace period. The keeper's 20-second startup/tick bounds, repeated-failure exit and durable stuck-nonce age remain active. Docker will restart an exited process; it cannot repair invalid configuration, lost keys/state, insufficient fees or an unavailable chain. The image healthcheck reads the keeper's durable SQLite progress status without HTTP or signer keys. Stalled/rejected work marks the container unhealthy while reconciliation continues; Docker does not restart solely on an unhealthy status. Use the service script `health` command and monitor structured stderr and fulfillment/refund metrics externally.

Updating from published images: each push to `main` that touches the keeper or its container runs `.github/workflows/keeper-image.yml`, which builds the linux/amd64 image, runs the container smoke and pushes it to `ghcr.io/d20dao/keeper` as `sha-<commit>` and `main`, printing the immutable digest in the run summary. Hosts therefore never compile next to a live keeper. Once per host, log Docker in to GHCR with a read-only package token (`docker login ghcr.io`). After the commit's checks pass, run `sh keeper.sh update ghcr.io/d20dao/keeper@sha256:<digest>` (prefixed with `KEEPER_INSTANCE=<name>` for a named instance): it pulls that digest, keeps the current image as `d20dao-keeper:rollback`, recreates the container once and waits up to three minutes for a healthy status, otherwise it restores the previous image. Only a digest, or the ID of an image already loaded on the host, is accepted, so the running bytes are exactly what CI tested or what was copied. The switch is a single stop/start (the current tick finishes within its 20-second bound); signed transactions are journaled before broadcast and reconciled after restart, and two keepers must never share a wallet, so no overlap is attempted. A health-triggered rollback restores the image only: journal schema changes are additive, and a backup (below) should precede risky releases.

Updating from source (no registry): stop, pull/review the intended Git revision, run `build` (use `KEEPER_BUILD_NETWORK=host` on hosts without a Docker bridge), then `restart`; state paths/volumes remain fixed. Building on the keeper host competes with the running keeper for CPU, so prefer published images or `docker save | ssh host docker load` from another machine.

## Moving keeper earnings to the treasury

The keeper share of each fee is paid to the keeper wallet at fulfillment. To move it without stopping the keeper or touching its nonce:

```sh
sh keeper.sh sweep --amount 25     # send exactly 25 USDC
sh keeper.sh sweep --keep 5        # send everything above 5 USDC, after gas
sh keeper.sh sweep --status        # queued request, transaction in flight, last result
sh keeper.sh sweep --cancel        # remove a request that has not been signed yet
```

PowerShell: `./keeper.ps1 sweep -SweepAmount 25`, `-SweepKeep 5`, `-SweepCancel`, or no option for the status.

The command only writes a request into the keeper journal; it never signs. The running keeper executes it in its next tick when its nonce lane is free (no unresolved game or epoch transaction, and the node's latest and pending nonces agree):

- The destination is always read from the coordinator's `feeRecipient()` (the treasury Safe on mainnet); it cannot be chosen on the command line.
- At least 1 USDC, or `MAX_TX_COST_WEI` when larger, plus the transfer gas stays in the wallet. A request that would go below it, or that exceeds a fee cap, is refused and nothing is signed.
- The signed transfer is committed to the journal before broadcast. Until its receipt is final the lane stays busy, so no game or epoch transaction can take its nonce; a crash rebroadcasts the identical bytes. A transfer that is not included within 30 seconds is replaced by a zero-value cancellation of its nonce (at most three times).
- Only one request can be queued or in flight. `SEND_TRANSACTIONS=false` never signs a sweep. Telegram, when configured, reports the result.

The protocol share is not in the keeper wallet: it accrues in the coordinator as `earnedFees` and the fee recipient withdraws it with `withdrawFees(recipient)` (a Safe transaction on mainnet).

## Validation

```sh
node scripts/keeper-docker-smoke.mjs
```

Run after `init` and `build`. The smoke uses public fixture keys and unique, labelled test volumes; it removes only those temporary Docker volumes/container, leaving fixture source files for inspection. It validates release-binary key parsing, import permissions/non-rotation, read-only key/root mounts, persistent journal identity across fresh containers, cross-container journal locking, refusal of ephemeral state and Compose policy. RPC is deliberately unavailable and sending disabled; no public transaction or paid API is performed. It does **not** claim an end-to-end live Arc settlement test. Native keeper integration tests cover actual local proof/settlement and SIGTERM behavior separately.

CI builds and runs the container smoke on native amd64 and arm64 Linux runners. PowerShell init/config and amd64 image smoke are also exercised locally on Windows Docker Desktop. macOS host automation is POSIX shell; it has not been manually tested on a physical Mac. See [GitHub runner support](https://docs.github.com/en/actions/reference/runners/github-hosted-runners) for native runner platforms.

## Drained identity migration

Keep the instance's fixed state volume (`d20dao-state-v1` for the single keeper) and `/var/lib/d20dao/locks` throughout migration. `KEEPER_DB` may select a new absolute database path inside that mounted volume; the entry point canonicalizes it and rejects traversal or symlinks outside the volume. Do not change/delete lock or retirement marker files, reuse a retired database, or select fresh state to bypass identity binding.

1. Quiesce request ingress. Let the old keeper finish outstanding work and reconcile every pending nonce; the source database must be drained.
2. Stop the keeper with `stop`. Back up the entire state volume, including locks, markers and WAL, and back up keys separately.
3. For transaction-wallet rotation, have the registry owner authorize the new committer before migration. Configure the destination `KEEPER_DB` (a NEW file in the same state volume), reviewed coordinator and sender/key pins in `keeper.env`. Set `KEYS_VOLUME` to a separately provisioned NEW volume name, then use `keys` with the destination transaction key and unchanged VRF key. Existing-volume key replacement remains prohibited.
4. Run `migrate` with `prepare`, inspect its report, then explicitly run `apply`. An interrupted migration uses `resume` with the same source and destination configuration. The wrapper never stops a running keeper or automatically applies a migration.
5. Start with `up` only after successful migration and review; resume ingress after health verification.

```powershell
./deploy/docker/keeper.ps1 migrate -FromDb /var/lib/d20dao/keeper.sqlite -MigrationMode prepare
./deploy/docker/keeper.ps1 migrate -FromDb /var/lib/d20dao/keeper.sqlite -MigrationMode apply
```

```sh
sh deploy/docker/keeper.sh migrate /var/lib/d20dao/keeper.sqlite prepare
sh deploy/docker/keeper.sh migrate /var/lib/d20dao/keeper.sqlite apply
```

Source paths are container paths. Migration runs a one-off container using the destination configuration and selected keys. It does not import native Windows database bindings. `KEYS_VOLUME` must be an unquoted Docker name containing only letters, digits, underscores, dots or hyphens, beginning with a letter or digit. Both wrappers read only that setting (never execute the env file), validate it, and pass it consistently to provisioning and Compose. A process environment value takes precedence over `keeper.env`; an empty value uses the default. Compose uses explicit `--env-file keeper.env` for interpolation. Avoid leaving an old process override set when selecting new keys.
Optional reporting: place `HEALTH_API_URL`, `HEALTH_API_KEY` and optionally `HEALTH_INTERVAL_SECONDS` in private `keeper.env`, then recreate the container with the script `restart` command. The keeper POSTs to the configured site endpoint; no port or inbound health API is opened. Treat the env file as a secret, never commit it, and configure an endpoint authorized for any retained history. The outbox resides in the state volume and survives container recreation/migration. See [receiver specification](../../docs/keeper-health-receiver.md).
