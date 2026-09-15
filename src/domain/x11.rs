use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    ) -> Option<Self> {
        let expected = format!("X{display}{}", if alternate { "_" } else { "" });
        if source.parent() != Some(Path::new("/tmp/.X11-unix"))
            || source.file_name().and_then(|name| name.to_str()) != Some(expected.as_str())
            || !canonical_path.is_absolute()
        {
            return None;
        }
        Some(Self {
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

    pub fn peer_identity(&self) -> (u32, u32, u32) {
        (self.peer_pid, self.peer_uid, self.peer_gid)
    }

    pub fn revision(&self) -> X11SocketRevision {
        self.revision
    }
}
