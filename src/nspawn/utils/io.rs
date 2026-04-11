use crate::nspawn::errors::{NspawnError, Result};
use fs2::FileExt;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::time::{sleep, Duration};

/// Manages transactional, locked and atomic writes to configuration files.
pub struct AsyncLockedWriter;

impl AsyncLockedWriter {
    /// Performs a transactional write operation on a file.
    ///
    /// The process follows these safety rules:
    /// 1. Uses a sidecar `.lock` file to avoid inode-switch race conditions.
    /// 2. Uses an async backoff loop to acquire the lock without blocking the Tokio executor.
    /// 3. Performs an atomic write via rename.
    pub async fn write_locked<F>(path: &Path, content_generator: F) -> Result<()>
    where
        F: FnOnce(Option<String>) -> Result<String>,
    {
        let path_buf = path.to_path_buf();
        let lock_path = path.with_extension("lock");
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
        let new_content = content_generator(existing_content)?;

        // Atomic update with durability
        {
            let mut f = fs::File::create(&tmp_path)
                .await
                .map_err(|e| NspawnError::Io(tmp_path.clone(), e))?;
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

        // Lock Hygiene: Delete lock file before closing handle
        let _ = fs::remove_file(&lock_path).await;

        Ok(())
    }

    /// Safely writes content to a file using atomic rename and fsync to ensure durability.
    /// Does not use a lock file.
    pub async fn write_atomic(path: &Path, content: &str) -> Result<()> {
        let tmp_path = path.with_extension("write.tmp");

        // 1. Ensure parent exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| NspawnError::Io(parent.to_path_buf(), e))?;
        }

        // 2. Write and sync
        {
            let mut f = fs::File::create(&tmp_path)
                .await
                .map_err(|e| NspawnError::Io(tmp_path.clone(), e))?;
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

    /// Safely copies a file using atomic rename to ensure the destination is никогда partially written.
    pub async fn atomic_copy(src: &Path, dest: &Path) -> Result<()> {
        let tmp_path = dest.with_extension("copy.tmp");

        // 1. Optimized copy to temp
        fs::copy(src, &tmp_path)
            .await
            .map_err(|e| NspawnError::Io(tmp_path.clone(), e))?;

        // 2. Ensure durability
        let f = fs::File::open(&tmp_path)
            .await
            .map_err(|e| NspawnError::Io(tmp_path.clone(), e))?;
        f.sync_data()
            .await
            .map_err(|e| NspawnError::Io(tmp_path.clone(), e))?;
        drop(f);

        // 3. Atomic swap
        fs::rename(&tmp_path, dest)
            .await
            .map_err(|e| NspawnError::Io(dest.to_path_buf(), e))?;

        // 4. Sync parent directory
        if let Some(parent) = dest.parent() {
            if let Ok(dir) = fs::File::open(parent).await {
                let _ = dir.sync_all().await;
            }
        }

        Ok(())
    }
}
