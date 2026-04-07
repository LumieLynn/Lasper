//! Filesystem type detection utilities.

use std::path::Path;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::utils::new_sync_command;

/// Detects the filesystem type of a given path using 'stat -f -c %T'.
pub fn get_filesystem_type(path: &Path) -> Result<String> {
    let out = new_sync_command("stat")
        .args(["-f", "-c", "%T", &path.to_string_lossy()])
        .output()
        .map_err(|e| NspawnError::Io(path.to_path_buf(), e))?;

    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(NspawnError::cmd_failed(
            "stat filesystem type",
            format!("stat -f -c %T {}", path.display()),
            &out,
        ))
    }
}
