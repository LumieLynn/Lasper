//! Runtime validation for a Wayland bind that was declared before container
//! startup. This module never creates or removes mounts.

use super::wayland_probe::WaylandProbeRequest;
use super::wayland_probe::{WaylandProbeObservation, WaylandTargetAccess};
use super::{
    MachineSessionRequest, MachineSessionTransport, MachineShellEnvironment, MachineShellRequest,
};
use crate::adapters::config::NspawnConfigStore;
use crate::application::sessions::{
    GuestCommand, SessionError, TerminalSessionHandle, TypedSessionEnvironment,
    ValidatedGuestUserName, WaylandPreparationRequest, WaylandSessionContext,
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
        Self::for_authorized_uid(
            machine,
            nspawn,
            crate::adapters::platform::capabilities::invoking_uid(),
        )
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
                        request.host_socket,
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

    pub(crate) async fn environment(
        &self,
        environment: &TypedSessionEnvironment,
    ) -> Result<MachineShellEnvironment, SessionError> {
        let display = match environment.wayland_context() {
            Some(context) => {
                revalidate_host_socket(context.host_socket(), self.authorized_uid).await?;
                Some(context.guest_socket())
            }
            None => None,
        };
        MachineShellEnvironment::shell(environment.terminal_environment().clone(), display).map_err(
            |error| SessionError::new(format!("build selected-user shell environment: {error}")),
        )
    }

    pub(crate) async fn open_selected_user_shell(
        &self,
        id: SessionId,
        machine: MachineName,
        user: ValidatedGuestUserName,
        environment: TypedSessionEnvironment,
        command: Option<GuestCommand>,
        size: SessionSize,
    ) -> Result<TerminalSessionHandle, SessionError> {
        let shell_environment = self.environment(&environment).await?;
        let request = MachineShellRequest::new(machine, user, shell_environment);
        let request = match command {
            Some(command) => request.with_command(command),
            None => request,
        };
        self.machine
            .open_local(MachineSessionRequest::shell(request), id, size)
            .await
    }

    pub(crate) async fn open_login_prompt(
        &self,
        id: SessionId,
        machine: MachineName,
        size: SessionSize,
    ) -> Result<TerminalSessionHandle, SessionError> {
        self.machine
            .open_local(MachineSessionRequest::login_prompt(machine), id, size)
            .await
    }
}

pub(super) async fn automatic_wayland(
    nspawn: &NspawnConfigStore,
    machine: &MachineName,
) -> Result<Option<crate::domain::wayland::HostWaylandSocket>, SessionError> {
    let Some(socket) = crate::adapters::platform::capabilities::current_wayland_socket()
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
    let detail = match access.target {
        WaylandTargetAccess::Accessible => {
            let revision = host.revision();
            if access.socket_identity == Some((revision.device, revision.inode)) {
                return Ok(());
            }
            "is not the current host socket (the bind may be stale)"
        }
        WaylandTargetAccess::Missing => "is missing",
        WaylandTargetAccess::Denied => "is not accessible to the guest user",
        WaylandTargetAccess::NotSocket => "is not a socket",
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
            target: WaylandTargetAccess::Accessible,
            socket_identity: Some((69, 100)),
        };
        let path = Path::new("/custom/desktop.sock");
        assert!(validate_target(&access, &host, path).is_ok());
        for identity in [None, Some((69, 101)), Some((70, 100))] {
            access.socket_identity = identity;
            assert!(validate_target(&access, &host, path).is_err());
        }
        access.socket_identity = Some((69, 100));
        for state in [
            WaylandTargetAccess::Missing,
            WaylandTargetAccess::Denied,
            WaylandTargetAccess::NotSocket,
        ] {
            access.target = state;
            assert!(validate_target(&access, &host, path).is_err());
        }
    }
}
