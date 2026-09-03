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
    discover_wayland_sockets_from(
        invoking_uid(),
        std::env::var_os("XDG_RUNTIME_DIR"),
        std::env::var_os("WAYLAND_DISPLAY"),
    )
    .await
}

async fn discover_wayland_sockets_from(
    session_uid: u32,
    xdg_runtime: Option<std::ffi::OsString>,
    configured_display: Option<std::ffi::OsString>,
) -> Vec<HostWaylandSocket> {
    let mut last_error = None;
    let configured_path = configured_display.map(PathBuf::from);
    let preferred_socket =
        if let Some(path) = configured_path.as_deref().filter(|path| path.is_absolute()) {
            match discover_absolute_wayland_socket(path, session_uid).await {
                Ok(socket) => Some(socket),
                Err(error) => {
                    last_error = Some(error);
                    None
                }
            }
        } else {
            None
        };

    let preferred_display = configured_path
        .as_deref()
        .filter(|path| !path.is_absolute())
        .and_then(Path::to_str)
        .and_then(|display| WaylandDisplay::new(display.to_string()).ok());
    for candidate in runtime_dir_candidates_from(session_uid, xdg_runtime) {
        let runtime = match validate_runtime_directory(&candidate, session_uid).await {
            Ok(runtime) => runtime,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        let mut sockets = discover_wayland_sockets_in(&runtime, session_uid).await;
        if !sockets.is_empty() {
            if let Some(preferred) = preferred_socket.as_ref() {
                sockets.retain(|socket| {
                    socket.display() != preferred.display()
                        && socket.canonical_path() != preferred.canonical_path()
                });
                sockets.insert(0, preferred.clone());
            } else {
                prioritize_display(&mut sockets, preferred_display.as_ref());
            }
            return sockets;
        }
    }

    if let Some(preferred) = preferred_socket {
        return vec![preferred];
    }

    if let Some(error) = last_error {
        log::warn!("Wayland socket discovery unavailable: {error}");
    }
    Vec::new()
}

async fn discover_absolute_wayland_socket(
    path: &Path,
    session_uid: u32,
) -> Result<HostWaylandSocket> {
    let display = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            NspawnError::Validation(format!(
                "WAYLAND_DISPLAY has no usable socket name: {}",
                path.display()
            ))
        })
        .and_then(|name| {
            WaylandDisplay::new(name.to_string())
                .map_err(|error| NspawnError::Validation(error.to_string()))
        })?;
    let parent = path.parent().ok_or_else(|| {
        NspawnError::Validation(format!(
            "WAYLAND_DISPLAY has no parent directory: {}",
            path.display()
        ))
    })?;
    let runtime = validate_wayland_directory(parent, session_uid, true).await?;
    inspect_wayland_socket(
        &runtime,
        &runtime.join(display.as_str()),
        display,
        session_uid,
    )
    .await
}

fn prioritize_display(sockets: &mut Vec<HostWaylandSocket>, preferred: Option<&WaylandDisplay>) {
    let Some(index) = preferred.and_then(|preferred| {
        sockets
            .iter()
            .position(|socket| socket.display() == preferred)
    }) else {
        return;
    };
    let preferred = sockets.remove(index);
    sockets.insert(0, preferred);
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
            if let Ok(socket) =
                inspect_wayland_socket(runtime, &entry.path(), display, session_uid).await
            {
                sockets.push(socket);
            }
        }
    }

    sockets.sort_by(|left, right| left.display().as_str().cmp(right.display().as_str()));
    sockets
}

