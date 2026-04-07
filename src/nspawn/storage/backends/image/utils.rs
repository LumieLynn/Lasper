//! Utility functions for disk image backend (device discovery, UUIDs, etc.)

use std::path::{Path, PathBuf};
use crate::nspawn::utils::new_command;
use crate::nspawn::errors::Result;

/// Get the standard Discoverable Partition Specification UUID for the root partition
/// based on the host architecture.
pub fn get_discoverable_root_uuid() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "B921B045-1DF0-41C3-AF44-4C6F280D3FAE", // ARM64 
        "x86_64" => "4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709",  // x86-64
        "x86" => "44479540-F297-41B2-9AF7-D131D5F0458A",     // x86 (32-bit)
        "arm" => "69DAD710-2CE4-4E3C-B16C-21A1D49ABED3",     // ARM (32-bit)
        "riscv64" => "1AE5EE25-DDF4-4BD0-8459-24AC0BBE1559", // RISC-V 64-bit
        _ => "4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709",         // Default fallback to x86_64
    }
}

/// Find a loop device associated with a specific file path.
pub async fn find_loop_device(file_path: &Path) -> Result<Option<PathBuf>> {
    let out = new_command("losetup").args(["-j", &file_path.to_string_lossy()]).output().await?;
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

/// Finds the first unattached NBD device by checking if it has an active PID.
pub async fn find_free_nbd_device() -> Result<Option<String>> {
    for i in 0..16 {
        let pid_path = format!("/sys/class/block/nbd{}/pid", i);
        if !Path::new(&pid_path).exists() {
            return Ok(Some(format!("/dev/nbd{}", i)));
        }
    }
    Ok(None)
}

/// Surgically finds which NBD device is backed by a specific image file 
/// by cross-referencing the PID with the process cmdline.
pub async fn find_nbd_device(file_path: &Path) -> Result<Option<PathBuf>> {
    let target_str = file_path.to_string_lossy();
    
    for i in 0..16 {
        let pid_path = format!("/sys/class/block/nbd{}/pid", i);
        if let Ok(pid_str) = tokio::fs::read_to_string(&pid_path).await {
            let pid = pid_str.trim();
            let cmdline_path = format!("/proc/{}/cmdline", pid);
            
            // Check if this qemu-nbd process was launched with our file path
            if let Ok(cmdline) = tokio::fs::read_to_string(&cmdline_path).await {
                // cmdline arguments are null-separated in /proc
                if cmdline.contains(target_str.as_ref()) {
                    return Ok(Some(PathBuf::from(format!("/dev/nbd{}", i))));
                }
            }
        }
    }
    Ok(None)
}

/// Check if a block device is LUKS encrypted.
pub async fn is_luks(device: &Path) -> bool {
    let out = new_command("cryptsetup").args(["isLuks", &device.to_string_lossy()]).output().await;
    match out {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}
