use super::execution::{
    GuestCommand, GuestTarget, InteractiveShellEnvironment, MappedGuestIdentity,
    ObservedGuestIdentity, ValidatedGuestUserName,
};
use crate::domain::machine::MachineName;
use crate::domain::session::{SessionId, SessionLifecycle, SessionSize, TerminalAttachmentKind};
use crate::domain::wayland::{HostWaylandSocket, WaylandDisplay};
use crate::domain::x11::HostX11Socket;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, watch};

pub(crate) const TERMINAL_COMMAND_CAPACITY: usize = 1024;
pub(crate) const TERMINAL_OUTPUT_CAPACITY: usize = 256;
pub(crate) const JOURNAL_OUTPUT_CAPACITY: usize = 1024;

/// Optional Wayland context requested for one selected-user shell.
///
/// Automatic and named selections are resolved by the session service. A
/// preselected socket still needs validation against the static projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaylandShellRequest {
    Automatic,
    Display(WaylandDisplay),
    Disabled,
    SelectedHostDisplay(HostWaylandSocket),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellOpenIntent {
    target: GuestTarget,
    wayland: WaylandShellRequest,
    x11: Option<X11SessionContext>,
    terminal_environment: InteractiveShellEnvironment,
    command: Option<GuestCommand>,
    size: SessionSize,
}

impl ShellOpenIntent {
    pub fn new(
        target: GuestTarget,
        wayland: WaylandShellRequest,
        terminal_environment: InteractiveShellEnvironment,
        size: SessionSize,
    ) -> Self {
        Self {
            target,
            wayland,
            x11: None,
            terminal_environment,
            command: None,
            size,
        }
    }

    /// Attach a command to the selected-user shell request.
    pub fn with_command(mut self, command: GuestCommand) -> Self {
        self.command = Some(command);
        self
    }

    pub fn with_x11(mut self, context: X11SessionContext) -> Self {
        self.x11 = Some(context);
        self
    }

    pub fn target(&self) -> &GuestTarget {
        &self.target
    }

    pub fn wayland(&self) -> &WaylandShellRequest {
        &self.wayland
    }

    pub fn x11(&self) -> Option<&X11SessionContext> {
        self.x11.as_ref()
    }

    pub fn terminal_environment(&self) -> &InteractiveShellEnvironment {
        &self.terminal_environment
    }

    pub fn command(&self) -> Option<&GuestCommand> {
        self.command.as_ref()
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
    guest_socket: PathBuf,
    identity: ObservedGuestIdentity,
}

impl WaylandSessionContext {
    pub(crate) fn verified(guest_socket: PathBuf, identity: ObservedGuestIdentity) -> Self {
        Self {
            guest_socket,
            identity,
        }
    }

    pub fn guest_socket(&self) -> &Path {
        &self.guest_socket
    }

    pub const fn identity(&self) -> ObservedGuestIdentity {
        self.identity
    }
}

/// Filesystem permission observations for the two guest-visible pathname
/// endpoints of an X11 projection. These do not describe abstract-socket
/// reachability or whether a client library will select that transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct X11FilesystemAccess {
    mount_writable: bool,
    client_writable: bool,
}

impl X11FilesystemAccess {
    pub(crate) const fn observed(mount_writable: bool, client_writable: bool) -> Self {
        Self {
            mount_writable,
            client_writable,
        }
    }

    pub const fn mount_writable(self) -> bool {
        self.mount_writable
    }

    pub const fn client_writable(self) -> bool {
        self.client_writable
    }

    pub const fn fully_writable(self) -> bool {
        self.mount_writable && self.client_writable
    }
}

/// Fresh evidence that one startup-configured X11 projection preserves the
/// selected host endpoint's identity at the guest mount and standard client
/// paths. Pathname permissions are retained separately because an X client
/// may instead use the Linux abstract transport. X server ACL state remains a
/// separate user-side observation and is intentionally absent here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11ProjectionContext {
    host_socket: HostX11Socket,
    guest_mount: PathBuf,
    guest_client_path: PathBuf,
    filesystem_access: X11FilesystemAccess,
    identity: MappedGuestIdentity,
}

