//! Main application state and event loop.

pub mod actions;
pub mod handlers;

use anyhow::Result;
use std::collections::HashMap;
use std::time::Instant;

use crate::events::{AppEvent, EventHandler};
use crate::nspawn::{
    models::{ContainerEntry, ContainerMetrics, CpuRepresentation},
    ops::{DefaultManager, NspawnManager},
    sys::ExecutionContext,
};
use crate::ui::core::{Component, FocusTracker};
use crate::ui::views::container_list::ContainerListComponent;
use crate::ui::views::detail_panel::DetailPanel;
use crate::ui::wizard::Wizard;
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal};
use std::io::Stdout;

pub use crate::ui::views::terminal_panel::TerminalManager;

/// Whether the user is in panel resize mode (toggled by `R`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResizeMode {
    Inactive,
    Active,
}

pub const CONTAINER_LIST_PCT_MIN: u16 = 15;
pub const CONTAINER_LIST_PCT_MAX: u16 = 50;
pub const DETAIL_PCT_MIN: u16 = 30;
pub const DETAIL_PCT_MAX: u16 = 85;

/// Screen-area rects for mouse hit-testing, populated on each render.
#[derive(Debug, Clone, Copy, Default)]
pub struct PanelLayout {
    pub list: Rect,
    pub detail: Rect,
    pub terminal: Option<Rect>,
}

pub struct AppUi {
    pub focus: FocusTracker,
    pub prev_active_idx: usize,
    pub container_list: ContainerListComponent,
    pub detail_panel: DetailPanel,

    pub show_wizard: bool,
    pub show_help: bool,
    pub power_menu: Option<crate::ui::widgets::power_menu::PowerMenu>,
    pub pane_height: u16,

    pub wizard: Option<Wizard>,

    pub status_message: Option<(String, crate::ui::StatusLevel)>,
    pub status_expiry: Option<Instant>,
    pub backend_tx: Option<tokio::sync::mpsc::Sender<crate::nspawn::ops::BackendCommand>>,
    pub app_tx: Option<tokio::sync::mpsc::Sender<AppEvent>>,
    pub quit_dialog: Option<crate::ui::widgets::dialogs::confirmation::ConfirmationDialog>,
    pub delete_dialog: Option<crate::ui::widgets::dialogs::confirmation::ConfirmationDialog>,
    pub active_dialog: Option<Box<dyn Component>>,

    pub resize_mode: ResizeMode,
    pub container_list_pct: u16,
    pub detail_pct: u16,

    /// Current panel screen rects for mouse hit-testing.
    pub panel_layout: PanelLayout,

    /// Signalled when the user confirms quit; included in the main
    /// `select!` so we break immediately instead of waiting for the next
    /// event or channel close.
    pub quit_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl AppUi {
    pub fn new() -> Self {
        Self {
            focus: FocusTracker::new(),
            prev_active_idx: 0,
            container_list: ContainerListComponent::new(),
            detail_panel: DetailPanel::new(),
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
            delete_dialog: None,
            active_dialog: None,
            resize_mode: ResizeMode::Inactive,
            container_list_pct: 30,
            detail_pct: 60,
            panel_layout: PanelLayout::default(),
            quit_tx: None,
        }
    }
}

// App

pub struct AppData {
    pub entries: Vec<ContainerEntry>,
    pub selected: usize,
    pub properties: Result<crate::nspawn::models::MachineProperties, String>,
    pub log_manager: crate::ui::views::detail_panel::log_manager::LogManager,
    pub config_content: Option<String>,
    pub dbus_active: bool,
    pub manager: std::sync::Arc<dyn NspawnManager>,
    pub exec_ctx: std::sync::Arc<ExecutionContext>,
    pub action_cooldown: Option<Instant>,
    pub transitions:
        std::collections::HashMap<String, (crate::nspawn::models::ContainerState, Instant)>,
    pub metrics: HashMap<String, ContainerMetrics>,
    pub cpu_cores: usize,
    pub cpu_representation: CpuRepresentation,

    // Dirty flags to avoid redundant O(N) calculations
    pub properties_dirty: bool,
    pub config_dirty: bool,
    pub details_dirty: bool,

