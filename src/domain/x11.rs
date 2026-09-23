use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum X11ValidationError {
    #[error("X11 endpoint source is not the standard path for display :{display}")]
    InvalidSource { display: u16 },
    #[error("X11 endpoint canonical path must be absolute: {0:?}")]
    CanonicalPathNotAbsolute(PathBuf),
    #[error("X11 endpoint peer PID is outside the Linux PID range: {0}")]
    InvalidPeerPid(u32),
    #[error("X11 bind target must be absolute: {0:?}")]
    BindTargetNotAbsolute(PathBuf),
    #[error("X11 bind target contains a relative path component: {0:?}")]
    BindTargetRelative(PathBuf),
    #[error("X11 bind target contains a control character")]
    BindTargetControlCharacter,
}

/// Identity returned by the Unix socket peer-credential query.
///
/// A zero PID is not a process identity.  Linux uses it when the peer lives
/// outside the caller's PID namespace (for example, the WSLg X server), while
/// the mapped UID/GID may still be available.  Keeping that case explicit
/// prevents callers from treating `/proc/0` as a server-generation proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum X11PeerIdentity {
    LocalProcess { pid: u32, uid: u32, gid: u32 },
    External { uid: u32, gid: u32 },
}

impl X11PeerIdentity {
    pub(crate) fn from_raw(pid: u32, uid: u32, gid: u32) -> Result<Self, X11ValidationError> {
        if pid > i32::MAX as u32 {
            return Err(X11ValidationError::InvalidPeerPid(pid));
        }
        Ok(if pid == 0 {
            Self::External { uid, gid }
        } else {
            Self::LocalProcess { pid, uid, gid }
        })
    }

    pub const fn pid(self) -> Option<u32> {
        match self {
            Self::LocalProcess { pid, .. } => Some(pid),
            Self::External { .. } => None,
        }
    }

    pub const fn uid(self) -> u32 {
        match self {
            Self::LocalProcess { uid, .. } | Self::External { uid, .. } => uid,
        }
    }

    pub const fn gid(self) -> u32 {
        match self {
            Self::LocalProcess { gid, .. } | Self::External { gid, .. } => gid,
        }
    }

    pub const fn is_local_process(self) -> bool {
        matches!(self, Self::LocalProcess { .. })
    }

    /// Preserve the pre-existing three-field representation at internal
    /// persistence boundaries.  External peers use the explicit zero-PID
    /// sentinel only at that boundary; no process-oriented code should use it
    /// as an actual PID.
    pub(crate) const fn legacy_tuple(self) -> (u32, u32, u32) {
        let pid = match self.pid() {
            Some(pid) => pid,
            None => 0,
        };
        (pid, self.uid(), self.gid())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct X11SocketRevision {
    pub device: u64,
    pub inode: u64,
    pub ctime_seconds: i64,
    pub ctime_nanoseconds: i64,
}

/// A filesystem X11 endpoint observed from the invoking desktop session.
///
/// `source` is the stable path written to an nspawn declaration. The canonical
/// path and socket/peer identities are observations for this discovery only;
/// they must be revalidated before a future runtime authorization operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostX11Socket {
    display: u16,
    alternate: bool,
    source: PathBuf,
    canonical_path: PathBuf,
    owner_uid: u32,
    owner_gid: u32,
    mode: u32,
    peer_pid: u32,
    peer_uid: u32,
    peer_gid: u32,
    revision: X11SocketRevision,
}

impl<'de> Deserialize<'de> for HostX11Socket {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Evidence {
            display: u16,
            alternate: bool,
            source: PathBuf,
            canonical_path: PathBuf,
            owner_uid: u32,
            owner_gid: u32,
            mode: u32,
            peer_pid: u32,
            peer_uid: u32,
            peer_gid: u32,
            revision: X11SocketRevision,
        }

        let evidence = Evidence::deserialize(deserializer)?;
        Self::from_verified_parts(
            evidence.display,
            evidence.alternate,
            evidence.source,
            evidence.canonical_path,
            evidence.owner_uid,
            evidence.owner_gid,
            evidence.mode,
            evidence.peer_pid,
            evidence.peer_uid,
            evidence.peer_gid,
            evidence.revision,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl HostX11Socket {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_verified_parts(
        display: u16,
        alternate: bool,
        source: PathBuf,
        canonical_path: PathBuf,
        owner_uid: u32,
        owner_gid: u32,
        mode: u32,
        peer_pid: u32,
        peer_uid: u32,
        peer_gid: u32,
        revision: X11SocketRevision,
    ) -> Result<Self, X11ValidationError> {
        let expected = format!("X{display}{}", if alternate { "_" } else { "" });
        if source.parent() != Some(Path::new("/tmp/.X11-unix"))
            || source.file_name().and_then(|name| name.to_str()) != Some(expected.as_str())
        {
            return Err(X11ValidationError::InvalidSource { display });
        }
        if !canonical_path.is_absolute() {
            return Err(X11ValidationError::CanonicalPathNotAbsolute(canonical_path));
        }
        let _ = X11PeerIdentity::from_raw(peer_pid, peer_uid, peer_gid)?;
        Ok(Self {
            display,
            alternate,
            source,
            canonical_path,
            owner_uid,
            owner_gid,
            mode: mode & 0o7777,
            peer_pid,
            peer_uid,
            peer_gid,
            revision,
        })
    }

    pub fn display(&self) -> u16 {
        self.display
    }

    pub fn alternate(&self) -> bool {
        self.alternate
    }

    pub fn source(&self) -> &Path {
        &self.source
    }

    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    pub fn owner_uid(&self) -> u32 {
        self.owner_uid
    }

    pub fn owner_gid(&self) -> u32 {
        self.owner_gid
    }

    pub fn mode(&self) -> u32 {
        self.mode
    }

    pub fn peer_identity(&self) -> X11PeerIdentity {
        X11PeerIdentity::from_raw(self.peer_pid, self.peer_uid, self.peer_gid)
            .expect("validated X11 socket stores a valid peer identity")
    }

    pub fn revision(&self) -> X11SocketRevision {
        self.revision
    }
}

/// A provisioning-time declaration that exposes one observed filesystem X11
/// endpoint inside a newly-created machine. Runtime ACL authorization is a
/// separate operation and is deliberately not represented here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct X11BindIntent {
    socket: HostX11Socket,
    target: PathBuf,
}

impl<'de> Deserialize<'de> for X11BindIntent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            socket: HostX11Socket,
            target: PathBuf,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.socket, wire.target).map_err(serde::de::Error::custom)
    }
}

