//! Runtime validation for a Wayland bind that was declared before container
//! startup. This module never creates or removes mounts.

use super::probe::SocketAccess;
use super::wayland_probe::{WaylandProbeObservation, WaylandProbeRequest};
use super::{MachineSessionRequest, MachineSessionTransport};
use crate::adapters::config::NspawnConfigStore;
use crate::application::sessions::{
    SessionError, ShellTarget, TerminalSessionHandle, WaylandPreparationRequest,
    WaylandSessionContext,
};
use crate::domain::machine::MachineName;
use crate::domain::session::{SessionId, SessionSize};

#[derive(Clone)]
pub(crate) struct WaylandSessionResolver {
    machine: MachineSessionTransport,
    nspawn: NspawnConfigStore,
    authorized_uid: u32,
}

impl WaylandSessionResolver {
    pub(crate) fn new(machine: MachineSessionTransport, nspawn: NspawnConfigStore) -> Self {
        Self::for_authorized_uid(machine, nspawn, crate::adapters::platform::invoking_uid())
    }

    pub(crate) fn for_authorized_uid(
        machine: MachineSessionTransport,
        nspawn: NspawnConfigStore,
        authorized_uid: u32,
    ) -> Self {
        Self {
            machine,
            nspawn,
            authorized_uid,
        }
    }

    pub(crate) async fn prepare(
        &self,
        request: WaylandPreparationRequest,
    ) -> Result<WaylandSessionContext, SessionError> {
        let source = revalidate_host_socket(&request.host_socket, self.authorized_uid).await?;

        let config = self
            .nspawn
            .inspect(request.target.machine().as_str())
            .await
            .map_err(|error| {
                SessionError::new(format!("inspect startup Wayland projection: {error}"))
            })?
            .ok_or_else(projection_not_configured)?;
        let targets = config.wayland_targets(&source).await.map_err(|error| {
            SessionError::new(format!("inspect startup Wayland projection: {error}"))
        })?;
        let mut failure = projection_not_configured();
        for guest_socket in targets {
            let probe = WaylandProbeRequest::target(
                request.target.machine().clone(),
                request.target.user().clone(),
                &guest_socket,
            )
            .map_err(|error| {
                SessionError::new(format!("validate Wayland probe target: {error}"))
            })?;
            let access = run_probe(&self.machine, probe, request.probe_id).await?;
            match validate_target(&access, &request.host_socket, &guest_socket) {
                Ok(()) => {
                    revalidate_host_socket(&request.host_socket, self.authorized_uid).await?;
                    return Ok(WaylandSessionContext::verified(
                        guest_socket,
                        access.identity,
                    ));
                }
                Err(error) => failure = error,
            }
        }
        Err(failure)
    }

    pub(crate) async fn automatic_wayland(
        &self,
        machine: &MachineName,
    ) -> Result<Option<crate::domain::wayland::HostWaylandSocket>, SessionError> {
        automatic_wayland(&self.nspawn, machine).await
    }

    /// Resolve and probe immediately before terminal creation. A shell carries
    /// the selected host socket, never an earlier Check Access observation.
    pub(crate) async fn prepare_display(
        &self,
        target: ShellTarget,
        socket: Option<crate::domain::wayland::HostWaylandSocket>,
        probe_id: SessionId,
    ) -> Result<Option<std::path::PathBuf>, SessionError> {
        match socket {
            Some(host_socket) => Ok(Some(
                self.prepare(WaylandPreparationRequest {
                    probe_id,
                    target,
                    host_socket,
                })
                .await?
                .guest_socket()
                .to_path_buf(),
            )),
            None => Ok(None),
        }
    }
}

pub(super) async fn automatic_wayland(
    nspawn: &NspawnConfigStore,
    machine: &MachineName,
) -> Result<Option<crate::domain::wayland::HostWaylandSocket>, SessionError> {
    let Some(socket) = crate::adapters::platform::wayland::current_wayland_socket()
        .await
        .map_err(|error| SessionError::new(format!("resolve current Wayland display: {error}")))?
    else {
        return Ok(None);
    };
    let Some(config) = nspawn.inspect(machine.as_str()).await.map_err(|error| {
        SessionError::new(format!("inspect startup Wayland projection: {error}"))
    })?
    else {
        return Ok(None);
    };
    let targets = config
        .wayland_targets(socket.canonical_path())
        .await
        .map_err(|error| {
            SessionError::new(format!("inspect startup Wayland projection: {error}"))
        })?;
    Ok((!targets.is_empty()).then_some(socket))
}

