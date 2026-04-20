//! Filesystem type detection utilities.

use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::sys::command::CommandLogged;
use std::path::Path;

/// Detects the filesystem type of a given path using 'stat -f -c %T'.
pub async fn get_filesystem_type(path: &Path) -> Result<String> {
    let out = crate::nspawn::sys::new_command("stat")
        .args(["-f", "-c", "%T", &path.to_string_lossy()])
        .logged_output("stat")
        .await
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
