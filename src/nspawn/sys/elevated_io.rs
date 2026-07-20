//! File I/O that transparently elevates via `sudo` or the elevated daemon.
//!
//! Routes each operation based on [`PermissionLevel`]:
//! - `Root`    → direct `tokio::fs`
//! - `Elevated` → daemon RPC (preferred) or `sudo` subprocess (fallback)
//! - `User`    → tries direct; returns `PermissionDenied` on EACCES/EPERM

use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::ops::PermissionLevel;
use std::path::Path;
use std::sync::Arc;

/// Wraps file I/O with elevation awareness.
#[derive(Clone, Debug)]
pub struct ElevatedIo {
    level: PermissionLevel,
    daemon: Option<Arc<crate::nspawn::sys::daemon::ElevatedDaemon>>,
}

impl ElevatedIo {
    pub fn new(level: PermissionLevel) -> Self {
        Self {
            level,
            daemon: None,
        }
    }

    /// Use the elevated daemon for file operations instead of `sudo`
    /// subprocesses. Only meaningful when `level` is `Elevated`.
    pub fn with_daemon(
        level: PermissionLevel,
        daemon: Arc<crate::nspawn::sys::daemon::ElevatedDaemon>,
    ) -> Self {
        Self {
            level,
            daemon: Some(daemon),
        }
    }

    /// Write `content` to `path`, elevating via daemon or `sudo tee` when needed.
    pub async fn write(&self, path: &Path, content: &str) -> Result<()> {
        match self.level {
            PermissionLevel::Root => Self::write_direct(path, content).await,
            PermissionLevel::Elevated => {
                if let Some(ref daemon) = self.daemon {
                    daemon.write_file(path, content).await
                } else {
                    Self::write_sudo(path, content).await
                }
            }
            PermissionLevel::User => Self::write_direct(path, content)
                .await
                .map_err(|e| deny_if_permission(e, path)),
        }
    }

    /// Remove a directory tree, elevating via daemon or `sudo rm -rf` when needed.
    pub async fn remove_dir_all(&self, path: &Path) -> Result<()> {
        match self.level {
            PermissionLevel::Root => tokio::fs::remove_dir_all(path)
                .await
                .map_err(|e| NspawnError::Io(path.to_path_buf(), e)),
            PermissionLevel::Elevated => {
                if let Some(ref daemon) = self.daemon {
                    daemon.remove_dir_all(path).await
                } else {
                    let out = crate::nspawn::sys::new_command("sudo")
                        .args(["rm", "-rf", &path.to_string_lossy()])
                        .output()
                        .await
                        .map_err(|e| NspawnError::Io(path.to_path_buf(), e))?;

                    if !out.status.success() {
                        return Err(NspawnError::cmd_failed(
                            "sudo rm",
                            format!("sudo rm -rf {}", path.display()),
                            &out,
                        ));
                    }
                    Ok(())
                }
            }
            PermissionLevel::User => tokio::fs::remove_dir_all(path)
                .await
                .map_err(|e| deny_if_permission(NspawnError::Io(path.to_path_buf(), e), path)),
        }
    }

    /// Create a directory (and parents), elevating via daemon or `sudo mkdir -p` when needed.
    pub async fn create_dir_all(&self, path: &Path) -> Result<()> {
        match self.level {
            PermissionLevel::Root => tokio::fs::create_dir_all(path)
                .await
                .map_err(|e| NspawnError::Io(path.to_path_buf(), e)),
            PermissionLevel::Elevated => {
                if let Some(ref daemon) = self.daemon {
                    daemon.create_dir_all(path).await
                } else {
                    let out = crate::nspawn::sys::new_command("sudo")
                        .args(["mkdir", "-p", &path.to_string_lossy()])
                        .output()
                        .await
                        .map_err(|e| NspawnError::Io(path.to_path_buf(), e))?;

                    if !out.status.success() {
                        return Err(NspawnError::cmd_failed(
                            "sudo mkdir",
                            format!("sudo mkdir -p {}", path.display()),
                            &out,
                        ));
                    }
                    Ok(())
                }
            }
            PermissionLevel::User => tokio::fs::create_dir_all(path)
                .await
                .map_err(|e| deny_if_permission(NspawnError::Io(path.to_path_buf(), e), path)),
        }
    }

    // ── private helpers ──

    async fn write_direct(path: &Path, content: &str) -> Result<()> {
        crate::nspawn::sys::io::AsyncLockedWriter::write_atomic(path, content).await
    }

    async fn write_sudo(path: &Path, content: &str) -> Result<()> {
        let mut child = crate::nspawn::sys::new_command("sudo")
            .args(["tee", &path.to_string_lossy()])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .map_err(|e| NspawnError::Io(path.to_path_buf(), e))?;

        use tokio::io::AsyncWriteExt;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(content.as_bytes())
                .await
                .map_err(|e| NspawnError::Io(path.to_path_buf(), e))?;
        }

        let out = child
            .wait_with_output()
            .await
            .map_err(|e| NspawnError::Io(path.to_path_buf(), e))?;

        if !out.status.success() {
            return Err(NspawnError::cmd_failed(
                "sudo tee",
                format!("sudo tee {}", path.display()),
                &out,
            ));
        }
        Ok(())
    }
}

fn is_permission_error(e: &NspawnError) -> bool {
    match e {
        NspawnError::Io(_, io_err) => {
            matches!(io_err.kind(), std::io::ErrorKind::PermissionDenied)
        }
        _ => false,
    }
}

fn deny_if_permission(e: NspawnError, path: &Path) -> NspawnError {
    if is_permission_error(&e) {
        log::warn!(
            "Permission denied for User-level operation on {} — run with -e to elevate.",
            path.display()
        );
        NspawnError::PermissionDenied
    } else {
        e
    }
}
