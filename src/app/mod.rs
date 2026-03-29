//! Main application state and event loop.

pub mod actions;
pub mod handlers;

use std::collections::HashMap;
use std::time::{Duration, Instant};
use anyhow::Result;
use crossterm::event::KeyEvent;
use ratatui::{backend::CrosstermBackend, Terminal, widgets::TableState};
use std::io::Stdout;

use crate::events::{AppEvent, EventHandler};
use crate::nspawn::{
    StatusLevel,
    manager::{NspawnManager, DefaultManager},
    models::ContainerEntry,
};
use crate::ui::wizard::Wizard;

// ── Simple enums ──────────────────────────────────────────────────────────────

/// The currently active detail pane in the main UI.
#[derive(Debug, Clone, PartialEq)]
pub enum DetailPane { Properties, Details, Logs, Config }

pub struct UiState {
    pub detail_pane: DetailPane,
    pub details_state: TableState,
    pub log_scroll: u16,
    pub config_scroll: u16,
    pub show_wizard: bool,
    pub show_help: bool,
    pub show_power_menu: bool,
    pub power_menu_selected: usize,
    pub pane_height: u16,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            detail_pane: DetailPane::Properties,
            details_state: TableState::default(),
            log_scroll: 0,
            config_scroll: 0,
            show_wizard: false,
            show_help: false,
            show_power_menu: false,
            power_menu_selected: 0,
            pane_height: 10,
        }
    }
}

// ── App ───────────────────────────────────────────────────────────────────────

/// Global application state.
pub struct App {
    pub is_root: bool,
    pub should_quit: bool,

    pub entries: Vec<ContainerEntry>,
    pub selected: usize,

    pub properties: Result<HashMap<String, String>, String>,
    pub log_lines: Vec<String>,
    pub config_content: Option<String>,

    pub wizard: Wizard,

    pub status_message: Option<(String, StatusLevel)>,
    pub status_expiry: Option<Instant>,

    pub dbus_active: bool,
    pub manager: std::sync::Arc<dyn NspawnManager>,

    pub ui: UiState,
    pub action_cooldown: Option<Instant>,
}

impl App {
    pub fn new(is_root: bool) -> Self {
        Self {
            is_root,
            should_quit: false,
            entries: Vec::new(),
            selected: 0,
            properties: Ok(HashMap::new()),
            log_lines: Vec::new(),
            config_content: None,
            wizard: Wizard::new(is_root),
            status_message: None,
            status_expiry: None,
            dbus_active: true,
            manager: std::sync::Arc::new(DefaultManager::new(is_root)),
            ui: UiState::default(),
            action_cooldown: None,
        }
    }

    /// Starts the main application loop.
    pub async fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        let mut events = EventHandler::new(100);
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let manager_clone = self.manager.clone();
        
        tokio::spawn(async move {
            loop {
                if let Ok(entries) = manager_clone.list_all().await {
                    let _ = tx.send(entries).await;
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            }
        });

        self.refresh().await;
        loop {
            while let Ok(entries) = rx.try_recv() {
                if let Some(time) = self.action_cooldown {
                    if Instant::now().duration_since(time) < Duration::from_secs(2) {
                        continue;
                    } else {
                        self.action_cooldown = None;
                    }
                }
                let prev_name = self.entries.get(self.selected).map(|e| e.name.clone());
                self.entries = entries;
                self.selected = prev_name
                    .and_then(|name| self.entries.iter().position(|e| e.name == name))
                    .unwrap_or(0)
                    .min(self.entries.len().saturating_sub(1));
                self.refresh_detail().await;
            }

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
    }
}