fn validate_target(
    access: &WaylandProbeObservation,
    host: &crate::domain::wayland::HostWaylandSocket,
    target: &std::path::Path,
) -> Result<(), SessionError> {
    let detail = match access.target.access {
        SocketAccess::Accessible | SocketAccess::Denied => {
            let revision = host.revision();
            if access.target.identity != Some((revision.device, revision.inode)) {
                "is not the current host socket (the bind may be stale)"
            } else if access.target.access == SocketAccess::Accessible {
                return Ok(());
            } else {
                "is not accessible to the guest user"
            }
        }
        SocketAccess::Unavailable => {
            "could not be inspected by the guest user (the path may be missing or inaccessible)"
        }
        SocketAccess::NotSocket => "is not a socket",
    };
    Err(SessionError::with_hint(
        format!("Wayland target {} {detail}", target.display()),
        "Check the configured bind and guest permissions; restart the machine if the host socket or startup configuration changed.",
    ))
}

async fn revalidate_host_socket(
    socket: &crate::domain::wayland::HostWaylandSocket,
    authorized_uid: u32,
) -> Result<std::path::PathBuf, SessionError> {
    crate::adapters::wayland::revalidate_host_socket(socket, authorized_uid)
        .await
        .map_err(|error| SessionError::new(format!("revalidate current Wayland socket: {error}")))
}

async fn run_probe(
    machine: &MachineSessionTransport,
    request: WaylandProbeRequest,
    id: SessionId,
) -> Result<WaylandProbeObservation, SessionError> {
    let size = SessionSize::new(80, 24).expect("fixed probe PTY size is valid");
    let mut handle: TerminalSessionHandle = machine
        .open_local(MachineSessionRequest::wayland_probe(request), id, size)
        .await?;
    super::wayland_probe::collect_wayland_probe(&mut handle)
        .await
        .map_err(|error| SessionError::new(format!("Wayland access probe failed: {error}")))
}

fn projection_not_configured() -> SessionError {
    SessionError::with_hint(
        "the selected display is not declared as a Wayland bind in the machine's startup configuration",
        "Configure Wayland access while the machine is stopped, then start or restart it; Lasper does not create runtime binds for shell sessions.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::session::probe::SocketObservation;
    use crate::application::sessions::ObservedGuestIdentity;
    use crate::domain::wayland::{HostWaylandSocket, SocketRevision, WaylandDisplay};
    use std::path::{Path, PathBuf};

    #[test]
    fn custom_target_requires_matching_socket_identity_and_guest_access() {
        let host = HostWaylandSocket::from_verified_parts(
            WaylandDisplay::new("wayland-1").unwrap(),
            PathBuf::from("/run/user/1000"),
            PathBuf::from("/run/user/1000/wayland-1"),
            1000,
            1000,
            1000,
            0o755,
            SocketRevision {
                device: 69,
                inode: 100,
                ctime_seconds: 0,
                ctime_nanoseconds: 0,
            },
        )
        .unwrap();
        let mut access = WaylandProbeObservation {
            identity: ObservedGuestIdentity::new(1234, 1234),
            target: SocketObservation {
                access: SocketAccess::Accessible,
                identity: Some((69, 100)),
            },
        };
        let path = Path::new("/custom/desktop.sock");
        assert!(validate_target(&access, &host, path).is_ok());
        for identity in [None, Some((69, 101)), Some((70, 100))] {
            access.target.identity = identity;
            assert!(validate_target(&access, &host, path).is_err());
        }
        access.target.identity = Some((69, 100));
        for state in [
            SocketAccess::Unavailable,
            SocketAccess::Denied,
            SocketAccess::NotSocket,
        ] {
            access.target.access = state;
            assert!(validate_target(&access, &host, path).is_err());
        }

        access.target.access = SocketAccess::Denied;
        let denied = validate_target(&access, &host, path).unwrap_err();
        assert!(denied
            .to_string()
            .contains("not accessible to the guest user"));
        access.target.identity = Some((69, 101));
        let wrong_socket = validate_target(&access, &host, path).unwrap_err();
        assert!(wrong_socket
            .to_string()
            .contains("not the current host socket"));
    }
}
