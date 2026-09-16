use std::sync::Arc;

use crate::application::sessions::{
    SessionError, SessionService, ShellTarget, X11ProjectionContext,
};
use crate::domain::x11::HostX11Socket;

const SERVER_INTERPRETED_FAMILY: u8 = 5;
const LOCAL_USER_KIND: &[u8] = b"localuser";

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct X11EndpointCatalog {
    pub sockets: Vec<HostX11Socket>,
    pub preferred_display: Option<u16>,
    pub diagnostics: Vec<String>,
}

#[async_trait::async_trait]
pub(crate) trait X11EndpointDiscoveryPort: Send + Sync {
    async fn discover(&self) -> X11EndpointCatalog;
}

pub(crate) struct X11EndpointDiscoveryService {
    port: Arc<dyn X11EndpointDiscoveryPort>,
}

impl X11EndpointDiscoveryService {
    pub(crate) fn new(port: Arc<dyn X11EndpointDiscoveryPort>) -> Self {
        Self { port }
    }

    pub async fn discover(&self) -> X11EndpointCatalog {
        self.port.discover().await
    }
}

/// Access-control mode reported by one X server's `ListHosts` reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum X11AccessControlMode {
    Enabled,
    Disabled,
    Unknown(u8),
}

impl X11AccessControlMode {
    pub(crate) const fn from_wire(value: u8) -> Self {
        match value {
            0 => Self::Disabled,
            1 => Self::Enabled,
            value => Self::Unknown(value),
        }
    }
}

/// One exact X11 host ACL entry. Raw protocol bytes are retained so a future
/// mutation path can only remove the representation that Lasper actually saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11AclEntry {
    family: u8,
    address: Vec<u8>,
}

impl X11AclEntry {
    pub(crate) fn from_wire(family: u8, address: Vec<u8>) -> Self {
        Self { family, address }
    }

    pub fn server_interpreted(&self) -> Option<(&str, &str)> {
        if self.family != SERVER_INTERPRETED_FAMILY {
            return None;
        }
        let separator = self.address.iter().position(|byte| *byte == 0)?;
        let kind = std::str::from_utf8(&self.address[..separator]).ok()?;
        let value = std::str::from_utf8(&self.address[separator + 1..]).ok()?;
        Some((kind, value))
    }

    fn is_numeric_local_user(&self, uid: u32) -> bool {
        let Some((kind, value)) = self.server_interpreted() else {
            return false;
        };
        kind.as_bytes() == LOCAL_USER_KIND && value == format!("#{uid}")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11AclSnapshot {
    mode: X11AccessControlMode,
    entries: Vec<X11AclEntry>,
}

impl X11AclSnapshot {
    pub(crate) fn from_wire(mode: u8, entries: Vec<X11AclEntry>) -> Self {
        Self {
            mode: X11AccessControlMode::from_wire(mode),
            entries,
        }
    }

    pub const fn mode(&self) -> X11AccessControlMode {
        self.mode
    }

    pub fn entries(&self) -> &[X11AclEntry] {
        &self.entries
    }

    fn has_numeric_local_user(&self, uid: u32) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.is_numeric_local_user(uid))
    }
}

