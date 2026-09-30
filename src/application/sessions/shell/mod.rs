//! Selected-user shell preparation and opening shared by CLI and embedded terminals.

use crate::application::x11::{
    X11AccessError, X11AccessService, X11SessionPreparation, X11SessionSelection,
};
use crate::domain::session::SessionSize;
use crate::domain::wayland::{HostWaylandSocket, WaylandDisplay};

use super::{
    GuestCommand, InteractiveShellEnvironment, SessionError, SessionService, ShellOpenError,
    ShellOpenIntent, ShellTarget, TerminalSessionHandle, TerminalSessionRequest,
    TypedSessionEnvironment, WaylandShellRequest,
};

#[derive(Clone, Debug)]
pub struct ShellLaunchRequest {
    intent: ShellOpenIntent,
    x11: Option<X11SessionSelection>,
    allow_wayland_fallback: bool,
}

impl ShellLaunchRequest {
    pub fn new(
        target: ShellTarget,
        wayland: WaylandShellRequest,
        terminal_environment: InteractiveShellEnvironment,
        size: SessionSize,
    ) -> Self {
        Self {
            intent: ShellOpenIntent::new(target, wayland, terminal_environment, size),
            x11: None,
            allow_wayland_fallback: false,
        }
    }

    pub fn with_command(mut self, command: GuestCommand) -> Self {
        self.intent = self.intent.with_command(command);
        self
    }

    pub fn with_x11(mut self, selection: X11SessionSelection) -> Self {
        self.x11 = Some(selection);
        self
    }

    pub fn with_wayland_fallback(mut self, allow: bool) -> Self {
        self.allow_wayland_fallback = allow;
        self
    }
}

pub struct ShellLaunchResult {
    pub handle: TerminalSessionHandle,
    pub x11: Option<X11SessionPreparation>,
    pub wayland_fallback: Option<WaylandFallbackCause>,
}

#[derive(Debug, thiserror::Error)]
pub enum ShellLaunchError {
    #[error("X11 session preparation failed: {0}")]
    X11(#[source] X11AccessError),
    #[error("{error}")]
    Open {
        #[source]
        error: ShellAttemptError,
        x11: Option<Box<X11SessionPreparation>>,
    },
}

impl SessionService {
    /// Prepare explicitly requested display access before opening the terminal.
    pub async fn launch_shell(
        &self,
        request: ShellLaunchRequest,
        x11_access: Option<&X11AccessService>,
    ) -> Result<ShellLaunchResult, ShellLaunchError> {
        let x11 = match request.x11 {
            Some(selection) => {
                let access = x11_access.ok_or_else(|| {
                    ShellLaunchError::X11(X11AccessError::Selection(
                        "X11 integration was requested without an access service".into(),
                    ))
                })?;
                Some(
                    access
                        .prepare_session(request.intent.target().clone(), selection)
                        .await
                        .map_err(ShellLaunchError::X11)?,
                )
            }
            None => None,
        };

        let mut intent = request.intent;
        if let Some(preparation) = &x11 {
            intent = intent.with_x11(preparation.context().clone());
        }

        let (handle, wayland_fallback) = match self
            .open_shell_with_fallback(intent, request.allow_wayland_fallback)
            .await
        {
            Ok(opened) => opened,
            Err(error) => {
                // X11 access follows the machine instance, not this terminal attempt.
                return Err(ShellLaunchError::Open {
                    error,
                    x11: x11.map(Box::new),
                });
            }
        };

        Ok(ShellLaunchResult {
            handle,
            x11,
            wayland_fallback,
        })
    }

    /// Open an intent without automatic X11 authorization or Wayland fallback.
    pub async fn open_shell(
        &self,
        intent: ShellOpenIntent,
    ) -> Result<TerminalSessionHandle, ShellOpenError> {
        let socket = self
            .resolve_shell_wayland(intent.target(), intent.wayland())
            .await
            .map_err(ShellOpenError::WaylandSelection)?;
        self.open_prepared_shell(&intent, socket).await
    }

