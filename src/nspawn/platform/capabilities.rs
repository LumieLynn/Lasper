use crate::nspawn::errors::{NspawnError, Result};
use std::os::unix::fs::FileTypeExt;

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
        "Could not determine host Wayland socket directory (XDG_RUNTIME_DIR or SUDO_UID missing)"
            .into(),
    ))
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
