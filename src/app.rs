//! Main application state and event loop.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::CrosstermBackend, Terminal, widgets::TableState};
use std::io::Stdout;

use crate::events::{AppEvent, EventHandler};
use crate::nspawn::{
    StatusLevel,
    manager::NspawnManager,
    machinectl::{ContainerEntry, SystemdManager},
};
use crate::ui::wizard::{Wizard, StepAction as WizardAction};

// ── Simple enums ──────────────────────────────────────────────────────────────

/// The currently active detail pane in the main UI.
#[derive(Debug, Clone, PartialEq)]
pub enum DetailPane { Properties, Details, Logs, Config }

// ── App ───────────────────────────────────────────────────────────────────────

/// Global application state.
pub struct App {
    pub is_root: bool,
    pub should_quit: bool,

    pub entries: Vec<ContainerEntry>,
    pub selected: usize,

    pub detail_pane: DetailPane,
    pub properties: HashMap<String, String>,
    pub details_state: TableState,
    pub log_lines: Vec<String>,
    pub log_scroll: u16,        // lines scrolled in Logs pane
    pub config_content: Option<String>,
    pub config_scroll: u16,     // lines scrolled in Config pane

    pub show_wizard: bool,
    pub wizard: Wizard,

    pub status_message: Option<(String, StatusLevel)>,
    pub status_expiry: Option<Instant>,

    pub show_help: bool,
    pub show_power_menu: bool,
    pub power_menu_selected: usize,
    pub dbus_active: bool,
    pub manager: Box<dyn NspawnManager>,
}

impl App {
    pub fn new(is_root: bool) -> Self {
        Self {
            is_root,
            should_quit: false,
            entries: Vec::new(),
            selected: 0,
            detail_pane: DetailPane::Properties,
            properties: HashMap::new(),
            details_state: TableState::default(),
            log_lines: Vec::new(),
            log_scroll: 0,
            config_content: None,
            config_scroll: 0,
            show_wizard: false,
            wizard: Wizard::new(is_root),
            status_message: None,
            status_expiry: None,
            show_help: false,
            show_power_menu: false,
            power_menu_selected: 0,
            dbus_active: true, // Default to true, will be updated on refresh
            manager: Box::new(SystemdManager::new(is_root)),
        }
    }

