use crate::domain::machine::MachineName;
use crate::domain::session::{SessionId, SessionLifecycle, SessionSize, TerminalAttachmentKind};
use crate::domain::wayland::HostWaylandSocket;
use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize};
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
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
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

impl<'de> Deserialize<'de> for ValidatedGuestUserName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
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

const MAX_GUEST_COMMAND_ARGS: usize = 256;
const MAX_GUEST_COMMAND_ARG_BYTES: usize = 4096;
const MAX_GUEST_COMMAND_TOTAL_BYTES: usize = 64 * 1024;

/// A command to execute as the selected guest user.
///
/// The executable is deliberately required to be an absolute path because
/// machine1's `OpenMachineShell` contract has the same requirement. Arguments
/// remain separate values all the way to the runtime transport; no shell
/// command string is ever constructed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestCommand {
    program: String,
    args: Vec<String>,
}

impl GuestCommand {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Result<Self, GuestCommandError> {
        let program = program.into();
        validate_guest_command_value(&program, true)?;
        if !Path::new(&program).is_absolute() {
            return Err(GuestCommandError::ProgramNotAbsolute);
        }
        if args.len() > MAX_GUEST_COMMAND_ARGS {
            return Err(GuestCommandError::TooManyArguments {
                maximum: MAX_GUEST_COMMAND_ARGS,
            });
        }

        let mut total_bytes = program.len();
        for argument in &args {
            validate_guest_command_value(argument, false)?;
            total_bytes = total_bytes
                .checked_add(argument.len())
                .and_then(|total| total.checked_add(1))
                .ok_or(GuestCommandError::TooLong {
                    maximum: MAX_GUEST_COMMAND_TOTAL_BYTES,
                })?;
        }
        if total_bytes > MAX_GUEST_COMMAND_TOTAL_BYTES {
            return Err(GuestCommandError::TooLong {
                maximum: MAX_GUEST_COMMAND_TOTAL_BYTES,
            });
        }