impl X11ProjectionContext {
    pub(crate) fn verified(
        host_socket: HostX11Socket,
        guest_mount: PathBuf,
        guest_client_path: PathBuf,
        filesystem_access: X11FilesystemAccess,
        identity: MappedGuestIdentity,
    ) -> Self {
        Self {
            host_socket,
            guest_mount,
            guest_client_path,
            filesystem_access,
            identity,
        }
    }

    pub fn host_socket(&self) -> &HostX11Socket {
        &self.host_socket
    }

    pub fn guest_mount(&self) -> &Path {
        &self.guest_mount
    }

    pub fn guest_client_path(&self) -> &Path {
        &self.guest_client_path
    }

    pub const fn filesystem_access(&self) -> X11FilesystemAccess {
        self.filesystem_access
    }

    pub const fn identity(&self) -> MappedGuestIdentity {
        self.identity
    }
}

/// Prepared X11 access for one selected-user shell.
///
/// The projection is fresh runtime evidence, while construction of this
/// context additionally means the invoking desktop user has completed the
/// X-server ACL preparation required for this exact target.  Keeping the
/// target alongside the evidence prevents a prepared authorization from
/// being attached to another machine or guest account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11SessionContext {
    target: GuestTarget,
    projection: X11ProjectionContext,
}

impl X11SessionContext {
    pub(crate) fn prepared(target: GuestTarget, projection: X11ProjectionContext) -> Self {
        Self { target, projection }
    }

    pub fn target(&self) -> &GuestTarget {
        &self.target
    }

    pub fn projection(&self) -> &X11ProjectionContext {
        &self.projection
    }

    pub fn display(&self) -> u16 {
        self.projection.host_socket().display()
    }
}

/// Closed display intent and terminal environment for a selected-user shell.
/// The execution adapter validates the Wayland projection before opening;
/// X11 authorization remains owned by the invoking desktop user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TypedSessionEnvironment {
    terminal: InteractiveShellEnvironment,
    wayland: Option<HostWaylandSocket>,
    x11: Option<X11SessionContext>,
}

impl TypedSessionEnvironment {
    pub(crate) fn terminal(terminal: InteractiveShellEnvironment) -> Self {
        Self {
            terminal,
            wayland: None,
            x11: None,
        }
    }

    pub(crate) fn wayland(
        terminal: InteractiveShellEnvironment,
        socket: HostWaylandSocket,
    ) -> Self {
        Self {
            terminal,
            wayland: Some(socket),
            x11: None,
        }
    }

    pub(crate) fn with_x11(mut self, context: X11SessionContext) -> Self {
        self.x11 = Some(context);
        self
    }

    pub(crate) fn terminal_environment(&self) -> &InteractiveShellEnvironment {
        &self.terminal
    }

    pub(crate) fn wayland_socket(&self) -> Option<&HostWaylandSocket> {
        self.wayland.as_ref()
    }