/// What the queried ACL proves about the mapped host UID. This deliberately
/// does not infer access from cookies or similarly named ACL entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum X11MappedUidAclStatus {
    AccessControlDisabled,
    ExactNumericEntryPresent,
    ExactNumericEntryAbsent,
    UnknownMode { exact_numeric_entry_present: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11AccessCheck {
    projection: X11ProjectionContext,
    acl: X11AclSnapshot,
    mapped_uid_status: X11MappedUidAclStatus,
}

impl X11AccessCheck {
    pub(crate) fn from_observations(projection: X11ProjectionContext, acl: X11AclSnapshot) -> Self {
        let exact_numeric_entry_present =
            acl.has_numeric_local_user(projection.identity().host_uid());
        let mapped_uid_status = match acl.mode() {
            X11AccessControlMode::Disabled => X11MappedUidAclStatus::AccessControlDisabled,
            X11AccessControlMode::Enabled if exact_numeric_entry_present => {
                X11MappedUidAclStatus::ExactNumericEntryPresent
            }
            X11AccessControlMode::Enabled => X11MappedUidAclStatus::ExactNumericEntryAbsent,
            X11AccessControlMode::Unknown(_) => X11MappedUidAclStatus::UnknownMode {
                exact_numeric_entry_present,
            },
        };
        Self {
            projection,
            acl,
            mapped_uid_status,
        }
    }

    pub fn projection(&self) -> &X11ProjectionContext {
        &self.projection
    }

    pub fn acl(&self) -> &X11AclSnapshot {
        &self.acl
    }

    pub const fn mapped_uid_status(&self) -> X11MappedUidAclStatus {
        self.mapped_uid_status
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct X11DesktopAccessError {
    message: String,
}

impl X11DesktopAccessError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum X11AccessError {
    #[error("{0}")]
    Projection(#[source] SessionError),
    #[error("X server ACL query failed: {0}")]
    Desktop(#[source] X11DesktopAccessError),
}

#[async_trait::async_trait]
pub(crate) trait X11DesktopAccessPort: Send + Sync {
    async fn snapshot(
        &self,
        socket: &HostX11Socket,
    ) -> Result<X11AclSnapshot, X11DesktopAccessError>;
}

/// Combines runtime namespace evidence with a caller-owned X server query.
/// The desktop query always stays in the invoking user's process, even when
/// the projection half is routed through the elevated daemon.
pub struct X11AccessService {
    sessions: Arc<SessionService>,
    desktop: Arc<dyn X11DesktopAccessPort>,
}

impl X11AccessService {
    pub(crate) fn new(
        sessions: Arc<SessionService>,
        desktop: Arc<dyn X11DesktopAccessPort>,
    ) -> Self {
        Self { sessions, desktop }
    }

    pub async fn check(
        &self,
        target: ShellTarget,
        socket: HostX11Socket,
    ) -> Result<X11AccessCheck, X11AccessError> {
        let (projection, acl) = tokio::join!(
            self.sessions.test_x11_projection(target, socket.clone()),
            self.desktop.snapshot(&socket),
        );
        let projection = projection.map_err(X11AccessError::Projection)?;
        let acl = acl.map_err(X11AccessError::Desktop)?;
        Ok(X11AccessCheck::from_observations(projection, acl))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::sessions::{
        MappedGuestIdentity, ObservedGuestIdentity, ObservedMachineInstance,
        ObservedNamespaceIdentity,
    };
    use crate::domain::x11::X11SocketRevision;

    fn projection(host_uid: u32) -> X11ProjectionContext {
        let namespace = ObservedNamespaceIdentity::new(1, 2);
        X11ProjectionContext::verified(
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
            .unwrap(),
            "/mnt/host-x11/X0".into(),
            "/tmp/.X11-unix/X0".into(),
            MappedGuestIdentity::verified(
                ObservedGuestIdentity::new(1000, 1000),
                host_uid,
                host_uid,
                ObservedMachineInstance::new(42, namespace, namespace),
            ),
        )
    }

    #[test]
    fn acl_keeps_raw_entries_and_only_matches_exact_numeric_local_user() {
        let uid = 1_437_402_088;
        let exact = X11AclEntry::from_wire(5, format!("localuser\0#{uid}").into_bytes());
        let named = X11AclEntry::from_wire(5, b"localuser\0Lumie".to_vec());
        let unknown = X11AclEntry::from_wire(250, vec![0, 255, 128]);
        let snapshot = X11AclSnapshot::from_wire(1, vec![named, unknown.clone(), exact]);
        let check = X11AccessCheck::from_observations(projection(uid), snapshot);

        assert_eq!(
            check.mapped_uid_status(),
            X11MappedUidAclStatus::ExactNumericEntryPresent
        );
        assert_eq!(check.acl().entries()[1], unknown);
    }

    #[test]
    fn disabled_and_unknown_modes_are_not_reported_as_managed_access() {
        let disabled = X11AccessCheck::from_observations(
            projection(1000),
            X11AclSnapshot::from_wire(0, vec![]),
        );
        assert_eq!(
            disabled.mapped_uid_status(),
            X11MappedUidAclStatus::AccessControlDisabled
        );

        let unknown = X11AccessCheck::from_observations(
            projection(1000),
            X11AclSnapshot::from_wire(
                7,
                vec![X11AclEntry::from_wire(5, b"localuser\0#1000".to_vec())],
            ),
        );
        assert_eq!(
            unknown.mapped_uid_status(),
            X11MappedUidAclStatus::UnknownMode {
                exact_numeric_entry_present: true,
            }
        );
    }
}
