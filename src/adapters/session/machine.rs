//! Transport-neutral machine session requests and route selection.
//!
//! The application asks for one of the two closed session operations below.
//! This module selects the already-composed D-Bus or CLI implementation; it
//! does not expose either wire shape to the rest of the session workflow.

use super::wayland_probe::WaylandProbeRequest;
use crate::application::sessions::{
    GuestCommand, InteractiveShellEnvironment, SessionError, TerminalSessionHandle,
    ValidatedGuestUserName,
};
use crate::domain::machine::MachineName;
use crate::domain::session::{SessionId, SessionSize, TerminalAttachmentKind};
use std::os::fd::OwnedFd;
use std::path::{Component, Path};

const MAX_ENVIRONMENT_VALUE_BYTES: usize = 4096;

/// The closed environment allowlist for a selected user's shell.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MachineShellEnvironment {
    terminal: InteractiveShellEnvironment,
    wayland_display: Option<String>,
}

impl MachineShellEnvironment {
    pub(crate) fn shell(
        terminal: InteractiveShellEnvironment,
        display: Option<&Path>,
    ) -> Result<Self, MachineShellEnvironmentError> {
        Ok(Self {
            terminal,
            wayland_display: display.map(validate_absolute_path).transpose()?,
        })
    }

    pub(crate) fn assignments(&self) -> Vec<String> {
        let mut assignments = vec![format!("TERM={}", self.terminal.term())];
        if let Some(value) = self.terminal.colorterm() {
            assignments.push(format!("COLORTERM={value}"));
        }
        if let Some(value) = self.terminal.no_color() {
            assignments.push(format!("NO_COLOR={value}"));
        }
        if let Some(display) = &self.wayland_display {
            assignments.push(format!("WAYLAND_DISPLAY={display}"));
        }
        assignments
    }

    pub(crate) fn terminal_environment(&self) -> &InteractiveShellEnvironment {
        &self.terminal
    }
}

fn validate_absolute_path(path: &Path) -> Result<String, MachineShellEnvironmentError> {
    if !path.is_absolute() {
        return Err(MachineShellEnvironmentError::NotAbsolute);
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(MachineShellEnvironmentError::RelativeComponent);
    }
    let value = path.to_str().ok_or(MachineShellEnvironmentError::NonUtf8)?;
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(MachineShellEnvironmentError::InvalidValue);
    }
    if value.len() > MAX_ENVIRONMENT_VALUE_BYTES {
        return Err(MachineShellEnvironmentError::TooLong {
            maximum: MAX_ENVIRONMENT_VALUE_BYTES,
        });
    }
    Ok(value.to_owned())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum MachineShellEnvironmentError {
    #[error("WAYLAND_DISPLAY must be an absolute path")]
    NotAbsolute,
    #[error("WAYLAND_DISPLAY must not contain relative path components")]
    RelativeComponent,
    #[error("WAYLAND_DISPLAY is not valid UTF-8")]
    NonUtf8,
    #[error("WAYLAND_DISPLAY contains an empty or control-character value")]
    InvalidValue,
    #[error("WAYLAND_DISPLAY exceeds the {maximum}-byte environment value limit")]
    TooLong { maximum: usize },
}

/// A selected user's machine1 shell request. An absent command asks systemd
/// to use the account's login shell; a present command carries one validated
/// executable and its argv values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MachineShellRequest {
    machine: MachineName,
    user: ValidatedGuestUserName,
    environment: MachineShellEnvironment,
    command: Option<GuestCommand>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MachineLoginRequest {
    machine: MachineName,
}

impl MachineLoginRequest {
    pub(crate) fn new(machine: MachineName) -> Self {
        Self { machine }
    }

    pub(crate) fn machine(&self) -> &MachineName {
        &self.machine
    }
}

impl MachineShellRequest {
    pub(crate) fn new(
        machine: MachineName,
        user: ValidatedGuestUserName,
        environment: MachineShellEnvironment,
    ) -> Self {
        Self {
            machine,
            user,
            environment,
            command: None,
        }
    }

    pub(crate) fn with_command(mut self, command: GuestCommand) -> Self {
        self.command = Some(command);
        self
    }

    pub(crate) fn machine(&self) -> &MachineName {
        &self.machine
    }

    pub(crate) fn user(&self) -> &ValidatedGuestUserName {
        &self.user
    }

    pub(crate) fn environment(&self) -> &MachineShellEnvironment {
        &self.environment
    }

    pub(crate) fn command(&self) -> Option<&GuestCommand> {
        self.command.as_ref()
    }
}

/// Closed set of session operations understood by both transports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MachineSessionRequest {
    Shell(MachineShellRequest),
    LoginPrompt(MachineLoginRequest),
    WaylandProbe(WaylandProbeRequest),
}

impl MachineSessionRequest {
    pub(crate) fn shell(request: MachineShellRequest) -> Self {
        Self::Shell(request)
    }

    pub(crate) fn login_prompt(machine: MachineName) -> Self {
        Self::LoginPrompt(MachineLoginRequest::new(machine))
    }

    pub(crate) fn wayland_probe(request: WaylandProbeRequest) -> Self {
        Self::WaylandProbe(request)
    }

    pub(crate) const fn context(&self) -> &'static str {
        match self {
            Self::Shell(_) => "open selected-user shell",
            Self::LoginPrompt(_) => "open machine login prompt",
            Self::WaylandProbe(_) => "open Wayland projection probe",
        }
    }
}

