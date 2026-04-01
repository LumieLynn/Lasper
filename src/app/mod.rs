//! Main application state and event loop.

pub mod actions;
pub mod handlers;

use std::collections::HashMap;
use std::time::Instant;
use anyhow::Result;
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

pub struct AppUi {
    pub detail_pane: DetailPane,
    pub details_state: TableState,
    pub log_scroll: u16,
    pub config_scroll: u16,
    pub show_wizard: bool,
    pub show_help: bool,
    pub show_power_menu: bool,
    pub power_menu_selected: usize,
    pub pane_height: u16,

    pub wizard: Option<Wizard>,

    pub status_message: Option<(String, StatusLevel)>,
    pub status_expiry: Option<Instant>,
    pub backend_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::ui::core::BackendCommand>>,
}

impl AppUi {
    pub fn new(_is_root: bool) -> Self {
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
            wizard: None,
            status_message: None,
            status_expiry: None,
            backend_tx: None,
        }
    }
}

// ── App ───────────────────────────────────────────────────────────────────────

pub struct AppData {
    pub entries: Vec<ContainerEntry>,
    pub selected: usize,
    pub properties: Result<HashMap<String, String>, String>,
    pub log_lines: Vec<String>,
    pub config_content: Option<String>,
    pub dbus_active: bool,
    pub manager: std::sync::Arc<dyn NspawnManager>,
    pub action_cooldown: Option<Instant>,
}

/// Global application state.
pub struct App {
    pub is_root: bool,
    pub should_quit: bool,
    pub data: AppData,
    pub ui: AppUi,
}

impl App {
    pub fn new(is_root: bool) -> Self {
        Self {
            is_root,
            should_quit: false,
            data: AppData {
                entries: Vec::new(),
                selected: 0,
                properties: Ok(HashMap::new()),
                log_lines: Vec::new(),
                config_content: None,
                dbus_active: true,
                manager: std::sync::Arc::new(DefaultManager::new(is_root)),
                action_cooldown: None,
            },
            ui: AppUi::new(is_root),
        }
    }

    /// Starts the main application loop.
    pub async fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        let mut events = EventHandler::new(100);
        let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::channel::<Vec<ContainerEntry>>(1);
        let (backend_tx, mut backend_rx) = tokio::sync::mpsc::unbounded_channel::<crate::ui::core::BackendCommand>();
        self.ui.backend_tx = Some(backend_tx);
        
        let manager_clone = self.data.manager.clone();
        tokio::spawn(async move {
            loop {
                if let Ok(entries) = manager_clone.list_all().await {
                    let _ = refresh_tx.send(entries).await;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });

        self.refresh().await;
        loop {
            while let Ok(entries) = refresh_rx.try_recv() {
                let prev_name = self.data.entries.get(self.data.selected).map(|e| e.name.clone());
                self.data.entries = entries;
                self.data.selected = prev_name
                    .and_then(|name| self.data.entries.iter().position(|e| e.name == name))
                    .unwrap_or(0)
                    .min(self.data.entries.len().saturating_sub(1));
                self.refresh_detail().await;
            }

            terminal.draw(|f| crate::ui::draw(f, self))?;
            
            tokio::select! {
                Some(event) = events.rx.recv() => {
                    match event {
                        AppEvent::Key(key) => self.handle_key(key).await,
                        AppEvent::Tick => self.tick().await,
                        AppEvent::BackendResult(res) => {
                            if let Some(wizard) = &mut self.ui.wizard {
                                wizard.process_message(crate::ui::core::AppMessage::BackendResult(res));
                            }
                        }
                    }
                }
                Some(cmd) = backend_rx.recv() => {
                    let tx = events.tx.clone();
                    tokio::spawn(async move {
                        match cmd {
                            crate::ui::core::BackendCommand::SubmitConfig(ctx) => {
                                let built = ctx.build_config();
                                let (deployer, storage) = ctx.get_deployer_and_storage();
                                let name = built.cfg.name.clone();
                                let cfg = built.cfg;

                                // Bridge mpsc (Deployer API) → broadcast (DeployStepView)
                                let (log_mpsc_tx, mut log_mpsc_rx) =
                                    tokio::sync::mpsc::unbounded_channel::<String>();
                                let log_bcast_tx = ctx.deploy.log_tx.clone();
                                tokio::spawn(async move {
                                    while let Some(msg) = log_mpsc_rx.recv().await {
                                        let _ = log_bcast_tx.send(msg);
                                    }
                                });

                                let done = ctx.deploy.done.clone();
                                let success = ctx.deploy.success.clone();

                                // Run the real deployment in its own task
                                tokio::spawn(async move {
                                    crate::nspawn::deploy::run_deploy_task(
                                        deployer, storage, name, cfg, log_mpsc_tx, done, success,
                                    )
                                    .await;
                                });

                                // Immediately navigate the wizard to the Deploy step
                                let _ = tx.send(AppEvent::BackendResult(
                                    crate::ui::core::BackendResponse::DeployStarted,
                                )).await;
                            }
                            crate::ui::core::BackendCommand::ValidateBridge(_) => {
                                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                                let _ = tx.send(AppEvent::BackendResult(crate::ui::core::BackendResponse::ValidationSuccess)).await;
                            }
                        }
                    });
                }
                else => break,
            }
            if self.should_quit { events.stop(); break; }
        }
        Ok(())
    }

    // ── Tick (auto-refresh + status expiry) ───────────────────────────────────

    async fn tick(&mut self) {
        // Expire status message
        if let Some(exp) = self.ui.status_expiry {
            if Instant::now() >= exp {
                self.ui.status_message = None;
                self.ui.status_expiry = None;
            }
        }
    }
}
