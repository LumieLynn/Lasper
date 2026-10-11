//! Resolve the workspace target and apply terminal launch effects.

use crate::domain::machine::MachineName;
use crate::tui::app::App;

impl App {
    pub(crate) async fn spawn_terminal_as_user(
        &mut self,
        machine: MachineName,
        user: crate::application::sessions::ValidatedGuestUserName,
        wayland: crate::application::sessions::WaylandShellRequest,
    ) -> bool {
        let Some(entry) = self.workspace.resources.machine(machine.as_str()).cloned() else {
            self.set_status(
                format!("{} is no longer running.", machine),
                crate::tui::foundation::status::StatusLevel::Warn,
            );
            return false;
        };
        let rows = self.workspace.terminal_rows();
        match self
            .terminal
            .spawn_as_user(&entry, user.clone(), wayland, rows, &self.ui.app_tx)
            .await
        {
            Ok(_session) => {
                self.set_focus(crate::tui::features::workspace::WorkspaceFocus::Terminal);
                self.request_detail_refresh();
                self.set_status(
                    format!("Opened {}@{}", user, machine),
                    crate::tui::foundation::status::StatusLevel::Info,
                );
                true
            }
            Err(message) => {
                self.set_status(message, crate::tui::foundation::status::StatusLevel::Error);
                false
            }
        }
    }

    fn terminal_entry_for_focus(&mut self) -> Option<crate::domain::runtime::MachineEntry> {
        match self.workspace.terminal_target() {
            Ok(entry) => entry,
            Err(message) => {
                self.set_status(message, crate::tui::foundation::status::StatusLevel::Info);
                None
            }
        }
    }

    pub async fn spawn_shell_prompt(&mut self) {
        let Some(entry) = self.terminal_entry_for_focus() else {
            return;
        };
        let rows = self.workspace.terminal_rows();
        match self
            .terminal
            .spawn_shell_prompt(&entry, rows, &self.ui.app_tx)
            .await
        {
            Ok(_) => {
                self.set_focus(crate::tui::features::workspace::WorkspaceFocus::Terminal);
                self.request_detail_refresh();
                self.set_status(
                    format!("Opened shell prompt for {}", entry.name),
                    crate::tui::foundation::status::StatusLevel::Info,
                );
            }
            Err(msg) => self.set_status(msg, crate::tui::foundation::status::StatusLevel::Error),
        }
    }

    pub async fn spawn_x11_shell_prompt(&mut self) {
        let Some(entry) = self.terminal_entry_for_focus() else {
            return;
        };
        let rows = self.workspace.terminal_rows();
        let x11_access = std::sync::Arc::clone(&self.services.x11_access);
        match self
            .terminal
            .spawn_x11_shell_prompt(&entry, rows, &self.ui.app_tx, x11_access)
            .await
        {
            Ok(_) => {
                self.set_focus(crate::tui::features::workspace::WorkspaceFocus::Terminal);
                self.request_detail_refresh();
                self.set_status(
                    format!("Opened Host X11 shell prompt for {}", entry.name),
                    crate::tui::foundation::status::StatusLevel::Info,
                );
            }
            Err(msg) => self.set_status(msg, crate::tui::foundation::status::StatusLevel::Error),
        }
    }

    pub async fn spawn_terminal(&mut self) {
        let Some(entry) = self.terminal_entry_for_focus() else {
            return;
        };
        let rows = self.workspace.terminal_rows();

        match self.terminal.spawn(&entry, rows, &self.ui.app_tx).await {
            Ok(session) => {
                self.set_focus(crate::tui::features::workspace::WorkspaceFocus::Terminal);
                self.request_detail_refresh();
                let message = match session.attach_kind {
                    crate::domain::session::TerminalAttachmentKind::Login => {
                        format!("Logged into {}", entry.name)
                    }
                    crate::domain::session::TerminalAttachmentKind::Namespace => {
                        format!("Attached to {} through its namespaces", entry.name)
                    }
                };
                self.set_status(message, crate::tui::foundation::status::StatusLevel::Info);
            }
            Err(msg) => {
                self.set_status(msg, crate::tui::foundation::status::StatusLevel::Error);
            }
        }
    }
}
