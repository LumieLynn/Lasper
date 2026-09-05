use crate::application::sessions::{
    GuestCommand, InteractiveShellEnvironment, ValidatedGuestUserName,
};
use crate::domain::machine::MachineName;
use crate::domain::session::{SessionSize, TerminalAttachmentKind};
use crate::domain::wayland::HostWaylandSocket;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::num::NonZeroU16;
use std::num::NonZeroU64;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct WireSessionId(NonZeroU64);

impl WireSessionId {
    pub(crate) fn new(value: u64) -> std::io::Result<Self> {
        NonZeroU64::new(value).map(Self).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "session id must be non-zero",
            )
        })
    }

    pub(crate) fn get(self) -> u64 {
        self.0.get()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SpawnJournalctlParams {
    pub session_id: WireSessionId,
    pub name: MachineName,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SpawnTerminalParams {
    pub session_id: WireSessionId,
    pub name: MachineName,
    pub size: WireTerminalSize,
    pub launch: WireTerminalLaunch,
}

/// Validated argv payload for a selected-user command crossing the elevated
/// daemon boundary. Deserialization reuses the application command rules so
/// malformed or relative executables are rejected before the daemon opens a
/// machine session.
#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireGuestCommand {
    pub program: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

impl<'de> Deserialize<'de> for WireGuestCommand {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawWireGuestCommand {
            program: String,
            #[serde(default)]
            args: Vec<String>,
        }

        let raw = RawWireGuestCommand::deserialize(deserializer)?;
        GuestCommand::new(raw.program.clone(), raw.args.clone())
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            program: raw.program,
            args: raw.args,
        })
    }
}

impl From<GuestCommand> for WireGuestCommand {
    fn from(value: GuestCommand) -> Self {
        Self {
            program: value.program().to_owned(),
            args: value.args().to_owned(),
        }
    }
}

impl TryFrom<WireGuestCommand> for GuestCommand {
    type Error = crate::application::sessions::GuestCommandError;

