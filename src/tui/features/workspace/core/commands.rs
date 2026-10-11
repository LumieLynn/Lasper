//! Requests that leave the workspace after its local state is committed.

use crate::tui::features::workspace::terminal_panel::TerminalInputStatus;

#[derive(Debug, PartialEq)]
pub(crate) enum WorkspaceCommand {
    Quit,
    Help,
    Leader,
    Start,
    Poweroff,
    ResourceActions,
    Wizard,
    Refresh,
    Shell,
    RemoveImage,
    TerminalInputRejected(TerminalInputStatus),
}

#[derive(Debug, PartialEq)]
pub(crate) struct WorkspaceInputResult {
    pub command: Option<WorkspaceCommand>,
    /// Allows App to dismiss a transient overlay after a local focus change.
    pub focus_changed: bool,
}
