use crate::adapters::error::{NspawnError, Result};
use crate::adapters::locking::{is_contention, remaining, timeout_error, LockWaitPolicy};
use fs2::FileExt;
use std::ffi::OsString;
use std::fs::File;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;

pub(crate) fn lock_path_for(path: &Path) -> std::path::PathBuf {
    let Some(file_name) = path.file_name() else {
        return path.with_extension("lock");
    };
    let mut lock_name = OsString::from(".");
    lock_name.push(file_name);
    lock_name.push(".lock");
    path.with_file_name(lock_name)
}

/// Manages transactional, locked and atomic writes to configuration files.
pub struct AsyncLockedWriter;

impl AsyncLockedWriter {
    /// Performs a transactional write operation on a file.
    ///
    /// The process follows these safety rules:
    /// 1. Uses a sidecar `.lock` file to avoid inode-switch race conditions.
    /// 2. Uses an async backoff loop to acquire the lock without blocking the Tokio executor.
    /// 3. Performs an atomic write via rename.
    #[allow(dead_code)] // kept for direct atomic write paths used by typed stores
    pub async fn write_locked<F>(path: &Path, content_generator: F) -> Result<()>
    where
        F: FnOnce(Option<String>) -> Result<String>,
    {
        Self::apply_locked_inner(path, None, move |existing| {
            content_generator(existing).map(|content| (Some(content), ()))
        })
        .await
    }

    /// Applies a locked update and returns a typed result. `None` keeps the
    /// existing file byte-for-byte without an atomic rewrite.
    pub async fn apply_locked<T, F>(path: &Path, content_generator: F) -> Result<T>
    where
        F: FnOnce(Option<String>) -> Result<(Option<String>, T)>,
    {
        Self::apply_locked_inner(path, None, content_generator).await
    }

    pub async fn apply_locked_with_mode<T, F>(
        path: &Path,
        mode: u32,
        content_generator: F,
    ) -> Result<T>
    where
        F: FnOnce(Option<String>) -> Result<(Option<String>, T)>,
    {
        Self::apply_locked_inner(path, Some(mode), content_generator).await
    }

    async fn apply_locked_inner<T, F>(
        path: &Path,
        mode: Option<u32>,
        content_generator: F,
    ) -> Result<T>
    where
        F: FnOnce(Option<String>) -> Result<(Option<String>, T)>,
    {
        let path_buf = path.to_path_buf();
        let lock_path = lock_path_for(path);
        // Ensure parent exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| NspawnError::Io(parent.to_path_buf(), e))?;
        }

        // Keep the guard alive until the atomic update and directory sync
        // complete; dropping it releases the advisory lock.
        let Some(_lock_file) = Self::acquire_stable_lock(&lock_path, true).await? else {
            return Err(NspawnError::Runtime(format!(
                "lock path {} disappeared while preparing a write",
                lock_path.display()
            )));
        };

        // Read existing content - FIX: Direct read to avoid TOCTOU
        let existing_content = match fs::read_to_string(&path_buf).await {
            Ok(c) => Some(c),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(NspawnError::Io(path_buf.clone(), e)),
        };

        // Mutate
        let (new_content, result) = content_generator(existing_content)?;

        let Some(new_content) = new_content else {
            return Ok(result);
        };

        let final_mode = mode.or(existing_mode(path)?).unwrap_or(0o600);
        persist_atomic(path, new_content.as_bytes(), final_mode).await?;

        // Sync parent directory
        if let Some(parent) = path.parent() {
            if let Ok(dir) = fs::File::open(parent).await {
                let _ = dir.sync_all().await;
            }
        }

        // Keep the sidecar lock file persistent. Removing it would allow a
        // concurrent process to open a new inode while another process still
        // holds a lock on the old, unlinked inode.

