//! Compose native systemd calls into locally owned or transferable sessions.

use super::{PreparedNativeCommand, PreparedNativeExecution};
use crate::adapters::error::NspawnError;
use crate::adapters::runtime::{
    dbus::{DbusBackend, GuestSessionOpenError},
    systemd_tools,
};
use crate::adapters::session::io::MachinePty;
use crate::application::sessions::execution::{
    GuestExecutionError, GuestExecutionFailure, GuestExecutionObservation, GuestExecutionPort,
    GuestExecutionRequest,
};
use crate::application::sessions::{SessionError, TerminalSessionHandle};
use crate::domain::machine::MachineName;
use crate::domain::session::{SessionId, SessionSize, TerminalAttachmentKind};

#[derive(Clone)]
pub(crate) enum NativeExecution {
    Dbus(DbusBackend),
    SystemdTools,
}

/// A machined PTY or a host client command awaiting local/daemon PTY delivery.
pub(crate) enum NativeSession {
    Pty(MachinePty),
    Command(PreparedNativeCommand),
}

impl NativeSession {
    pub(crate) fn into_local(
        self,
        id: SessionId,
        size: SessionSize,
    ) -> Result<TerminalSessionHandle, GuestExecutionError> {
        match self {
            Self::Pty(pty) => crate::adapters::session::io::spawn_machine_terminal(pty, id, size)
                .map_err(|error| {
                    GuestExecutionError::new(GuestExecutionFailure::OutcomeUnknown, error)
                }),
            Self::Command(command) => {
                // Resolve the prepared command after PTY setup, immediately
                // before spawn, just as daemon delivery does.
                crate::adapters::session::io::spawn_direct_terminal(
                    || command.into_command(),
                    id,
                    TerminalAttachmentKind::Login,
                    size,
                )
            }
        }
    }
}

#[async_trait::async_trait]
impl GuestExecutionPort for NativeExecution {
    type Prepared = PreparedNativeExecution;

    async fn observe(
        &self,
        machine: &MachineName,
        deadline: tokio::time::Instant,
    ) -> Result<GuestExecutionObservation, SessionError> {
        self.observe_native(machine, deadline).await
    }

    /// Preparation observes the guest manager without submitting a process.
    /// The returned owner is consumed when the request is actually opened.
    async fn prepare(
        &self,
        request: GuestExecutionRequest,
        deadline: tokio::time::Instant,
    ) -> Result<PreparedNativeExecution, GuestExecutionError> {
        let instance = super::readiness::require_native(
            self,
            request.target().machine(),
            deadline,
            uzers::get_effective_uid() == 0,
        )
        .await?;
        Ok(PreparedNativeExecution::new(
            self.clone(),
            request,
            instance,
            deadline,
        ))
    }
}

impl NativeExecution {
    /// A getty login is not selected-user execution. Subscribe before opening
    /// its PTY so an intervening machine removal cannot strand the forwarder.
    pub(crate) async fn login(&self, machine: &MachineName) -> Result<NativeSession, SessionError> {
        match self {
            Self::Dbus(dbus) => {
                let removed = dbus
                    .watch_login_machine(machine)
                    .await
                    .map_err(|error| map_native_error("observe machine login lifetime", error))?;
                let master = dbus
                    .open_machine_login(machine)
                    .await
                    .map_err(|error| map_native_error("open machine login prompt", error))?;
                Ok(NativeSession::Pty(MachinePty {
                    master,
                    machine_removed: Some(removed),
                }))
            }
            Self::SystemdTools => Ok(NativeSession::Command(PreparedNativeCommand::new(
                machine.clone(),
                None,
                None,
                systemd_tools::session::machine_login_command(machine),
            ))),
        }
    }

    pub(crate) async fn machine_leader(&self, machine: &MachineName) -> Result<u32, SessionError> {
        match self {
            Self::Dbus(dbus) => dbus
                .machine_leader(machine)
                .await
                .map_err(|error| map_native_error("inspect machine instance", error)),
            Self::SystemdTools => {
                systemd_tools::query::machine_leader(machine)
                    .await
                    .map_err(|error| {
                        SessionError::new(format!(
                            "inspect machine instance through systemd runtime state: {error}",
                        ))
                    })
            }
        }
    }

    #[cfg(test)]
    pub(crate) const fn uses_dbus(&self) -> bool {
        matches!(self, Self::Dbus(_))
    }
}

pub(super) fn map_native_error(context: &str, error: NspawnError) -> SessionError {
    let message = format!("{context} through D-Bus: {error}");
    if error.is_polkit_rejection() {
        SessionError::with_hint(
            message,
            "Authorize the machine1 shell request through the desktop authentication agent.",
        )
    } else {
        SessionError::new(message)
    }
}

pub(super) fn map_execution_error(
    context: &str,
    error: GuestSessionOpenError,
) -> GuestExecutionError {
    let (failure, error) = match error {
        GuestSessionOpenError::NotAttempted(error) => (GuestExecutionFailure::NotAttempted, error),
        GuestSessionOpenError::Rejected(error) => (GuestExecutionFailure::Rejected, error),
        GuestSessionOpenError::OutcomeUnknown(error) => {
            (GuestExecutionFailure::OutcomeUnknown, error)
        }
    };
    GuestExecutionError::new(failure, map_native_error(context, error))
}
