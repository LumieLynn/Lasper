//! Compose native systemd calls into locally owned or transferable sessions.

use crate::adapters::error::NspawnError;
use crate::adapters::runtime::{dbus::DbusBackend, systemd_tools};
use crate::adapters::session::io::MachinePty;
use crate::application::sessions::execution::GuestExecutionRequest;
use crate::application::sessions::{SessionError, TerminalSessionHandle};
use crate::domain::machine::MachineName;
use crate::domain::session::{SessionId, SessionSize, TerminalAttachmentKind};
use portable_pty::CommandBuilder;

#[derive(Clone)]
pub(crate) enum NativeExecution {
    Dbus(DbusBackend),
    SystemdTools,
}

/// A machined PTY or a host client command awaiting local/daemon PTY delivery.
pub(crate) enum NativeSession {
    Pty(MachinePty),
    Command(CommandBuilder),
}

impl NativeSession {
    pub(crate) fn into_local(
        self,
        id: SessionId,
        size: SessionSize,
    ) -> Result<TerminalSessionHandle, SessionError> {
        match self {
            Self::Pty(pty) => crate::adapters::session::io::spawn_machine_terminal(pty, id, size),
            Self::Command(command) => crate::adapters::session::io::spawn_direct_terminal(
                command,
                id,
                TerminalAttachmentKind::Login,
                size,
            ),
        }
    }
}

impl NativeExecution {
    pub(crate) async fn open(
        &self,
        request: GuestExecutionRequest,
    ) -> Result<NativeSession, SessionError> {
        self.require_native(request.target().machine()).await?;
        match self {
            Self::Dbus(dbus) => {
                let master = dbus
                    .open_guest_session(&request)
                    .await
                    .map_err(|error| map_native_error("execute selected guest process", error))?;
                Ok(NativeSession::Pty(MachinePty {
                    master,
                    machine_removed: None,
                }))
            }
            Self::SystemdTools => Ok(NativeSession::Command(
                systemd_tools::session::guest_session_command(&request),
            )),
        }
    }

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
            Self::SystemdTools => Ok(NativeSession::Command(
                systemd_tools::session::machine_login_command(machine),
            )),
        }
    }

    pub(crate) async fn open_local(
        &self,
        request: GuestExecutionRequest,
        id: SessionId,
        size: SessionSize,
    ) -> Result<TerminalSessionHandle, SessionError> {
        self.open(request).await?.into_local(id, size)
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

fn map_native_error(context: &str, error: NspawnError) -> SessionError {
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
