use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crate::ui::wizard::StepAction as WizardAction;
use crate::ui::wizard::Wizard;
use crate::nspawn::StatusLevel;
use super::{App, DetailPane};

impl App {
    pub async fn handle_key(&mut self, key: KeyEvent) {
        if self.ui.show_wizard {
            match self.ui.wizard.handle_key(key, &self.data.entries, self.is_root).await {
                WizardAction::None => {}
                WizardAction::Close => { self.ui.show_wizard = false; }
                WizardAction::CloseRefresh => {
                    self.ui.show_wizard = false;
                    self.refresh().await;
                }
                WizardAction::Status(msg, level) => {
                    self.set_status(msg, level);
                }
                WizardAction::Next | WizardAction::Prev => {}
            }
            return;
        }
        if self.ui.show_help { self.ui.show_help = false; return; }
        if self.ui.show_power_menu {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.ui.show_power_menu = false,
                KeyCode::Up | KeyCode::Char('k') => {
                    self.ui.power_menu_selected = self.ui.power_menu_selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.ui.power_menu_selected = (self.ui.power_menu_selected + 1).min(6);
                }
                KeyCode::Enter => {
                    let idx = self.ui.power_menu_selected;
                    self.ui.show_power_menu = false;
                    match idx {
                        0 => self.action_start().await,
                        1 => self.action_poweroff().await,
                        2 => self.action_reboot().await,
                        3 => self.action_terminate().await,
                        4 => self.action_kill().await,
                        5 => self.action_enable().await,
                        6 => self.action_disable().await,
                        _ => {}
                    }
                }
                _ => {}
            }
            return;
        }

        let step = (self.ui.pane_height / 2).max(1);

        match key.code {
            KeyCode::Char('q') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            // Detail pane scrolling
            KeyCode::Up   if self.ui.detail_pane == DetailPane::Config => { self.ui.config_scroll = self.ui.config_scroll.saturating_sub(1); }
            KeyCode::Down if self.ui.detail_pane == DetailPane::Config => { self.ui.config_scroll += 1; }
            KeyCode::PageUp   if self.ui.detail_pane == DetailPane::Config => { self.ui.config_scroll = self.ui.config_scroll.saturating_sub(step); }
            KeyCode::PageDown if self.ui.detail_pane == DetailPane::Config => { self.ui.config_scroll += step; }
            KeyCode::Up   if self.ui.detail_pane == DetailPane::Logs => { self.ui.log_scroll = self.ui.log_scroll.saturating_sub(1); }
            KeyCode::Down if self.ui.detail_pane == DetailPane::Logs => { self.ui.log_scroll += 1; }
            KeyCode::PageUp   if self.ui.detail_pane == DetailPane::Logs => { self.ui.log_scroll = self.ui.log_scroll.saturating_sub(step); }
            KeyCode::PageDown if self.ui.detail_pane == DetailPane::Logs => { self.ui.log_scroll += step; }

            // Details tab scrolling
            KeyCode::Up if self.ui.detail_pane == DetailPane::Details => {
                let i = match self.ui.details_state.selected() {
                    Some(i) => if i == 0 { 0 } else { i - 1 },
                    None => 0,
                };
                self.ui.details_state.select(Some(i));
            }
            KeyCode::Down if self.ui.detail_pane == DetailPane::Details => {
                let len = self.data.properties.as_ref().map(|p| p.len()).unwrap_or(0);
                let i = match self.ui.details_state.selected() {
                    Some(i) => (i + 1).min(len.saturating_sub(1)),
                    None => 0,
                };
                self.ui.details_state.select(Some(i));
            }
            KeyCode::PageUp if self.ui.detail_pane == DetailPane::Details => {
                let i = match self.ui.details_state.selected() {
                    Some(i) => i.saturating_sub(step as usize),
                    None => 0,
                };
                self.ui.details_state.select(Some(i));
            }
            KeyCode::PageDown if self.ui.detail_pane == DetailPane::Details => {
                let len = self.data.properties.as_ref().map(|p| p.len()).unwrap_or(0);
                let i = match self.ui.details_state.selected() {
                    Some(i) => (i + step as usize).min(len.saturating_sub(1)),
                    None => 0,
                };
                self.ui.details_state.select(Some(i));
            }
            // General navigation
            KeyCode::Char('j') | KeyCode::Down => self.select_next(),
            KeyCode::Char('k') | KeyCode::Up   => self.select_prev(),
            KeyCode::Char('p') => { self.ui.detail_pane = DetailPane::Properties; self.refresh_detail().await; }
            KeyCode::Char('d') => {
                self.ui.detail_pane = DetailPane::Details;
                self.ui.details_state.select(Some(0));
                self.refresh_detail().await;
            }
            KeyCode::Char('l') => { self.ui.detail_pane = DetailPane::Logs; self.ui.log_scroll = 0; self.refresh_detail().await; }
            KeyCode::Char('c') => { self.ui.detail_pane = DetailPane::Config; self.ui.config_scroll = 0; self.refresh_detail().await; }
            KeyCode::Char('s') => self.action_start().await,
            KeyCode::Char('S') => self.action_poweroff().await,
            KeyCode::Char('x') | KeyCode::Enter => {
                if !self.data.entries.is_empty() {
                    self.ui.show_power_menu = true;
                    self.ui.power_menu_selected = 0;
                }
            }
            KeyCode::Char('n') | KeyCode::Char('a') => {
                if self.is_root {
                    self.ui.wizard = Wizard::new(self.is_root);
                    self.ui.show_wizard = true;
                } else {
                    self.set_status("Root required — run: sudo lasper".into(), StatusLevel::Error);
                }
            }
            KeyCode::Char('r') => self.refresh().await,
            KeyCode::Char('?') => self.ui.show_help = true,
            _ => {}
        }
    }
}