impl X11BindIntent {
    pub fn same_path(socket: HostX11Socket) -> Self {
        Self::new(socket.clone(), socket.source().to_path_buf())
            .expect("an observed X11 source is an absolute bind target")
    }

    pub fn new(socket: HostX11Socket, target: PathBuf) -> Result<Self, X11ValidationError> {
        validate_bind_target(&target)?;
        Ok(Self { socket, target })
    }

    pub fn socket(&self) -> &HostX11Socket {
        &self.socket
    }

    pub fn target(&self) -> &Path {
        &self.target
    }
}

fn validate_bind_target(target: &Path) -> Result<(), X11ValidationError> {
    if !target.is_absolute() {
        return Err(X11ValidationError::BindTargetNotAbsolute(
            target.to_path_buf(),
        ));
    }
    if target.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        return Err(X11ValidationError::BindTargetRelative(target.to_path_buf()));
    }
    if target.to_string_lossy().chars().any(char::is_control) {
        return Err(X11ValidationError::BindTargetControlCharacter);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socket() -> HostX11Socket {
        HostX11Socket::from_verified_parts(
            0,
            false,
            "/tmp/.X11-unix/X0".into(),
            "/tmp/.X11-unix/X0".into(),
            1000,
            1000,
            0o777,
            42,
            1000,
            1000,
            X11SocketRevision {
                device: 1,
                inode: 2,
                ctime_seconds: 3,
                ctime_nanoseconds: 4,
            },
        )
        .unwrap()
    }

    #[test]
    fn wire_round_trip_revalidates_endpoint_invariants() {
        let encoded = serde_json::to_value(socket()).unwrap();
        let decoded: HostX11Socket = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(decoded, socket());

        let mut invalid = encoded;
        invalid["source"] = serde_json::json!("/run/user/1000/not-an-x11-socket");
        assert!(serde_json::from_value::<HostX11Socket>(invalid).is_err());
    }

    #[test]
    fn zero_peer_pid_is_an_external_identity_not_a_local_process() {
        let external = HostX11Socket::from_verified_parts(
            0,
            false,
            "/tmp/.X11-unix/X0".into(),
            "/tmp/.X11-unix/X0".into(),
            1000,
            1000,
            0o777,
            0,
            1000,
            1000,
            X11SocketRevision {
                device: 1,
                inode: 2,
                ctime_seconds: 3,
                ctime_nanoseconds: 4,
            },
        )
        .unwrap();
        assert_eq!(
            external.peer_identity(),
            X11PeerIdentity::External {
                uid: 1000,
                gid: 1000
            }
        );
        assert!(!external.peer_identity().is_local_process());

        let encoded = serde_json::to_value(&external).unwrap();
        let decoded: HostX11Socket = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, external);
    }

    #[test]
    fn peer_pid_above_linux_range_remains_invalid() {
        let error = X11PeerIdentity::from_raw(i32::MAX as u32 + 1, 1000, 1000).unwrap_err();
        assert_eq!(
            error,
            X11ValidationError::InvalidPeerPid(i32::MAX as u32 + 1)
        );
    }

    #[test]
    fn bind_intent_validates_guest_target_and_preserves_source_evidence() {
        let socket = socket();
        let intent = X11BindIntent::new(socket.clone(), "/mnt/x11/X0".into()).unwrap();
        assert_eq!(intent.socket(), &socket);
        assert_eq!(intent.target(), Path::new("/mnt/x11/X0"));
        assert!(X11BindIntent::new(socket.clone(), "relative/X0".into()).is_err());
        assert!(X11BindIntent::new(socket, "/mnt/../x11/X0".into()).is_err());

        let encoded = serde_json::json!({
            "socket": serde_json::to_value(intent.socket()).unwrap(),
            "target": "relative/X0"
        });
        assert!(serde_json::from_value::<X11BindIntent>(encoded).is_err());
    }
}
