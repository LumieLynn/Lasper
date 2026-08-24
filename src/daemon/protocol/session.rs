use crate::domain::machine::MachineName;
use crate::domain::session::TerminalAttachmentKind;
use crate::nspawn::models::TerminalSize;
use serde::{Deserialize, Serialize};
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
    pub size: TerminalSize,
}

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
