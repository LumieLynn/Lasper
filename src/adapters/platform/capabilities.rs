use crate::adapters::error::{NspawnError, Result};
use crate::domain::wayland::{HostWaylandSocket, SocketRevision, WaylandDisplay};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Returns runtime-directory candidates in discovery priority order.
///
/// `XDG_RUNTIME_DIR` is authoritative when present, including non-standard
/// layouts such as WSLg. The UID-derived systemd path is the fallback for
/// sessions that do not export the variable or whose XDG directory has no
/// usable Wayland socket.
fn runtime_dir_candidates(session_uid: u32) -> Vec<PathBuf> {
    runtime_dir_candidates_from(session_uid, std::env::var_os("XDG_RUNTIME_DIR"))
}

fn runtime_dir_candidates_from(
    session_uid: u32,
    xdg_runtime: Option<std::ffi::OsString>,
) -> Vec<PathBuf> {
    let uid_path = PathBuf::from(format!("/run/user/{session_uid}"));
    let mut candidates = Vec::new();
    if let Some(dir) = xdg_runtime {
        candidates.push(PathBuf::from(dir));
    }
    if !candidates.iter().any(|candidate| candidate == &uid_path) {
        candidates.push(uid_path);
    }
    candidates
}

/// Discovers session sockets and captures evidence that can be revalidated by
/// the privileged configuration writer immediately before applying a bind.
pub async fn discover_wayland_sockets() -> Vec<HostWaylandSocket> {
    let session_uid = invoking_uid();
    let mut last_error = None;
    for candidate in runtime_dir_candidates(session_uid) {
        let runtime = match validate_runtime_directory(&candidate, session_uid).await {
            Ok(runtime) => runtime,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        let sockets = discover_wayland_sockets_in(&runtime, session_uid).await;
        if !sockets.is_empty() {
            return sockets;
        }
    }

    if let Some(error) = last_error {
        log::warn!("Wayland socket discovery unavailable: {error}");
    }
    Vec::new()
}

async fn discover_wayland_sockets_in(runtime: &Path, session_uid: u32) -> Vec<HostWaylandSocket> {
    let mut sockets = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(&runtime).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };

            // Match wayland-* but exclude .lock files.
            // Use tokio::fs::metadata (stat, follows symlinks) instead of
            // entry.metadata() (lstat) so that WSLg symlink-to-socket entries
            // are detected correctly. The resolved target may live outside
            // the runtime directory; the entry itself is still constrained
            // to this verified directory and the target is checked below.
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
                runtime.to_path_buf(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    #[test]
    fn xdg_runtime_precedes_uid_runtime_fallback() {
        let candidates =
            runtime_dir_candidates_from(uzers::get_current_uid(), Some("/custom/runtime".into()));
        assert_eq!(candidates[0], PathBuf::from("/custom/runtime"));
    }

    #[tokio::test]
    async fn socket_discovery_follows_wayland_symlink() {
        let runtime = tempfile::tempdir().unwrap();
        let target_runtime = tempfile::tempdir().unwrap();
        tokio::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();
        tokio::fs::set_permissions(
            target_runtime.path(),
            std::fs::Permissions::from_mode(0o700),
        )
        .await
        .unwrap();
        let target = target_runtime.path().join("wayland-real");
        let _listener = UnixListener::bind(&target).unwrap();
        std::os::unix::fs::symlink(&target, runtime.path().join("wayland-0")).unwrap();

        let sockets = discover_wayland_sockets_in(runtime.path(), uzers::get_current_uid()).await;
        assert_eq!(sockets.len(), 1);
        assert_eq!(sockets[0].display().as_str(), "wayland-0");
        assert_eq!(
            sockets[0].canonical_path(),
            std::fs::canonicalize(target).unwrap()
        );
    }
}
