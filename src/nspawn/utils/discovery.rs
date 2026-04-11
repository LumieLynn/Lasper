use std::fs;
use std::os::unix::fs::FileTypeExt;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::utils::command::new_sync_command;

/// Determines the host's runtime directory (XDG_RUNTIME_DIR).
/// Returns an error if it cannot be determined reliably.
pub async fn get_xdg_runtime() -> Result<String> {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return Ok(dir);
    }

    // Fallback to /run/user/<SUDO_UID> if Lasper is run via sudo
    if let Ok(uid) = std::env::var("SUDO_UID") {
        let path = format!("/run/user/{}", uid);
        if tokio::fs::metadata(&path).await.is_ok() {
            return Ok(path);
        }
    }

    Err(NspawnError::Runtime(
        "Could not determine host Wayland socket directory (XDG_RUNTIME_DIR or SUDO_UID missing)".into()
    ))
}

/// Checks if the host system (Kernel >= 5.12 and systemd >= 248) supports :idmap mounts.
pub fn supports_idmap() -> bool {
    // 1. Check systemd version
    let systemd_ok = new_sync_command("systemd-nspawn")
        .arg("--version")
        .output()
        .ok()
        .and_then(|out| {
            let s = String::from_utf8_lossy(&out.stdout);
            // Example: systemd 255 (255.4-1.fc40)
            s.lines().next().and_then(|line| {
                line.split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse::<u32>().ok())
            })
        })
        .map(|v| v >= 248)
        .unwrap_or(false);

    if !systemd_ok {
        return false;
    }

    // 2. Check kernel version (uname -r)
    new_sync_command("uname")
        .arg("-r")
        .output()
        .ok()
        .and_then(|out| {
            let s = String::from_utf8_lossy(&out.stdout);
            // Example: 6.8.1-1.fc40.x86_64
            let parts: Vec<&str> = s.split('.').collect();
            if parts.len() >= 2 {
                let major = parts[0].parse::<u32>().unwrap_or(0);
                let minor = parts[1].split('-').next().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
                Some(major > 5 || (major == 5 && minor >= 12))
            } else {
                None
            }
        })
        .unwrap_or(false)
}

/// Scans the host's runtime directory for available Wayland sockets.
pub async fn scan_available_wayland_sockets() -> Vec<String> {
    let xdg_runtime = match get_xdg_runtime().await {
        Ok(dir) => dir,
        Err(_) => return Vec::new(),
    };

    let mut sockets = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(&xdg_runtime).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();

            // Match wayland-* but exclude .lock files
            if name.starts_with("wayland-") && !name.ends_with(".lock") {
                if let Ok(meta) = entry.metadata().await {
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
