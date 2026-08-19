use crate::nspawn::errors::{NspawnError, Result};
use fs2::FileExt;
use std::ffi::OsString;
use std::fs::File;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::time::{sleep, Duration};

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
    const MAX_LOCK_ATTEMPTS: usize = 100;
    const LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);

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
        let tmp_path = path.with_extension("tmp");

        // Ensure parent exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| NspawnError::Io(parent.to_path_buf(), e))?;
        }

        // Keep the guard alive until the atomic update and directory sync
        // complete; dropping it releases the advisory lock.
        let _lock_file = Self::acquire_stable_lock(&lock_path, true)
            .await?
            .expect("create=true always returns a lock file");

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

        // Atomic update with durability
        {
            let mut options = fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            if let Some(mode) = mode {
                options.mode(mode);
            }
            let mut f = options
                .open(&tmp_path)
                .await
                .map_err(|e| NspawnError::Io(tmp_path.clone(), e))?;
            if let Some(mode) = mode {
                f.set_permissions(std::fs::Permissions::from_mode(mode))
                    .await
                    .map_err(|e| NspawnError::Io(tmp_path.clone(), e))?;
            }
            f.write_all(new_content.as_bytes())
                .await
                .map_err(|e| NspawnError::Io(tmp_path.clone(), e))?;
            f.sync_data()
                .await
                .map_err(|e| NspawnError::Io(tmp_path.clone(), e))?;
        }

        fs::rename(&tmp_path, &path_buf)
            .await
            .map_err(|e| NspawnError::Io(path_buf.clone(), e))?;

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
        let mut attempts = 0;
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
                    Err(error) if attempts < Self::MAX_LOCK_ATTEMPTS => {
                        attempts += 1;
                        sleep(Self::LOCK_RETRY_DELAY).await;
                    }
                    Err(error) => {
                        return Err(NspawnError::Runtime(format!(
                            "Could not acquire lock on {:?} after {} attempts: {}",
                            lock_path, attempts, error
                        )))
                    }
                }
            }

            if Self::locked_inode_is_current(&lock_file, lock_path)? {
                return Ok(Some(lock_file));
            }

            let _ = fs2::FileExt::unlock(&lock_file);
            if attempts >= Self::MAX_LOCK_ATTEMPTS {
                return Err(NspawnError::Runtime(format!(
                    "Lock path {:?} kept changing while it was acquired",
                    lock_path
                )));
            }
            attempts += 1;
            sleep(Self::LOCK_RETRY_DELAY).await;
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
        let tmp_path = path.with_extension("write.tmp");

        // 1. Ensure parent exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| NspawnError::Io(parent.to_path_buf(), e))?;
        }

        // 2. Write and sync
        {
            let mut options = fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            if let Some(mode) = mode {
                options.mode(mode);
            }
            let mut f = options
                .open(&tmp_path)
                .await
                .map_err(|e| NspawnError::Io(tmp_path.clone(), e))?;
            if let Some(mode) = mode {
                f.set_permissions(std::fs::Permissions::from_mode(mode))
                    .await
                    .map_err(|e| NspawnError::Io(tmp_path.clone(), e))?;
            }
            f.write_all(content.as_bytes())
                .await
                .map_err(|e| NspawnError::Io(tmp_path.clone(), e))?;
            f.sync_data()
                .await
                .map_err(|e| NspawnError::Io(tmp_path.clone(), e))?;
        }

        // 3. Atomic swap
        fs::rename(&tmp_path, path)
            .await
            .map_err(|e| NspawnError::Io(path.to_path_buf(), e))?;

        // 4. Sync parent directory
        if let Some(parent) = path.parent() {
            if let Ok(dir) = fs::File::open(parent).await {
                let _ = dir.sync_all().await;
            }
        }

        Ok(())
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
}