        Ok(result)
    }

    /// Remove the current sidecar inode only after locking it and confirming
    /// that its target no longer exists. Writers validate the inode again
    /// after acquiring it, so a writer queued on an unlinked inode retries on
    /// the replacement instead of entering a split critical section.
    pub async fn remove_lock_if_target_absent(target: &Path, lock_path: &Path) -> Result<bool> {
        let Some(lock_file) = Self::acquire_stable_lock(lock_path, false).await? else {
            return Ok(false);
        };

        match fs::symlink_metadata(target).await {
            Ok(_) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::remove_file(lock_path)
                    .await
                    .map_err(|error| NspawnError::Io(lock_path.to_path_buf(), error))?;
                drop(lock_file);
                Ok(true)
            }
            Err(error) => Err(NspawnError::Io(target.to_path_buf(), error)),
        }
    }

    async fn acquire_stable_lock(lock_path: &Path, create: bool) -> Result<Option<File>> {
        Self::acquire_stable_lock_with_policy(lock_path, create, LockWaitPolicy::default()).await
    }

    async fn acquire_stable_lock_with_policy(
        lock_path: &Path,
        create: bool,
        policy: LockWaitPolicy,
    ) -> Result<Option<File>> {
        let started = std::time::Instant::now();
        let mut attempts = 0usize;
        'open: loop {
            let lock_file = match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(create)
                .truncate(false)
                .open(lock_path)
            {
                Ok(file) => file,
                Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(None)
                }
                Err(error) => return Err(NspawnError::Io(lock_path.to_path_buf(), error)),
            };

            loop {
                match lock_file.try_lock_exclusive() {
                    Ok(()) => break,
                    Err(error) => {
                        if !is_contention(&error) {
                            return Err(NspawnError::Io(lock_path.to_path_buf(), error));
                        }
                        attempts = attempts.saturating_add(1);
                        wait_for_lock_retry(lock_path, started, attempts, policy).await?;
                    }
                }
            }

            if Self::locked_inode_is_current(&lock_file, lock_path)? {
                return Ok(Some(lock_file));
            }

            let _ = fs2::FileExt::unlock(&lock_file);
            attempts = attempts.saturating_add(1);
            wait_for_lock_retry(lock_path, started, attempts, policy).await?;
            continue 'open;
        }
    }

    fn locked_inode_is_current(lock_file: &File, lock_path: &Path) -> Result<bool> {
        let opened = lock_file
            .metadata()
            .map_err(|error| NspawnError::Io(lock_path.to_path_buf(), error))?;
        let current = match std::fs::symlink_metadata(lock_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(NspawnError::Io(lock_path.to_path_buf(), error)),
        };
        if !current.file_type().is_file() {
            return Err(NspawnError::Validation(format!(
                "Lock path {} is not a regular file",
                lock_path.display()
            )));
        }
        Ok(opened.dev() == current.dev() && opened.ino() == current.ino())
    }

    /// Safely writes content to a file using atomic rename and fsync to ensure durability.
    /// Does not use a lock file.
    #[allow(dead_code)]
    pub async fn write_atomic(path: &Path, content: &str) -> Result<()> {
        Self::write_atomic_with_mode(path, content, None).await
    }

    /// Safely writes content with an explicit final mode using atomic rename.
    /// Does not use a lock file.
    pub async fn write_atomic_with_mode(
        path: &Path,
        content: &str,
        mode: Option<u32>,
    ) -> Result<()> {
        // 1. Ensure parent exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| NspawnError::Io(parent.to_path_buf(), e))?;
        }

        let final_mode = mode.or(existing_mode(path)?).unwrap_or(0o600);
        persist_atomic(path, content.as_bytes(), final_mode).await
    }
}

fn existing_mode(path: &Path) -> Result<Option<u32>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(Some(metadata.mode() & 0o7777)),
        Ok(_) => Err(NspawnError::Validation(format!(
            "Atomic write target {} is not a regular file",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(NspawnError::Io(path.to_path_buf(), error)),
    }
}

async fn persist_atomic(path: &Path, content: &[u8], mode: u32) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let prefix = path
        .file_name()
        .map(|name| {
            let mut prefix = OsString::from(".");
            prefix.push(name);
            prefix.push(".lasper-");
            prefix
        })
        .unwrap_or_else(|| OsString::from(".lasper-"));
    let named = tempfile::Builder::new()
        .prefix(&prefix)
        .suffix(".tmp")
        .tempfile_in(parent)
        .map_err(|error| NspawnError::Io(parent.to_path_buf(), error))?;
    named
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(|error| NspawnError::Io(named.path().to_path_buf(), error))?;
    let (file, temp_path) = named.into_parts();
    let temp_name: PathBuf = temp_path.to_path_buf();
    let mut file = fs::File::from_std(file);
    file.write_all(content)
        .await
        .map_err(|error| NspawnError::Io(temp_name.clone(), error))?;
    file.sync_data()
        .await
        .map_err(|error| NspawnError::Io(temp_name.clone(), error))?;
    drop(file);

    temp_path
        .persist(path)
        .map_err(|error| NspawnError::Io(path.to_path_buf(), error.error))?;
    if let Ok(directory) = fs::File::open(parent).await {
        let _ = directory.sync_all().await;
    }
    Ok(())
}

