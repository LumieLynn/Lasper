//! Runtime validation for a Wayland bind that was declared before container
//! startup. This module never creates or removes mounts.

use super::wayland_probe::{WaylandProbeObservation, WaylandTargetAccess};
use crate::adapters::config::NspawnConfigStore;
use crate::adapters::runtime::dbus::DbusBackend;
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
    machine1: Option<DbusBackend>,
    nspawn: NspawnConfigStore,
}

impl WaylandSessionResolver {
    pub(crate) fn new(machine1: Option<DbusBackend>, nspawn: NspawnConfigStore) -> Self {
        Self { machine1, nspawn }
    }

    pub(crate) fn is_available(&self) -> bool {
        self.machine1.is_some()
    }

    pub(crate) async fn prepare(
        &self,
        request: WaylandPreparationRequest,
    ) -> Result<WaylandSessionContext, SessionError> {
        let machine1 = self.machine1.as_ref().ok_or_else(|| {
            SessionError::new("Wayland shell validation requires the machine1 D-Bus transport")
        })?;
        let source = revalidate_host_socket(&request.host_socket).await?;

        let identity_request = Machine1WaylandProbeRequest::identity(
            request.target.machine().clone(),
            request.target.user().clone(),
        );
        let identity = run_probe(
            machine1,
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
        let access = run_probe(machine1, access_request, request.access_probe_id, "access").await?;
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
        revalidate_host_socket(&request.host_socket).await?;
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
        let Some(context) = environment.wayland_context() else {
            return Ok(Machine1Environment::empty());
        };
        revalidate_host_socket(context.host_socket()).await?;
        Machine1Environment::wayland(context.guest_socket()).map_err(|error| {
            SessionError::new(format!("build Wayland session environment: {error}"))
        })
    }

    /// Open a selected-user shell through machine1 when that transport is
    /// available. `None` lets CLI-only callers use their existing fallback,
    /// but a verified Wayland context is never downgraded to that fallback.
    pub(crate) async fn open_selected_user_shell(
        &self,
        id: SessionId,
        machine: MachineName,
        user: ValidatedGuestUserName,
        environment: TypedSessionEnvironment,
        size: SessionSize,
    ) -> Result<Option<TerminalSessionHandle>, SessionError> {
        let Some(machine1) = self.machine1.as_ref() else {
            return if environment.wayland_context().is_some() {
                Err(SessionError::new(
                    "Wayland shells require the machine1 D-Bus transport",
                ))
            } else {
                Ok(None)
            };
        };
        let machine1_environment = self.environment(&environment).await?;
        let request = crate::adapters::runtime::machine1::Machine1ShellRequest::new(
            machine,
            user,
            machine1_environment,
        );
        let pty = machine1
            .open_machine_session(Machine1OpenRequest::shell(request))
            .await
            .map_err(map_machine1_error)?;
        super::pty::spawn_machine1_terminal(pty.master, id, size).map(Some)
    }
}

async fn revalidate_host_socket(
    socket: &crate::domain::wayland::HostWaylandSocket,
) -> Result<std::path::PathBuf, SessionError> {
    crate::adapters::wayland::revalidate_host_socket(socket, uzers::get_current_uid())
        .await
        .map_err(|error| SessionError::new(format!("revalidate current Wayland socket: {error}")))
}

async fn run_probe(
    machine1: &DbusBackend,
    request: Machine1WaylandProbeRequest,
    id: SessionId,
    phase: &'static str,
) -> Result<WaylandProbeObservation, SessionError> {
    let pty = machine1
        .open_machine_session(Machine1OpenRequest::wayland_probe(request))
        .await
        .map_err(|error| map_machine1_error_with_context("open Wayland projection probe", error))?;
    let size = SessionSize::new(80, 24).expect("fixed probe PTY size is valid");
    let mut handle: TerminalSessionHandle =
        super::pty::spawn_machine1_terminal(pty.master, id, size)?;
    super::wayland_probe::collect_wayland_probe(&mut handle)
        .await
        .map_err(|error| SessionError::new(format!("Wayland {phase} probe failed: {error}")))
}

fn map_machine1_error(error: crate::adapters::error::NspawnError) -> SessionError {
    map_machine1_error_with_context("open selected-user shell", error)
}

fn map_machine1_error_with_context(
    context: &str,
    error: crate::adapters::error::NspawnError,
) -> SessionError {
    let message = format!("{context} through machine1: {error}");
    if error.is_polkit_rejection() {
        SessionError::with_hint(
            message,
            "Authorize the machine1 shell request through the desktop authentication agent.",
        )
    } else {
        SessionError::new(message)
    }
}

fn projection_not_configured() -> SessionError {
    SessionError::with_hint(
        "the selected display is not declared as a Wayland bind in the machine's startup configuration",
        "Configure Wayland access while the machine is stopped, then start or restart it; Lasper does not create runtime binds for shell sessions.",
    )
}
