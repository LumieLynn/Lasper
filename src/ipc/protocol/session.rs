use crate::domain::machine::MachineName;
use crate::domain::session::{SessionSize, TerminalAttachmentKind};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::num::NonZeroU16;
use std::num::NonZeroU64;

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
}
