use crate::application::sessions::SessionError;

use super::protocol::ProbeFrame;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SocketAccess {
    /// The guest's shell write-access test succeeded for an observed pathname
    /// socket. This is advisory VFS evidence, not a successful connection or
    /// X server authorization; abstract Unix sockets do not use this check.
    Accessible,
    Denied,
    NotSocket,
    /// stat could not observe the path. No errno or existence is inferred
    /// from shell test predicates, which also fail on unsearchable parents.
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SocketObservation {
    pub(crate) access: SocketAccess,
    pub(crate) identity: Option<(u64, u64)>,
}

impl SocketObservation {
    pub(crate) fn parse(
        frame: &mut ProbeFrame<'_>,
        access_key: &[u8],
        device_key: &[u8],
        inode_key: &[u8],
    ) -> Result<Self, SessionError> {
        let access = match frame.required(access_key)? {
            b"ACCESSIBLE" => SocketAccess::Accessible,
            b"DENIED" => SocketAccess::Denied,
            b"NOT_SOCKET" => SocketAccess::NotSocket,
            b"UNAVAILABLE" => SocketAccess::Unavailable,
            _ => return Err(frame.error("returned an invalid socket access state")),
        };
        let identity = match (
            access,
            frame.optional_u64(device_key)?,
            frame.optional_u64(inode_key)?,
        ) {
            (SocketAccess::Accessible | SocketAccess::Denied, Some(device), Some(inode)) => {
                Some((device, inode))
            }
            (SocketAccess::Accessible | SocketAccess::Denied, _, _) => {
                return Err(frame.error("socket observation is missing DEVICE or INODE"))
            }
            (_, None, None) => None,
            _ => return Err(frame.error("returned socket identity without an observed socket")),
        };
        Ok(Self { access, identity })
    }
}
