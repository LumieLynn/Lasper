//! Main application state and event loop.

pub mod actions;
pub mod handlers;

use anyhow::Result;
use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use crate::events::{AppEvent, EventHandler};
use crate::nspawn::{
    models::ContainerEntry,
    ops::{DefaultManager, NspawnManager},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::Stdout;
use crate::ui::views::container_list::ContainerListComponent;
use crate::ui::views::detail_panel::DetailPanel;
use crate::ui::wizard::Wizard;

// ── Simple enums ──────────────────────────────────────────────────────────────

/// The currently active detail pane in the main UI.
#[derive(Debug, Clone, PartialEq)]
pub enum DetailPane {
    Properties,
    Details,
    Logs,
    Config,
    Metrics,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CpuRepresentation {
    /// Aggregate usage across all cores (e.g., 230% for 2.3 cores)
    Aggregate,
    /// Normalized to total system capacity (e.g., 28% for 230% on an 8-core system)
    Normalized,
}

#[derive(Debug, Clone)]
pub struct ContainerMetrics {
    /// Time-series for CPU usage: (timestamp_offset_secs, percentage)
    pub cpu_history: Vec<(f64, f64)>,
    /// Time-series for RAM usage: (timestamp_offset_secs, megabytes)
    pub ram_history: Vec<(f64, f64)>,
}

impl Default for ContainerMetrics {
    fn default() -> Self {
        Self {
            cpu_history: Vec::with_capacity(61),
            ram_history: Vec::with_capacity(61),
        }
    }
}

/// Which top-level panel has keyboard focus.
#[derive(Debug, Clone, PartialEq)]
pub enum ActivePanel {
    ContainerList,
    DetailPanel,
    TerminalPanel,
}

pub struct AppUi {
    pub active_panel: ActivePanel,
    pub container_list: ContainerListComponent,
    pub detail_panel: DetailPanel,

    pub show_terminal: bool,
    pub show_wizard: bool,
    pub show_help: bool,
    pub power_menu: Option<crate::ui::widgets::power_menu::PowerMenu>,
    pub pane_height: u16,

    pub wizard: Option<Wizard>,

    pub status_message: Option<(String, crate::ui::StatusLevel)>,
    pub status_expiry: Option<Instant>,
    pub backend_tx: Option<tokio::sync::mpsc::Sender<crate::ui::core::BackendCommand>>,
    pub app_tx: Option<tokio::sync::mpsc::Sender<AppEvent>>,
    pub quit_dialog: Option<crate::ui::widgets::confirmation::ConfirmationDialog>,
}

impl AppUi {
    pub fn new(_is_root: bool) -> Self {
        Self {
            active_panel: ActivePanel::ContainerList,
            container_list: ContainerListComponent::new(),
            detail_panel: DetailPanel::new(),
            show_terminal: false,
            show_wizard: false,
            show_help: false,
            power_menu: None,
            pane_height: 10,
            wizard: None,
            status_message: None,
            status_expiry: None,
            backend_tx: None,
            app_tx: None,
            quit_dialog: None,
        }
    }

    pub fn toggle_focus(&mut self) {
        self.active_panel = match self.active_panel {
            ActivePanel::ContainerList => ActivePanel::DetailPanel,
            ActivePanel::DetailPanel => {
                if self.show_terminal {
                    ActivePanel::TerminalPanel
                } else {
                    ActivePanel::ContainerList
                }
            }
            ActivePanel::TerminalPanel => ActivePanel::ContainerList,
        };
    }
}

pub struct TerminalSession {
    pub container_name: String,
    pub terminal: std::sync::Arc<
        parking_lot::Mutex<
            vt100::Parser<crate::nspawn::adapters::comm::pty::PtyReply>,
        >,
    >,
    pub pty_tx: tokio::sync::mpsc::Sender<crate::nspawn::adapters::comm::pty::PtyMessage>,
    pub handle: crate::nspawn::adapters::comm::pty::TerminalHandle,
    pub scroll_offset: usize,
    pub insert_mode: bool,
}

// ── App ───────────────────────────────────────────────────────────────────────

pub struct AppData {
    pub entries: Vec<ContainerEntry>,
    pub selected: usize,
    pub properties: Result<crate::nspawn::models::MachineProperties, String>,
    pub log_lines: VecDeque<String>,
    pub log_stream: Option<(String, tokio::task::JoinHandle<()>)>,
    pub config_content: Option<String>,
    pub dbus_active: bool,
    pub manager: std::sync::Arc<dyn NspawnManager>,
    pub action_cooldown: Option<Instant>,
    pub transitions:
        std::collections::HashMap<String, (crate::nspawn::models::ContainerState, Instant)>,
    pub metrics: HashMap<String, ContainerMetrics>,
    pub cpu_cores: usize,
    pub cpu_representation: CpuRepresentation,

    // Terminal state
    pub terminal_sessions: Vec<TerminalSession>,
    pub active_terminal_idx: usize,
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
                properties: Ok(crate::nspawn::models::MachineProperties::default()),
                log_lines: VecDeque::with_capacity(5000),
                log_stream: None,
                config_content: None,
                dbus_active: true,
                manager: std::sync::Arc::new(DefaultManager::new(is_root)),
                action_cooldown: None,
                transitions: std::collections::HashMap::new(),
                metrics: HashMap::new(),
                cpu_cores: std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1),
                cpu_representation: CpuRepresentation::Aggregate,
                terminal_sessions: Vec::new(),
                active_terminal_idx: 0,
            },
            ui: AppUi::new(is_root),
        }
    }

    /// Helper to apply active transitions (Starting/Exiting) to a list of entries.
    pub fn merge_transitional_states(
        &mut self,
        mut entries: Vec<crate::nspawn::models::ContainerEntry>,
    ) -> Vec<crate::nspawn::models::ContainerEntry> {
        let now = Instant::now();
        let timeout = std::time::Duration::from_secs(10);

        // Filter out timed out or resolved transitions.
        self.data.transitions.retain(|name, (state, start_time)| {
            if now.duration_since(*start_time) > timeout {
                return false;
            }
            // If backend already matches the target, remove the transition.
            if let Some(entry) = entries.iter().find(|e| &e.name == name) {
                match state {
                    crate::nspawn::models::ContainerState::Starting => {
                        if entry.state == crate::nspawn::models::ContainerState::Running {
                            return false;
                        }
                    }
                    crate::nspawn::models::ContainerState::Exiting => {
                        if entry.state == crate::nspawn::models::ContainerState::Off {
                            return false;
                        }
                    }
                    _ => {}
                }
            }
            true
        });

        // Apply remaining transitions to the entry list.
        for entry in &mut entries {
            if let Some((trans_state, _)) = self.data.transitions.get(&entry.name) {
                entry.state = trans_state.clone();
            }
        }
        entries
    }

    /// Update entries and selection state from a background refresh.
    async fn sync_entries(&mut self, entries: Vec<ContainerEntry>) {
        let prev_name = self
            .data
            .entries
            .get(self.data.selected)
            .map(|e| e.name.clone());
        self.data.entries = self.merge_transitional_states(entries);
        let active_names: std::collections::HashSet<&String> =
            self.data.entries.iter().map(|e| &e.name).collect();
        self.data.metrics.retain(|name, _| active_names.contains(name));
        self.data.selected = prev_name
            .and_then(|name| self.data.entries.iter().position(|e| e.name == name))
            .unwrap_or(0)
            .min(self.data.entries.len().saturating_sub(1));
        self.refresh_detail().await;

        if let Some(wizard) = &mut self.ui.wizard {
            wizard.context.entries = self.data.entries.clone();
        }

        // Check if any DBus call fell back to CLI during this background refresh
        if self.data.dbus_active {
            if let Some(reason) = self.data.manager.did_fallback() {
                self.set_status(
                    format!("DBus fallback: {}", reason),
                    crate::ui::StatusLevel::Warn,
                );
            }
        }
    }

    /// Forward backend response to the active wizard/context.
    fn handle_backend_result(&mut self, res: crate::ui::core::BackendResponse) {
        if let Some(wizard) = &mut self.ui.wizard {
            let action = wizard.process_message(crate::ui::core::AppMessage::Backend(res));
            if let crate::ui::wizard::StepAction::Status(msg, level) = action {
                self.set_status(msg, level);
            }
        }
    }

    /// Update metrics history for a container.
    fn update_metrics(&mut self, name: String, time_x: f64, cpu: f64, ram: f64) {
        let metrics = self.data.metrics.entry(name).or_default();
        metrics.cpu_history.push((time_x, cpu));
        metrics.ram_history.push((time_x, ram));
        if metrics.cpu_history.len() > 60 {
            metrics.cpu_history.remove(0);
        }
        if metrics.ram_history.len() > 60 {
            metrics.ram_history.remove(0);
        }
    }

    /// Processes a single application event.
    async fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Key(key) => self.handle_key(key).await,
            AppEvent::Tick => self.tick().await,
            AppEvent::BackendResult(res) => self.handle_backend_result(res),
            AppEvent::ActionDone(msg, level) => {
                self.set_status(msg, level);
                self.refresh().await;
            }
            AppEvent::MetricsUpdate(name, time_x, cpu, ram) => {
                self.update_metrics(name, time_x, cpu, ram)
            }
            AppEvent::LogLine(line) => {
                self.data.log_lines.push_back(line);
                if self.data.log_lines.len() > 5000 {
                    self.data.log_lines.pop_front();
                }
            }
            AppEvent::TerminalRedraw => {}
        }
    }

    /// Starts the main application loop.
    pub async fn run(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        let mut events = EventHandler::new(100);
        let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::channel::<Vec<ContainerEntry>>(1);
        let (backend_tx, mut backend_rx) =
            tokio::sync::mpsc::channel::<crate::ui::core::BackendCommand>(100);

        self.ui.backend_tx = Some(backend_tx);
        self.ui.app_tx = Some(events.tx.clone());

        // Start nspawn metrics collection engine
        crate::nspawn::ops::inspect::metrics::spawn_collector(
            events.tx.clone(),
            self.data.cpu_cores,
            self.data.cpu_representation,
        );

        // Start data monitoring engine (DBus + Inotify)
        let (dirty_tx, mut dirty_rx) = tokio::sync::mpsc::channel::<()>(2);
        self.data.manager.watch(dirty_tx.clone()).await;

        // Start background refresh thread
        let manager_clone = self.data.manager.clone();
        let refresh_tx_clone = refresh_tx.clone();
        tokio::spawn(async move {
            while dirty_rx.recv().await.is_some() {
                if let Ok(entries) = manager_clone.list_all().await {
                    let _ = refresh_tx_clone.send(entries).await;
                }
            }
        });

        let _ = dirty_tx.send(()).await;

        loop {
            while let Ok(entries) = refresh_rx.try_recv() {
                self.sync_entries(entries).await;
            }

            // Render a frame
            terminal.draw(|f| crate::ui::draw(f, self))?;

            tokio::select! {
                Some(event) = events.rx.recv() => {
                    self.handle_event(event).await;
                    // Drain all pending events to batch UI updates
                    while let Ok(event) = events.rx.try_recv() {
                        self.handle_event(event).await;
                    }
                }
                Some(cmd) = backend_rx.recv() => {
                    crate::nspawn::ops::handlers::handle_command(cmd, events.tx.clone());
                }
                else => break,
            }

            if self.should_quit {
                break;
            }
        }
        self.cleanup_all_terminals();
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
