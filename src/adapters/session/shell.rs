//! Common shell preparation after desktop-side X11 authorization.
//! Local and daemon delivery validate the same Wayland projection and submit
//! the same guest process request; only their PTY delivery differs.

use crate::adapters::session::projection::WaylandSessionResolver;
use crate::application::sessions::execution::{GuestEnvironment, GuestExecutionRequest};
use crate::application::sessions::{
    GuestCommand, GuestTarget, InteractiveShellEnvironment, SessionError, TerminalOpenError,
};
use crate::domain::session::SessionId;
use crate::domain::wayland::HostWaylandSocket;

pub(crate) struct SelectedUserShell {
    pub(crate) target: GuestTarget,
    pub(crate) terminal: InteractiveShellEnvironment,
    pub(crate) wayland: Option<HostWaylandSocket>,
    pub(crate) x11_display: Option<u16>,
    pub(crate) command: Option<GuestCommand>,
}

impl SelectedUserShell {
    /// Use the same preparation budget for local and daemon execution, leaving
    /// five seconds for the daemon's 150-second FD handoff deadline.
    pub(crate) fn preparation_deadline() -> tokio::time::Instant {
        tokio::time::Instant::now() + std::time::Duration::from_secs(145)
    }

    pub(crate) async fn prepare(
        self,
        wayland: &WaylandSessionResolver,
        probe_id: SessionId,
        deadline: tokio::time::Instant,
    ) -> Result<GuestExecutionRequest, TerminalOpenError> {
        let display = wayland
            .prepare_display(self.target.clone(), self.wayland, probe_id, deadline)
            .await
            .map_err(TerminalOpenError::WaylandPreparation)?;
        let environment =
            GuestEnvironment::shell(self.terminal, display.as_deref(), self.x11_display).map_err(
                |error| {
                    SessionError::new(format!("build selected-user shell environment: {error}"))
                },
            )?;
        Ok(match self.command {
            Some(command) => GuestExecutionRequest::command(self.target, environment, command),
            None => GuestExecutionRequest::login_shell(self.target, environment),
        })
    }
}