async fn wait_for_lock_retry(
    lock_path: &Path,
    started: std::time::Instant,
    attempts: usize,
    policy: LockWaitPolicy,
) -> Result<()> {
    let Some(wait_budget) = remaining(policy, started) else {
        return Err(timeout_error(lock_path, started, attempts));
    };
    if wait_budget.is_zero() {
        return Err(timeout_error(lock_path, started, attempts));
    }
    sleep(policy.retry_delay.min(wait_budget)).await;
    match remaining(policy, started) {
        Some(wait_budget) if !wait_budget.is_zero() => Ok(()),
        _ => Err(timeout_error(lock_path, started, attempts)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lock_cleanup_only_removes_lock_for_an_absent_target() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("settings.conf");
        let lock = lock_path_for(&target);
        tokio::fs::write(&target, "[Exec]\nBoot=no\n")
            .await
            .unwrap();
        tokio::fs::write(&lock, "").await.unwrap();

        assert!(
            !AsyncLockedWriter::remove_lock_if_target_absent(&target, &lock)
                .await
                .unwrap()
        );
        assert!(lock.exists());

        tokio::fs::remove_file(&target).await.unwrap();
        assert!(
            AsyncLockedWriter::remove_lock_if_target_absent(&target, &lock)
                .await
                .unwrap()
        );
        assert!(!lock.exists());
    }

    #[tokio::test]
    async fn lock_contention_reports_a_bounded_targeted_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let lock = directory.path().join(".settings.conf.lock");
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock)
            .unwrap();
        holder.lock_exclusive().unwrap();

        let error = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            AsyncLockedWriter::acquire_stable_lock_with_policy(
                &lock,
                true,
                LockWaitPolicy {
                    timeout: std::time::Duration::from_millis(30),
                    retry_delay: std::time::Duration::from_millis(5),
                },
            ),
        )
        .await
        .expect("lock acquisition ignored its deadline")
        .unwrap_err();

        match error {
            NspawnError::Io(path, source) => {
                assert_eq!(path, lock);
                assert_eq!(source.kind(), std::io::ErrorKind::TimedOut);
                assert!(source.to_string().contains("attempts"));
            }
            other => panic!("unexpected lock error: {other}"),
        }
        holder.unlock().unwrap();
    }

    #[test]
    fn replaced_lock_path_is_not_the_inode_already_locked() {
        let directory = tempfile::tempdir().unwrap();
        let lock = directory.path().join(".settings.conf.lock");
        let replacement = directory.path().join("replacement.lock");
        std::fs::write(&lock, "old").unwrap();
        let old = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock)
            .unwrap();
        old.lock_exclusive().unwrap();

        std::fs::write(&replacement, "new").unwrap();
        std::fs::rename(&replacement, &lock).unwrap();

        assert!(!AsyncLockedWriter::locked_inode_is_current(&old, &lock).unwrap());
        old.unlock().unwrap();
    }

    #[tokio::test]
    async fn atomic_write_does_not_follow_a_predictable_temporary_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("settings.conf");
        let victim = directory.path().join("victim");
        let legacy_temp = target.with_extension("write.tmp");
        std::fs::write(&victim, "untouched").unwrap();
        std::os::unix::fs::symlink(&victim, &legacy_temp).unwrap();

        AsyncLockedWriter::write_atomic_with_mode(&target, "new", Some(0o640))
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
        assert!(legacy_temp.is_symlink());
    }

    #[tokio::test]
    async fn atomic_rewrite_preserves_existing_mode_without_an_override() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("settings.conf");
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();

        AsyncLockedWriter::write_atomic(&target, "new")
            .await
            .unwrap();

        let mode = std::fs::symlink_metadata(&target).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }
}