    // Terminal state
    pub terminal: TerminalManager,
}

/// Global application state.
pub struct App {
    pub permissions: std::sync::Arc<dyn crate::nspawn::ops::PermissionManager>,
    pub config: std::sync::Arc<crate::config::AppConfig>,
    pub should_quit: bool,
    pub data: AppData,
    pub ui: AppUi,
}

impl App {
    pub fn new(
        permissions: std::sync::Arc<dyn crate::nspawn::ops::PermissionManager>,
        cli_mode: bool,
        log_buffer_lines: usize,
        exec_ctx: std::sync::Arc<ExecutionContext>,
        config: std::sync::Arc<crate::config::AppConfig>,
    ) -> Self {
        let manager = std::sync::Arc::new(DefaultManager::new(
            permissions.clone(),
            cli_mode,
            exec_ctx.clone(),
        ));
        Self {
            permissions,
            config,
            should_quit: false,
            data: AppData {
                entries: Vec::new(),
                selected: 0,
                properties: Ok(crate::nspawn::models::MachineProperties::default()),
                log_manager: crate::ui::views::detail_panel::log_manager::LogManager::new(
                    log_buffer_lines,
                ),
                config_content: None,
                dbus_active: !cli_mode,
                manager,
                exec_ctx,
                action_cooldown: None,
                transitions: std::collections::HashMap::new(),
                metrics: HashMap::new(),
                cpu_cores: std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1),
                cpu_representation: CpuRepresentation::Normalized,
                properties_dirty: true,
                config_dirty: true,
                details_dirty: true,
                terminal: TerminalManager::new(),
            },
            ui: AppUi::new(),
        }
    }

    /// Fire the quit signal so the event loop breaks out of `select!`
    /// immediately, then mark `should_quit`.
    fn signal_quit(&mut self) {
        log::info!("[lasper] signal_quit() called");
        if let Some(tx) = self.ui.quit_tx.take() {
            let _ = tx.send(());
        }
        self.should_quit = true;
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
        self.data
            .metrics
            .retain(|name, _| active_names.contains(name));
        self.data
            .log_manager
            .remove_stale(&active_names.into_iter().cloned().collect());
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
    fn handle_backend_result(&mut self, res: crate::nspawn::ops::BackendResponse) {
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
            AppEvent::Mouse(mouse) => self.handle_mouse(mouse).await,
            AppEvent::Tick => self.tick().await,
            AppEvent::BackendResult(res) => self.handle_backend_result(res),
            AppEvent::ActionDone(msg, level) => {
                self.set_status(msg, level);
                self.refresh().await;
            }
            AppEvent::ContainerActionFailed {
                name,
                previous_state,
                message,
            } => {
                self.rollback_container_transition(&name, previous_state);
                self.set_status(message, crate::ui::StatusLevel::Error);
                self.refresh().await;
            }
            AppEvent::MetricsUpdate(name, time_x, cpu, ram) => {
                self.update_metrics(name, time_x, cpu, ram)
            }
            AppEvent::TerminalRedraw => {}
        }
    }

