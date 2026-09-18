//! Drained-only, offline ownership transition. No API sampling or broadcasting.
//! Markers are a fail-closed write-ahead protocol, not an atomic multi-file rename.
use crate::{
    abi::{Coordinator as C, EpochRegistry as E},
    config::Config,
    journal::Journal,
    prover,
    rpc::{Rpc, quantity},
};
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_signer_local::PrivateKeySigner;
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteSynchronous},
};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Prepare,
    Apply,
    Resume,
}

pub fn parse_args(args: &[String]) -> Result<(PathBuf, Mode)> {
    ensure!(
        args.len() == 3 && args[0] == "--from",
        "Usage: migrate --from <old-db> --prepare|--apply|--resume"
    );
    let mode = match args[2].as_str() {
        "--prepare" => Mode::Prepare,
        "--apply" => Mode::Apply,
        "--resume" => Mode::Resume,
        _ => bail!(
            "Migration requires exactly --prepare, --apply, or --resume; no bypass flags supported"
        ),
    };
    Ok((PathBuf::from(&args[1]), mode))
}
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(suffix);
    value.into()
}
pub fn ensure_startable(path: &Path) -> Result<()> {
    // Match lease::bind's canonical identity, including when the operator uses
    // a symlink to the original journal during interrupted-transition recovery.
    let canonical = if path.try_exists()? {
        std::fs::canonicalize(path)?
    } else {
        std::fs::canonicalize(path.parent().context("Journal parent missing")?)?
            .join(path.file_name().context("Journal filename missing")?)
    };
    ensure!(
        !sidecar(&canonical, ".migration-blocked").try_exists()?,
        "Journal is migration-pending or retired. Keep ingress stopped; use the identical destination configuration and migrate --from <original-db> --resume. Never delete migration markers or scope locks"
    );
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Plan {
    version: u8,
    from: PathBuf,
    to: PathBuf,
    chain: u64,
    old_coordinator: Address,
    new_coordinator: Address,
    old_sender: Address,
    new_sender: Address,
    old_instance: String,
    new_instance: String,
    old_nonce: u64,
    new_nonce: u64,
    code_hash: B256,
    protocol_hash: B256,
    runtime_pins: crate::proxy::RuntimePins,
    public_key: [U256; 2],
}
impl Plan {
    fn scope(&self, old: bool) -> String {
        if old {
            format!(
                "{}:{}:{}",
                self.chain, self.old_coordinator, self.old_sender
            )
        } else {
            format!(
                "{}:{}:{}",
                self.chain, self.new_coordinator, self.new_sender
            )
        }
    }
    fn binding(&self, old: bool) -> Value {
        json!({"version":1,"journal":if old {&self.from} else {&self.to},"instance":if old {&self.old_instance} else {&self.new_instance}})
    }
    fn tombstone(&self) -> Value {
        json!({"version":2,"retired_by_migration":self.new_instance,"destination":self.to})
    }
}
fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    for directory in path
        .parent()
        .context("Missing parent directory")?
        .ancestors()
    {
        File::open(directory)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn create_record(path: &Path, plan: &Plan) -> Result<()> {
    // Publish only complete, synced bytes. Hard links are atomic and cannot
    // replace an existing record; unsupported filesystems fail closed.
    let bytes = serde_json::to_vec(plan)?;
    let mut sequence = 0u64;
    let (temporary, mut file) = loop {
        let temporary = sidecar(path, &format!(".pending-{sequence}"));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                sequence = sequence
                    .checked_add(1)
                    .context("Too many pending records")?;
            }
            Err(error) => return Err(error.into()),
        }
    };
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::hard_link(&temporary, path).context("Atomic migration record publication failed")?;
    sync_parent(path)?;
    std::fs::remove_file(temporary)?;
    sync_parent(path)
}
fn check_record(path: &Path, plan: &Plan) -> Result<()> {
    let actual: Plan = serde_json::from_slice(&std::fs::read(path)?)
        .context("Partial migration marker: fail-closed recovery requires restoring the original intact migration record from backup; never erase markers")?;
    ensure!(
        &actual == plan,
        "Migration marker belongs to a different transition; refusing recovery"
    );
    Ok(())
}
fn ensure_record(path: &Path, plan: &Plan) -> Result<()> {
    if path.try_exists()? {
        check_record(path, plan)
    } else {
        create_record(path, plan)
    }
}
fn journal_lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))?;
    FileExt::try_lock_exclusive(&file).context("Another keeper holds a migration journal lock")?;
    Ok(file)
}
struct ScopeLock {
    file: File,
    old: bool,
    new: bool,
}
fn scope_locks(dir: &Path, plan: &Plan) -> Result<Vec<ScopeLock>> {
    std::fs::create_dir_all(dir)?;
    let mut paths = BTreeMap::new();
    for (old, coordinator, sender) in [
        (true, plan.old_coordinator, plan.old_sender),
        (false, plan.new_coordinator, plan.new_sender),
    ] {
        for (kind, address) in [("wallet", sender), ("coordinator", coordinator)] {
            let flags = paths
                .entry(dir.join(format!("{}-{kind}-{address:x}.lock", plan.chain)))
                .or_insert((false, false));
            if old {
                flags.0 = true;
            } else {
                flags.1 = true;
            }
        }
    }
    paths
        .into_iter()
        .map(|(path, (old, new))| {
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)?;
            FileExt::try_lock_exclusive(&file)
                .context("Another keeper holds an old or destination scope")?;
            sync_parent(&path)?;
            Ok(ScopeLock { file, old, new })
        })
        .collect()
}
fn read_binding(file: &mut File) -> Result<Option<Value>> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if bytes.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_slice(&bytes).context("Partial scope binding: fail closed; preserve all files for reviewed recovery; never edit or delete a lock")?))
    }
}
fn write_binding(file: &mut File, value: &Value) -> Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&serde_json::to_vec(value)?)?;
    let length = file.stream_position()?;
    file.set_len(length)?;
    file.sync_all()?;
    Ok(())
}
fn validate_bindings(locks: &mut [ScopeLock], plan: &Plan, resume: bool) -> Result<()> {
    for lock in locks {
        let actual = read_binding(&mut lock.file)?;
        let valid = if lock.old {
            actual.as_ref() == Some(&plan.binding(true))
                || (resume
                    && (actual.as_ref() == Some(&plan.tombstone())
                        || (lock.new && actual.as_ref() == Some(&plan.binding(false)))))
        } else {
            actual.is_none() || (resume && actual.as_ref() == Some(&plan.binding(false)))
        };
        ensure!(
            valid,
            "Source identity does not match existing scope bindings, or destination belongs to another journal; migration refused"
        );
    }
    Ok(())
}
// Only a fully identified transition that has reached the binding phase may
// repair torn writes. Inspect every binding first so unrelated valid ownership
// is never overwritten, even if a different lock is corrupt.
async fn recover_bindings(source: &Journal, locks: &mut [ScopeLock], plan: &Plan) -> Result<()> {
    let (chain, coordinator, sender, instance) = identity(source).await?;
    ensure!(
        (chain, coordinator, sender, instance)
            == (
                plan.chain,
                plan.old_coordinator,
                plan.old_sender,
                plan.old_instance.clone()
            ),
        "Recovery source identity mismatch"
    );
    check_record(&sidecar(&plan.to, ".migration.json"), plan)?;
    check_record(&sidecar(&plan.from, ".migration-blocked"), plan)?;
    let complete = sidecar(&plan.to, ".migration-complete");
    if complete.try_exists()? {
        check_record(&complete, plan)?;
    }
    check_record(&sidecar(&plan.to, ".migration-blocked"), plan)?;
    verify_destination(&plan.to, plan).await?;
    let mut torn = Vec::new();
    for (index, lock) in locks.iter_mut().enumerate() {
        lock.file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        lock.file.read_to_end(&mut bytes)?;
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(actual) => ensure!(
                (lock.old && (actual == plan.binding(true) || actual == plan.tombstone()))
                    || (lock.new && actual == plan.binding(false)),
                "Unrelated scope binding; refusing recovery"
            ),
            Err(_) => torn.push(index),
        }
    }
    for index in torn {
        let lock = &mut locks[index];
        let expected = if lock.new {
            plan.binding(false)
        } else {
            plan.tombstone()
        };
        write_binding(&mut lock.file, &expected)?;
    }
    Ok(())
}
async fn open_readonly(path: &Path) -> Result<Journal> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(false)
                .read_only(true)
                .synchronous(SqliteSynchronous::Full),
        )
        .await?;
    Ok(Journal { pool })
}
async fn required_meta(journal: &Journal, key: &str) -> Result<String> {
    journal.meta(key).await?.with_context(|| {
        format!("Source journal missing durable {key}; restore the correctly bound journal")
    })
}
async fn identity(journal: &Journal) -> Result<(u64, Address, Address, String)> {
    ensure!(
        required_meta(journal, "schema_version").await? == "1",
        "Unsupported source journal schema"
    );
    let scope = required_meta(journal, "scope").await?;
    let parts: Vec<_> = scope.split(':').collect();
    ensure!(parts.len() == 3, "Malformed source scope");
    let instance = required_meta(journal, "instance_id").await?;
    ensure!(
        instance.len() == 64 && instance.bytes().all(|b| b.is_ascii_hexdigit()),
        "Malformed source instance identity"
    );
    Ok((
        parts[0].parse()?,
        parts[1].parse()?,
        parts[2].parse()?,
        instance,
    ))
}
async fn verified_plan(cfg: &Config, source: &Journal, from: PathBuf, to: PathBuf) -> Result<Plan> {
    let (chain, old_coordinator, old_sender, old_instance) = identity(source).await?;
    ensure!(chain == cfg.chain_id, "Cross-chain migration is prohibited");
    let key = prover::read_key(&cfg.vrf_key_file)?;
    let secret = prover::read_key(&cfg.tx_key_file)?;
    let signer = PrivateKeySigner::from_bytes(&B256::from_slice(&secret.to_bytes()))?;
    let new_sender = signer.address();
    ensure!(
        new_sender != prover::address(&key),
        "Use separate VRF and transaction keys"
    );
    if chain != 31337 {
        let fixture = k256::SecretKey::from_slice(&U256::from(123456789u64).to_be_bytes::<32>())?;
        ensure!(
            prover::public_key(&key) != prover::public_key(&fixture),
            "Public VRF test key is prohibited off local chain"
        );
    }
    ensure!(
        old_sender != new_sender || old_coordinator != cfg.coordinator || from != to,
        "Migration must change journal path or identity"
    );
    let public_key = prover::public_key(&key);
    let new_instance = hex::encode(keccak256(serde_json::to_vec(&json!([
        &from,
        &to,
        &old_instance,
        chain,
        cfg.coordinator,
        new_sender,
        public_key
    ]))?));
    let mut observed = None;
    for url in &cfg.rpc_urls {
        // Every configured endpoint must agree; no failover hides an ambiguous nonce.
        let rpc = Rpc::new(vec![url.clone()])?;
        ensure!(
            quantity(&rpc.request("eth_chainId", json!([])).await?)? == chain,
            "RPC chain mismatch"
        );
        let code: Bytes = serde_json::from_value(
            rpc.request("eth_getCode", json!([cfg.coordinator, "latest"]))
                .await?,
        )?;
        ensure!(!code.is_empty(), "Destination coordinator has no code");
        let code_hash = keccak256(&code);
        ensure!(
            cfg.code_hash.is_none_or(|hash| hash == code_hash),
            "Destination code pin mismatch"
        );
        ensure!(
            rpc.call(cfg.coordinator, C::publicKeyXCall {}).await? == public_key[0]
                && rpc.call(cfg.coordinator, C::publicKeyYCall {}).await? == public_key[1],
            "Destination VRF key mismatch"
        );
        let runtime_pins = crate::proxy::RuntimePins::observe(&rpc, cfg).await?;
        let protocol_hash = crate::worker::validate_configuration_pin(&rpc, cfg).await?;
        let registry = rpc.call(cfg.coordinator, C::epochRegistryCall {}).await?;
        ensure!(
            rpc.call(registry, E::committerCall {}).await? == new_sender,
            "Destination epoch committer does not match transaction wallet"
        );
        let old_nonce = drained(&rpc, source, old_coordinator, old_sender).await?;
        let new_nonce = rpc.nonce(new_sender, "latest").await?;
        ensure!(
            new_nonce == rpc.nonce(new_sender, "pending").await?,
            "Destination wallet has pending or ambiguous transactions"
        );
        runtime_pins.verify(&rpc).await?;
        let values = (code_hash, protocol_hash, old_nonce, new_nonce, runtime_pins);
        ensure!(
            observed.is_none_or(|previous| previous == values),
            "RPC endpoints disagree on migration identity/nonces"
        );
        observed = Some(values);
    }
    let (code_hash, protocol_hash, old_nonce, new_nonce, runtime_pins) =
        observed.context("No verified RPC endpoints")?;
    Ok(Plan {
        version: 1,
        from,
        to,
        chain,
        old_coordinator,
        new_coordinator: cfg.coordinator,
        old_sender,
        new_sender,
        old_instance,
        new_instance,
        old_nonce,
        new_nonce,
        code_hash,
        protocol_hash,
        runtime_pins,
        public_key,
    })
}
async fn drained(
    rpc: &Rpc,
    source: &Journal,
    coordinator: Address,
    sender: Address,
) -> Result<u64> {
    if let Some(saved) = source.meta("runtime:pins").await? {
        let pins: crate::proxy::RuntimePins = serde_json::from_str(&saved)?;
        pins.verify(rpc).await?;
    }
    let unresolved: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM txs WHERE state != 'resolved'")
        .fetch_one(&source.pool)
        .await?;
    ensure!(
        unresolved == 0,
        "Source has unresolved signed attempts; reconcile them with the original keeper before migrating"
    );
    ensure!(
        source.meta(crate::sweep::ATTEMPT_KEY).await?.is_none()
            && source.meta(crate::sweep::REQUEST_KEY).await?.is_none(),
        "Source has a queued or in-flight operator sweep; let it resolve or remove the request before migrating"
    );
    let nonce = rpc.nonce(sender, "latest").await?;
    ensure!(
        nonce == rpc.nonce(sender, "pending").await?,
        "Old wallet has pending or ambiguous transactions"
    );
    ensure!(
        nonce >= source.nonce_floor().await?,
        "Old wallet on-chain nonce is behind durable nonce floor"
    );
    let head = rpc.head().await?;
    let live_epochs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM epoch_work WHERE start>? AND state NOT IN ('committed','expired')",
    )
    .bind(i64::try_from(head.number)?)
    .fetch_one(&source.pool)
    .await?;
    ensure!(
        live_epochs == 0,
        "Source has still-live epoch preparation; stop the publisher and wait until its preparation window ends before migrating"
    );
    let live: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE deadline>=? AND state NOT IN ('served','refunded','expired')")
        .bind(i64::try_from(head.timestamp)?).fetch_one(&source.pool).await?;
    ensure!(
        live == 0,
        "Source has still-live saved game work/proofs; drain it before migrating"
    );
    let end: u64 = rpc
        .call(coordinator, C::nextRequestIdCall {})
        .await?
        .try_into()?;
    // Latest request is a conservative global drain barrier. Requiring it to expire
    // avoids skipping undiscovered gaps, fulfilled-tail ordering, or truncated pages.
    if end > 1 {
        let last = rpc
            .call(
                coordinator,
                C::getRequestCall {
                    id: U256::from(end - 1),
                },
            )
            .await?;
        ensure!(
            last.deadline < head.timestamp,
            "Old coordinator request ingress is not drained: stop new requests and wait until the newest request deadline has passed"
        );
    }
    Ok(nonce)
}