#[derive(Clone)]
pub(crate) enum MachineSessionTransport {
    Dbus(crate::adapters::runtime::dbus::DbusBackend),
    Cli,
}

/// Result of opening one closed machine session operation through the
/// selected route.  A D-Bus session hands ownership of a machined PTY to the
/// caller; a CLI session hands back a command that the caller must spawn in a
/// PTY.  The daemon and direct adapters consume the same result shape.
pub(crate) enum MachineSessionOpening {
    Dbus(MachinePty),
    Cli(Box<crate::adapters::session::terminal_attach::TerminalAttachCommand>),
}

pub(crate) struct MachinePty {
    pub(crate) master: OwnedFd,
    pub(crate) machine_removed:
        Option<tokio::sync::oneshot::Receiver<crate::domain::session::SessionLifecycle>>,
}

impl MachineSessionTransport {
    /// Open one closed machine operation using the route selected at
    /// composition time.
    pub(crate) async fn open(
        &self,
        request: MachineSessionRequest,
    ) -> Result<MachineSessionOpening, SessionError> {
        let context = request.context();
        match self {
            Self::Dbus(dbus) => {
                let master = dbus
                    .open_machine_session(request)
                    .await
                    .map_err(|error| map_machine_session_error(context, error))?;
                Ok(MachineSessionOpening::Dbus(master))
            }
            Self::Cli => {
                let command = crate::adapters::runtime::cli::machine_session_command(request)
                    .map_err(|error| {
                        SessionError::new(format!("build {context} machinectl command: {error}"))
                    })?;
                Ok(MachineSessionOpening::Cli(Box::new(command)))
            }
        }
    }

    /// Open a session in the current process and return an application-owned
    /// terminal handle, regardless of the selected transport.
    pub(crate) async fn open_local(
        &self,
        request: MachineSessionRequest,
        id: SessionId,
        size: SessionSize,
    ) -> Result<TerminalSessionHandle, SessionError> {
        let opening = self.open(request).await?;
        match opening {
            MachineSessionOpening::Dbus(pty) => super::pty::spawn_machine_terminal(pty, id, size),
            MachineSessionOpening::Cli(command) => super::pty::spawn_direct_terminal(
                (*command).into_pty_command().map_err(|error| {
                    SessionError::new(format!("validate machinectl session command: {error}"))
                })?,
                id,
                TerminalAttachmentKind::Login,
                size,
            ),
        }
    }

    /// Kept for composition diagnostics; callers should use [`open`] rather
    /// than branching on this value when opening a session.
    #[cfg(test)]
    pub(crate) const fn uses_dbus(&self) -> bool {
        matches!(self, Self::Dbus(_))
    }
}

fn map_machine_session_error(
    context: &'static str,
    error: crate::adapters::error::NspawnError,
) -> SessionError {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> MachineName {
        MachineName::new("demo").unwrap()
    }

    fn user() -> ValidatedGuestUserName {
        ValidatedGuestUserName::new("alice").unwrap()
    }

    #[test]
    fn shell_environment_is_typed_and_deterministic() {
        let terminal = InteractiveShellEnvironment::new(
            "xterm-256color".into(),
            Some("truecolor".into()),
            Some(String::new()),
        )
        .unwrap();
        let environment = MachineShellEnvironment::shell(
            terminal,
            Some(Path::new("/run/lasper/wayland/1000/wayland-0")),
        )
        .unwrap();
        assert_eq!(
            environment.assignments(),
            [
                "TERM=xterm-256color",
                "COLORTERM=truecolor",
                "NO_COLOR=",
                "WAYLAND_DISPLAY=/run/lasper/wayland/1000/wayland-0",
            ]
        );
        assert_eq!(
            MachineShellEnvironment::shell(InteractiveShellEnvironment::default(), None)
                .unwrap()
                .assignments(),
            ["TERM=dumb"]
        );
    }

    #[test]
    fn environment_rejects_relative_or_control_paths() {
        for path in ["wayland-0", "/run/../tmp/socket", "/run/socket\n"] {
            assert!(MachineShellEnvironment::shell(
                InteractiveShellEnvironment::default(),
                Some(Path::new(path)),
            )
            .is_err());
        }
    }

    #[test]
    fn session_requests_keep_shell_and_probe_operations_closed() {
        let shell = MachineSessionRequest::shell(MachineShellRequest::new(
            machine(),
            user(),
            MachineShellEnvironment::default(),
        ));
        assert_eq!(shell.context(), "open selected-user shell");

        let login = MachineSessionRequest::login_prompt(machine());
        assert_eq!(login.context(), "open machine login prompt");

        let probe = MachineSessionRequest::wayland_probe(
            WaylandProbeRequest::target(machine(), user(), Path::new("/custom/display.sock"))
                .unwrap(),
        );
        assert_eq!(probe.context(), "open Wayland projection probe");
    }

    #[test]
    fn shell_request_can_carry_a_validated_guest_command() {
        let request =
            MachineShellRequest::new(machine(), user(), MachineShellEnvironment::default())
                .with_command(GuestCommand::new("/bin/echo", vec!["hello".into()]).unwrap());
        assert_eq!(request.command().unwrap().program(), "/bin/echo");
        assert_eq!(request.command().unwrap().args(), ["hello"]);
    }
}
