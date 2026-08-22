use crate::domain::wayland::{HostWaylandSocket, SocketRevision, WaylandDisplay};
use crate::nspawn::errors::{NspawnError, Result};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Determines the host's runtime directory (XDG_RUNTIME_DIR).
/// Returns an error if it cannot be determined reliably.
async fn get_xdg_runtime() -> Result<String> {
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

/// Discovers session sockets and captures evidence that can be revalidated by
/// the privileged configuration writer immediately before applying a bind.
pub async fn discover_wayland_sockets() -> Vec<HostWaylandSocket> {
    let runtime = match get_xdg_runtime().await {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => return Vec::new(),
    };
    let session_uid = invoking_uid();
    let runtime = match validate_runtime_directory(&runtime, session_uid).await {
        Ok(runtime) => runtime,
        Err(error) => {
            log::warn!("Ignoring unsafe Wayland runtime directory: {error}");
            return Vec::new();
        }
    };

    let mut sockets = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(&runtime).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };

            // Match wayland-* but exclude .lock files.
            // Use tokio::fs::metadata (stat, follows symlinks) instead of
            // entry.metadata() (lstat) so that WSL symlink-to-socket entries
            // are detected correctly.
            if !name.starts_with("wayland-") || name.ends_with(".lock") {
                continue;
            }
            let Ok(display) = WaylandDisplay::new(name.to_string()) else {
                continue;
            };
            let requested = entry.path();
            let Ok(canonical) = tokio::fs::canonicalize(&requested).await else {
                continue;
            };
            if !canonical.starts_with(&runtime) {
                continue;
            }
            let Ok(meta) = tokio::fs::metadata(&canonical).await else {
                continue;
            };
            if !meta.file_type().is_socket() || meta.uid() != session_uid {
                continue;
            }
            let revision = SocketRevision {
                device: meta.dev(),
                inode: meta.ino(),
                ctime_seconds: meta.ctime(),
                ctime_nanoseconds: meta.ctime_nsec(),
            };
            if let Ok(socket) = HostWaylandSocket::from_verified_parts(
                display,
                runtime.clone(),
                canonical,
                session_uid,
                meta.uid(),
                meta.gid(),
                meta.permissions().mode(),
                revision,
            ) {
                sockets.push(socket);
            }
        }
    }

    sockets.sort_by(|left, right| left.display().as_str().cmp(right.display().as_str()));
    sockets
}

async fn validate_runtime_directory(path: &Path, session_uid: u32) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(NspawnError::Validation(format!(
            "XDG runtime directory is not absolute: {}",
            path.display()
        )));
    }
    let canonical = tokio::fs::canonicalize(path)
        .await
        .map_err(|error| NspawnError::Io(path.to_path_buf(), error))?;
    let metadata = tokio::fs::metadata(&canonical)
        .await
        .map_err(|error| NspawnError::Io(canonical.clone(), error))?;
    if !metadata.is_dir() || metadata.uid() != session_uid {
        return Err(NspawnError::Validation(format!(
            "XDG runtime directory is not owned by uid {session_uid}"
        )));
    }
    if metadata.permissions().mode() & 0o022 != 0 {
        return Err(NspawnError::Validation(
            "XDG runtime directory is writable by group or others".into(),
        ));
    }
    Ok(canonical)
}

pub(crate) fn invoking_uid() -> u32 {
    if uzers::get_current_uid() == 0 {
        if let Ok(uid) = std::env::var("SUDO_UID") {
            if let Ok(uid) = uid.parse() {
                return uid;
            }
        }
    }
    uzers::get_current_uid()
}