    /// Starts the main application loop.
    pub async fn run(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        // Initialize the global theme before any rendering.
        crate::ui::theme::init_theme(crate::ui::theme::load_theme(self.config.theme.as_ref()));

        let mut events = EventHandler::new(100);
        let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::channel::<Vec<ContainerEntry>>(1);
        let (backend_tx, mut backend_rx) =
            tokio::sync::mpsc::channel::<crate::nspawn::ops::BackendCommand>(100);

        self.ui.backend_tx = Some(backend_tx);
        self.ui.app_tx = Some(events.tx.clone());

        // Quit signal — the oneshot fires in the select! below so we
        // break out of the event loop immediately when the user confirms
        // quit, instead of blocking until a background task sends an event.
        let (quit_tx, mut quit_rx) = tokio::sync::oneshot::channel::<()>();
        self.ui.quit_tx = Some(quit_tx);

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
                log::debug!("Refresh: dirty_rx nudge, running list_all...");
                if let Ok(entries) = manager_clone.list_all().await {
                    let _ = refresh_tx_clone.send(entries).await;
                }
            }
        });

        log::debug!("Refresh: initial nudge");
        let _ = dirty_tx.send(()).await;

        loop {
            // Drain at most 3 refresh batches per frame so rapid background
            // updates can't starve user-input events from the select! below.
            for _ in 0..3 {
                match refresh_rx.try_recv() {
                    Ok(entries) => self.sync_entries(entries).await,
                    Err(_) => break,
                }
            }

            // Drain per-buffer log channels before rendering
            self.data.log_manager.drain_all();

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
                _ = &mut quit_rx => {
                    log::info!("[lasper] select!: quit_rx fired");
                    break;
                }
                else => {
                    log::info!("[lasper] select!: else branch");
                    break;
                }
            }

            if self.should_quit {
                log::info!("[lasper] main loop: should_quit=true, breaking");
                break;
            }
        }
        log::info!("[lasper] run() cleaning up...");
        self.data.terminal.cleanup_all();
        self.data.log_manager.cleanup_all();
        // Shut down the EventStream while the terminal is still in raw mode
        // so the internal stdin thread can unblock quickly instead of
        // blocking on a cooked-mode read after terminal restore.
        events.shutdown();
        log::info!("[lasper] run() returning Ok(())");
        Ok(())
    }

    // Tick (auto-refresh + status expiry)

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::models::{ContainerEntry, ContainerState};
    use std::time::{Duration, Instant};

    fn make_entry(name: &str, state: ContainerState) -> ContainerEntry {
        ContainerEntry {
            name: name.to_string(),
            state,
            image_type: None,
            readonly: false,
            usage: None,
            address: None,
            all_addresses: vec![],
        }
    }

    fn make_app() -> App {
        App::new(
            std::sync::Arc::new(crate::nspawn::ops::DefaultPermissionManager::new()),
            false, // cli_mode
            0,
            std::sync::Arc::new(crate::nspawn::sys::ExecutionContext::new(
                crate::nspawn::ops::PermissionLevel::User,
                None,
            )),
            std::sync::Arc::new(crate::config::AppConfig::default()),
        )
    }

    mod merge_transitional_states {
        use super::*;

        #[test]
        fn adds_starting_overlay() {
            let mut app = make_app();
            app.data.transitions.insert(
                "test".to_string(),
                (ContainerState::Starting, Instant::now()),
            );

            let entries = vec![make_entry("test", ContainerState::Off)];
            let result = app.merge_transitional_states(entries);

            assert_eq!(result[0].state, ContainerState::Starting);
        }

        #[test]
        fn adds_exiting_overlay() {
            let mut app = make_app();
            app.data.transitions.insert(
                "test".to_string(),
                (ContainerState::Exiting, Instant::now()),
            );

            let entries = vec![make_entry("test", ContainerState::Running)];
            let result = app.merge_transitional_states(entries);

            assert_eq!(result[0].state, ContainerState::Exiting);
        }

        #[test]
        fn expires_stale() {
            let mut app = make_app();
            app.data.transitions.insert(
                "test".to_string(),
                (
                    ContainerState::Starting,
                    Instant::now() - Duration::from_secs(11),
                ),
            );

            let entries = vec![make_entry("test", ContainerState::Off)];
            let result = app.merge_transitional_states(entries);

            assert_eq!(result[0].state, ContainerState::Off);
            assert!(app.data.transitions.is_empty());
        }

        #[test]
        fn removes_when_backend_resolved() {
            let mut app = make_app();
            app.data.transitions.insert(
                "test".to_string(),
                (ContainerState::Starting, Instant::now()),
            );

            let entries = vec![make_entry("test", ContainerState::Running)];
            let result = app.merge_transitional_states(entries);

            assert_eq!(result[0].state, ContainerState::Running);
            assert!(app.data.transitions.is_empty());
        }

        #[test]
        fn failed_start_rolls_back_optimistic_state_immediately() {
            let mut app = make_app();
            app.data.entries = vec![make_entry("test", ContainerState::Starting)];
            app.data.transitions.insert(
                "test".to_string(),
                (ContainerState::Starting, Instant::now()),
            );

            app.rollback_container_transition("test", Some(ContainerState::Off));

            assert_eq!(app.data.entries[0].state, ContainerState::Off);
            assert!(!app.data.transitions.contains_key("test"));
        }
    }

    mod select_next_prev {
        use super::*;

        #[test]
        fn next_wraps() {
            let mut app = make_app();
            app.data.entries = vec![
                make_entry("a", ContainerState::Off),
                make_entry("b", ContainerState::Off),
                make_entry("c", ContainerState::Off),
            ];
            app.data.selected = 2;

            app.select_next();
            assert_eq!(app.data.selected, 0);
        }

        #[test]
        fn prev_wraps() {
            let mut app = make_app();
            app.data.entries = vec![
                make_entry("a", ContainerState::Off),
                make_entry("b", ContainerState::Off),
                make_entry("c", ContainerState::Off),
            ];
            app.data.selected = 0;

            app.select_prev();
            assert_eq!(app.data.selected, 2);
        }

        #[test]
        fn next_empty_no_panic() {
            let mut app = make_app();
            app.data.entries = vec![];
            app.data.selected = 0;

            app.select_next();
        }

        #[test]
        fn prev_empty_no_panic() {
            let mut app = make_app();
            app.data.entries = vec![];
            app.data.selected = 0;

            app.select_prev();
        }
    }

    mod action_cooldown {
        use super::*;

        #[test]
        fn allows_first() {
            let mut app = make_app();
            assert!(app.check_action_cooldown());
        }

        #[test]
        fn blocks_within_2s() {
            let mut app = make_app();
            assert!(app.check_action_cooldown());
            assert!(!app.check_action_cooldown());
        }
    }

    mod set_status {
        use super::*;

        #[test]
        fn sets_message_and_expiry() {
            let mut app = make_app();
            app.set_status("hello".into(), crate::ui::StatusLevel::Info);

            let (msg, level) = app.ui.status_message.as_ref().unwrap();
            assert_eq!(msg, "hello");
            assert_eq!(*level, crate::ui::StatusLevel::Info);
            assert!(app.ui.status_expiry.is_some());
        }
    }
}
