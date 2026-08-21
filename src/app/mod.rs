//! Main application state and event loop.

pub mod actions;
pub mod handlers;

use anyhow::Result;
use std::collections::HashMap;
use std::time::Instant;

use crate::application::sessions::SessionService;
use crate::events::{AppEvent, EventHandler};
use crate::nspawn::{
    models::{
        ContainerEntry, ContainerMetrics, CpuRepresentation, ImageEntry, ImageName, RuntimeSnapshot,
    },
    ops::{ImageLifecycleService, MachineLifecycleService, RuntimeCatalog, RuntimeUpdate},
    sys::ExecutionContext,
};
use crate::ui::core::{Component, FocusTracker};
use crate::ui::views::container_list::ContainerListComponent;
use crate::ui::views::detail_panel::DetailPanel;
use crate::ui::views::detail_panel::DetailTarget;
use crate::ui::views::image_list::ImageListComponent;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorSource {
    Machine,
    Image,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainFocusSlot {
    Machines,
    MachineInspector,
    Images,
    ImageInspector,
    Terminal,
}

pub const CONTAINER_LIST_PCT_MIN: u16 = 15;
pub const CONTAINER_LIST_PCT_MAX: u16 = 50;
pub const DETAIL_PCT_MIN: u16 = 30;
pub const DETAIL_PCT_MAX: u16 = 85;
pub const LEFT_MACHINES_PCT_MIN: u16 = 20;
pub const LEFT_MACHINES_PCT_MAX: u16 = 80;
const MAX_EVENTS_PER_FRAME: usize = 64;

/// Screen-area rects for mouse hit-testing, populated on each render.
#[derive(Debug, Clone, Copy, Default)]
pub struct PanelLayout {
    pub machines: Rect,
    pub images: Rect,
    pub detail: Rect,
    pub terminal: Option<Rect>,
}

pub struct PendingImageRemoval {
    target: ImageName,
    dialog: crate::ui::widgets::dialogs::confirmation::ConfirmationDialog,
}

impl PendingImageRemoval {
    fn new(
        target: ImageName,
        dialog: crate::ui::widgets::dialogs::confirmation::ConfirmationDialog,
    ) -> Self {
        Self { target, dialog }
    }

    fn target(&self) -> &ImageName {
        &self.target
    }

    fn cleanup_artifacts(&self) -> bool {
        self.dialog.checkbox_checked().unwrap_or(false)
    }
}

impl Component for PendingImageRemoval {
    fn render(&mut self, f: &mut ratatui::Frame, area: Rect) {
        self.dialog.render(f, area);
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> crate::ui::core::EventResult {
        self.dialog.handle_key(key)
    }
}

pub struct AppUi {
    pub focus: FocusTracker,
    pub prev_active_idx: usize,
    pub inspector_source: InspectorSource,
    pub container_list: ContainerListComponent,
    pub image_list: ImageListComponent,
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
    pub delete_dialog: Option<PendingImageRemoval>,
    pub active_dialog: Option<Box<dyn Component>>,

    pub resize_mode: ResizeMode,
    pub container_list_pct: u16,
    pub left_machines_pct: u16,
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
            inspector_source: InspectorSource::Machine,
            container_list: ContainerListComponent::new(),
            image_list: ImageListComponent::new(),
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
            left_machines_pct: 50,
            detail_pct: 45,
            panel_layout: PanelLayout::default(),
            quit_tx: None,
        }
    }
}

// App

pub struct AppData {
    /// Running systemd-machined instances plus optimistic `Starting` rows.
    /// Persistent images live in `images`.
    pub entries: Vec<ContainerEntry>,
    /// Normally visible images. Hidden dot-prefixed images are kept separate.
    pub images: Vec<ImageEntry>,
    pub internal_images: Vec<ImageEntry>,
    pub image_selected: usize,
    pub internal_image_selected: usize,
    pub selected: usize,
    pub properties: Result<crate::nspawn::models::MachineProperties, String>,
    pub log_manager: crate::ui::views::detail_panel::log_manager::LogManager,
    pub config_content: Option<String>,
    pub config_path: Option<std::path::PathBuf>,
    pub detail_target: DetailTarget,
    pub unit_name: Option<String>,
    pub unit_drop_ins: Vec<crate::nspawn::adapters::config::systemd_unit::SystemdDropIn>,
    pub dbus_active: bool,
    pub session_service: std::sync::Arc<SessionService>,
    pub runtime_catalog: std::sync::Arc<RuntimeCatalog>,
    pub machine_lifecycle: std::sync::Arc<MachineLifecycleService>,
    pub image_lifecycle: std::sync::Arc<ImageLifecycleService>,
    pub exec_ctx: std::sync::Arc<ExecutionContext>,
    pub action_cooldown: Option<Instant>,
    pub metrics: HashMap<String, ContainerMetrics>,
    pub cpu_cores: usize,
    pub cpu_representation: CpuRepresentation,