pub async fn run(cfg: Config, source_path: &Path, mode: Mode) -> Result<()> {
    ensure!(source_path.is_absolute(), "--from must be an absolute path");
    let from = std::fs::canonicalize(source_path).context("Source journal does not exist")?;
    let parent = std::fs::canonicalize(cfg.db.parent().context("Destination parent missing")?)
        .context("Provision the destination directory separately before migration")?;
    let to = parent.join(cfg.db.file_name().context("Destination filename missing")?);
    ensure!(
        from != to,
        "Migration cannot overwrite the original journal"
    );
    // Same lock-path aliases (e.g. old.sqlite and old.db) are also rejected.
    ensure!(
        from.with_extension("lock") != to.with_extension("lock"),
        "Source and destination journal locks alias"
    );
    let _source_lock = journal_lock(&from)?;
    let _destination_lock = journal_lock(&to)?;
    let source = open_readonly(&from).await?;
    let result = run_locked(&cfg, &source, from, to, mode).await;
    source.pool.close().await;
    result
}
async fn run_locked(
    cfg: &Config,
    source: &Journal,
    from: PathBuf,
    to: PathBuf,
    mode: Mode,
) -> Result<()> {
    // Obtain old scope from durable DB metadata, then hold old AND new scopes
    // before network inspection. The first plan here has no network-derived fields.
    let (chain, old_coordinator, old_sender, old_instance) = identity(source).await?;
    ensure!(chain == cfg.chain_id, "Cross-chain migration is prohibited");
    let secret = prover::read_key(&cfg.tx_key_file)?;
    let sender = PrivateKeySigner::from_bytes(&B256::from_slice(&secret.to_bytes()))?.address();
    let placeholder = Plan {
        version: 1,
        from: from.clone(),
        to: to.clone(),
        chain,
        old_coordinator,
        new_coordinator: cfg.coordinator,
        old_sender,
        new_sender: sender,
        old_instance,
        new_instance: String::new(),
        old_nonce: 0,
        new_nonce: 0,
        code_hash: B256::ZERO,
        protocol_hash: B256::ZERO,
        runtime_pins: crate::proxy::RuntimePins::default(),
        public_key: [U256::ZERO; 2],
    };
    let mut locks = scope_locks(&cfg.lock_dir, &placeholder)?;
    let intent = sidecar(&to, ".migration.json");
    let old_marker = sidecar(&from, ".migration-blocked");
    // Source marker is written first. If the process stops before the intent
    // file exists, that same complete marker is the recovery record.
    let recovery_record = if intent.try_exists()? {
        &intent
    } else {
        &old_marker
    };
    let saved = if recovery_record.try_exists()? {
        ensure!(
            mode != Mode::Prepare,
            "Migration intent already exists; use identical configuration with --apply or --resume. Do not delete intent, DB, markers, or locks"
        );
        Some(serde_json::from_slice::<Plan>(&std::fs::read(recovery_record)?).context("Partial migration intent; fail closed and preserve all files for reviewed recovery")?)
    } else {
        ensure!(
            mode != Mode::Resume,
            "No migration intent exists; use --prepare then --apply"
        );
        None
    };
    if saved.is_none() {
        ensure!(
            !to.try_exists()?,
            "Destination database already exists; never overwrite an unrelated journal"
        );
        ensure_startable(&from)?;
        ensure_startable(&to)?;
        validate_bindings(&mut locks, &placeholder, false)?;
    }
    let plan = verified_plan(cfg, source, from, to).await?;
    ensure!(
        plan.new_sender == placeholder.new_sender,
        "Destination transaction key changed while acquiring ownership; retry with stable key files"
    );
    if let Some(saved) = &saved {
        ensure!(
            saved == &plan,
            "Recovery configuration, source identity, pins, or nonces changed; fail closed. Restore identical approved configuration and reconcile chain state before --resume"
        );
    }
    if let Err(error) = validate_bindings(&mut locks, &plan, saved.is_some()) {
        if saved.is_some() && mode == Mode::Resume {
            recover_bindings(source, &mut locks, &plan).await?;
            validate_bindings(&mut locks, &plan, true)?;
        } else {
            return Err(error);
        }
    }
    if mode == Mode::Prepare {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        println!(
            "Read-only preparation complete. Keep old ingress stopped. Provision rotated keys separately; run identical destination configuration with --apply. No live takeover is supported."
        );
        return Ok(());
    }
    ensure_record(&sidecar(&plan.from, ".migration-blocked"), &plan)?;
    ensure_record(&intent, &plan)?;
    transition(source, &mut locks, &plan, |_| Ok(())).await?;
    println!(
        "Migration complete: {}. Original journal and audit snapshots preserved; old journal permanently retired. Start only the destination keeper.",
        plan.to.display()
    );
    Ok(())
}