    /// Starts the main application loop.
    pub async fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        let mut events = EventHandler::new(100);
        self.refresh().await;
        loop {
            terminal.draw(|f| crate::ui::draw(f, self))?;
            match events.rx.recv().await {
                Some(AppEvent::Key(key)) => self.handle_key(key).await,
                Some(AppEvent::Tick) => self.tick().await,
                Option::None => break,
            }
            if self.should_quit { events.stop(); break; }
        }
        Ok(())
    }

    // ── Tick (auto-refresh + status expiry) ───────────────────────────────────

    async fn tick(&mut self) {
        // Expire status message
        if let Some(exp) = self.status_expiry {
            if Instant::now() >= exp {
                self.status_message = None;
                self.status_expiry = None;
            }
        }
        self.refresh().await;
    }

    pub async fn refresh(&mut self) {
        if self.show_wizard || self.show_help || self.show_power_menu { return; }
        self.dbus_active = self.manager.is_dbus_available().await;
        match self.manager.list_all().await {
            Ok(entries) => {
                self.entries = entries;
                self.selected = self.selected.min(self.entries.len().saturating_sub(1));
            }
            Err(e) => log::error!("list_all: {}", e),
        }
        self.refresh_detail().await;

        // Check if any DBus call fell back to CLI during this refresh
        if self.dbus_active && self.manager.did_fallback() {
            self.set_status("⚡ DBus call failed — used CLI fallback".into(), StatusLevel::Warn);
        }
    }

    async fn refresh_detail(&mut self) {
        let entry: ContainerEntry = match self.entries.get(self.selected) {
            Some(e) => e.clone(),
            Option::None => {
                self.properties.clear();
                self.log_lines.clear();
                self.config_content = Option::None;
                return;
            }
        };
        match self.detail_pane {
            DetailPane::Properties | DetailPane::Details => {
                if entry.state.is_running() {
                    match self.manager.get_properties(&entry.name).await {
                        Ok(mut p) => {
                            if !entry.all_addresses.is_empty() {
                                p.insert("IPAddresses".into(), entry.all_addresses.join(", "));
                            }
                            if let Some(ufs) = p.get("UnitFileState") {
                                p.insert("Enabled".into(), ufs.clone());
                            }
                            // Preserve storage type as "Type" and rename machinectl's "Type" to "Class"
                            if let Some(image_type) = &entry.image_type {
                                if let Some(machine_type) = p.remove("Type") {
                                    p.insert("Class".into(), machine_type);
                                }
                                p.insert("Type".into(), image_type.clone());
                            }
                            self.properties = p;
                        }
                        Err(e) => { self.properties.clear(); log::debug!("{e}"); }
                    }
                } else {
                    self.properties.clear();
                    if let Some(t) = &entry.image_type { self.properties.insert("Type".into(), t.clone()); }
                    self.properties.insert("ReadOnly".into(), entry.readonly.to_string());
                    if let Some(u) = &entry.usage { self.properties.insert("Usage".into(), u.clone()); }
                    self.properties.insert("State".into(), entry.state.label().into());

                    // Try to get UnitFileState for enabled status
                    if let Ok(p) = self.manager.get_properties(&entry.name).await {
                        if let Some(ufs) = p.get("UnitFileState") {
                            self.properties.insert("Enabled".into(), ufs.clone());
                        }
                    }
                }
            }
            DetailPane::Logs => {
                if entry.state.is_running() {
                    match self.manager.get_logs(&entry.name, 100).await {
                        Ok(l) => self.log_lines = l,
                        Err(e) => self.log_lines = vec![format!("Error: {e}")],
                    }
                } else {
                    self.log_lines = vec!["Container is not running.".into()];
                }
            }
            DetailPane::Config => {
                let new_content = crate::nspawn::config::NspawnConfig::load(&entry.name).map(|c| c.content);
                if self.config_content != new_content { self.config_scroll = 0; }
                self.config_content = new_content;
            }
        }
    }

    // ── Key routing ───────────────────────────────────────────────────────────

    async fn handle_key(&mut self, key: KeyEvent) {
        if self.show_wizard {
            match self.wizard.handle_key(key, &self.entries, self.is_root).await {
                WizardAction::None => {}
                WizardAction::Close => { self.show_wizard = false; }
                WizardAction::CloseRefresh => {
                    self.show_wizard = false;
                    self.refresh().await;
                }
                WizardAction::Status(msg, level) => {
                    self.set_status(msg, level);
                }
                WizardAction::Next | WizardAction::Prev => {}
            }
            return;
        }
        if self.show_help { self.show_help = false; return; }
        if self.show_power_menu {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.show_power_menu = false,
                KeyCode::Up | KeyCode::Char('k') => {
                    self.power_menu_selected = self.power_menu_selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.power_menu_selected = (self.power_menu_selected + 1).min(6);
                }
                KeyCode::Enter => {
                    let idx = self.power_menu_selected;
                    self.show_power_menu = false;
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

        match key.code {
            KeyCode::Char('q') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            // Detail pane scrolling
            KeyCode::Up   if self.detail_pane == DetailPane::Config => { self.config_scroll = self.config_scroll.saturating_sub(1); }
            KeyCode::Down if self.detail_pane == DetailPane::Config => { self.config_scroll += 1; }
            KeyCode::PageUp   if self.detail_pane == DetailPane::Config => { self.config_scroll = self.config_scroll.saturating_sub(10); }
            KeyCode::PageDown if self.detail_pane == DetailPane::Config => { self.config_scroll += 10; }
            KeyCode::Up   if self.detail_pane == DetailPane::Logs => { self.log_scroll = self.log_scroll.saturating_sub(1); }
            KeyCode::Down if self.detail_pane == DetailPane::Logs => { self.log_scroll += 1; }
            KeyCode::PageUp   if self.detail_pane == DetailPane::Logs => { self.log_scroll = self.log_scroll.saturating_sub(10); }
            KeyCode::PageDown if self.detail_pane == DetailPane::Logs => { self.log_scroll += 10; }

            // Details tab scrolling
            KeyCode::Up if self.detail_pane == DetailPane::Details => {
                let i = match self.details_state.selected() {
                    Some(i) => if i == 0 { 0 } else { i - 1 },
                    None => 0,
                };
                self.details_state.select(Some(i));
            }
            KeyCode::Down if self.detail_pane == DetailPane::Details => {
                let i = match self.details_state.selected() {
                    Some(i) => (i + 1).min(self.properties.len().saturating_sub(1)),
                    None => 0,
                };
                self.details_state.select(Some(i));
            }
            KeyCode::PageUp if self.detail_pane == DetailPane::Details => {
                let i = match self.details_state.selected() {
                    Some(i) => i.saturating_sub(10),
                    None => 0,
                };
                self.details_state.select(Some(i));
            }
            KeyCode::PageDown if self.detail_pane == DetailPane::Details => {
                let i = match self.details_state.selected() {
                    Some(i) => (i + 10).min(self.properties.len().saturating_sub(1)),
                    None => 0,
                };
                self.details_state.select(Some(i));
            }
            // General navigation
            KeyCode::Char('j') | KeyCode::Down => self.select_next(),
            KeyCode::Char('k') | KeyCode::Up   => self.select_prev(),
            KeyCode::Char('p') => { self.detail_pane = DetailPane::Properties; self.refresh_detail().await; }
            KeyCode::Char('d') => {
                self.detail_pane = DetailPane::Details;
                self.details_state.select(Some(0));
                self.refresh_detail().await;
            }
            KeyCode::Char('l') => { self.detail_pane = DetailPane::Logs; self.log_scroll = 0; self.refresh_detail().await; }
            KeyCode::Char('c') => { self.detail_pane = DetailPane::Config; self.config_scroll = 0; self.refresh_detail().await; }
            KeyCode::Char('s') => self.action_start().await,
            KeyCode::Char('S') => self.action_poweroff().await,
            KeyCode::Char('x') | KeyCode::Enter => {
                if !self.entries.is_empty() {
                    self.show_power_menu = true;
                    self.power_menu_selected = 0;
                }
            }
            KeyCode::Char('n') | KeyCode::Char('a') => {
                if self.is_root {
                    self.wizard = Wizard::new(self.is_root);
                    self.show_wizard = true;
                } else {
                    self.set_status("Root required — run: sudo lasper".into(), StatusLevel::Error);
                }
            }
            KeyCode::Char('r') => self.refresh().await,
            KeyCode::Char('?') => self.show_help = true,
            _ => {}
        }
    }

    // ── Actions ───────────────────────────────────────────────────────────────

    pub fn set_status(&mut self, msg: String, level: StatusLevel) {
        self.status_message = Some((msg, level));
        self.status_expiry = Some(Instant::now() + Duration::from_secs(4));
    }

    fn select_next(&mut self) {
        if !self.entries.is_empty() {
            self.selected = (self.selected + 1) % self.entries.len();
        }
    }

    fn select_prev(&mut self) {
        if !self.entries.is_empty() {
            self.selected = if self.selected == 0 { self.entries.len() - 1 } else { self.selected - 1 };
        }
    }

    async fn action_start(&mut self) {
        if let Some(e) = self.entries.get(self.selected) {
            if !e.state.is_running() {
                match self.manager.start(&e.name).await {
                    Ok(_) => {
                        let suffix = if self.manager.did_fallback() { " (via CLI fallback)" } else { "" };
                        self.set_status(format!("Started {}{}", e.name, suffix), StatusLevel::Success);
                    }
                    Err(err) => self.set_status(format!("Error: {err}"), StatusLevel::Error),
                }
            }
        }
    }

    async fn action_poweroff(&mut self) {
        if let Some(e) = self.entries.get(self.selected) {
            if e.state.is_running() {
                match self.manager.poweroff(&e.name).await {
                    Ok(_) => {
                        let suffix = if self.manager.did_fallback() { " (via CLI fallback)" } else { "" };
                        self.set_status(format!("Powered off {}{}", e.name, suffix), StatusLevel::Success);
                    }
                    Err(err) => self.set_status(format!("Error: {err}"), StatusLevel::Error),
                }
            }
        }
    }

    async fn action_terminate(&mut self) {
        if let Some(e) = self.entries.get(self.selected) {
            if e.state.is_running() {
                match self.manager.terminate(&e.name).await {
                    Ok(_) => {
                        let suffix = if self.manager.did_fallback() { " (via CLI fallback)" } else { "" };
                        self.set_status(format!("Terminated {}{}", e.name, suffix), StatusLevel::Success);
                    }
                    Err(err) => self.set_status(format!("Error: {err}"), StatusLevel::Error),
                }
            }
        }
    }

    async fn action_reboot(&mut self) {
        if let Some(e) = self.entries.get(self.selected) {
            if e.state.is_running() {
                match self.manager.reboot(&e.name).await {
                    Ok(_) => {
                        let suffix = if self.manager.did_fallback() { " (via CLI fallback)" } else { "" };
                        self.set_status(format!("Rebooting {}{}", e.name, suffix), StatusLevel::Success);
                    }
                    Err(err) => self.set_status(format!("Error: {err}"), StatusLevel::Error),
                }
            }
        }
    }

    async fn action_kill(&mut self) {
        if let Some(e) = self.entries.get(self.selected) {
            if e.state.is_running() {
                // For now, just send SIGTERM via kill
                match self.manager.kill(&e.name, "SIGTERM").await {
                    Ok(_) => {
                        let suffix = if self.manager.did_fallback() { " (via CLI fallback)" } else { "" };
                        self.set_status(format!("Sent SIGTERM to {}{}", e.name, suffix), StatusLevel::Success);
                    }
                    Err(err) => self.set_status(format!("Error: {err}"), StatusLevel::Error),
                }
            }
        }
    }

    async fn action_enable(&mut self) {
        if let Some(e) = self.entries.get(self.selected) {
            match self.manager.enable(&e.name).await {
                Ok(_) => self.set_status(format!("Enabled {}", e.name), StatusLevel::Success),
                Err(err) => self.set_status(format!("Error: {err}"), StatusLevel::Error),
            }
        }
    }

    async fn action_disable(&mut self) {
        if let Some(e) = self.entries.get(self.selected) {
            match self.manager.disable(&e.name).await {
                Ok(_) => self.set_status(format!("Disabled {}", e.name), StatusLevel::Success),
                Err(err) => self.set_status(format!("Error: {err}"), StatusLevel::Error),
            }
        }
    }

    pub fn selected_entry(&self) -> Option<&ContainerEntry> {
        self.entries.get(self.selected)
    }
}