async fn inspect_wayland_socket(
    runtime: &Path,
    requested: &Path,
    display: WaylandDisplay,
    session_uid: u32,
) -> Result<HostWaylandSocket> {
    let canonical = tokio::fs::canonicalize(requested)
        .await
        .map_err(|error| NspawnError::Io(requested.to_path_buf(), error))?;
    let metadata = tokio::fs::metadata(&canonical)
        .await
        .map_err(|error| NspawnError::Io(canonical.clone(), error))?;
    if !metadata.file_type().is_socket() {
        return Err(NspawnError::Validation(format!(
            "Wayland display is not a Unix socket: {}",
            requested.display()
        )));
    }
    if metadata.uid() != session_uid {
        return Err(NspawnError::Validation(format!(
            "Wayland socket is not owned by uid {session_uid}: {}",
            requested.display()
        )));
    }
    HostWaylandSocket::from_verified_parts(
        display,
        runtime.to_path_buf(),
        canonical,
        session_uid,
        metadata.uid(),
        metadata.gid(),
        metadata.permissions().mode(),
        SocketRevision {
            device: metadata.dev(),
            inode: metadata.ino(),
            ctime_seconds: metadata.ctime(),
            ctime_nanoseconds: metadata.ctime_nsec(),
        },
    )
    .map_err(|error| NspawnError::Validation(error.to_string()))
}

async fn validate_runtime_directory(path: &Path, session_uid: u32) -> Result<PathBuf> {
    validate_wayland_directory(path, session_uid, false).await
}

async fn validate_wayland_directory(
    path: &Path,
    session_uid: u32,
    allow_root_owner: bool,
) -> Result<PathBuf> {
    let label = if allow_root_owner {
        "WAYLAND_DISPLAY parent directory"
    } else {
        "XDG runtime directory"
    };
    if !path.is_absolute() {
        return Err(NspawnError::Validation(format!(
            "{label} is not absolute: {}",
            path.display()
        )));
    }
    let canonical = tokio::fs::canonicalize(path)
        .await
        .map_err(|error| NspawnError::Io(path.to_path_buf(), error))?;
    let metadata = tokio::fs::metadata(&canonical)
        .await
        .map_err(|error| NspawnError::Io(canonical.clone(), error))?;
    let trusted_owner = metadata.uid() == session_uid || (allow_root_owner && metadata.uid() == 0);
    if !metadata.is_dir() || !trusted_owner {
        return Err(NspawnError::Validation(format!(
            "{label} is not owned by uid {session_uid}{}",
            if allow_root_owner { " or root" } else { "" }
        )));
    }
    if metadata.permissions().mode() & 0o022 != 0 {
        return Err(NspawnError::Validation(format!(
            "{label} is writable by group or others"
        )));
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

    #[tokio::test]
    async fn absolute_wayland_display_is_preferred_without_hiding_other_sockets() {
        let runtime = tempfile::tempdir().unwrap();
        tokio::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();
        let socket_path = runtime.path().join("compositor.sock");
        let _preferred = UnixListener::bind(&socket_path).unwrap();
        let _other = UnixListener::bind(runtime.path().join("wayland-0")).unwrap();

        let sockets = discover_wayland_sockets_from(
            uzers::get_current_uid(),
            Some(runtime.path().as_os_str().to_os_string()),
            Some(socket_path.as_os_str().to_os_string()),
        )
        .await;

        assert_eq!(sockets.len(), 2);
        assert_eq!(sockets[0].display().as_str(), "compositor.sock");
        assert_eq!(
            sockets[0].canonical_path(),
            std::fs::canonicalize(socket_path).unwrap()
        );
        assert_eq!(sockets[1].display().as_str(), "wayland-0");
    }

    #[tokio::test]
    async fn relative_wayland_display_is_preferred_within_xdg_runtime() {
        let runtime = tempfile::tempdir().unwrap();
        tokio::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();
        let _first = UnixListener::bind(runtime.path().join("wayland-0")).unwrap();
        let _preferred = UnixListener::bind(runtime.path().join("wayland-1")).unwrap();

        let sockets = discover_wayland_sockets_from(
            uzers::get_current_uid(),
            Some(runtime.path().as_os_str().to_os_string()),
            Some("wayland-1".into()),
        )
        .await;

        assert_eq!(sockets.len(), 2);
        assert_eq!(sockets[0].display().as_str(), "wayland-1");
    }
}