    // Dirty flags to avoid redundant O(N) calculations
    pub properties_dirty: bool,
    pub config_dirty: bool,
    pub unit_dirty: bool,
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
        let session_service = crate::nspawn::adapters::session::compose_session_service(
            permissions.level(),
            &exec_ctx,
        );
        let runtime_catalog = crate::nspawn::ops::runtime_catalog_adapter::compose_runtime_catalog(
            permissions.level(),
            cli_mode,
            &exec_ctx,
        );
        let operation_registry = crate::nspawn::ops::OperationRegistry::new();
        let image_lifecycle = std::sync::Arc::new(
            crate::nspawn::ops::image_lifecycle_adapter::compose_image_lifecycle(
                runtime_catalog.clone(),
                operation_registry.clone(),
                permissions.level(),
                cli_mode,
                &exec_ctx,
            ),
        );
        let machine_lifecycle =
            crate::nspawn::ops::machine_lifecycle_adapter::compose_machine_lifecycle(
                runtime_catalog.clone(),
                operation_registry,
                permissions.level(),
                cli_mode,
                &exec_ctx,
            );
        Self {
            permissions,
            config,
            should_quit: false,
            data: AppData {
                entries: Vec::new(),
                images: Vec::new(),
                internal_images: Vec::new(),
                image_selected: 0,
                internal_image_selected: 0,
                selected: 0,
                properties: Ok(crate::nspawn::models::MachineProperties::default()),
                log_manager: crate::ui::views::detail_panel::log_manager::LogManager::new(
                    log_buffer_lines,
                ),
                config_content: None,
                config_path: None,
                detail_target: DetailTarget::Empty,
                unit_name: None,
                unit_drop_ins: Vec::new(),
                dbus_active: !cli_mode,
                session_service: session_service.clone(),
                runtime_catalog,
                machine_lifecycle,
                image_lifecycle,
                exec_ctx,
                action_cooldown: None,
                metrics: HashMap::new(),
                cpu_cores: std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1),
                cpu_representation: CpuRepresentation::Normalized,
                properties_dirty: true,
                config_dirty: true,
                unit_dirty: true,
                details_dirty: true,
                terminal: TerminalManager::new(session_service),
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

    /// Set focus while keeping the last non-terminal panel available for
    /// restoring focus when the terminal is closed or hidden.
    pub(crate) fn set_focus_idx(&mut self, idx: usize) {
        if idx != 3 {
            self.ui.prev_active_idx = idx.min(2);
        }
        match idx {
            0 => self.ui.inspector_source = InspectorSource::Machine,
            1 => self.ui.inspector_source = InspectorSource::Image,
            _ => {}
        }
        self.ui.focus.active_idx = idx;
        self.update_detail_target();
    }

    pub(crate) fn cycle_main_focus(&mut self, forward: bool) {
        const WITHOUT_TERMINAL: &[MainFocusSlot] = &[
            MainFocusSlot::Machines,
            MainFocusSlot::MachineInspector,
            MainFocusSlot::Images,
            MainFocusSlot::ImageInspector,
        ];
        const WITH_TERMINAL: &[MainFocusSlot] = &[
            MainFocusSlot::Machines,
            MainFocusSlot::MachineInspector,
            MainFocusSlot::Images,
            MainFocusSlot::ImageInspector,
            MainFocusSlot::Terminal,
        ];
        const MAXIMIZED_TERMINAL: &[MainFocusSlot] = &[
            MainFocusSlot::Machines,
            MainFocusSlot::Images,
            MainFocusSlot::Terminal,
        ];

        let slots = if self.data.terminal.is_showing() && self.data.terminal.maximized {
            MAXIMIZED_TERMINAL
        } else if self.data.terminal.is_showing() {
            WITH_TERMINAL
        } else {
            WITHOUT_TERMINAL
        };
        let current = match self.ui.focus.active_idx {
            0 => MainFocusSlot::Machines,
            1 => MainFocusSlot::Images,
            2 if self.ui.inspector_source == InspectorSource::Image => {
                MainFocusSlot::ImageInspector
            }
            2 => MainFocusSlot::MachineInspector,
            3 => MainFocusSlot::Terminal,
            _ => MainFocusSlot::Machines,
        };
        let current_idx = slots.iter().position(|slot| *slot == current).unwrap_or(0);
        let next_idx = if forward {
            (current_idx + 1) % slots.len()
        } else {
            (current_idx + slots.len() - 1) % slots.len()
        };

        match slots[next_idx] {
            MainFocusSlot::Machines => self.set_focus_idx(0),
            MainFocusSlot::MachineInspector => {
                self.ui.inspector_source = InspectorSource::Machine;
                self.set_focus_idx(2);
            }
            MainFocusSlot::Images => self.set_focus_idx(1),
            MainFocusSlot::ImageInspector => {
                self.ui.inspector_source = InspectorSource::Image;
                self.set_focus_idx(2);
            }
            MainFocusSlot::Terminal => self.set_focus_idx(3),
        }
    }

    pub(crate) fn restore_non_terminal_focus(&mut self) {
        self.set_focus_idx(self.ui.prev_active_idx.min(2));
    }

    pub(crate) fn update_detail_target(&mut self) {
        let target = match self.ui.focus.active_idx {
            0 => self.machine_detail_target(),
            1 => self.image_detail_target(),
            2 if self.ui.inspector_source == InspectorSource::Image => {
                match &self.data.detail_target {
                    DetailTarget::Image { name, internal }
                        if self
                            .data
                            .images
                            .iter()
                            .chain(self.data.internal_images.iter())
                            .any(|image| image.name == *name && image.is_hidden() == *internal) =>
                    {
                        self.data.detail_target.clone()
                    }
                    _ => self.image_detail_target(),
                }
            }
            2 => match &self.data.detail_target {
                DetailTarget::Machine(name)
                    if self.data.entries.iter().any(|entry| entry.name == *name) =>
                {
                    self.data.detail_target.clone()
                }
                _ => self.machine_detail_target(),
            },
            3 => self
                .data
                .terminal
                .active_session()
                .and_then(|session| {
                    let name = session.container_name.as_str();
                    self.data
                        .entries
                        .iter()
                        .find(|entry| entry.name == name)
                        .map(|_| DetailTarget::Machine(name.to_string()))
                })
                .unwrap_or_else(|| self.machine_detail_target()),
            _ => DetailTarget::Empty,
        };

        if target != self.data.detail_target {
            self.data.detail_target = target;
            self.ui
                .detail_panel
                .ensure_pane_for_target(&self.data.detail_target);
            self.data.properties_dirty = true;
            self.data.details_dirty = true;
            self.data.config_dirty = true;
            self.data.unit_dirty = true;
            self.data.config_content = None;
            self.data.config_path = None;
            self.data.unit_name = None;
            self.data.unit_drop_ins.clear();
        }
    }

    fn machine_detail_target(&self) -> DetailTarget {
        self.data
            .entries
            .get(self.data.selected)
            .map(|entry| DetailTarget::Machine(entry.name.clone()))
            .unwrap_or_default()
    }

    fn image_detail_target(&self) -> DetailTarget {
        let (images, selected) = if self.ui.image_list.shows_internal() {
            (
                &self.data.internal_images,
                self.data.internal_image_selected,
            )
        } else {
            (&self.data.images, self.data.image_selected)
        };
        images
            .get(selected)
            .map(|image| DetailTarget::Image {
                name: image.name.clone(),
                internal: image.is_hidden(),
            })
            .unwrap_or_default()
    }

    /// Update entries and selection state from a background refresh.
    async fn sync_entries(&mut self, entries: Vec<ContainerEntry>) {
        let prev_name = self
            .data
            .entries
            .get(self.data.selected)
            .map(|e| e.name.clone());
        self.data.entries = self.data.machine_lifecycle.project_machines(entries);
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
        self.update_detail_target();
        if !self.data.detail_target.is_image() {
            self.refresh_detail().await;
        }

        if let Some(wizard) = &mut self.ui.wizard {
            wizard.context.entries = self.data.entries.clone();
            wizard.context.images = self.data.images.clone();
        }
    }

    /// Apply the independent machine/image snapshot returned by the backend.
    async fn sync_snapshot(&mut self, snapshot: RuntimeSnapshot) {
        let RuntimeSnapshot { machines, images } = snapshot;
        let running: Vec<_> = machines
            .into_iter()
            .filter(|e| e.state.is_running())
            .collect();
        self.sync_entries(running).await;

        let previous_name = self
            .data
            .images
            .get(self.data.image_selected)
            .map(|image| image.name.clone());
        let previous_internal_name = self
            .data
            .internal_images
            .get(self.data.internal_image_selected)
            .map(|image| image.name.clone());
        let (mut visible, mut internal): (Vec<_>, Vec<_>) =
            images.into_iter().partition(|image| !image.is_hidden());
        visible.sort();
        internal.sort();
        self.data.images = visible;
        self.data.internal_images = internal;
        self.data.image_selected = previous_name
            .and_then(|name| self.data.images.iter().position(|image| image.name == name))
            .unwrap_or(0)
            .min(self.data.images.len().saturating_sub(1));
        self.data.internal_image_selected = previous_internal_name
            .and_then(|name| {
                self.data
                    .internal_images
                    .iter()
                    .position(|image| image.name == name)
            })
            .unwrap_or(0)
            .min(self.data.internal_images.len().saturating_sub(1));
        self.update_detail_target();
        if self.data.detail_target.is_image() {
            self.refresh_detail().await;
        }
        if let Some(wizard) = &mut self.ui.wizard {
            wizard.context.images = self.data.images.clone();
        }
    }

    async fn sync_runtime_query(
        &mut self,
        query: crate::nspawn::ops::RuntimeQuery<RuntimeSnapshot>,
    ) {
        self.data.dbus_active = query.route.is_dbus();
        if let Some(fallback) = query.fallback {
            self.set_status(
                format!(
                    "{} unavailable: {}; using {}",
                    fallback.from.label(),
                    fallback.reason,
                    fallback.to.label()
                ),
                crate::ui::StatusLevel::Warn,
            );
        }
        self.sync_snapshot(query.value).await;
    }

    /// Forward backend response to the active wizard/context.
    fn handle_backend_result(&mut self, res: crate::nspawn::ops::BackendResponse) {
        if let Some(wizard) = &mut self.ui.wizard {
            let action = wizard.process_message(crate::ui::core::AppMessage::Backend(res));
            match action {
                crate::ui::wizard::StepAction::Status(msg, level) => {
                    self.set_status(msg, level);
                }
                crate::ui::wizard::StepAction::OpenDialog(dialog) => {
                    self.ui.active_dialog = Some(dialog);
                }
                crate::ui::wizard::StepAction::CloseDialog => {
                    self.ui.active_dialog = None;
                }
                _ => {}
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
            AppEvent::MachineActionFinished(outcome) => {
                let (message, level) = machine_outcome_status(outcome);
                self.refresh().await;
                self.set_status(message, level);
            }
            AppEvent::MetricsUpdate(name, time_x, cpu, ram) => {
                self.update_metrics(name, time_x, cpu, ram)
            }
            AppEvent::TerminalRedraw => self.data.terminal.clear_redraw_pending(),
        }
    }

    /// Starts the main application loop.
    pub async fn run(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        // Initialize the global theme before any rendering.
        crate::ui::theme::init_theme(crate::ui::theme::load_theme(self.config.theme.as_ref()));

        let mut events = EventHandler::new(100);
        let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::channel::<RuntimeUpdate>(4);
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

        self.data.runtime_catalog.watch(refresh_tx).await;

        loop {
            // Drain at most 3 refresh batches per frame so rapid background
            // updates can't starve user-input events from the select! below.
            for _ in 0..3 {
                match refresh_rx.try_recv() {
                    Ok(RuntimeUpdate::Snapshot(snapshot)) => {
                        self.sync_runtime_query(snapshot).await;
                    }
                    Ok(RuntimeUpdate::BackendFailure {
                        message,
                        consecutive_failures,
                    }) => {
                        log::warn!(
                            "Status observer failure #{}: {}",
                            consecutive_failures,
                            message
                        );
                        if consecutive_failures == 1 || consecutive_failures % 12 == 0 {
                            self.set_status_for(
                                format!("Status observer unavailable: {}", message),
                                crate::ui::StatusLevel::Warn,
                                std::time::Duration::from_secs(6),
                            );
                        }
                    }
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
                    // Batch a bounded number of events so a busy PTY cannot
                    // starve rendering, keyboard input, or the quit signal.
                    for _ in 1..MAX_EVENTS_PER_FRAME {
                        let Ok(event) = events.rx.try_recv() else { break };
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

fn machine_outcome_status(
    outcome: crate::nspawn::ops::MachineLifecycleOutcome,
) -> (String, crate::ui::StatusLevel) {
    use crate::nspawn::ops::MachineLifecycleResult;

    let fallback = outcome
        .fallback
        .map(|fallback| format!(" (CLI fallback: {})", fallback.reason))
        .unwrap_or_default();
    let machine = outcome.machine.as_str();
    match outcome.result {
        MachineLifecycleResult::Succeeded => (
            format!("{} {}{}", outcome.action.success_label(), machine, fallback),
            crate::ui::StatusLevel::Success,
        ),
        MachineLifecycleResult::NotAttempted(reason) => (
            format!(
                "{} {} was not attempted: {}{}",
                outcome.action.audit_label(),
                machine,
                reason,
                fallback
            ),
            crate::ui::StatusLevel::Error,
        ),
        MachineLifecycleResult::Rejected { reason, .. } => (
            format!(
                "{} {} was rejected: {}{}",
                outcome.action.audit_label(),
                machine,
                reason,
                fallback
            ),
            crate::ui::StatusLevel::Warn,
        ),
        MachineLifecycleResult::Failed(reason) => (
            format!(
                "{} {} failed: {}{}",
                outcome.action.audit_label(),
                machine,
                reason,
                fallback
            ),
            crate::ui::StatusLevel::Error,
        ),
        MachineLifecycleResult::OutcomeUnknown(reason) => (
            format!(
                "{} {} outcome is unknown: {}{}",
                outcome.action.audit_label(),
                machine,
                reason,
                fallback
            ),
            crate::ui::StatusLevel::Warn,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::models::{ContainerEntry, ContainerState, ImageEntry};
    use std::time::Duration;

    fn make_entry(name: &str, state: ContainerState) -> ContainerEntry {
        ContainerEntry {
            name: name.to_string(),
            state,
            address: None,
            all_addresses: vec![],
        }
    }

    fn make_image(name: &str) -> ImageEntry {
        ImageEntry {
            name: name.to_string(),
            image_type: "directory".to_string(),
            readonly: false,
            usage: None,
            dbus_object_path: None,
        }
    }

    fn make_internal_image(name: &str) -> ImageEntry {
        make_image(name)
    }

    fn make_app() -> App {
        App::new(
            std::sync::Arc::new(crate::nspawn::ops::DefaultPermissionManager::new()),
            false, // cli_mode
            0,
            std::sync::Arc::new(
                crate::nspawn::sys::ExecutionContext::new(
                    crate::nspawn::ops::PermissionLevel::User,
                    None,
                )
                .unwrap(),
            ),
            std::sync::Arc::new(crate::config::AppConfig::default()),
        )
    }

    mod image_start_transitions {
        use super::*;
        use crate::events::AppEvent;
        use crate::nspawn::ops::machine_lifecycle::{
            MachineControlOutcome, MockMachineControl, MockMachineObservation,
            MockMachineStartDiagnostics, MockMachineStartPreparation, RoutedMachineControlOutcome,
        };
        use crate::nspawn::ops::route::ExecutionRoute;
        use crate::nspawn::ops::{MachineLifecycleResult, OperationRegistry};

        fn prepare_image_start(
            control_outcome: MachineControlOutcome,
        ) -> (App, tokio::sync::mpsc::Receiver<AppEvent>) {
            let successful = matches!(control_outcome, MachineControlOutcome::Succeeded);
            let mut control = MockMachineControl::new();
            control
                .expect_execute()
                .once()
                .returning(move |_, _| RoutedMachineControlOutcome {
                    outcome: control_outcome.clone(),
                    route: ExecutionRoute::DirectDbus,
                    fallback: None,
                });
            let mut preparation = MockMachineStartPreparation::new();
            preparation.expect_prepare().once().returning(|_| Ok(()));
            let mut observation = MockMachineObservation::new();
            if successful {
                observation.expect_inspect().once().returning(|_, _| {
                    let mut properties = crate::nspawn::models::MachineProperties::default();
                    properties.insert(
                        crate::nspawn::models::GROUP_SYSTEMD_UNIT,
                        "ActiveState".into(),
                        "active".into(),
                    );
                    Ok(properties)
                });
            }
            observation.expect_invalidate().once().return_const(());
            let diagnostics = MockMachineStartDiagnostics::new();
            let lifecycle = std::sync::Arc::new(MachineLifecycleService::new(
                std::sync::Arc::new(control),
                std::sync::Arc::new(preparation),
                std::sync::Arc::new(observation),
                std::sync::Arc::new(diagnostics),
                OperationRegistry::new(),
            ));
            let mut app = make_app();
            app.data.machine_lifecycle = lifecycle;
            app.data.images = vec![make_image("test")];
            app.ui.focus.active_idx = 1;
            let (tx, rx) = tokio::sync::mpsc::channel(2);
            app.ui.app_tx = Some(tx);
            (app, rx)
        }

        #[tokio::test]
        async fn image_start_adds_machine_before_backend_completion() {
            let (mut app, mut rx) = prepare_image_start(MachineControlOutcome::Succeeded);

            app.action_start();

            assert_eq!(
                app.data.entries,
                vec![make_entry("test", ContainerState::Starting)]
            );

            let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("start task should finish")
                .expect("start task should report a result");
            tokio::task::yield_now().await;
            assert_eq!(app.data.exec_ctx.host_operations.active_count(), 0);
            assert!(matches!(
                event,
                AppEvent::MachineActionFinished(outcome)
                    if outcome.result == MachineLifecycleResult::Succeeded
            ));

            let resolved = app
                .data
                .machine_lifecycle
                .project_machines(vec![make_entry("test", ContainerState::Running)]);
            assert_eq!(resolved, vec![make_entry("test", ContainerState::Running)]);
        }

        #[tokio::test]
        async fn image_start_failure_removes_synthetic_machine() {
            let (mut app, mut rx) = prepare_image_start(MachineControlOutcome::Failed {
                reason: "start rejected".into(),
            });

            app.action_start();

            let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("start task should finish")
                .expect("start task should report a result");
            let AppEvent::MachineActionFinished(outcome) = event else {
                panic!("start failure should report a semantic outcome");
            };
            assert_eq!(
                outcome.result,
                MachineLifecycleResult::Failed("start rejected".into())
            );
            assert!(app
                .data
                .machine_lifecycle
                .project_machines(vec![])
                .is_empty());
        }

        #[tokio::test]
        async fn image_start_does_not_dispatch_again_while_machine_is_starting() {
            let (mut app, mut rx) = prepare_image_start(MachineControlOutcome::Succeeded);

            app.action_start();
            app.data.action_cooldown = None;
            app.action_start();

            let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("first start task should finish")
                .expect("first start task should report a result");
            assert!(matches!(event, AppEvent::MachineActionFinished(..)));
            assert_eq!(
                app.ui
                    .status_message
                    .as_ref()
                    .map(|(message, _)| message.as_str()),
                Some("test is already starting.")
            );
        }

        #[tokio::test]
        async fn mstack_start_warns_but_still_dispatches_to_systemd() {
            let (mut app, mut rx) = prepare_image_start(MachineControlOutcome::Succeeded);
            app.data.images[0].image_type = "mstack".into();

            app.action_start();

            assert_eq!(app.data.entries[0].state, ContainerState::Starting);
            assert_eq!(
                app.ui
                    .status_message
                    .as_ref()
                    .map(|(message, _)| message.as_str()),
                Some(
                    "test is an OCI application and may not be bootable; systemd will still try to start it."
                )
            );
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(1), rx.recv())
                    .await
                    .expect("mstack start should finish")
                    .expect("mstack start should report a result"),
                AppEvent::MachineActionFinished(..)
            ));
        }
    }

    mod image_removal {
        use super::*;
        use crate::events::AppEvent;
        use crate::nspawn::ops::image_lifecycle::{
            ArtifactCleanupReport, ImageControlOutcome, MockImageControl, MockImageRuntime,
            MockManagedArtifactCleanup, UnitDisableReport,
        };
        use crate::nspawn::ops::OperationRegistry;

        fn prepare_image_removal(
            control: MockImageControl,
            cleanup: MockManagedArtifactCleanup,
        ) -> (App, tokio::sync::mpsc::Receiver<AppEvent>) {
            let mut runtime = MockImageRuntime::new();
            runtime.expect_list_machines().returning(|| Ok(Vec::new()));
            let mut app = make_app();
            app.data.image_lifecycle = std::sync::Arc::new(ImageLifecycleService::new(
                std::sync::Arc::new(runtime),
                std::sync::Arc::new(control),
                std::sync::Arc::new(cleanup),
                OperationRegistry::new(),
            ));
            app.data.images = vec![make_image("first"), make_image("second")];
            app.ui.focus.active_idx = 1;
            let (tx, rx) = tokio::sync::mpsc::channel(2);
            app.ui.app_tx = Some(tx);
            (app, rx)
        }

        #[tokio::test]
        async fn confirmation_binds_target_across_selection_and_focus_changes() {
            let mut control = MockImageControl::new();
            control
                .expect_disable_unit()
                .withf(|name| name.as_str() == "first")
                .once()
                .returning(|_| UnitDisableReport::Disabled);
            control
                .expect_remove_image()
                .withf(|name| name.as_str() == "first")
                .once()
                .returning(|_| ImageControlOutcome::Removed);
            let mut cleanup = MockManagedArtifactCleanup::new();
            cleanup
                .expect_cleanup()
                .withf(|name| name.as_str() == "first")
                .once()
                .returning(|_| ArtifactCleanupReport::Removed);
            let (mut app, mut rx) = prepare_image_removal(control, cleanup);

            app.show_delete_dialog();
            app.data.image_selected = 1;
            app.ui.focus.active_idx = 0;
            app.action_remove();

            let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("remove task should finish")
                .expect("remove task should report a result");
            assert!(matches!(
                event,
                AppEvent::ActionDone(message, crate::ui::StatusLevel::Success)
                    if message == "Removed image first and Lasper artifacts"
            ));
        }

        #[tokio::test]
        async fn confirmation_can_skip_lasper_artifact_cleanup() {
            let mut control = MockImageControl::new();
            control
                .expect_disable_unit()
                .returning(|_| UnitDisableReport::Disabled);
            control
                .expect_remove_image()
                .withf(|name| name.as_str() == "first")
                .once()
                .returning(|_| ImageControlOutcome::Removed);
            let mut cleanup = MockManagedArtifactCleanup::new();
            cleanup.expect_cleanup().never();
            let (mut app, mut rx) = prepare_image_removal(control, cleanup);

            app.show_delete_dialog();
            let result = app
                .ui
                .delete_dialog
                .as_mut()
                .expect("delete dialog")
                .handle_key(crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char(' '),
                    crossterm::event::KeyModifiers::NONE,
                ));
            assert!(matches!(result, crate::ui::core::EventResult::Consumed));
            app.action_remove();

            let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("remove task should finish")
                .expect("remove task should report a result");
            assert!(matches!(
                event,
                AppEvent::ActionDone(message, crate::ui::StatusLevel::Success)
                    if message == "Removed image first"
            ));
        }

        #[tokio::test]
        async fn cleanup_failure_preserves_successful_removal_result() {
            let mut control = MockImageControl::new();
            control
                .expect_disable_unit()
                .returning(|_| UnitDisableReport::Disabled);
            control
                .expect_remove_image()
                .once()
                .returning(|_| ImageControlOutcome::Removed);
            let mut cleanup = MockManagedArtifactCleanup::new();
            cleanup
                .expect_cleanup()
                .once()
                .returning(|_| ArtifactCleanupReport::Failed(vec!["cleanup failed".into()]));
            let (mut app, mut rx) = prepare_image_removal(control, cleanup);

            app.show_delete_dialog();
            app.action_remove();

            let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("remove task should finish")
                .expect("remove task should report a result");
            assert!(matches!(
                event,
                AppEvent::ActionDone(message, crate::ui::StatusLevel::Warn)
                    if message.contains("Removed image first")
                        && message.contains("cleanup failed")
            ));
        }

        #[tokio::test]
        async fn ambiguous_artifacts_are_reported_as_preserved() {
            let mut control = MockImageControl::new();
            control
                .expect_disable_unit()
                .returning(|_| UnitDisableReport::Disabled);
            control
                .expect_remove_image()
                .once()
                .returning(|_| ImageControlOutcome::Removed);
            let mut cleanup = MockManagedArtifactCleanup::new();
            cleanup.expect_cleanup().once().returning(|_| {
                ArtifactCleanupReport::PreservedAmbiguous(vec![
                    "legacy drop-in was preserved".into()
                ])
            });
            let (mut app, mut rx) = prepare_image_removal(control, cleanup);

            app.show_delete_dialog();
            app.action_remove();

            let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("remove task should finish")
                .expect("remove task should report a result");
            assert!(matches!(
                event,
                AppEvent::ActionDone(message, crate::ui::StatusLevel::Warn)
                    if message.contains("Removed image first")
                        && message.contains("legacy drop-in was preserved")
            ));
        }

        #[tokio::test]
        async fn removal_failure_never_runs_optional_cleanup() {
            let mut control = MockImageControl::new();
            control
                .expect_disable_unit()
                .returning(|_| UnitDisableReport::Disabled);
            control
                .expect_remove_image()
                .once()
                .returning(|_| ImageControlOutcome::Failed {
                    reason: "remove failed".into(),
                });
            let mut cleanup = MockManagedArtifactCleanup::new();
            cleanup.expect_cleanup().never();
            let (mut app, mut rx) = prepare_image_removal(control, cleanup);

            app.show_delete_dialog();
            app.action_remove();

            let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("remove task should finish")
                .expect("remove task should report a result");
            assert!(matches!(
                event,
                AppEvent::ActionDone(message, crate::ui::StatusLevel::Error)
                    if message == "Remove failed: remove failed"
            ));
        }

        #[test]
        fn confirmation_rejects_a_target_that_disappeared() {
            let mut control = MockImageControl::new();
            control.expect_disable_unit().never();
            control.expect_remove_image().never();
            let mut cleanup = MockManagedArtifactCleanup::new();
            cleanup.expect_cleanup().never();
            let (mut app, _rx) = prepare_image_removal(control, cleanup);

            app.show_delete_dialog();
            app.data.images.clear();
            app.action_remove();

            assert_eq!(
                app.ui
                    .status_message
                    .as_ref()
                    .map(|(message, _)| message.as_str()),
                Some("Image first changed before confirmation; refresh and try again.")
            );
        }
    }

    mod tar_risk_confirmation {
        use super::*;
        use crate::nspawn::ops::{BackendCommand, BackendResponse, PermissionLevel};
        use crate::ui::wizard::{Wizard, WizardStep};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        async fn prepare_confirmation() -> (
            App,
            tokio::sync::mpsc::Receiver<crate::nspawn::ops::BackendCommand>,
        ) {
            let mut app = make_app();
            let (command_tx, command_rx) = tokio::sync::mpsc::channel(4);
            let mut wizard = Wizard::new(
                vec![],
                vec![],
                false,
                command_tx,
                PermissionLevel::User,
                app.data.exec_ctx.clone(),
                app.config.clone(),
            )
            .await;
            wizard.step = WizardStep::Review;
            wizard.context.source.kind = crate::ui::wizard::context::SourceKind::Pull;
            wizard.context.source.pull_url = "https://example.test/rootfs.tar".into();
            wizard.context.source.is_pull_raw = false;
            wizard.context.basic.name = "tar-test".into();
            wizard.active_view = None;
            app.ui.wizard = Some(wizard);
            app.ui.show_wizard = true;
            app.handle_backend_result(BackendResponse::TarImportRiskConfirmationRequired(
                "GNU tar 1.34 lacks hard-link confinement".into(),
            ));
            (app, command_rx)
        }

        #[tokio::test]
        async fn topmost_tar_dialog_consumes_keys_and_decline_never_submits() {
            let (mut app, mut command_rx) = prepare_confirmation().await;
            assert!(app.ui.active_dialog.is_some());

            app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
                .await;
            assert!(app.ui.active_dialog.is_some());
            assert_eq!(app.ui.wizard.as_ref().unwrap().step, WizardStep::Review);
            assert!(command_rx.try_recv().is_err());

            app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))
                .await;
            assert!(app.ui.active_dialog.is_none());
            assert_eq!(app.ui.wizard.as_ref().unwrap().step, WizardStep::Review);
            assert!(command_rx.try_recv().is_err());
            assert!(!app
                .ui
                .wizard
                .as_ref()
                .unwrap()
                .context
                .deploy
                .cancellation
                .is_requested());
        }

        #[tokio::test]
        async fn accepting_tar_risk_submits_once_and_enter_does_not_cancel_deployment() {
            let (mut app, mut command_rx) = prepare_confirmation().await;

            app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
                .await;
            assert!(app.ui.active_dialog.is_none());
            let command = command_rx.try_recv().expect("confirmed deployment command");
            let BackendCommand::SubmitConfig(context) = command else {
                panic!("tar confirmation must submit the configured deployment");
            };
            assert!(context.source.unsafe_remote_tar_accepted());
            assert!(command_rx.try_recv().is_err());

            app.handle_backend_result(BackendResponse::DeployStarted);
            assert_eq!(app.ui.wizard.as_ref().unwrap().step, WizardStep::Deploy);
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .await;

            let wizard = app.ui.wizard.as_ref().unwrap();
            assert_eq!(wizard.step, WizardStep::Deploy);
            assert!(!wizard.context.deploy.cancellation.is_requested());
            assert!(command_rx.try_recv().is_err());
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

        #[test]
        fn image_navigation_is_independent_from_machine_selection() {
            let mut app = make_app();
            app.data.entries = vec![make_entry("machine", ContainerState::Running)];
            app.data.images = vec![make_image("a"), make_image("b")];
            app.ui.focus.active_idx = 1;

            app.select_next();

            assert_eq!(app.data.image_selected, 1);
            assert_eq!(app.data.selected, 0);
        }

        #[test]
        fn internal_image_navigation_has_an_independent_selection() {
            let mut app = make_app();
            app.data.images = vec![make_image("regular")];
            app.data.internal_images = vec![
                make_internal_image(".internal-a"),
                make_internal_image(".internal-b"),
            ];
            app.ui.focus.active_idx = 1;
            let _ = app.ui.image_list.handle_key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char(']'),
                    crossterm::event::KeyModifiers::NONE,
                ),
                app.data.images.len(),
            );

            app.select_next();

            assert_eq!(app.data.internal_image_selected, 1);
            assert_eq!(app.data.image_selected, 0);
        }
    }

    mod focus_and_modal_input {
        use super::*;
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;

        fn app_with_machine_and_image() -> App {
            let mut app = make_app();
            app.data.entries = vec![make_entry("machine", ContainerState::Running)];
            app.data.images = vec![make_image("image")];
            app.set_focus_idx(0);
            app
        }

        #[test]
        fn tab_cycle_pairs_each_list_with_its_inspector() {
            let mut app = app_with_machine_and_image();

            app.cycle_main_focus(true);
            assert_eq!(app.ui.focus.active_idx, 2);
            assert_eq!(app.ui.inspector_source, InspectorSource::Machine);
            assert_eq!(
                app.data.detail_target,
                DetailTarget::Machine("machine".into())
            );

            app.cycle_main_focus(true);
            assert_eq!(app.ui.focus.active_idx, 1);
            assert!(matches!(app.data.detail_target, DetailTarget::Image { .. }));

            app.cycle_main_focus(true);
            assert_eq!(app.ui.focus.active_idx, 2);
            assert_eq!(app.ui.inspector_source, InspectorSource::Image);
            assert!(matches!(app.data.detail_target, DetailTarget::Image { .. }));

            app.cycle_main_focus(true);
            assert_eq!(app.ui.focus.active_idx, 0);
        }

        #[test]
        fn reverse_tab_cycle_is_the_exact_inverse() {
            let mut app = app_with_machine_and_image();

            app.cycle_main_focus(false);
            assert_eq!(app.ui.focus.active_idx, 2);
            assert_eq!(app.ui.inspector_source, InspectorSource::Image);

            app.cycle_main_focus(false);
            assert_eq!(app.ui.focus.active_idx, 1);

            app.cycle_main_focus(false);
            assert_eq!(app.ui.focus.active_idx, 2);
            assert_eq!(app.ui.inspector_source, InspectorSource::Machine);

            app.cycle_main_focus(false);
            assert_eq!(app.ui.focus.active_idx, 0);
        }

        #[test]
        fn terminal_joins_the_cycle_and_maximized_mode_skips_inspectors() {
            let mut app = app_with_machine_and_image();
            app.data.terminal.show = true;
            app.ui.inspector_source = InspectorSource::Image;
            app.set_focus_idx(2);

            app.cycle_main_focus(true);
            assert_eq!(app.ui.focus.active_idx, 3);
            app.cycle_main_focus(true);
            assert_eq!(app.ui.focus.active_idx, 0);

            app.data.terminal.maximized = true;
            app.set_focus_idx(0);
            app.cycle_main_focus(true);
            assert_eq!(app.ui.focus.active_idx, 1);
            app.cycle_main_focus(true);
            assert_eq!(app.ui.focus.active_idx, 3);
        }

        #[test]
        fn terminal_is_taller_than_detail_by_default() {
            let ui = AppUi::new();
            assert!(ui.detail_pct < 50);
            assert_eq!(ui.detail_pct, 45);
        }

        #[test]
        fn focus_restore_tracks_the_latest_non_terminal_panel() {
            let mut app = make_app();
            app.set_focus_idx(1);
            app.set_focus_idx(3);
            assert_eq!(app.ui.prev_active_idx, 1);

            app.set_focus_idx(0);
            app.set_focus_idx(3);
            assert_eq!(app.ui.prev_active_idx, 0);
            app.restore_non_terminal_focus();
            assert_eq!(app.ui.focus.active_idx, 0);
        }

        #[test]
        fn inspector_keeps_the_last_image_as_its_terminal_resource() {
            let mut app = make_app();
            app.data.images = vec![make_image("workstation")];
            app.data.entries = vec![make_entry("workstation", ContainerState::Running)];

            app.set_focus_idx(1);
            app.set_focus_idx(2);

            let image = app
                .focused_image_resource()
                .expect("Inspector should retain the image target");
            assert_eq!(image.name, "workstation");
            assert!(app.image_has_running_machine(image));
        }

        #[test]
        fn image_terminal_requires_an_exact_running_machine() {
            let mut app = make_app();
            app.data.images = vec![make_image("workstation")];
            app.data.entries = vec![make_entry("workstation", ContainerState::Starting)];
            app.set_focus_idx(1);

            let image = app.focused_image_resource().unwrap();
            assert!(!app.image_has_running_machine(image));

            app.data.entries[0].state = ContainerState::Running;
            let image = app.focused_image_resource().unwrap();
            assert!(app.image_has_running_machine(image));
        }

        #[tokio::test]
        async fn inspector_overview_uses_its_retained_image_target() {
            let mut app = make_app();
            app.data.images = vec![make_image("retained"), make_image("list-selection")];
            app.data.image_selected = 1;
            app.data.detail_target = DetailTarget::Image {
                name: "retained".into(),
                internal: false,
            };
            app.ui.focus.active_idx = 2;
            app.ui.inspector_source = InspectorSource::Image;
            app.ui.detail_panel.active_pane =
                crate::ui::views::detail_panel::DetailPane::ImageOverview;

            app.refresh_detail().await;

            let image_properties = app
                .data
                .properties
                .as_ref()
                .unwrap()
                .get_group("Image")
                .unwrap();
            assert_eq!(image_properties.get("Name").unwrap(), "retained");
        }

        #[tokio::test]
        async fn image_unit_clears_stale_state_for_non_machine_image_name() {
            let name = "Ubuntu Resolute 镜像";
            let mut app = make_app();
            app.data.images = vec![make_image(name)];
            app.data.detail_target = DetailTarget::Image {
                name: name.into(),
                internal: false,
            };
            app.data.unit_name = Some("systemd-nspawn@stale.service".into());
            app.ui.focus.active_idx = 2;
            app.ui.inspector_source = InspectorSource::Image;
            app.ui.detail_panel.active_pane = crate::ui::views::detail_panel::DetailPane::ImageUnit;

            app.refresh_detail().await;

            assert!(app.data.unit_name.is_none());
            assert!(app.data.unit_drop_ins.is_empty());
            assert!(app.data.properties.as_ref().unwrap().groups.is_empty());
        }

        #[tokio::test]
        async fn visible_help_consumes_mouse_before_background_focus() {
            let mut app = make_app();
            app.ui.focus.active_idx = 2;
            app.ui.show_help = true;
            app.ui.panel_layout.machines = Rect::new(0, 0, 20, 20);

            app.handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 2,
                modifiers: KeyModifiers::NONE,
            })
            .await;

            assert_eq!(app.ui.focus.active_idx, 2);
        }
    }

    #[tokio::test]
    async fn image_refresh_sorts_by_name_and_preserves_selection() {
        let mut app = make_app();
        app.data.images = vec![make_image("selected")];
        app.data.internal_images = vec![make_internal_image(".selected-internal")];

        app.sync_snapshot(RuntimeSnapshot::new(
            vec![],
            vec![
                make_image("z-image"),
                make_image("selected"),
                make_image("a-image"),
                make_internal_image(".z-internal"),
                make_internal_image(".selected-internal"),
                make_internal_image(".a-internal"),
            ],
        ))
        .await;

        let names: Vec<_> = app
            .data
            .images
            .iter()
            .map(|image| image.name.as_str())
            .collect();
        assert_eq!(names, ["a-image", "selected", "z-image"]);
        assert_eq!(app.data.image_selected, 1);
        let internal_names: Vec<_> = app
            .data
            .internal_images
            .iter()
            .map(|image| image.name.as_str())
            .collect();
        assert_eq!(
            internal_names,
            [".a-internal", ".selected-internal", ".z-internal"]
        );
        assert_eq!(app.data.internal_image_selected, 1);
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
