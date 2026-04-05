//! General utilities for system environment discovery.

use std::fs;
use std::os::unix::fs::FileTypeExt;

/// Scans the host's runtime directory for available Wayland sockets.
///
/// This handles the discovery of sockets like `wayland-0`, `wayland-1`, etc.,
/// while robustly handling `SUDO_UID` and excluding lock files.
pub fn scan_available_wayland_sockets() -> Vec<String> {
    let uid = std::env::var("SUDO_UID")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .map(|u| u.to_string())
        .unwrap_or_else(|| "1000".to_string());

    let xdg_runtime = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/run/user/{}", uid));

    let mut sockets = Vec::new();
    if let Ok(entries) = fs::read_dir(&xdg_runtime) {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();

            // Match wayland-* but exclude .lock files
            if name.starts_with("wayland-") && !name.ends_with(".lock") {
                if let Ok(meta) = entry.metadata() {
                    if meta.file_type().is_socket() {
                        sockets.push(name.to_string());
                    }
                }
            }
        }
    }
    
    sockets.sort();
    sockets
}

/// Creates a new `tokio::process::Command` with `LC_ALL=C` set.
pub fn new_command(program: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.env("LC_ALL", "C");
    cmd
}

/// Creates a new `std::process::Command` with `LC_ALL=C` set.
pub fn new_sync_command(program: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    cmd.env("LC_ALL", "C");
    cmd
}