    async fn open_prepared_shell(
        &self,
        intent: &ShellOpenIntent,
        socket: Option<HostWaylandSocket>,
    ) -> Result<TerminalSessionHandle, ShellOpenError> {
        if intent
            .x11()
            .is_some_and(|context| context.target() != intent.target())
        {
            return Err(ShellOpenError::X11Context(SessionError::new(
                "prepared X11 access belongs to a different shell target",
            )));
        }
        let terminal_environment = intent.terminal_environment().clone();
        let command = intent.command().cloned();
        let mut environment = match socket {
            None => TypedSessionEnvironment::terminal(terminal_environment),
            Some(socket) => TypedSessionEnvironment::wayland(
                terminal_environment,
                self.test_wayland(intent.target().clone(), socket)
                    .await
                    .map_err(ShellOpenError::WaylandPreparation)?,
            ),
        };
        if let Some(context) = intent.x11().cloned() {
            environment = environment.with_x11(context);
        }
        let target = intent.target();
        self.port
            .open_terminal(TerminalSessionRequest::selected_user_shell_with_command(
                self.allocate_id(),
                target.machine().clone(),
                target.user().clone(),
                environment,
                command,
                intent.size(),
            ))
            .await
            .map_err(ShellOpenError::Terminal)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ShellAttemptError {
    #[error("{0}")]
    Initial(#[source] ShellOpenError),
    #[error("{cause}; terminal-only fallback failed: {error}")]
    Fallback {
        cause: WaylandFallbackCause,
        #[source]
        error: ShellOpenError,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum WaylandFallbackCause {
    #[error("Wayland socket selection failed: {0}")]
    SocketSelection(#[source] SessionError),
    #[error("Wayland validation failed: {0}")]
    Validation(#[source] SessionError),
}

impl WaylandFallbackCause {
    pub fn context(&self) -> &'static str {
        match self {
            Self::SocketSelection(_) => "Wayland socket selection failed",
            Self::Validation(_) => "Wayland validation failed",
        }
    }

    pub fn error(&self) -> &SessionError {
        match self {
            Self::SocketSelection(error) | Self::Validation(error) => error,
        }
    }
}

impl SessionService {
    /// Retry only failures before terminal creation. X11 context, command,
    /// environment and size remain attached to the original intent.
    async fn open_shell_with_fallback(
        &self,
        intent: ShellOpenIntent,
        allow_fallback: bool,
    ) -> Result<(TerminalSessionHandle, Option<WaylandFallbackCause>), ShellAttemptError> {
        // Explicit display requests never silently select a different session.
        let allow_fallback =
            allow_fallback && matches!(intent.wayland(), WaylandShellRequest::Automatic);
        let (socket, mut cause) = match self
            .resolve_shell_wayland(intent.target(), intent.wayland())
            .await
        {
            Ok(socket) => (socket, None),
            Err(error) if allow_fallback => {
                (None, Some(WaylandFallbackCause::SocketSelection(error)))
            }
            Err(error) => {
                return Err(ShellAttemptError::Initial(
                    ShellOpenError::WaylandSelection(error),
                ))
            }
        };
        let has_socket = socket.is_some();
        let result = match self.open_prepared_shell(&intent, socket).await {
            Err(ShellOpenError::WaylandPreparation(error)) if allow_fallback && has_socket => {
                cause = Some(WaylandFallbackCause::Validation(error));
                self.open_prepared_shell(&intent, None).await
            }
            result => result,
        };
        match result {
            Ok(handle) => Ok((handle, cause)),
            Err(error) => Err(match cause {
                Some(cause) => ShellAttemptError::Fallback { cause, error },
                None => ShellAttemptError::Initial(error),
            }),
        }
    }

    async fn resolve_shell_wayland(
        &self,
        target: &ShellTarget,
        selection: &WaylandShellRequest,
    ) -> Result<Option<HostWaylandSocket>, SessionError> {
        match selection {
            WaylandShellRequest::Disabled => Ok(None),
            WaylandShellRequest::Automatic => self.automatic_wayland(target.machine()).await,
            WaylandShellRequest::SelectedHostDisplay(socket) => Ok(Some(socket.clone())),
            WaylandShellRequest::Display(display) => {
                select_wayland_socket(self.discover_host_wayland_sockets().await, display)
                    .map(Some)
                    .map_err(SessionError::new)
            }
        }
    }
}

fn select_wayland_socket(
    mut sockets: Vec<HostWaylandSocket>,
    display: &WaylandDisplay,
) -> Result<HostWaylandSocket, String> {
    if sockets.is_empty() {
        return Err("no usable host Wayland socket was discovered".into());
    }
    let Some(index) = sockets
        .iter()
        .position(|socket| socket.display() == display)
    else {
        let available = sockets
            .iter()
            .map(|socket| socket.display().as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "Wayland display {display} was not discovered (available: {available})"
        ));
    };
    Ok(sockets.remove(index))
}

#[cfg(test)]
mod tests;
