use super::{ActivePanel, App};
use crate::ui::StatusLevel;
use crate::ui::core::{AppMessage, Component, ContainerMessage, EventResult, ListMessage};

use crate::ui::wizard::StepAction as WizardAction;
use crate::ui::wizard::Wizard;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

impl App {
    pub async fn handle_key(&mut self, key: KeyEvent) {
        // ── Overlay: Quit Confirmation ────────────────────────────────────────
        if self.ui.quit_dialog.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    self.cleanup_all_terminals();
                    self.should_quit = true;
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.ui.quit_dialog = None;
                }
                _ => {}
            }
            return;
        }

        // ── Terminal Panel (Priority) ─────────────────────────────────────────
        if self.ui.active_panel == ActivePanel::TerminalPanel {
            if let Some(session) = self.data.terminal_sessions.get_mut(self.data.active_terminal_idx) {
                // Tab switching: Ctrl+Number, Alt+Number, or just Number (in Normal mode)
                let new_idx = match key.code {
                    KeyCode::Char('1') if key.modifiers.contains(KeyModifiers::ALT) => Some(0),
                    KeyCode::Char('2') if key.modifiers.contains(KeyModifiers::ALT) => Some(1),
                    KeyCode::Char('3') if key.modifiers.contains(KeyModifiers::ALT) => Some(2),
                    KeyCode::Char('4') if key.modifiers.contains(KeyModifiers::ALT) => Some(3),
                    KeyCode::Char('5') if key.modifiers.contains(KeyModifiers::ALT) => Some(4),
                    KeyCode::Char('6') if key.modifiers.contains(KeyModifiers::ALT) => Some(5),
                    KeyCode::Char('7') if key.modifiers.contains(KeyModifiers::ALT) => Some(6),
                    KeyCode::Char('8') if key.modifiers.contains(KeyModifiers::ALT) => Some(7),
                    KeyCode::Char('9') if key.modifiers.contains(KeyModifiers::ALT) => Some(8),
                    _ => None,
                };

                if let Some(idx) = new_idx {
                    if idx < self.data.terminal_sessions.len() {
                        self.data.active_terminal_idx = idx;
                        let name = self.data.terminal_sessions[idx].container_name.clone();
                        if let Some(pos) = self.data.entries.iter().position(|e| e.name == name) {
                            self.data.selected = pos;
                            self.refresh_detail().await;
                        }
                    }
                    return;
                }

                if session.insert_mode {
                    let is_toggle = key.code == KeyCode::Char('x') && key.modifiers.contains(KeyModifiers::ALT);
                    
                    if is_toggle {
                        session.insert_mode = false;
                        return;
                    }
                    
                    // Forward other keys to PTY
                    let bytes = crate::ui::views::terminal_panel::encode_key(key);
                    let _ = session.pty_tx.try_send(crate::nspawn::adapters::comm::pty::PtyMessage::Data(bytes));
                    return;
                } else {
                    // Normal Mode keys - Handle locally and RETURN to prevent pollution
                    let normal_idx = match key.code {
                        KeyCode::Enter | KeyCode::Char('i') => {
                            session.insert_mode = true;
                            session.scroll_offset = 0;
                            return;
                        }
                        KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::ALT) => {
                            session.insert_mode = true;
                            session.scroll_offset = 0;
                            return;
                        }
                        KeyCode::Char('x') => {
                            self.close_active_terminal();
                            return;
                        }
                        KeyCode::PageUp => {
                            let mut screen = session.terminal.lock().screen().clone();
                            screen.set_scrollback(usize::MAX);
                            let max_scroll = screen.scrollback();
                            session.scroll_offset = session.scroll_offset.saturating_add(10).min(max_scroll);
                            return;
                        }
                        KeyCode::PageDown => {
                            session.scroll_offset = session.scroll_offset.saturating_sub(10);
                            return;
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            let mut screen = session.terminal.lock().screen().clone();
                            screen.set_scrollback(usize::MAX);
                            let max_scroll = screen.scrollback();
                            session.scroll_offset = session.scroll_offset.saturating_add(1).min(max_scroll);
                            return;
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            session.scroll_offset = session.scroll_offset.saturating_sub(1);
                            return;
                        }
                        KeyCode::Char('1') => Some(0),
                        KeyCode::Char('2') => Some(1),
                        KeyCode::Char('3') => Some(2),
                        KeyCode::Char('4') => Some(3),
                        KeyCode::Char('5') => Some(4),
                        KeyCode::Char('6') => Some(5),
                        KeyCode::Char('7') => Some(6),
                        KeyCode::Char('8') => Some(7),
                        KeyCode::Char('9') => Some(8),
                        KeyCode::Tab | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Char('t') => None,
                        _ => return, // Consume other keys in Normal Mode
                    };

                    if let Some(idx) = normal_idx {
                        if idx < self.data.terminal_sessions.len() {
                            self.data.active_terminal_idx = idx;
                            let name = self.data.terminal_sessions[idx].container_name.clone();
                            if let Some(pos) = self.data.entries.iter().position(|e| e.name == name) {
                                self.data.selected = pos;
                                self.refresh_detail().await;
                            }
                        }
                        return;
                    }
                }
            }
        }

        // ── Overlay: wizard ───────────────────────────────────────────────────
        if self.ui.show_wizard {
            if let Some(wizard) = &mut self.ui.wizard {
                match wizard.handle_key(key) {
                    WizardAction::None => {}
                    WizardAction::Close => {
                        self.ui.show_wizard = false;
                        self.ui.wizard = None;
                    }
                    WizardAction::CloseRefresh => {
                        self.ui.show_wizard = false;
                        self.ui.wizard = None;
                        self.refresh().await;
                    }
                    WizardAction::Status(msg, level) => {
                        self.set_status(msg, level);
                    }
                    WizardAction::Next | WizardAction::Prev => {}
                }
            } else {
                self.ui.show_wizard = false;
            }
            return;
        }

        // ── Overlay: help ─────────────────────────────────────────────────────
        if self.ui.show_help {
            self.ui.show_help = false;
            return;
        }

        // ── Overlay: power menu ───────────────────────────────────────────────
        if let Some(pm) = &mut self.ui.power_menu {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.ui.power_menu = None,
                KeyCode::Enter => {
                    let idx = pm.get_selected();
                    self.ui.power_menu = None;
                    match idx {
                        0 => self.action_start(),
                        1 => self.action_poweroff(),
                        2 => self.action_reboot(),
                        3 => self.action_terminate(),
                        4 => self.action_kill(),
                        5 => self.action_enable(),
                        6 => self.action_disable(),
                        _ => {}
                    }
                }
                _ => {
                    let _ = pm.handle_key(key);
                }
            }
            return;
        }

        // Update pane_height into the detail panel before processing keys.
        self.ui.detail_panel.pane_height = self.ui.pane_height;

        // ── Global keys ───────────────────────────────────────────────────────
        match key.code {
            KeyCode::Char('q') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if !self.data.terminal_sessions.is_empty() {
                    self.ui.quit_dialog = Some(crate::ui::widgets::confirmation::ConfirmationDialog::new(
                        "Quit Lasper?",
                        "Active terminal sessions are still running.\nQuit and terminate all logins?",
                    ));
                } else {
                    self.should_quit = true;
                }
                return;
            }
            KeyCode::Char('?') => {
                self.ui.show_help = true;
                return;
            }
            // Focus toggle
            KeyCode::Tab => {
                self.ui.toggle_focus();
                return;
            }
            // Container actions (always global regardless of focus)
            KeyCode::Char('s') => {
                self.action_start();
                return;
            }
            KeyCode::Char('S') => {
                self.action_poweroff();
                return;
            }
            KeyCode::Char('x') | KeyCode::Enter if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if !self.data.entries.is_empty() {
                    self.ui.power_menu = Some(crate::ui::widgets::power_menu::PowerMenu::new(0));
                }
                return;
            }
            KeyCode::Char('n') | KeyCode::Char('a') => {
                if self.is_root {
                    let nvidia_installed = std::path::Path::new("/usr/bin/nvidia-ctk").exists();
                    if let Some(tx) = &self.ui.backend_tx {
                        self.ui.wizard = Some(Wizard::new(
                            self.data.entries.clone(),
                            nvidia_installed,
                            tx.clone(),
                        ).await);
                        self.ui.show_wizard = true;
                    }
                } else {
                    self.set_status(
                        "Root required — run: sudo lasper".into(),
                        StatusLevel::Error,
                    );
                }
                return;
            }
            KeyCode::Char('r') => {
                self.refresh().await;
                return;
            }
            KeyCode::Char('t') => {
                if self.ui.show_terminal {
                    self.ui.show_terminal = false;
                    self.ui.active_panel = ActivePanel::ContainerList;
                } else {
                    self.spawn_terminal();
                }
                return;
            }
            _ => {}
        }

        // ── Route to focused panel ────────────────────────────────────────────
        match self.ui.active_panel {
            ActivePanel::ContainerList => {
                let result = self.ui.container_list.handle_key(key);
                self.handle_container_list_result(result).await;
            }
            ActivePanel::DetailPanel => {
                let result = self.ui.detail_panel.handle_key(key);
                self.handle_detail_panel_result(result).await;
            }
            ActivePanel::TerminalPanel => {}
        }
    }

    async fn handle_container_list_result(&mut self, result: EventResult) {
        match result {
            EventResult::Message(AppMessage::List(ListMessage::Next)) => {
                self.select_next();
                self.sync_terminal_to_selected();
                self.refresh_detail().await;
            }
            EventResult::Message(AppMessage::List(ListMessage::Prev)) => {
                self.select_prev();
                self.sync_terminal_to_selected();
                self.refresh_detail().await;
            }
            _ => {}
        }
    }

    /// Processes an `EventResult` produced by the detail panel.
    async fn handle_detail_panel_result(&mut self, result: EventResult) {
        match result {
            EventResult::Message(AppMessage::Container(ContainerMessage::PaneChanged(_pane))) => {
                // The active_pane is already updated inside the component.
                // Just trigger a data refresh for the new pane.
                self.refresh_detail().await;
            }
            EventResult::Consumed | EventResult::Ignored => {}
            _ => {}
        }
    }
}
