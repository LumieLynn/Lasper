use super::{ActivePanel, App};
use crate::nspawn::StatusLevel;
use crate::ui::core::{AppMessage, Component, ContainerMessage, EventResult, ListMessage};

use crate::ui::wizard::StepAction as WizardAction;
use crate::ui::wizard::Wizard;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

impl App {
    pub async fn handle_key(&mut self, key: KeyEvent) {
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
                self.should_quit = true;
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
            KeyCode::Char('x') | KeyCode::Enter => {
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
        }
    }

    async fn handle_container_list_result(&mut self, result: EventResult) {
        match result {
            EventResult::Message(AppMessage::List(ListMessage::Next)) => {
                self.select_next();
                self.refresh_detail().await;
            }
            EventResult::Message(AppMessage::List(ListMessage::Prev)) => {
                self.select_prev();
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