        Ok(Self { program, args })
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Return the complete process argument vector, including argv[0].
    pub fn argv(&self) -> Vec<String> {
        std::iter::once(self.program.clone())
            .chain(self.args.iter().cloned())
            .collect()
    }
}

fn validate_guest_command_value(value: &str, program: bool) -> Result<(), GuestCommandError> {
    if program && value.is_empty() {
        return Err(GuestCommandError::EmptyProgram);
    }
    if value.len() > MAX_GUEST_COMMAND_ARG_BYTES {
        return Err(GuestCommandError::ArgumentTooLong {
            maximum: MAX_GUEST_COMMAND_ARG_BYTES,
        });
    }
    if value.contains('\0') {
        return Err(GuestCommandError::NulByte);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GuestCommandError {
    #[error("guest command executable cannot be empty")]
    EmptyProgram,
    #[error("guest command executable must be an absolute path")]
    ProgramNotAbsolute,
    #[error("guest command contains a NUL byte")]
    NulByte,
    #[error("guest command argument exceeds the {maximum}-byte limit")]
    ArgumentTooLong { maximum: usize },
    #[error("guest command has more than {maximum} arguments")]
    TooManyArguments { maximum: usize },
    #[error("guest command exceeds the {maximum}-byte total limit")]
    TooLong { maximum: usize },
}

const MAX_TERM_BYTES: usize = 256;
const MAX_TERMINAL_ENVIRONMENT_VALUE_BYTES: usize = 4096;

/// Terminal capability variables inherited by one selected-user shell. The
/// variable names are fixed; callers cannot add arbitrary entries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InteractiveShellEnvironment {
    term: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    colorterm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    no_color: Option<String>,
}

impl InteractiveShellEnvironment {
    /// Environment used by the embedded TUI terminal.  The TUI owns a
    /// terminal emulator rather than the user's outer terminal, so inheriting
    /// the host TERM would describe the wrong terminal capabilities.
    pub fn embedded() -> Self {
        let captured = Self::capture();
        Self {
            term: "xterm-256color".to_string(),
            colorterm: captured.colorterm,
            no_color: captured.no_color,
        }
    }

    pub fn capture() -> Self {
        let term = std::env::var("TERM")
            .ok()
            .filter(|value| valid_term(value))
            .unwrap_or_else(|| "dumb".to_string());
        let colorterm = captured_terminal_value("COLORTERM");
        let no_color = captured_terminal_value("NO_COLOR");
        Self {
            term,
            colorterm,
            no_color,
        }
    }

    pub(crate) fn new(
        term: String,
        colorterm: Option<String>,
        no_color: Option<String>,
    ) -> Result<Self, InteractiveShellEnvironmentError> {
        if !valid_term(&term) {
            return Err(InteractiveShellEnvironmentError::InvalidTerm);
        }
        for (name, value) in [("COLORTERM", &colorterm), ("NO_COLOR", &no_color)] {
            if value
                .as_deref()
                .is_some_and(|value| !valid_terminal_value(value))
            {
                return Err(InteractiveShellEnvironmentError::InvalidValue { name });
            }
        }
        Ok(Self {
            term,
            colorterm,
            no_color,
        })
    }

    pub fn term(&self) -> &str {
        &self.term
    }

    pub fn colorterm(&self) -> Option<&str> {
        self.colorterm.as_deref()
    }

    pub fn no_color(&self) -> Option<&str> {
        self.no_color.as_deref()
    }
}

impl Default for InteractiveShellEnvironment {
    fn default() -> Self {
        Self {
            term: "dumb".to_string(),
            colorterm: None,
            no_color: None,
        }
    }
}

impl<'de> Deserialize<'de> for InteractiveShellEnvironment {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Environment {
            term: String,
            #[serde(default)]
            colorterm: Option<String>,
            #[serde(default)]
            no_color: Option<String>,
        }

        let environment = Environment::deserialize(deserializer)?;
        Self::new(
            environment.term,
            environment.colorterm,
            environment.no_color,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InteractiveShellEnvironmentError {
    #[error("TERM is not a valid terminal type name")]
    InvalidTerm,
    #[error("{name} contains control characters or exceeds the value limit")]
    InvalidValue { name: &'static str },
}

fn captured_terminal_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| valid_terminal_value(value))
}

fn valid_term(value: &str) -> bool {
    !value.is_empty()
        && value != "unknown"
        && value.len() <= MAX_TERM_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'+' | b'.'))
}

fn valid_terminal_value(value: &str) -> bool {
    value.len() <= MAX_TERMINAL_ENVIRONMENT_VALUE_BYTES && !value.chars().any(char::is_control)
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
    terminal_environment: InteractiveShellEnvironment,
    command: Option<GuestCommand>,
    size: SessionSize,
}

impl ShellOpenIntent {
    pub fn new(
        target: ShellTarget,
        wayland: WaylandShellRequest,
        terminal_environment: InteractiveShellEnvironment,
        size: SessionSize,
    ) -> Self {
        Self {
            target,
            wayland,
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

    pub(crate) fn with_wayland(mut self, wayland: WaylandShellRequest) -> Self {
        self.wayland = wayland;
        self
    }

    pub fn target(&self) -> &ShellTarget {
        &self.target
    }

    pub fn wayland(&self) -> &WaylandShellRequest {
        &self.wayland
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TypedSessionEnvironment {
    terminal: InteractiveShellEnvironment,
    wayland: Option<WaylandSessionContext>,
}

impl TypedSessionEnvironment {
    pub(crate) fn terminal(terminal: InteractiveShellEnvironment) -> Self {
        Self {
            terminal,
            wayland: None,
        }
    }

    pub(crate) fn wayland(
        terminal: InteractiveShellEnvironment,
        context: WaylandSessionContext,
    ) -> Self {
        Self {
            terminal,
            wayland: Some(context),
        }
    }

    pub(crate) fn terminal_environment(&self) -> &InteractiveShellEnvironment {
        &self.terminal
    }

    pub(crate) fn wayland_context(&self) -> Option<&WaylandSessionContext> {
        self.wayland.as_ref()
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

/// Preserve whether a selected-user shell failed while preparing Wayland or
/// while opening the terminal itself. Callers may safely retry only the first
/// phase without risking a duplicate shell attachment.
#[derive(Debug, thiserror::Error)]
pub enum ShellOpenError {
    #[error("{0}")]
    WaylandPreparation(#[source] SessionError),
    #[error("{0}")]
    Terminal(#[source] SessionError),
}

impl ShellOpenError {
    pub fn session_error(&self) -> &SessionError {
        match self {
            Self::WaylandPreparation(error) | Self::Terminal(error) => error,
        }
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
    fn guest_command_keeps_argv_boundaries_and_requires_absolute_program() {
        let command = GuestCommand::new(
            "/usr/bin/kitty",
            vec!["--class".into(), "a b".into(), String::new()],
        )
        .unwrap();
        assert_eq!(command.program(), "/usr/bin/kitty");
        assert_eq!(command.args(), ["--class", "a b", ""]);
        assert_eq!(command.argv(), ["/usr/bin/kitty", "--class", "a b", ""]);
        assert!(matches!(
            GuestCommand::new("kitty", Vec::new()),
            Err(GuestCommandError::ProgramNotAbsolute)
        ));
    }

    #[test]
    fn guest_command_rejects_nul_and_unbounded_values() {
        assert!(matches!(
            GuestCommand::new("/bin/echo\0bad", Vec::new()),
            Err(GuestCommandError::NulByte)
        ));
        assert!(matches!(
            GuestCommand::new("/bin/echo", vec!["x".repeat(4097)]),
            Err(GuestCommandError::ArgumentTooLong { .. })
        ));
    }

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
    fn guest_user_name_deserialization_reapplies_validation() {
        let user: ValidatedGuestUserName = serde_json::from_str(r#""alice""#).unwrap();
        assert_eq!(user.as_str(), "alice");
        assert!(serde_json::from_str::<ValidatedGuestUserName>(r#""../root""#).is_err());
    }

    #[test]
    fn interactive_shell_environment_is_typed_and_wire_validated() {
        let environment = InteractiveShellEnvironment::new(
            "xterm-kitty".into(),
            Some("truecolor".into()),
            Some(String::new()),
        )
        .unwrap();
        let encoded = serde_json::to_value(&environment).unwrap();
        let decoded: InteractiveShellEnvironment = serde_json::from_value(encoded).unwrap();

        assert_eq!(decoded.term(), "xterm-kitty");
        assert_eq!(decoded.colorterm(), Some("truecolor"));
        assert_eq!(decoded.no_color(), Some(""));
        assert!(InteractiveShellEnvironment::new("bad term".into(), None, None).is_err());
        assert!(
            InteractiveShellEnvironment::new("xterm".into(), Some("truecolor\n".into()), None,)
                .is_err()
        );
        assert!(
            serde_json::from_value::<InteractiveShellEnvironment>(serde_json::json!({
                "term": "xterm",
                "unexpected": "value"
            }))
            .is_err()
        );
    }

    #[test]
    fn embedded_terminal_environment_uses_tui_capabilities() {
        let environment = InteractiveShellEnvironment::embedded();
        assert_eq!(environment.term(), "xterm-256color");
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
