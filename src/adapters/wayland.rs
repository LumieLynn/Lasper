use crate::adapters::error::{NspawnError, Result};
use crate::domain::wayland::{
    HostWaylandSocket, SocketRevision, WaylandBindPolicy, WaylandDisplay,
};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub(crate) const CONTAINER_WAYLAND_ROOT: &str = "/run/lasper/wayland";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WaylandBind {
    source: PathBuf,
    target: PathBuf,
    policy: WaylandBindPolicy,
}

impl WaylandBind {
    pub(crate) fn new(
        source: impl Into<PathBuf>,
        target_uid: u32,
        display: &WaylandDisplay,
        policy: WaylandBindPolicy,
    ) -> Self {
        Self {
            source: source.into(),
            target: container_socket_path(target_uid, display),
            policy,
        }
    }

    pub(crate) fn source(&self) -> &Path {
        &self.source
    }

    pub(crate) fn target(&self) -> &Path {
        &self.target
    }

    pub(crate) fn policy(&self) -> WaylandBindPolicy {
        self.policy
    }
}

pub(crate) fn container_socket_path(uid: u32, display: &WaylandDisplay) -> PathBuf {
    Path::new(CONTAINER_WAYLAND_ROOT)
        .join(uid.to_string())
        .join(display.as_str())
}

/// Re-observe a previously captured host socket and require an exact metadata
/// match. This is point-in-time evidence only: it does not pin the inode across
/// a later `systemd-nspawn` launch, whose `Bind=` source remains a pathname.
pub(crate) async fn revalidate_host_socket(
    source: &HostWaylandSocket,
    authorized_uid: u32,
) -> Result<PathBuf> {
    if source.session_uid() != authorized_uid || source.owner_uid() != authorized_uid {
        return Err(NspawnError::Validation(format!(
            "Wayland grant does not belong to authorized uid {authorized_uid}"
        )));
    }
    let runtime = source.runtime_dir();
    if !runtime.is_absolute() {
        return Err(NspawnError::Validation(
            "Wayland runtime directory must be absolute".into(),
        ));
    }
    let canonical_runtime = tokio::fs::canonicalize(runtime)
        .await
        .map_err(|error| NspawnError::Io(runtime.to_path_buf(), error))?;
    if canonical_runtime != runtime {
        return Err(NspawnError::Validation(
            "Wayland runtime directory evidence is no longer canonical".into(),
        ));
    }
    let metadata = tokio::fs::metadata(runtime)
        .await
        .map_err(|error| NspawnError::Io(runtime.to_path_buf(), error))?;
    if !metadata.is_dir() || (metadata.uid() != 0 && metadata.uid() != authorized_uid) {
        return Err(NspawnError::Validation(format!(
            "Wayland socket directory is not owned by uid {authorized_uid} or root"
        )));
    }
    if metadata.permissions().mode() & 0o022 != 0 {
        return Err(NspawnError::Validation(
            "Wayland socket directory is writable by group or others".into(),
        ));
    }

    let requested_socket = runtime.join(source.display().as_str());
    if requested_socket.parent() != Some(runtime) {
        return Err(NspawnError::Validation(
            "Wayland socket entry escaped its runtime directory".into(),
        ));
    }
    let canonical_socket = tokio::fs::canonicalize(&requested_socket)
        .await
        .map_err(|error| NspawnError::Io(requested_socket.clone(), error))?;
    // WSLg exposes the display as a symlink whose verified target may be
    // outside the session runtime directory.
    if canonical_socket != source.canonical_path() {
        return Err(NspawnError::Validation(
            "Wayland socket was replaced after discovery".into(),
        ));
    }
    let observed = observe_socket_target(&canonical_socket).await?;
    if observed.owner_uid != source.owner_uid()
        || observed.owner_gid != source.owner_gid()
        || observed.mode != source.mode()
        || observed.revision != source.revision()
    {
        return Err(NspawnError::Validation(
            "Wayland socket metadata changed after discovery".into(),
        ));
    }
    Ok(canonical_socket)
}

struct ObservedWaylandSocket {
    owner_uid: u32,
    owner_gid: u32,
    mode: u32,
    revision: SocketRevision,
}

async fn observe_socket_target(path: &Path) -> Result<ObservedWaylandSocket> {
    let path_text = path
        .to_str()
        .ok_or_else(|| NspawnError::Validation("Wayland socket path is not valid UTF-8".into()))?;
    if path_text.chars().any(char::is_control) || !path.is_absolute() {
        return Err(NspawnError::Validation(format!(
            "Invalid Wayland socket path: {path_text:?}"
        )));
    }

    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| NspawnError::Io(path.to_path_buf(), error))?;
    if !metadata.file_type().is_socket() {
        return Err(NspawnError::Validation(format!(
            "Wayland path is not a socket: {}",
            path.display()
        )));
    }
    Ok(ObservedWaylandSocket {
        owner_uid: metadata.uid(),
        owner_gid: metadata.gid(),
        mode: metadata.permissions().mode() & 0o7777,
        revision: SocketRevision {
            device: metadata.dev(),
            inode: metadata.ino(),
            ctime_seconds: metadata.ctime(),
            ctime_nanoseconds: metadata.ctime_nsec(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_is_namespaced_by_numeric_container_identity_and_display() {
        let display = WaylandDisplay::new("wayland-2").unwrap();
        assert_eq!(
            container_socket_path(1001, &display),
            Path::new("/run/lasper/wayland/1001/wayland-2")
        );
    }
}
