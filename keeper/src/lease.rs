use alloy_primitives::Address;
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

/// Host-local ownership, independent of the database path and working directory.
/// Wallet scope spans coordinators; coordinator scope spans transaction wallets.
pub fn acquire(dir: &Path, chain: u64, coordinator: Address, sender: Address) -> Result<Vec<File>> {
    std::fs::create_dir_all(dir)?;
    let mut locks = Vec::new();
    for (kind, address) in [("wallet", sender), ("coordinator", coordinator)] {
        let path = dir.join(format!("{chain}-{kind}-{address:x}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        FileExt::try_lock_exclusive(&file)
            .with_context(|| format!("Another keeper holds this {kind} scope lock"))?;
        locks.push(file);
    }
    // Persist new lock directory entries before any API work (Unix crash durability).
    #[cfg(unix)]
    for directory in std::fs::canonicalize(dir)?.ancestors() {
        File::open(directory)?.sync_all()?;
    }
    Ok(locks)
}

/// Empty legacy scope files may bind once. Nonempty metadata is never replaced:
/// truncated writes fail closed and require explicit operator recovery.
/// Both bindings are synced before startup is allowed to fetch any API response.
pub fn bind(locks: &mut [File], journal_path: &Path, instance: &str) -> Result<()> {
    let canonical = std::fs::canonicalize(journal_path)?;
    let expected = serde_json::json!({"version": 1, "journal": canonical, "instance": instance});
    // Validate every existing binding before creating any new binding.
    let mut empty = Vec::new();
    for (i, file) in locks.iter_mut().enumerate() {
        file.seek(SeekFrom::Start(0))?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        if contents.is_empty() {
            empty.push(i);
        } else {
            let actual: serde_json::Value = serde_json::from_str(&contents)
                .context("Invalid scope journal binding; explicit operator recovery required")?;
            ensure!(
                actual == expected,
                "Scope is bound to a different journal path or instance; restore the original journal or perform explicit operator migration"
            );
        }
    }
    for i in empty {
        let file = &mut locks[i];
        file.write_all(&serde_json::to_vec(&expected)?)?;
        file.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn binding_survives_restart_and_rejects_new_or_replaced_journals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.sqlite");
        let a = Address::repeat_byte(1);
        let j = crate::journal::Journal::open(&path, "scope").await.unwrap();
        let id = j.meta("instance_id").await.unwrap().unwrap();
        let mut locks = acquire(dir.path(), 31337, a, a).unwrap();
        bind(&mut locks, &path, &id).unwrap();
        drop(locks);
        j.pool.close().await;
        let j = crate::journal::Journal::open(&path, "scope").await.unwrap();
        assert_eq!(j.meta("instance_id").await.unwrap().unwrap(), id);
        let mut locks = acquire(dir.path(), 31337, a, a).unwrap();
        bind(&mut locks, &dir.path().join(".").join("jobs.sqlite"), &id).unwrap();
        let other_path = dir.path().join("other.sqlite");
        let other = crate::journal::Journal::open(&other_path, "scope")
            .await
            .unwrap();
        let other_id = other.meta("instance_id").await.unwrap().unwrap();
        assert!(bind(&mut locks, &other_path, &other_id).is_err());
        other.pool.close().await;
        j.pool.close().await;
        // Preserve the old DB: simulate replacement after a clean close.
        std::fs::rename(&path, dir.path().join("original.sqlite")).unwrap();
        let replaced = crate::journal::Journal::open(&path, "scope").await.unwrap();
        let replacement_id = replaced.meta("instance_id").await.unwrap().unwrap();
        assert_ne!(id, replacement_id);
        assert!(bind(&mut locks, &path, &replacement_id).is_err());
        replaced.pool.close().await;
    }
    #[test]
    fn partial_binding_fails_closed_without_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        std::fs::write(&path, b"db").unwrap();
        let a = Address::repeat_byte(1);
        let mut locks = acquire(dir.path(), 1, a, a).unwrap();
        locks[0].write_all(b"{partial").unwrap();
        locks[0].sync_all().unwrap();
        assert!(bind(&mut locks, &path, "id").is_err());
        locks[0].seek(SeekFrom::Start(0)).unwrap();
        let mut contents = String::new();
        locks[0].read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "{partial");
    }
    #[test]
    fn scope_locks_span_databases_wallets_and_coordinators() {
        let dir = tempfile::tempdir().unwrap();
        let a = Address::repeat_byte(1);
        let b = Address::repeat_byte(2);
        let first = acquire(dir.path(), 31337, a, a).unwrap();
        assert!(
            acquire(dir.path(), 31337, b, a).is_err(),
            "same wallet, other coordinator"
        );
        assert!(
            acquire(dir.path(), 31337, a, b).is_err(),
            "same coordinator, other wallet"
        );
        let independent = acquire(dir.path(), 31337, b, b).unwrap();
        drop(independent);
        drop(first);
        assert!(
            acquire(dir.path(), 31337, a, a).is_ok(),
            "lock not released on drop"
        );
    }
}
