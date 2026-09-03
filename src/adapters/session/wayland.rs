//! Runtime validation for a Wayland bind that was declared before container
//! startup. This module never creates or removes mounts.

use super::wayland_probe::{WaylandProbeObservation, WaylandTargetAccess};
use crate::adapters::config::NspawnConfigStore;
use crate::adapters::runtime::machine1::{
    Machine1Environment, Machine1OpenRequest, Machine1WaylandProbeRequest,
};
use crate::application::sessions::{
    SessionError, TerminalSessionHandle, TypedSessionEnvironment, ValidatedGuestUserName,
    WaylandPreparationRequest, WaylandSessionContext,
};
use crate::domain::machine::MachineName;
use crate::domain::session::{SessionId, SessionSize};

#[derive(Clone)]
pub(crate) struct WaylandSessionResolver {
    machine: super::MachineSessionTransport,
    nspawn: NspawnConfigStore,
    authorized_uid: u32,
}

impl WaylandSessionResolver {
    pub(crate) fn new(machine: super::MachineSessionTransport, nspawn: NspawnConfigStore) -> Self {
        Self::for_authorized_uid(
            machine,
            nspawn,
            crate::adapters::platform::capabilities::invoking_uid(),
        )
    }

    pub(crate) fn for_authorized_uid(
        machine: super::MachineSessionTransport,
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

        let identity_request = Machine1WaylandProbeRequest::identity(
            request.target.machine().clone(),
            request.target.user().clone(),
        );
        let identity = run_probe(
            &self.machine,
            identity_request,
            request.identity_probe_id,
            "identity",
        )
        .await?;
        if identity.target != WaylandTargetAccess::Unchecked {
            return Err(SessionError::new(
                "Wayland identity probe returned unexpected target evidence",
            ));
        }

        let guest_socket = crate::adapters::wayland::container_socket_path(
            identity.identity.uid(),
            request.host_socket.display(),
        );
        let config = self
            .nspawn
            .inspect(request.target.machine().as_str())
            .await
            .map_err(|error| {
                SessionError::new(format!("inspect startup Wayland projection: {error}"))
            })?
            .ok_or_else(projection_not_configured)?;
        if !config
            .has_wayland_projection(&source, &guest_socket)
            .map_err(|error| {
                SessionError::new(format!("inspect startup Wayland projection: {error}"))
            })?
        {
            return Err(projection_not_configured());
        }

        let access_request = Machine1WaylandProbeRequest::target(
            request.target.machine().clone(),
            request.target.user().clone(),
            &guest_socket,
        )
        .map_err(|error| SessionError::new(format!("validate Wayland probe target: {error}")))?;
        let access = run_probe(
            &self.machine,
            access_request,
            request.access_probe_id,
            "access",
        )
        .await?;
        if access.identity != identity.identity {
            return Err(SessionError::new(
                "guest identity changed while validating the Wayland projection",
            ));
        }
        match access.target {
            WaylandTargetAccess::Accessible => {}
            WaylandTargetAccess::Missing => {
                return Err(SessionError::with_hint(
                    format!(
                        "startup Wayland projection is missing at {}",
                        guest_socket.display()
                    ),
                    "The bind is applied only when the container starts; restart the machine after changing its .nspawn configuration.",
                ));
            }
            WaylandTargetAccess::Denied => {
                return Err(SessionError::new(format!(
                    "guest user {} cannot access the projected Wayland socket {}",
                    request.target.user(),
                    guest_socket.display()
                )));
            }
            WaylandTargetAccess::NotSocket => {
                return Err(SessionError::new(format!(
                    "projected Wayland target is not a socket: {}",
                    guest_socket.display()
                )));
            }
            WaylandTargetAccess::Unchecked => {
                return Err(SessionError::new(
                    "Wayland target probe did not check the projected socket",
                ));
            }
        }

        // Close the host-side replacement race as far as this pathname-based
        // design allows before handing the context to the terminal opener.
        revalidate_host_socket(&request.host_socket, self.authorized_uid).await?;
        Ok(WaylandSessionContext::verified(
            request.host_socket,
            guest_socket,
            identity.identity,
        ))
    }

    pub(crate) async fn environment(
        &self,
        environment: &TypedSessionEnvironment,
    ) -> Result<Machine1Environment, SessionError> {
        let display = match environment.wayland_context() {
            Some(context) => {
                revalidate_host_socket(context.host_socket(), self.authorized_uid).await?;
                Some(context.guest_socket())
            }
            None => None,
        };
        Machine1Environment::shell(environment.terminal_environment().clone(), display).map_err(
            |error| SessionError::new(format!("build selected-user shell environment: {error}")),
        )
    }

    pub(crate) async fn open_selected_user_shell(
        &self,
        id: SessionId,
        machine: MachineName,
        user: ValidatedGuestUserName,
        environment: TypedSessionEnvironment,
        size: SessionSize,
    ) -> Result<TerminalSessionHandle, SessionError> {
        let machine1_environment = self.environment(&environment).await?;
        let request = crate::adapters::runtime::machine1::Machine1ShellRequest::new(
            machine,
            user,
            machine1_environment,
        );
        self.machine
            .open_local(Machine1OpenRequest::shell(request), id, size)
            .await
    }
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
    machine: &super::MachineSessionTransport,
    request: Machine1WaylandProbeRequest,
    id: SessionId,
    phase: &'static str,
) -> Result<WaylandProbeObservation, SessionError> {
    let size = SessionSize::new(80, 24).expect("fixed probe PTY size is valid");
    let mut handle: TerminalSessionHandle = machine
        .open_local(Machine1OpenRequest::wayland_probe(request), id, size)
        .await?;
    super::wayland_probe::collect_wayland_probe(&mut handle)
        .await
        .map_err(|error| SessionError::new(format!("Wayland {phase} probe failed: {error}")))
}

fn projection_not_configured() -> SessionError {
    SessionError::with_hint(
        "the selected display is not declared as a Wayland bind in the machine's startup configuration",
        "Configure Wayland access while the machine is stopped, then start or restart it; Lasper does not create runtime binds for shell sessions.",
    )
}
