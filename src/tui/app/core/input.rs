//! Common input priority, footer shortcuts and quit confirmation.
//!
//! Input is delivered to the foreground component, which returns a semantic
//! command. Feature-specific consequences are applied by App integrations.

use crate::tui::app::App;
use crate::tui::foundation::input::UiInput;
use crossterm::event::{KeyEvent, MouseEvent};

fn quit_confirmation_message(terminal_sessions: usize, host_operations: usize) -> Option<String> {
    if terminal_sessions == 0 && host_operations == 0 {
        return None;
    }

    let mut warnings = Vec::new();
    if terminal_sessions > 0 {
        warnings.push(format!(
            "{terminal_sessions} active terminal session{} will be terminated.",
            if terminal_sessions == 1 { "" } else { "s" }
        ));
    }
    if host_operations > 0 {
        warnings.push(format!(
            "{host_operations} host operation{} still running. Quitting now may interrupt {} and leave partial host changes.",
            if host_operations == 1 { " is" } else { "s are" },
            if host_operations == 1 { "it" } else { "them" }
        ));
    }
    warnings.push("Quit anyway?".to_string());
    Some(warnings.join("\n"))
}

impl App {
    pub async fn handle_key(&mut self, key: KeyEvent) {
        self.handle_ui_input(UiInput::Key(key)).await;
    }

    pub async fn handle_mouse(&mut self, mouse: MouseEvent) {
        self.handle_ui_input(UiInput::Mouse(mouse)).await;
    }

    /// Both input sources follow the same priority: the visible modal layer,
    /// then the workspace. Views interpret input; App applies their commands.
    async fn handle_ui_input(&mut self, input: UiInput) {
        let input = if let UiInput::Mouse(mouse) = input {
            match self.ui.footer_hits.handle_mouse(mouse) {
                Some(crate::tui::foundation::footer::FooterPointerInput::Key(key)) => {
                    UiInput::Key(key)
                }
                Some(crate::tui::foundation::footer::FooterPointerInput::Consumed) => return,
                None => input,
            }
        } else {
            input
        };
        if let Some(command) = self.ui.overlays.handle_input(input) {
            self.apply_overlay_command(command).await;
            return;
        }
        let result = self.workspace.handle_input(input, &mut self.terminal);
        self.apply_workspace_input_result(result).await;
    }
}

// Application-level commands

impl App {
    pub(in crate::tui::app) fn request_quit(&mut self) {
        let message = quit_confirmation_message(
            self.terminal.sessions.len(),
            self.services.host_operations.active_count(),
        );
        if let Some(message) = message {
            self.ui.overlays.quit_dialog = Some(
                crate::tui::foundation::components::confirmation::ConfirmationDialog::new(
                    "Quit Lasper?",
                    message,
                ),
            );
        } else {
            self.signal_quit();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::quit_confirmation_message;
    #[test]
    fn quit_without_live_resources_needs_no_confirmation() {
        assert_eq!(quit_confirmation_message(0, 0), None);
    }
    #[test]
    fn quit_warning_combines_terminals_and_host_operations() {
        let message = quit_confirmation_message(2, 1).unwrap();
        assert!(message.contains("2 active terminal sessions will be terminated."));
        assert!(message.contains("1 host operation is still running."));
        assert!(message.contains("leave partial host changes"));
        assert!(message.ends_with("Quit anyway?"));
    }
}