    pub(crate) fn x11_context(&self) -> Option<&X11SessionContext> {
        self.x11.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TerminalLaunch {
    LoginPrompt,
    SelectedUserShell {
        user: ValidatedGuestUserName,
        environment: Box<TypedSessionEnvironment>,
        command: Option<GuestCommand>,
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
    pub(crate) fn login_prompt(id: SessionId, machine: MachineName, size: SessionSize) -> Self {
        Self {
            id,
            machine,
            size,
            launch: TerminalLaunch::LoginPrompt,
        }
    }

    pub(crate) fn selected_user_shell_with_command(
        id: SessionId,
        machine: MachineName,
        user: ValidatedGuestUserName,
        environment: TypedSessionEnvironment,
        command: Option<GuestCommand>,
        size: SessionSize,
    ) -> Self {
        Self {
            id,
            machine,
            size,
            launch: TerminalLaunch::SelectedUserShell {
                user,
                environment: Box::new(environment),
                command,
            },
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
    pub probe_id: SessionId,
    pub target: GuestTarget,
    pub host_socket: HostWaylandSocket,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11ProjectionProbeRequest {
    pub probe_id: SessionId,
    pub target: GuestTarget,
    pub host_socket: HostX11Socket,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct SessionError {
    message: String,
    details: Option<Box<SessionErrorDetails>>,
}

#[derive(Debug, Default)]
struct SessionErrorDetails {
    hint: Option<String>,
    execution_failure: Option<super::execution::GuestExecutionFailure>,
}

impl SessionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            details: None,
        }
    }

    pub fn with_hint(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            details: Some(Box::new(SessionErrorDetails {
                hint: Some(hint.into()),
                execution_failure: None,
            })),
        }
    }

    pub fn hint(&self) -> Option<&str> {
        self.details
            .as_ref()
            .and_then(|details| details.hint.as_deref())
    }

    pub(crate) fn execution_failure(&self) -> Option<super::execution::GuestExecutionFailure> {
        self.details
            .as_ref()
            .and_then(|details| details.execution_failure)
    }

    pub(crate) fn with_execution_failure(
        mut self,
        failure: super::execution::GuestExecutionFailure,
    ) -> Self {
        self.details
            .get_or_insert_with(Default::default)
            .execution_failure = Some(failure);
        self
    }

    pub(crate) fn context(mut self, context: &str) -> Self {
        self.message = format!("{context}: {}", self.message);
        self
    }
}

impl From<std::io::Error> for SessionError {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}

/// Only a failure before opening the selected-user terminal can safely be
/// retried without Wayland. Transport errors after an open request are terminal
/// failures: the peer may already have created the session.
#[derive(Debug, thiserror::Error)]
pub enum TerminalOpenError {
    #[error("{0}")]
    WaylandPreparation(#[source] SessionError),
    #[error("{0}")]
    Terminal(#[from] SessionError),
}

impl TerminalOpenError {
    pub fn into_session_error(self) -> SessionError {
        match self {
            Self::WaylandPreparation(error) | Self::Terminal(error) => error,
        }
    }
}

impl From<std::io::Error> for TerminalOpenError {
    fn from(error: std::io::Error) -> Self {
        Self::Terminal(error.into())
    }
}

/// Preserve whether a selected-user shell failed while preparing Wayland or
/// while opening the terminal itself. Callers may safely retry only the first
/// phase without risking a duplicate shell attachment.
#[derive(Debug, thiserror::Error)]
pub enum ShellOpenError {
    #[error("{0}")]
    WaylandSelection(#[source] SessionError),
    #[error("{0}")]
    X11Context(#[source] SessionError),
    #[error("{0}")]
    WaylandPreparation(#[source] SessionError),
    #[error("{0}")]
    Terminal(#[source] SessionError),
}

impl ShellOpenError {
    pub fn session_error(&self) -> &SessionError {
        match self {
            Self::WaylandSelection(error)
            | Self::X11Context(error)
            | Self::WaylandPreparation(error)
            | Self::Terminal(error) => error,
        }
    }
}

impl From<TerminalOpenError> for ShellOpenError {
    fn from(error: TerminalOpenError) -> Self {
        match error {
            TerminalOpenError::WaylandPreparation(error) => Self::WaylandPreparation(error),
            TerminalOpenError::Terminal(error) => Self::Terminal(error),
        }
    }
}

#[async_trait]
pub trait SessionPort: Send + Sync + 'static {
    async fn discover_host_wayland_sockets(&self) -> Vec<HostWaylandSocket>;

    async fn automatic_wayland(
        &self,
        machine: &MachineName,
    ) -> Result<Option<HostWaylandSocket>, SessionError>;

    async fn open_terminal(
        &self,
        request: TerminalSessionRequest,
    ) -> Result<TerminalSessionHandle, TerminalOpenError>;

    async fn prepare_wayland(
        &self,
        request: WaylandPreparationRequest,
    ) -> Result<WaylandSessionContext, SessionError>;

    async fn probe_x11_projection(
        &self,
        request: X11ProjectionProbeRequest,
    ) -> Result<X11ProjectionContext, SessionError>;

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

    pub(crate) async fn send_input(&self, bytes: Vec<u8>) -> SessionSendStatus {
        match self.tx.send(TerminalCommand::Input(bytes)).await {
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

    pub(crate) async fn wait(&mut self) -> SessionLifecycle {
        loop {
            let state = self.lifecycle.borrow().clone();
            if !state.is_running() {
                return state;
            }
            if self.lifecycle.changed().await.is_err() {
                return SessionLifecycle::Failed(
                    "terminal lifecycle channel closed while the session was running".into(),
                );
            }
        }
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
