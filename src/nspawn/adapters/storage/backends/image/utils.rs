//! Utility functions for disk image backend (device discovery, UUIDs, etc.)

use crate::nspawn::errors::Result;
use crate::nspawn::sys::{log_output, CommandRunner, ElevatedIo};
use std::path::{Path, PathBuf};

/// Get the standard Discoverable Partition Specification UUID for the root partition.
pub fn get_discoverable_root_uuid() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "B921B045-1DF0-41C3-AF44-4C6F280D3FAE",
        "x86_64" => "4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709",
        "x86" => "44479540-F297-41B2-9AF7-D131D5F0458A",
        "arm" => "69DAD710-2CE4-4E3C-B16C-21A1D49ABED3",
        "riscv64" => "1AE5EE25-DDF4-4BD0-8459-24AC0BBE1559",
        _ => "4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709",
    }
}

/// Find a loop device associated with a specific file path.
pub async fn find_loop_device(
    file_path: &Path,
    cmd_runner: &dyn CommandRunner,
    _io: &ElevatedIo,
) -> Result<Option<PathBuf>> {
    let out = cmd_runner
        .run(
            "losetup",
            vec!["-j".into(), file_path.to_string_lossy().to_string()],
        )
        .await?;
    log_output("losetup", &out);
    if !out.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if let Some(line) = stdout.lines().next() {
        if let Some(dev) = line.split(':').next() {
            return Ok(Some(PathBuf::from(dev)));
        }
    }
    Ok(None)
}