async fn transition<F: FnMut(usize) -> Result<()>>(
    source: &Journal,
    locks: &mut [ScopeLock],
    plan: &Plan,
    mut checkpoint: F,
) -> Result<()> {
    let old_marker = sidecar(&plan.from, ".migration-blocked");
    let new_marker = sidecar(&plan.to, ".migration-blocked");
    let complete = sidecar(&plan.to, ".migration-complete");
    if complete.try_exists()? {
        check_record(&complete, plan)?;
        check_record(&old_marker, plan)?;
        verify_destination(&plan.to, plan).await?;
        for lock in locks {
            let expected = if lock.new {
                plan.binding(false)
            } else {
                plan.tombstone()
            };
            ensure!(
                read_binding(&mut lock.file)?.as_ref() == Some(&expected),
                "Completed migration binding changed; fail closed"
            );
        }
        if new_marker.try_exists()? {
            check_record(&new_marker, plan)?;
            std::fs::remove_file(&new_marker)?;
            sync_parent(&new_marker)?;
        }
        return Ok(());
    }
    // This remains permanently as the source retirement guard, including when
    // source and destination have no common wallet/coordinator scope.
    ensure_record(&old_marker, plan)?;
    checkpoint(1)?;
    ensure_record(&new_marker, plan)?;
    checkpoint(2)?;
    if !plan.to.try_exists()? {
        // Never publish an uninitialized snapshot. Interrupted staging files are
        // retained as evidence, never overwritten/deleted; resume uses a new name.
        let mut sequence = 0u64;
        let staging = loop {
            let candidate = sidecar(&plan.to, &format!(".migration-snapshot-{sequence}"));
            if !candidate.try_exists()? {
                break candidate;
            }
            sequence = sequence
                .checked_add(1)
                .context("Too many migration staging files")?;
        };
        sqlx::query("VACUUM INTO ?")
            .bind(staging.to_str().context("Destination path must be UTF-8")?)
            .execute(&source.pool)
            .await?;
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&staging)?
            .sync_all()?;
        sync_parent(&staging)?;
        checkpoint(3)?;
        prepare_destination(&staging, plan).await?;
        verify_destination(&staging, plan).await?;
        checkpoint(5)?;
        // Target is absent and its exclusive journal lock is held throughout.
        ensure!(
            !plan.to.try_exists()?,
            "Destination appeared during migration; refusing replacement"
        );
        std::fs::rename(&staging, &plan.to)?;
        sync_parent(&plan.to)?;
    } else {
        verify_destination(&plan.to, plan).await.context(
            "Existing destination is unrelated or corrupt; fail closed and preserve all evidence",
        )?;
    }
    checkpoint(4)?;
    verify_destination(&plan.to, plan).await?;
    // Revoke ALL old ownership before granting ANY destination ownership.
    for (i, lock) in locks.iter_mut().enumerate() {
        if lock.old {
            write_binding(&mut lock.file, &plan.tombstone())?;
            checkpoint(10 + i)?;
        }
    }
    for (i, lock) in locks.iter_mut().enumerate() {
        if lock.new {
            write_binding(&mut lock.file, &plan.binding(false))?;
            checkpoint(20 + i)?;
        }
    }
    create_record(&complete, plan)?;
    checkpoint(30)?;
    // Only this final durable removal makes ordinary startup eligible.
    std::fs::remove_file(&new_marker)?;
    sync_parent(&new_marker)?;
    checkpoint(31)?;
    Ok(())
}
async fn prepare_destination(path: &Path, plan: &Plan) -> Result<()> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(false)
                .synchronous(SqliteSynchronous::Full),
        )
        .await?;
    let result = initialize_snapshot(&pool, plan).await;
    pool.close().await;
    result?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?
        .sync_all()?;
    sync_parent(path)
}
async fn initialize_snapshot(pool: &SqlitePool, plan: &Plan) -> Result<()> {
    let mut tx = pool.begin().await?;
    let scope: String = sqlx::query_scalar("SELECT value FROM meta WHERE key='scope'")
        .fetch_one(&mut *tx)
        .await?;
    let instance: String = sqlx::query_scalar("SELECT value FROM meta WHERE key='instance_id'")
        .fetch_one(&mut *tx)
        .await?;
    ensure!(
        scope == plan.scope(true) && instance == plan.old_instance,
        "Snapshot source identity mismatch"
    );
    // Existing archives are copied by VACUUM too. Append this transition's
    // complete logical source snapshot, including prior health/backoff metadata.
    // A source journal written before batched fulfillment has no member table yet.
    sqlx::raw_sql(crate::journal::BATCH_MEMBERS_DDL)
        .execute(&mut *tx)
        .await?;
    sqlx::raw_sql("CREATE TABLE IF NOT EXISTS migration_archive_meta AS SELECT CAST('' AS TEXT) AS migration_id,* FROM meta WHERE 0; CREATE TABLE IF NOT EXISTS migration_archive_jobs AS SELECT CAST('' AS TEXT) AS migration_id,* FROM jobs WHERE 0; CREATE TABLE IF NOT EXISTS migration_archive_txs AS SELECT CAST('' AS TEXT) AS migration_id,* FROM txs WHERE 0; CREATE TABLE IF NOT EXISTS migration_archive_batch_members AS SELECT CAST('' AS TEXT) AS migration_id,* FROM batch_members WHERE 0;").execute(&mut *tx).await?;
    sqlx::query("INSERT INTO migration_archive_meta SELECT ?,* FROM meta")
        .bind(&plan.new_instance)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO migration_archive_jobs SELECT ?,* FROM jobs")
        .bind(&plan.new_instance)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO migration_archive_txs SELECT ?,* FROM txs")
        .bind(&plan.new_instance)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO migration_archive_batch_members SELECT ?,* FROM batch_members")
        .bind(&plan.new_instance)
        .execute(&mut *tx)
        .await?;
    // Administrative metadata reset is not evidence that the retired service recovered.
    sqlx::query("INSERT INTO meta(key,value) VALUES('audit:suspended','1') ON CONFLICT(key) DO UPDATE SET value='1'")
        .execute(&mut *tx).await?;
    if plan.old_coordinator != plan.new_coordinator {
        sqlx::query("DELETE FROM jobs").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM meta WHERE key IN ('cursor','send_cursor') OR key LIKE 'preflight_retry:%' OR key LIKE 'health:%'")
            .execute(&mut *tx)
            .await?;
    }
    if plan.old_sender != plan.new_sender {
        sqlx::query("DELETE FROM txs").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM meta WHERE key LIKE 'health:%' OR key LIKE 'nonce_started:%'")
            .execute(&mut *tx)
            .await?;
    }
    // Member rows join jobs to signed attempts; they go with whichever side was cleared.
    if plan.old_coordinator != plan.new_coordinator || plan.old_sender != plan.new_sender {
        sqlx::query("DELETE FROM batch_members")
            .execute(&mut *tx)
            .await?;
    }
    for (key, value) in [
        ("scope", plan.scope(false)),
        ("instance_id", plan.new_instance.clone()),
        ("nonce_floor", plan.new_nonce.to_string()),
        ("runtime:pins", serde_json::to_string(&plan.runtime_pins)?),
        ("migration_id", plan.new_instance.clone()),
    ] {
        sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value").bind(key).bind(value).execute(&mut *tx).await?;
    }
    sqlx::query("DELETE FROM meta WHERE key='audit:suspended'")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
async fn verify_destination(path: &Path, plan: &Plan) -> Result<()> {
    let journal = open_readonly(path).await?;
    let result: Result<()> = async {
        let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(&journal.pool)
            .await?;
        ensure!(
            integrity == "ok",
            "Destination snapshot integrity check failed"
        );
        ensure!(
            required_meta(&journal, "scope").await? == plan.scope(false)
                && required_meta(&journal, "instance_id").await? == plan.new_instance
                && required_meta(&journal, "migration_id").await? == plan.new_instance,
            "Destination does not belong to this completed snapshot initialization"
        );
        Ok(())
    }
    .await;
    journal.pool.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn migration_preserves_pending_report_origin_without_false_recovery_events() {
        let dir = tempfile::tempdir().unwrap();
        let (source, plan, mut locks, _) = fixture(dir.path(), true, false).await;
        source.pool.close().await;
        let j = Journal::open(&plan.from, &plan.scope(true)).await.unwrap();
        crate::health::blocked(&j, "settlement", 100).await.unwrap();
        crate::health::assess(&j, true, 102, 1, None, 120)
            .await
            .unwrap();
        let payload = r#"{"reportId":"retained","nodeId":"original-node"}"#;
        sqlx::query("INSERT INTO telemetry_outbox VALUES(1,'retained',?,0,0)")
            .bind(payload)
            .execute(&j.pool)
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events")
            .fetch_one(&j.pool)
            .await
            .unwrap();
        j.pool.close().await;
        let source = open_readonly(&plan.from).await.unwrap();
        transition(&source, &mut locks, &plan, |_| Ok(()))
            .await
            .unwrap();
        let target = open_readonly(&plan.to).await.unwrap();
        let retained: String = sqlx::query_scalar("SELECT payload FROM telemetry_outbox")
            .fetch_one(&target.pool)
            .await
            .unwrap();
        assert_eq!(retained, payload);
        let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events")
            .fetch_one(&target.pool)
            .await
            .unwrap();
        assert_eq!(after, count, "administrative reset fabricated events");
        let origins: Vec<String> = sqlx::query_scalar("SELECT DISTINCT origin FROM audit_events")
            .fetch_all(&target.pool)
            .await
            .unwrap();
        assert_eq!(origins, [plan.scope(true)]);
        assert!(target.meta("audit:suspended").await.unwrap().is_none());
        target.pool.close().await;
        source.pool.close().await;
    }
    async fn fixture(
        root: &Path,
        rotate_wallet: bool,
        rotate_coordinator: bool,
    ) -> (Journal, Plan, Vec<ScopeLock>, Vec<u8>) {
        let root = std::fs::canonicalize(root).unwrap();
        let from = root.join("old.sqlite");
        let to = root.join("new.sqlite");
        let old = Address::repeat_byte(1);
        let other = Address::repeat_byte(2);
        let journal = Journal::open(&from, &format!("31337:{old}:{old}"))
            .await
            .unwrap();
        journal.discovered("1", 1, "2").await.unwrap();
        sqlx::query("INSERT INTO epoch_work(key,registry,catalog,epoch,start,api,state) VALUES('epoch-fixture','registry','catalog',1,0,'immutable-epoch-packet','committed')").execute(&journal.pool).await.unwrap();
        journal
            .prepared("1", "immutable-proof", "immutable-calldata")
            .await
            .unwrap();
        sqlx::query("INSERT INTO txs(job,nonce,hash,raw,kind,fee,state,gas,priority,payload,created) VALUES('1',7,'hash','signed-by-old-wallet','fulfill','1','resolved',1,'1','immutable-calldata',1)").execute(&journal.pool).await.unwrap();
        // Resolved batch history: two served members under one earlier nonce.
        for id in ["2", "3"] {
            journal.discovered(id, 1, "4").await.unwrap();
            journal
                .prepared(id, "member-proof", "member-call")
                .await
                .unwrap();
        }
        let members = vec!["2".to_string(), "3".to_string()];
        let key = crate::journal::batch_key(&members).unwrap();
        journal
            .signed_batch(
                &crate::journal::Attempt {
                    id: 0,
                    job: key.clone(),
                    nonce: 6,
                    hash: "batch-hash".into(),
                    raw: "batch-signed-by-old-wallet".into(),
                    kind: "fulfill_batch".into(),
                    fee: "1".into(),
                    state: "signed".into(),
                    gas: 1,
                    priority: "1".into(),
                    payload: "batch-calldata".into(),
                    created: 1,
                    broadcast: 0,
                },
                &members,
            )
            .await
            .unwrap();
        journal
            .resolve_nonce_batch(6, &key, &[("2".into(), "served"), ("3".into(), "served")])
            .await
            .unwrap();
        sqlx::query("UPDATE meta SET value='8' WHERE key='nonce_floor'")
            .execute(&journal.pool)
            .await
            .unwrap();
        let old_instance = required_meta(&journal, "instance_id").await.unwrap();
        let mut owned = crate::lease::acquire(&root.join("locks"), 31337, old, old).unwrap();
        crate::lease::bind(&mut owned, &from, &old_instance).unwrap();
        drop(owned);
        journal.pool.close().await;
        let before = std::fs::read(&from).unwrap();
        let source = open_readonly(&from).await.unwrap();
        let plan = Plan {
            version: 1,
            from,
            to,
            chain: 31337,
            old_coordinator: old,
            new_coordinator: if rotate_coordinator { other } else { old },
            old_sender: old,
            new_sender: if rotate_wallet { other } else { old },
            old_instance,
            new_instance: "ab".repeat(32),
            old_nonce: 8,
            new_nonce: if rotate_wallet { 1 } else { 8 },
            code_hash: B256::ZERO,
            protocol_hash: B256::ZERO,
            runtime_pins: crate::proxy::RuntimePins::default(),
            public_key: [U256::ZERO; 2],
        };
        let locks = scope_locks(&root.join("locks"), &plan).unwrap();
        (source, plan, locks, before)
    }
    #[tokio::test]
    async fn every_persistent_phase_resumes_without_source_mutation_or_lost_evidence() {
        for (wallet, coordinator) in [(true, false), (false, true), (true, true), (false, false)] {
            // Lock list has 2..4 entries; only checkpoints that correspond to
            // an actual old/new member fire. Others exercise ordinary success.
            for phase in [1, 2, 3, 5, 4, 10, 11, 12, 13, 20, 21, 22, 23, 30, 31] {
                let dir = tempfile::tempdir().unwrap();
                let (source, plan, mut locks, before) =
                    fixture(dir.path(), wallet, coordinator).await;
                validate_bindings(&mut locks, &plan, false).unwrap();
                let mut hit = false;
                let result = transition(&source, &mut locks, &plan, |step| {
                    if step == phase {
                        hit = true;
                        bail!("injected process stop at {step}");
                    }
                    Ok(())
                })
                .await;
                assert_eq!(result.is_err(), hit, "phase {phase}");
                assert!(Journal::open(&plan.from, &plan.scope(true)).await.is_err());
                if phase != 31 && hit {
                    assert!(
                        !plan.to.exists() || ensure_startable(&plan.to).is_err(),
                        "phase {phase}"
                    );
                }
                drop(locks);
                let lock_dir = std::fs::canonicalize(dir.path()).unwrap().join("locks");
                let mut locks = scope_locks(&lock_dir, &plan).unwrap();
                validate_bindings(&mut locks, &plan, true).unwrap();
                transition(&source, &mut locks, &plan, |_| Ok(()))
                    .await
                    .unwrap();
                transition(&source, &mut locks, &plan, |_| Ok(()))
                    .await
                    .unwrap();
                assert!(ensure_startable(&plan.to).is_ok());
                assert!(ensure_startable(&plan.from).is_err());
                let target = Journal::open(&plan.to, &plan.scope(false)).await.unwrap();
                assert_eq!(target.nonce_floor().await.unwrap(), plan.new_nonce);
                let api: String =
                    sqlx::query_scalar("SELECT api FROM epoch_work WHERE key='epoch-fixture'")
                        .fetch_one(&target.pool)
                        .await
                        .unwrap();
                let proof: String =
                    sqlx::query_scalar("SELECT proof FROM migration_archive_jobs WHERE id='1'")
                        .fetch_one(&target.pool)
                        .await
                        .unwrap();
                let raw: String =
                    sqlx::query_scalar("SELECT raw FROM migration_archive_txs WHERE job='1'")
                        .fetch_one(&target.pool)
                        .await
                        .unwrap();
                assert_eq!(api, "immutable-epoch-packet");
                assert_eq!(proof, "immutable-proof");
                assert_eq!(raw, "signed-by-old-wallet");
                let active_jobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs")
                    .fetch_one(&target.pool)
                    .await
                    .unwrap();
                let active_txs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM txs")
                    .fetch_one(&target.pool)
                    .await
                    .unwrap();
                assert_eq!(active_jobs, 3 * i64::from(!coordinator));
                assert_eq!(active_txs, 2 * i64::from(!wallet));
                let archived_members: Vec<(String, i64)> = sqlx::query_as("SELECT request_id,position FROM migration_archive_batch_members ORDER BY position")
                    .fetch_all(&target.pool).await.unwrap();
                assert_eq!(archived_members, vec![("2".into(), 0), ("3".into(), 1)]);
                let active_members: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM batch_members")
                    .fetch_one(&target.pool)
                    .await
                    .unwrap();
                assert_eq!(active_members, 2 * i64::from(!coordinator && !wallet));
                target.pool.close().await;
                source.pool.close().await;
                assert_eq!(
                    std::fs::read(&plan.from).unwrap(),
                    before,
                    "source changed at phase {phase}"
                );
            }
        }
    }
    #[tokio::test]
    async fn source_journal_without_member_table_migrates_with_an_empty_member_archive() {
        let dir = tempfile::tempdir().unwrap();
        let (source, plan, mut locks, _) = fixture(dir.path(), true, false).await;
        source.pool.close().await;
        let old = Journal::open(&plan.from, &plan.scope(true)).await.unwrap();
        sqlx::raw_sql("DROP TABLE batch_members")
            .execute(&old.pool)
            .await
            .unwrap();
        old.pool.close().await;
        let source = open_readonly(&plan.from).await.unwrap();
        transition(&source, &mut locks, &plan, |_| Ok(()))
            .await
            .unwrap();
        let target = open_readonly(&plan.to).await.unwrap();
        for (table, count) in [
            ("batch_members", "SELECT COUNT(*) FROM batch_members"),
            (
                "migration_archive_batch_members",
                "SELECT COUNT(*) FROM migration_archive_batch_members",
            ),
        ] {
            let rows: i64 = sqlx::query_scalar(count)
                .fetch_one(&target.pool)
                .await
                .unwrap();
            assert_eq!(rows, 0, "{table}");
        }
        target.pool.close().await;
        source.pool.close().await;
        assert!(Journal::open(&plan.to, &plan.scope(false)).await.is_ok());
    }
    #[tokio::test]
    async fn source_identity_unrelated_target_and_partial_records_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let (source, plan, mut locks, _) = fixture(dir.path(), true, false).await;
        let mut wrong = plan.clone();
        wrong.old_instance = "cd".repeat(32);
        assert!(validate_bindings(&mut locks, &wrong, false).is_err());
        std::fs::write(sidecar(&plan.from, ".migration-blocked"), b"{partial").unwrap();
        assert!(
            transition(&source, &mut locks, &plan, |_| Ok(()))
                .await
                .is_err()
        );
        assert!(ensure_startable(&plan.from).is_err());
        assert!(!plan.to.exists());
        source.pool.close().await;

        let dir = tempfile::tempdir().unwrap();
        let (source, plan, mut locks, _) = fixture(dir.path(), true, false).await;
        let unrelated = Journal::open(&plan.to, "unrelated").await.unwrap();
        let unrelated_id = required_meta(&unrelated, "instance_id").await.unwrap();
        unrelated.pool.close().await;
        assert!(
            transition(&source, &mut locks, &plan, |_| Ok(()))
                .await
                .is_err()
        );
        let unchanged = open_readonly(&plan.to).await.unwrap();
        assert_eq!(
            required_meta(&unchanged, "instance_id").await.unwrap(),
            unrelated_id
        );
        unchanged.pool.close().await;
        source.pool.close().await;
    }
    #[tokio::test]
    async fn torn_binding_and_conflicting_marker_fail_closed_without_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let (source, plan, mut locks, _) = fixture(dir.path(), true, true).await;
        locks[0].file.seek(SeekFrom::Start(0)).unwrap();
        locks[0].file.set_len(0).unwrap();
        locks[0].file.write_all(b"{partial").unwrap();
        locks[0].file.sync_all().unwrap();
        assert!(validate_bindings(&mut locks, &plan, true).is_err());
        let mut wrong = plan.clone();
        wrong.new_nonce += 1;
        create_record(&sidecar(&plan.from, ".migration-blocked"), &wrong).unwrap();
        assert!(
            transition(&source, &mut locks, &plan, |_| Ok(()))
                .await
                .is_err()
        );
        assert!(check_record(&sidecar(&plan.from, ".migration-blocked"), &wrong).is_ok());
        source.pool.close().await;
    }
    #[tokio::test]
    async fn torn_binding_resume_requires_intact_intent_and_preserves_unrelated_bindings() {
        for torn_bytes in [
            b"".as_slice(),
            b"{\"version\":",
            b"{\"version\":2}stale-tail",
        ] {
            for index in 0..4 {
                let dir = tempfile::tempdir().unwrap();
                let (source, plan, mut locks, _) = fixture(dir.path(), true, true).await;
                create_record(&sidecar(&plan.to, ".migration.json"), &plan).unwrap();
                assert!(
                    transition(&source, &mut locks, &plan, |step| {
                        if step == 4 {
                            bail!("stop before binding writes");
                        }
                        Ok(())
                    })
                    .await
                    .is_err()
                );
                let lock = &mut locks[index];
                lock.file.seek(SeekFrom::Start(0)).unwrap();
                lock.file.set_len(0).unwrap();
                lock.file.write_all(torn_bytes).unwrap();
                lock.file.sync_all().unwrap();
                let other = (index + 1) % locks.len();
                let original = read_binding(&mut locks[other].file).unwrap();
                write_binding(&mut locks[other].file, &json!({"unrelated":true})).unwrap();
                assert!(recover_bindings(&source, &mut locks, &plan).await.is_err());
                assert_eq!(
                    read_binding(&mut locks[other].file).unwrap(),
                    Some(json!({"unrelated":true}))
                );
                if let Some(original) = original {
                    write_binding(&mut locks[other].file, &original).unwrap();
                } else {
                    locks[other].file.set_len(0).unwrap();
                }
                let intent = sidecar(&plan.to, ".migration.json");
                let intact = std::fs::read(&intent).unwrap();
                std::fs::write(&intent, b"{torn").unwrap();
                assert!(recover_bindings(&source, &mut locks, &plan).await.is_err());
                std::fs::write(&intent, intact).unwrap();
                recover_bindings(&source, &mut locks, &plan).await.unwrap();
                validate_bindings(&mut locks, &plan, true).unwrap();
                transition(&source, &mut locks, &plan, |_| Ok(()))
                    .await
                    .unwrap();
                assert!(ensure_startable(&plan.to).is_ok());
                source.pool.close().await;
            }
        }
    }
    #[tokio::test]
    async fn wallet_rotation_archives_and_clears_wallet_health_only() {
        let dir = tempfile::tempdir().unwrap();
        let (source, plan, mut locks, _) = fixture(dir.path(), true, false).await;
        source.pool.close().await;
        let writable = Journal::open(&plan.from, &plan.scope(true)).await.unwrap();
        for key in ["health:wallet", "nonce_started:7", "preflight_retry:1"] {
            sqlx::query("INSERT INTO meta(key,value) VALUES(?,'old')")
                .bind(key)
                .execute(&writable.pool)
                .await
                .unwrap();
        }
        writable.pool.close().await;
        let source = open_readonly(&plan.from).await.unwrap();
        transition(&source, &mut locks, &plan, |_| Ok(()))
            .await
            .unwrap();
        let target = open_readonly(&plan.to).await.unwrap();
        for key in ["health:wallet", "nonce_started:7"] {
            assert!(target.meta(key).await.unwrap().is_none());
            let archived: String =
                sqlx::query_scalar("SELECT value FROM migration_archive_meta WHERE key=?")
                    .bind(key)
                    .fetch_one(&target.pool)
                    .await
                    .unwrap();
            assert_eq!(archived, "old");
        }
        assert_eq!(
            target.meta("preflight_retry:1").await.unwrap().as_deref(),
            Some("old")
        );
        let api: String =
            sqlx::query_scalar("SELECT api FROM epoch_work WHERE key='epoch-fixture'")
                .fetch_one(&target.pool)
                .await
                .unwrap();
        assert_eq!(api, "immutable-epoch-packet");
        target.pool.close().await;
        source.pool.close().await;
    }
    #[tokio::test]
    async fn record_publication_never_overwrites_and_alias_startup_is_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let (source, plan, _, _) = fixture(dir.path(), true, false).await;
        let path = sidecar(&plan.from, ".migration-blocked");
        let pending = sidecar(&path, ".pending-0");
        std::fs::write(&pending, b"{torn staging").unwrap();
        create_record(&path, &plan).unwrap();
        check_record(&path, &plan).unwrap();
        assert_eq!(std::fs::read(&pending).unwrap(), b"{torn staging");
        let mut unrelated = plan.clone();
        unrelated.new_nonce += 1;
        assert!(create_record(&path, &unrelated).is_err());
        check_record(&path, &plan).unwrap();
        std::fs::create_dir(dir.path().join("alias")).unwrap();
        assert!(ensure_startable(&dir.path().join("alias/../old.sqlite")).is_err());
        #[cfg(unix)]
        {
            let alias = dir.path().join("symlink.sqlite");
            std::os::unix::fs::symlink(&plan.from, &alias).unwrap();
            assert!(ensure_startable(&alias).is_err());
        }
        source.pool.close().await;
    }
    #[test]
    fn parser_has_no_bypass_or_implicit_apply() {
        for args in [
            vec![],
            vec!["--from", "old"],
            vec!["--from", "old", "--force"],
            vec!["--from", "old", "--apply", "--force"],
        ] {
            assert!(parse_args(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err());
        }
        assert_eq!(
            parse_args(&["--from".into(), "old".into(), "--prepare".into()])
                .unwrap()
                .1,
            Mode::Prepare
        );
    }
}