    fn try_from(value: WireGuestCommand) -> Result<Self, Self::Error> {
        GuestCommand::new(value.program, value.args)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "launch", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum WireTerminalLaunch {
    DefaultAttachment,
    LoginPrompt,
    SelectedUserShell {
        user: ValidatedGuestUserName,
        terminal: Box<InteractiveShellEnvironment>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wayland: Option<HostWaylandSocket>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<WireGuestCommand>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrepareWaylandParams {
    pub probe_id: WireSessionId,
    pub machine: MachineName,
    pub user: ValidatedGuestUserName,
    pub host_socket: HostWaylandSocket,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PrepareWaylandResponse {
    Ready {
        guest_socket: PathBuf,
        uid: u32,
        gid: u32,
    },
    Failed {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
    },
}

/// Raw terminal dimensions used only at the privileged FD boundary.
///
/// The application and session adapters exchange the validated domain
/// `SessionSize`; this type keeps the JSON representation local to IPC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawTerminalSize", into = "RawTerminalSize")]
pub(crate) struct WireTerminalSize {
    cols: NonZeroU16,
    rows: NonZeroU16,
}

impl WireTerminalSize {
    pub(crate) fn cols(self) -> u16 {
        self.cols.get()
    }

    pub(crate) fn rows(self) -> u16 {
        self.rows.get()
    }

    pub(crate) fn into_session_size(self) -> SessionSize {
        // NonZeroU16 fields make this conversion infallible by construction.
        SessionSize::from_nonzero(self.cols, self.rows)
    }
}

impl From<SessionSize> for WireTerminalSize {
    fn from(value: SessionSize) -> Self {
        let (cols, rows) = value.into_nonzero();
        Self { cols, rows }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTerminalSize {
    cols: u16,
    rows: u16,
}

impl TryFrom<RawTerminalSize> for WireTerminalSize {
    type Error = WireTerminalSizeError;

    fn try_from(value: RawTerminalSize) -> Result<Self, Self::Error> {
        let cols = NonZeroU16::new(value.cols).ok_or(WireTerminalSizeError)?;
        let rows = NonZeroU16::new(value.rows).ok_or(WireTerminalSizeError)?;
        Ok(Self { cols, rows })
    }
}

impl From<WireTerminalSize> for RawTerminalSize {
    fn from(value: WireTerminalSize) -> Self {
        Self {
            cols: value.cols(),
            rows: value.rows(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WireTerminalSizeError;

impl fmt::Display for WireTerminalSizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("terminal dimensions must be non-zero u16 values")
    }
}

impl std::error::Error for WireTerminalSizeError {}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SpawnTerminalResponse {
    pub attach_kind: WireTerminalAttachmentKind,
    pub lifecycle: WireTerminalLifecycleSource,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireTerminalLifecycleSource {
    DaemonStatus,
    PtyEof,
    MachineRemoved,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireTerminalAttachmentKind {
    Login,
    Namespace,
}

impl From<TerminalAttachmentKind> for WireTerminalAttachmentKind {
    fn from(value: TerminalAttachmentKind) -> Self {
        match value {
            TerminalAttachmentKind::Login => Self::Login,
            TerminalAttachmentKind::Namespace => Self::Namespace,
        }
    }
}

impl From<WireTerminalAttachmentKind> for TerminalAttachmentKind {
    fn from(value: WireTerminalAttachmentKind) -> Self {
        match value {
            WireTerminalAttachmentKind::Login => Self::Login,
            WireTerminalAttachmentKind::Namespace => Self::Namespace,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CloseSessionParams {
    pub session_id: WireSessionId,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum WireSessionLifecycle {
    Exited { success: bool, code: Option<i32> },
    Failed { message: String },
}

impl From<WireSessionLifecycle> for crate::domain::session::SessionLifecycle {
    fn from(value: WireSessionLifecycle) -> Self {
        match value {
            WireSessionLifecycle::Exited { success, code } => Self::Exited { success, code },
            WireSessionLifecycle::Failed { message } => Self::Failed(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_session_id_rejects_zero() {
        assert!(serde_json::from_str::<WireSessionId>("0").is_err());
        assert_eq!(serde_json::from_str::<WireSessionId>("1").unwrap().get(), 1);
    }

    #[test]
    fn selected_user_launch_revalidates_the_wire_username() {
        let valid: WireTerminalLaunch = serde_json::from_value(serde_json::json!({
            "launch": "selected_user_shell",
            "user": "alice",
            "terminal": { "term": "xterm-256color" }
        }))
        .unwrap();
        assert!(matches!(
            valid,
            WireTerminalLaunch::SelectedUserShell {
                terminal,
                wayland: None,
                ..
            } if terminal.term() == "xterm-256color"
        ));

        assert!(
            serde_json::from_value::<WireTerminalLaunch>(serde_json::json!({
                "launch": "selected_user_shell",
                "user": "../root",
                "terminal": { "term": "xterm-256color" }
            }))
            .is_err()
        );

        assert!(
            serde_json::from_value::<WireTerminalLaunch>(serde_json::json!({
                "launch": "selected_user_shell",
                "user": "alice",
                "terminal": { "term": "bad term" }
            }))
            .is_err()
        );

        let command: WireTerminalLaunch = serde_json::from_value(serde_json::json!({
            "launch": "selected_user_shell",
            "user": "alice",
            "terminal": { "term": "xterm-256color" },
            "command": { "program": "/usr/bin/kitty", "args": ["--single-instance"] }
        }))
        .unwrap();
        assert!(matches!(
            command,
            WireTerminalLaunch::SelectedUserShell {
                command: Some(command),
                ..
            } if command.program == "/usr/bin/kitty" && command.args == ["--single-instance"]
        ));

        let encoded = serde_json::to_value(WireTerminalLaunch::SelectedUserShell {
            user: ValidatedGuestUserName::new("alice").unwrap(),
            terminal: Box::new(InteractiveShellEnvironment::default()),
            wayland: None,
            command: Some(
                GuestCommand::new("/usr/bin/kitty", vec!["--single-instance".into()])
                    .unwrap()
                    .into(),
            ),
        })
        .unwrap();
        assert_eq!(
            encoded["command"]["program"],
            serde_json::Value::String("/usr/bin/kitty".into())
        );

        assert!(
            serde_json::from_value::<WireTerminalLaunch>(serde_json::json!({
                "launch": "selected_user_shell",
                "user": "alice",
                "terminal": { "term": "xterm-256color" },
                "command": { "program": "kitty", "args": [] }
            }))
            .is_err()
        );
    }

    #[test]
    fn wayland_failure_preserves_its_actionable_hint() {
        let response = PrepareWaylandResponse::Failed {
            message: "projection is missing".into(),
            hint: Some("restart the machine".into()),
        };
        let encoded = serde_json::to_value(response).unwrap();
        let decoded: PrepareWaylandResponse = serde_json::from_value(encoded).unwrap();

        assert!(matches!(
            decoded,
            PrepareWaylandResponse::Failed { hint: Some(hint), .. }
                if hint == "restart the machine"
        ));
    }
}
