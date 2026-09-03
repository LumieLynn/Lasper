use crate::domain::machine::MachineName;
use crate::domain::session::{SessionId, SessionLifecycle, SessionSize, TerminalAttachmentKind};
use crate::domain::wayland::HostWaylandSocket;
use async_trait::async_trait;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, watch};

pub(crate) const TERMINAL_COMMAND_CAPACITY: usize = 1024;
pub(crate) const TERMINAL_OUTPUT_CAPACITY: usize = 256;
pub(crate) const JOURNAL_OUTPUT_CAPACITY: usize = 1024;

/// A guest account name accepted by machine1's selected-user shell contract.
/// It is passed as one D-Bus/argv value after validation, never interpolated
/// into a shell command.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ValidatedGuestUserName(String);

impl ValidatedGuestUserName {
    pub fn new(value: impl Into<String>) -> Result<Self, GuestUserNameError> {
        let value = value.into();
        let bytes = value.as_bytes();
        if bytes.is_empty() {
            return Err(GuestUserNameError::Empty);
        }
        if bytes.len() > 32 {
            return Err(GuestUserNameError::TooLong);
        }
        if !value.is_ascii() {
            return Err(GuestUserNameError::NonAscii);
        }
        if bytes[0] == b'-' {
            return Err(GuestUserNameError::LeadingDash);
        }
        if bytes.iter().any(|byte| {
            byte.is_ascii_whitespace()
                || byte.is_ascii_control()
                || matches!(*byte, b'/' | b'\\' | b'@')
        }) {
            return Err(GuestUserNameError::InvalidCharacter);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ValidatedGuestUserName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<&str> for ValidatedGuestUserName {
    type Error = GuestUserNameError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GuestUserNameError {
    #[error("guest username cannot be empty")]
    Empty,
    #[error("guest username exceeds the 32-byte limit")]
    TooLong,
    #[error("guest username must be ASCII")]
    NonAscii,
    #[error("guest username cannot start with '-'")]
    LeadingDash,
    #[error("guest username contains whitespace, control, path-separator, or '@' characters")]
    InvalidCharacter,
}

/// The only user-selected identity accepted by the shell workflow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellTarget {
    machine: MachineName,
    user: ValidatedGuestUserName,
}

impl ShellTarget {
    pub fn new(machine: MachineName, user: ValidatedGuestUserName) -> Self {
        Self { machine, user }
    }

    pub fn machine(&self) -> &MachineName {
        &self.machine
    }

    pub fn user(&self) -> &ValidatedGuestUserName {
        &self.user
    }
}

/// Optional Wayland context requested for one selected-user shell.
///
/// The host socket is produced by the local discovery adapter. The UI never
/// submits an arbitrary path, and the session adapter must revalidate this
/// evidence against the static `.nspawn` projection before opening the PTY.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaylandShellRequest {
    Disabled,
    SelectedHostDisplay(HostWaylandSocket),
}

impl WaylandShellRequest {
    pub fn host_socket(&self) -> Option<&HostWaylandSocket> {
        match self {
            Self::Disabled => None,
            Self::SelectedHostDisplay(socket) => Some(socket),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellOpenIntent {
    target: ShellTarget,
    wayland: WaylandShellRequest,
    size: SessionSize,
}

impl ShellOpenIntent {
    pub fn new(target: ShellTarget, wayland: WaylandShellRequest, size: SessionSize) -> Self {
        Self {
            target,
            wayland,
            size,
        }
    }

    pub fn target(&self) -> &ShellTarget {
        &self.target
    }

    pub fn wayland(&self) -> &WaylandShellRequest {
        &self.wayland
    }

    pub const fn size(&self) -> SessionSize {
        self.size
    }
}

/// Runtime evidence that one startup-configured projection is usable by the
/// selected guest account. It is deliberately not serializable: callers must
/// obtain a fresh value from `SessionPort::prepare_wayland`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaylandSessionContext {
    host_socket: HostWaylandSocket,
    guest_socket: PathBuf,
    identity: ObservedGuestIdentity,
}

impl WaylandSessionContext {
    pub(crate) fn verified(
        host_socket: HostWaylandSocket,
        guest_socket: PathBuf,
        identity: ObservedGuestIdentity,
    ) -> Self {
        Self {
            host_socket,
            guest_socket,
            identity,
        }
    }

    pub fn host_socket(&self) -> &HostWaylandSocket {
        &self.host_socket
    }

    pub fn guest_socket(&self) -> &Path {
        &self.guest_socket
    }

    pub const fn identity(&self) -> ObservedGuestIdentity {
        self.identity
    }
}

/// Closed, feature-owned environment allowlist for `OpenMachineShell`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TypedSessionEnvironment {
    wayland: Option<WaylandSessionContext>,
}

impl TypedSessionEnvironment {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    pub(crate) fn wayland(context: WaylandSessionContext) -> Self {
        Self {
            wayland: Some(context),
        }
    }

    pub(crate) fn wayland_context(&self) -> Option<&WaylandSessionContext> {
        self.wayland.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TerminalLaunch {
    DefaultAttachment,
    SelectedUserShell {
        user: ValidatedGuestUserName,
        environment: TypedSessionEnvironment,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalSessionRequest {
    pub id: SessionId,
    pub machine: MachineName,
    pub size: SessionSize,
    pub(crate) launch: TerminalLaunch,
}

impl TerminalSessionRequest {
    pub(crate) fn default_attachment(
        id: SessionId,
        machine: MachineName,
        size: SessionSize,
    ) -> Self {
        Self {
            id,
            machine,
            size,
            launch: TerminalLaunch::DefaultAttachment,
        }
    }

    pub(crate) fn selected_user_shell(
        id: SessionId,
        machine: MachineName,
        user: ValidatedGuestUserName,
        environment: TypedSessionEnvironment,
        size: SessionSize,
    ) -> Self {
        Self {
            id,
            machine,
            size,
            launch: TerminalLaunch::SelectedUserShell { user, environment },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalSessionRequest {
    pub id: SessionId,
    pub machine: MachineName,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaylandPreparationRequest {
    pub identity_probe_id: SessionId,
    pub access_probe_id: SessionId,
    pub target: ShellTarget,
    pub host_socket: HostWaylandSocket,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObservedGuestIdentity {
    uid: u32,
    gid: u32,
}

impl ObservedGuestIdentity {
    pub fn new(uid: u32, gid: u32) -> Self {
        Self { uid, gid }
    }

    pub fn uid(self) -> u32 {
        self.uid
    }

    pub fn gid(self) -> u32 {
        self.gid
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct SessionError {
    message: String,
    hint: Option<String>,
}

impl SessionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            hint: None,
        }
    }

    pub fn with_hint(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            hint: Some(hint.into()),
        }
    }

    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }
}

impl From<std::io::Error> for SessionError {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}

#[async_trait]
pub trait SessionPort: Send + Sync + 'static {
    async fn discover_host_wayland_sockets(&self) -> Vec<HostWaylandSocket>;

    async fn open_terminal(
        &self,
        request: TerminalSessionRequest,
    ) -> Result<TerminalSessionHandle, SessionError>;

    async fn prepare_wayland(
        &self,
        request: WaylandPreparationRequest,
    ) -> Result<WaylandSessionContext, SessionError>;

    async fn open_journal(
        &self,
        request: JournalSessionRequest,
    ) -> Result<JournalSessionHandle, SessionError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionSendStatus {
    Queued,
    Full,
    Closed,
}

#[derive(Debug)]
pub(crate) enum TerminalCommand {
    Input(Vec<u8>),
    Reply(Vec<u8>),
    Resize(SessionSize),
}

#[derive(Clone)]
pub struct TerminalSessionInput {
    tx: mpsc::Sender<TerminalCommand>,
}

impl TerminalSessionInput {
    pub fn try_input(&self, bytes: Vec<u8>) -> SessionSendStatus {
        send_terminal_command(&self.tx, TerminalCommand::Input(bytes))
    }

    pub async fn send_reply(&self, bytes: Vec<u8>) -> SessionSendStatus {
        match self.tx.send(TerminalCommand::Reply(bytes)).await {
            Ok(()) => SessionSendStatus::Queued,
            Err(_) => SessionSendStatus::Closed,
        }
    }

    pub fn try_resize(&self, size: SessionSize) -> SessionSendStatus {
        send_terminal_command(&self.tx, TerminalCommand::Resize(size))
    }
}

fn send_terminal_command(
    tx: &mpsc::Sender<TerminalCommand>,
    command: TerminalCommand,
) -> SessionSendStatus {
    match tx.try_send(command) {
        Ok(()) => SessionSendStatus::Queued,
        Err(mpsc::error::TrySendError::Full(_)) => SessionSendStatus::Full,
        Err(mpsc::error::TrySendError::Closed(_)) => SessionSendStatus::Closed,
    }
}

pub struct TerminalSessionHandle {
    id: SessionId,
    attachment: TerminalAttachmentKind,
    input: TerminalSessionInput,
    output: Option<mpsc::Receiver<Vec<u8>>>,
    lifecycle: watch::Receiver<SessionLifecycle>,
    resize_failed: Arc<AtomicBool>,
    close: Option<oneshot::Sender<()>>,
}

impl TerminalSessionHandle {
    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn attachment(&self) -> TerminalAttachmentKind {
        self.attachment
    }

    pub fn input(&self) -> TerminalSessionInput {
        self.input.clone()
    }

    pub fn take_output(&mut self) -> Option<mpsc::Receiver<Vec<u8>>> {
        self.output.take()
    }

    pub fn lifecycle(&self) -> SessionLifecycle {
        self.lifecycle.borrow().clone()
    }

    pub fn take_resize_failure(&self) -> bool {
        self.resize_failed.swap(false, Ordering::AcqRel)
    }

    pub fn close(&mut self) {
        if let Some(close) = self.close.take() {
            let _ = close.send(());
        }
    }
}

impl Drop for TerminalSessionHandle {
    fn drop(&mut self) {
        self.close();
    }
}

pub(crate) struct TerminalSessionEndpoint {
    pub commands: mpsc::Receiver<TerminalCommand>,
    pub output: mpsc::Sender<Vec<u8>>,
    pub lifecycle: watch::Sender<SessionLifecycle>,
    pub resize_failed: Arc<AtomicBool>,
    pub close: oneshot::Receiver<()>,
}

pub(crate) fn terminal_session_channel(
    id: SessionId,
    attachment: TerminalAttachmentKind,
) -> (TerminalSessionHandle, TerminalSessionEndpoint) {
    let (command_tx, command_rx) = mpsc::channel(TERMINAL_COMMAND_CAPACITY);
    let (output_tx, output_rx) = mpsc::channel(TERMINAL_OUTPUT_CAPACITY);
    let (lifecycle_tx, lifecycle_rx) = watch::channel(SessionLifecycle::Running);
    let (close_tx, close_rx) = oneshot::channel();
    let resize_failed = Arc::new(AtomicBool::new(false));
    (
        TerminalSessionHandle {
            id,
            attachment,
            input: TerminalSessionInput { tx: command_tx },
            output: Some(output_rx),
            lifecycle: lifecycle_rx,
            resize_failed: Arc::clone(&resize_failed),
            close: Some(close_tx),
        },
        TerminalSessionEndpoint {
            commands: command_rx,
            output: output_tx,
            lifecycle: lifecycle_tx,
            resize_failed,
            close: close_rx,
        },
    )
}

pub struct JournalSessionHandle {
    id: SessionId,
    output: mpsc::Receiver<String>,
    lifecycle: watch::Receiver<SessionLifecycle>,
    close: Option<oneshot::Sender<()>>,
}

impl JournalSessionHandle {
    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn try_recv(&mut self) -> Result<String, mpsc::error::TryRecvError> {
        self.output.try_recv()
    }

    pub fn lifecycle(&self) -> SessionLifecycle {
        self.lifecycle.borrow().clone()
    }

    pub fn close(&mut self) {
        if let Some(close) = self.close.take() {
            let _ = close.send(());
        }
    }
}

impl Drop for JournalSessionHandle {
    fn drop(&mut self) {
        self.close();
    }
}

pub(crate) struct JournalSessionEndpoint {
    pub output: mpsc::Sender<String>,
    pub lifecycle: watch::Sender<SessionLifecycle>,
    pub close: oneshot::Receiver<()>,
}

pub(crate) fn journal_session_channel(
    id: SessionId,
) -> (JournalSessionHandle, JournalSessionEndpoint) {
    let (output_tx, output_rx) = mpsc::channel(JOURNAL_OUTPUT_CAPACITY);
    let (lifecycle_tx, lifecycle_rx) = watch::channel(SessionLifecycle::Running);
    let (close_tx, close_rx) = oneshot::channel();
    (
        JournalSessionHandle {
            id,
            output: output_rx,
            lifecycle: lifecycle_rx,
            close: Some(close_tx),
        },
        JournalSessionEndpoint {
            output: output_tx,
            lifecycle: lifecycle_tx,
            close: close_rx,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_user_name_accepts_numeric_and_common_system_accounts() {
        assert_eq!(
            ValidatedGuestUserName::new("1000").unwrap().as_str(),
            "1000"
        );
        assert_eq!(
            ValidatedGuestUserName::new("alice-1").unwrap().as_str(),
            "alice-1"
        );
        assert_eq!(
            ValidatedGuestUserName::new("nvidia$").unwrap().as_str(),
            "nvidia$"
        );
    }

    #[test]
    fn guest_user_name_rejects_target_and_argument_delimiters() {
        for value in [
            "",
            "-root",
            "alice@host",
            "alice/name",
            "alice\\name",
            "alice name",
            "alice\n",
        ] {
            assert!(ValidatedGuestUserName::new(value).is_err(), "{value:?}");
        }
    }

    #[test]
    fn terminal_input_and_output_channels_are_bounded() {
        let id = SessionId::new(1).unwrap();
        let (mut handle, mut endpoint) =
            terminal_session_channel(id, TerminalAttachmentKind::Login);
        for _ in 0..TERMINAL_COMMAND_CAPACITY {
            assert_eq!(
                handle.input().try_input(vec![b'x']),
                SessionSendStatus::Queued
            );
        }
        assert_eq!(
            handle.input().try_input(vec![b'x']),
            SessionSendStatus::Full
        );

        for _ in 0..TERMINAL_OUTPUT_CAPACITY {
            endpoint.output.try_send(vec![b'x']).unwrap();
        }
        assert!(matches!(
            endpoint.output.try_send(vec![b'x']),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        assert!(handle.take_output().is_some());
        assert!(matches!(
            endpoint.commands.try_recv(),
            Ok(TerminalCommand::Input(_))
        ));
    }

    #[test]
    fn dropping_a_handle_requests_session_close() {
        let id = SessionId::new(1).unwrap();
        let (handle, mut endpoint) = journal_session_channel(id);
        drop(handle);
        assert!(endpoint.close.try_recv().is_ok());
    }
}
