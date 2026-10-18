//! Deliver runtime updates and apply commands leaving the workspace.

use crate::application::RuntimeUpdate;
use crate::domain::runtime::{ImageEntry, MachineEntry, RuntimeSnapshot};
use crate::tui::app::App;
use crate::tui::features::workspace::inspector::refresh::DetailRefreshCompletion;
use crate::tui::features::workspace::{WorkspaceCommand, WorkspaceFocus, WorkspaceInputResult};
use crate::tui::foundation::status::StatusLevel;
use std::time::Duration;

impl App {
    /// Request a runtime refresh without waiting on host I/O.
    ///
    /// The runtime observer owns the snapshot query and publishes the result
    /// back to the main loop. Keeping this call synchronous prevents a slow
    /// D-Bus or systemd tools backend from blocking keyboard and mouse dispatch.
    pub fn refresh(&mut self) {
        if self.ui.overlays.wizard.is_some()
            || self.ui.overlays.help.is_some()
            || self.ui.overlays.resource_action_menu.is_some()
        {
            return;
        }
        self.services.runtime_catalog.invalidate();
    }

    pub(crate) fn focused_image_resource(&self) -> Option<&ImageEntry> {
        self.workspace.focused_image()
    }

    pub(crate) fn focused_machine_resource(&self) -> Option<&MachineEntry> {
        self.workspace.focused_machine()
    }

    /// Set focus while keeping the last non-terminal destination available
    /// for restoring focus when the terminal is closed or hidden.
    pub(crate) fn set_focus(&mut self, focus: WorkspaceFocus) {
        let machine = self
            .terminal
            .active_session()
            .map(|session| session.machine_name.as_str());
        if self.workspace.set_focus(focus, machine) && self.ui.leader_active() {
            self.ui.close_leader();
        }
    }

    /// Apply the independent machine/image snapshot returned by the backend.
    pub(in crate::tui::app) fn sync_snapshot(&mut self, snapshot: RuntimeSnapshot) {
        let lifecycle = &self.services.machine_lifecycle;
        lifecycle.observe_machines(&snapshot.machines);
        let machine = self
            .terminal
            .active_session()
            .map(|session| session.machine_name.as_str());
        self.workspace
            .replace_snapshot(snapshot, machine, |observed| {
                lifecycle.project_machines(observed)
            });
        if let Some(wizard) = &mut self.ui.overlays.wizard {
            wizard.draft.entries = self.workspace.resources.catalog.entries.clone();
            wizard.draft.images = self.workspace.resources.catalog.images.clone();
        }
    }

    fn sync_runtime_query(&mut self, query: crate::application::RuntimeQuery<RuntimeSnapshot>) {
        self.workspace.resources.catalog.dbus_active = query.route.is_dbus();
        if let Some(fallback) = query.fallback {
            self.set_status(
                format!(
                    "{} unavailable: {}; using {}",
                    fallback.from.label(),
                    fallback.reason,
                    fallback.to.label()
                ),
                crate::tui::foundation::status::StatusLevel::Warn,
            );
        }
        self.sync_snapshot(query.value);
    }

    pub(in crate::tui::app) fn prepare_workspace_frame(
        &mut self,
        detail_tx: &tokio::sync::mpsc::Sender<DetailRefreshCompletion>,
    ) {
        if self.services.machine_lifecycle.expire_transitions() {
            self.apply_machine_projection();
        }
        self.terminal.reclaim_exited_scrollback();
        self.start_detail_refresh(detail_tx);
    }

    pub(in crate::tui::app) fn apply_runtime_update(&mut self, update: RuntimeUpdate) {
        match update {
            RuntimeUpdate::Snapshot(snapshot) => self.sync_runtime_query(snapshot),
            RuntimeUpdate::BackendFailure {
                message,
                consecutive_failures,
            } => {
                log::warn!(
                    "Status observer failure #{}: {}",
                    consecutive_failures,
                    message
                );
                if consecutive_failures == 1 || consecutive_failures % 12 == 0 {
                    self.set_status_for(
                        format!("Status observer unavailable: {message}"),
                        crate::tui::foundation::status::StatusLevel::Warn,
                        Duration::from_secs(6),
                    );
                }
            }
        }
    }

    pub(in crate::tui::app) async fn apply_workspace_input_result(
        &mut self,
        result: WorkspaceInputResult,
    ) {
        if result.focus_changed && self.ui.leader_active() {
            self.ui.close_leader();
        }
        let Some(command) = result.command else {
            return;
        };
        match command {
            WorkspaceCommand::Quit => self.request_quit(),
            WorkspaceCommand::Help => self.ui.overlays.open_help(),
            WorkspaceCommand::Leader => self.ui.open_leader(),
            WorkspaceCommand::Start => self.action_start(),
            WorkspaceCommand::Poweroff => self.action_poweroff(),
            WorkspaceCommand::ResourceActions => self.open_resource_actions(),
            WorkspaceCommand::Wizard => self.begin_wizard(),
            WorkspaceCommand::Refresh => self.refresh(),
            WorkspaceCommand::Shell => self.spawn_shell_prompt().await,
            WorkspaceCommand::RemoveImage => self.show_delete_dialog(),
            WorkspaceCommand::TerminalInputRejected(status) => match status {
                crate::tui::features::workspace::terminal_panel::TerminalInputStatus::Queued => {}
                crate::tui::features::workspace::terminal_panel::TerminalInputStatus::Full => self
                    .set_status(
                        "Terminal input queue is full; input was dropped.".into(),
                        StatusLevel::Warn,
                    ),
                crate::tui::features::workspace::terminal_panel::TerminalInputStatus::Closed => {
                    self.set_status(
                        "Terminal input channel is closed.".into(),
                        StatusLevel::Error,
                    )
                }
            },
        }
    }
}
