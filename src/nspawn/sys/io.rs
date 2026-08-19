use crate::nspawn::errors::{NspawnError, Result};
use fs2::FileExt;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
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

        // Acquire lock (Async Backoff Loop)
        let lock_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| NspawnError::Io(lock_path.clone(), e))?;

        let mut attempts = 0;
        let max_attempts = 100; // 100 * 10ms = 1s timeout
        loop {
            match lock_file.try_lock_exclusive() {
                Ok(_) => break,
                Err(_) if attempts < max_attempts => {
                    attempts += 1;
                    sleep(Duration::from_millis(10)).await;
                }
                Err(e) => {
                    return Err(NspawnError::Runtime(format!(
                        "Could not acquire lock on {:?} after {} attempts: {}",
                        lock_path, attempts, e
                    )))
                }
            }
        }

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
