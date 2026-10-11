//! Filesystem type detection utilities.

use crate::adapters::error::{NspawnError, Result};
use std::path::Path;

const FILESYSTEM_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const MAX_FILESYSTEM_QUERY_OUTPUT_BYTES: usize = 64 * 1024;

/// Detects the filesystem type of a given path using 'stat -f -c %T'.
pub async fn get_filesystem_type(path: &Path) -> Result<String> {
    let mut command = crate::adapters::process::new_command("stat");
    command.args(["-f", "-c", "%T", &path.to_string_lossy()]);
    let out = crate::adapters::process::run_bounded_child_command(
        command,
        None,
        FILESYSTEM_QUERY_TIMEOUT,
        "stat filesystem type",
        MAX_FILESYSTEM_QUERY_OUTPUT_BYTES,
    )
    .await
    .map_err(|e| NspawnError::Io(path.to_path_buf(), e))?;
    crate::adapters::process::log_output("stat", &out);

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
