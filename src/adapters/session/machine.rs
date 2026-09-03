//! Closed transport selection for machine1-compatible session operations.

use crate::adapters::runtime::machine1::{Machine1OpenRequest, Machine1Pty};
use crate::application::sessions::{SessionError, TerminalSessionHandle};
use crate::domain::session::{SessionId, SessionSize, TerminalAttachmentKind};

#[derive(Clone)]
pub(crate) enum MachineSessionTransport {
    Dbus(crate::adapters::runtime::dbus::DbusBackend),
    Cli,
}

impl MachineSessionTransport {
    pub(crate) async fn open_local(
        &self,
        request: Machine1OpenRequest,
        id: SessionId,
        size: SessionSize,
    ) -> Result<TerminalSessionHandle, SessionError> {
        match self {
            Self::Dbus(dbus) => {
                let context = request.context();
                let pty = dbus
                    .open_machine_session(request)
                    .await
                    .map_err(|error| map_machine1_error(context, error))?;
                super::pty::spawn_machine1_terminal(pty.master, id, size)
            }
            Self::Cli => {
                let context = request.context();
                let command = super::terminal_attach::machine1(request).map_err(|error| {
                    SessionError::new(format!("build {context} machinectl command: {error}"))
                })?;
                super::pty::spawn_direct_terminal(
                    command.into_pty_command().map_err(|error| {
                        SessionError::new(format!("validate {context} machinectl command: {error}"))
                    })?,
                    id,
                    TerminalAttachmentKind::Login,
                    size,
                )
            }
        }
    }

    pub(crate) async fn open_dbus(
        &self,
        request: Machine1OpenRequest,
    ) -> Result<Option<Machine1Pty>, SessionError> {
        let Self::Dbus(dbus) = self else {
            return Ok(None);
        };
        let context = request.context();
        dbus.open_machine_session(request)
            .await
            .map(Some)
            .map_err(|error| map_machine1_error(context, error))
    }

    pub(crate) const fn uses_dbus(&self) -> bool {
        matches!(self, Self::Dbus(_))
    }
}

fn map_machine1_error(
    context: &'static str,
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
