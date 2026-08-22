//! Main application state and event loop.

pub mod actions;
pub(crate) mod detail_refresh;
pub mod focus;
pub mod handlers;
pub mod modal;

use anyhow::Result;
use std::collections::HashMap;
use std::time::Instant;

use crate::application::provisioning::{ProvisioningPreparationService, ProvisioningService};
use crate::application::sessions::SessionService;
use crate::application::{
    ImageLifecycleService, MachineLifecycleService, RuntimeCatalog, RuntimeUpdate,
};
use crate::composition::{ApplicationServices, ExecutionContext};
use crate::nspawn::models::{
    ContainerEntry, ContainerMetrics, CpuRepresentation, ImageEntry, ImageName, RuntimeSnapshot,
};
use crate::tui::core::Component;
use crate::tui::events::{AppEvent, EventHandler};
use crate::tui::views::container_list::ContainerListComponent;
use crate::tui::views::detail_panel::DetailPanel;
use crate::tui::views::detail_panel::DetailTarget;
use crate::tui::views::image_list::ImageListComponent;
use crate::tui::wizard::Wizard;
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal};
use std::io::Stdout;

pub use crate::tui::views::terminal_panel::TerminalManager;

/// Whether the user is in panel resize mode (toggled by `R`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResizeMode {
    Inactive,
    Active,
}

pub use self::focus::WorkspaceFocus;
pub use self::modal::ModalLayer;

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
    dialog: crate::tui::widgets::dialogs::confirmation::ConfirmationDialog,
}

impl PendingImageRemoval {
    fn new(
        target: ImageName,
        dialog: crate::tui::widgets::dialogs::confirmation::ConfirmationDialog,
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

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> crate::tui::core::EventResult {
        self.dialog.handle_key(key)
    }
}

pub struct AppUi {
    /// Semantic destination currently owning focus on the main workspace.
    /// This deliberately distinguishes the two inspector contexts even
    /// though they share one visible detail panel.
    pub focus: WorkspaceFocus,
    /// Last non-terminal focus, used when closing or hiding the terminal.
    pub prev_focus: WorkspaceFocus,
    pub container_list: ContainerListComponent,
    pub image_list: ImageListComponent,
    pub detail_panel: DetailPanel,

    pub show_wizard: bool,
    pub show_help: bool,
    pub power_menu: Option<crate::tui::widgets::power_menu::PowerMenu>,
    pub pane_height: u16,

    pub wizard: Option<Wizard>,

    pub status_message: Option<(String, crate::tui::StatusLevel)>,
    pub status_expiry: Option<Instant>,
    pub app_tx: Option<tokio::sync::mpsc::Sender<AppEvent>>,
    pub quit_dialog: Option<crate::tui::widgets::dialogs::confirmation::ConfirmationDialog>,
    pub delete_dialog: Option<PendingImageRemoval>,
    pub active_dialog: Option<Box<dyn Component>>,
    next_wizard_instance: u64,
    next_deployment_preflight: u64,
    pending_deployment_preflight: Option<u64>,

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
            focus: WorkspaceFocus::Machines,
            prev_focus: WorkspaceFocus::Machines,
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
            app_tx: None,
            quit_dialog: None,
            delete_dialog: None,
            active_dialog: None,
            next_wizard_instance: 1,
            next_deployment_preflight: 1,
            pending_deployment_preflight: None,
            resize_mode: ResizeMode::Inactive,
            container_list_pct: 30,
            left_machines_pct: 50,
            detail_pct: 45,
            panel_layout: PanelLayout::default(),
            quit_tx: None,
        }
    }

    /// Return the interactive topmost overlay.  Rendering uses the same
    /// bottom-to-top order in `ui::layout`; input must only reach this layer.
    pub fn modal_layer(&self) -> Option<ModalLayer> {
        if self.active_dialog.is_some() {
            Some(ModalLayer::Dialog)
        } else if self.delete_dialog.is_some() {
            Some(ModalLayer::DeleteConfirmation)
        } else if self.quit_dialog.is_some() {
            Some(ModalLayer::QuitConfirmation)
        } else if self.show_help {
            Some(ModalLayer::Help)
        } else if self.show_wizard {
            Some(ModalLayer::Wizard)
        } else if self.power_menu.is_some() {
            Some(ModalLayer::PowerMenu)
        } else {
            None
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
    pub log_manager: crate::tui::views::detail_panel::log_manager::LogManager,
    pub config_content: Option<String>,
    pub config_path: Option<std::path::PathBuf>,
    pub detail_target: DetailTarget,
    pub unit_name: Option<String>,
    pub unit_drop_ins: Vec<crate::adapters::config::systemd_unit::SystemdDropIn>,
    pub dbus_active: bool,
    pub session_service: std::sync::Arc<SessionService>,
    pub runtime_catalog: std::sync::Arc<RuntimeCatalog>,
    pub machine_lifecycle: std::sync::Arc<MachineLifecycleService>,
    pub image_lifecycle: std::sync::Arc<ImageLifecycleService>,
    pub provisioning: std::sync::Arc<ProvisioningService>,
    pub provisioning_preparation: std::sync::Arc<ProvisioningPreparationService>,
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

    pub(crate) detail_refresh: detail_refresh::DetailRefreshState,

    // Terminal state
    pub terminal: TerminalManager,
}

/// Global application state.
pub struct App {
    pub permissions: std::sync::Arc<dyn crate::composition::PermissionManager>,
    pub config: std::sync::Arc<crate::config::AppConfig>,
    pub should_quit: bool,
    pub data: AppData,
    pub ui: AppUi,
}

impl App {
    pub fn new(
        permissions: std::sync::Arc<dyn crate::composition::PermissionManager>,
        cli_mode: bool,
        log_buffer_lines: usize,
        services: ApplicationServices,
        exec_ctx: std::sync::Arc<ExecutionContext>,
        config: std::sync::Arc<crate::config::AppConfig>,
    ) -> Self {
        let ApplicationServices {
            session: session_service,
            runtime: runtime_catalog,
            machine_lifecycle,
            image_lifecycle,
            provisioning,
            provisioning_preparation,
        } = services;
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
                log_manager: crate::tui::views::detail_panel::log_manager::LogManager::new(
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
                provisioning,
                provisioning_preparation,
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
                detail_refresh: detail_refresh::DetailRefreshState::default(),
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

    /// Set focus while keeping the last non-terminal destination available
    /// for restoring focus when the terminal is closed or hidden.
    pub(crate) fn set_focus(&mut self, focus: WorkspaceFocus) {
        if !focus.is_terminal() {
            self.ui.prev_focus = focus;
        }
        self.ui.focus = focus;
        self.update_detail_target();
    }

    pub(crate) fn cycle_main_focus(&mut self, forward: bool) {
        const WITHOUT_TERMINAL: &[WorkspaceFocus] = &[
            WorkspaceFocus::Machines,
            WorkspaceFocus::MachineInspector,
            WorkspaceFocus::Images,
            WorkspaceFocus::ImageInspector,
        ];
        const WITH_TERMINAL: &[WorkspaceFocus] = &[
            WorkspaceFocus::Machines,
            WorkspaceFocus::MachineInspector,
            WorkspaceFocus::Images,
            WorkspaceFocus::ImageInspector,
            WorkspaceFocus::Terminal,
        ];
        const MAXIMIZED_TERMINAL: &[WorkspaceFocus] = &[
            WorkspaceFocus::Machines,
            WorkspaceFocus::Images,
            WorkspaceFocus::Terminal,
        ];

        let slots = if self.data.terminal.is_showing() && self.data.terminal.maximized {
            MAXIMIZED_TERMINAL
        } else if self.data.terminal.is_showing() {
            WITH_TERMINAL
        } else {
            WITHOUT_TERMINAL
        };
        let current = self.ui.focus;
        let current_idx = slots.iter().position(|slot| *slot == current).unwrap_or(0);
        let next_idx = if forward {
            (current_idx + 1) % slots.len()
        } else {
            (current_idx + slots.len() - 1) % slots.len()
        };

        self.set_focus(slots[next_idx]);
    }

    pub(crate) fn restore_non_terminal_focus(&mut self) {
        let focus = if self.ui.prev_focus.is_terminal() {
            WorkspaceFocus::Machines
        } else {
            self.ui.prev_focus
        };
        self.set_focus(focus);
    }

    pub(crate) fn update_detail_target(&mut self) {
        let target = match self.ui.focus {
            WorkspaceFocus::Machines => self.machine_detail_target(),
            WorkspaceFocus::Images => self.image_detail_target(),
            WorkspaceFocus::ImageInspector => match &self.data.detail_target {
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
            },
            WorkspaceFocus::MachineInspector => match &self.data.detail_target {
                DetailTarget::Machine(name)
                    if self.data.entries.iter().any(|entry| entry.name == *name) =>
                {
                    self.data.detail_target.clone()
                }
                _ => self.machine_detail_target(),
            },
            WorkspaceFocus::Terminal => self
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
        };

        if target != self.data.detail_target {
            self.data.detail_target = target;
            self.ui
                .detail_panel
                .ensure_pane_for_target(&self.data.detail_target);
            // Background detail reads must never leave the previous target's
            // properties visible while the new request is in flight.
            self.data.properties = Ok(crate::nspawn::models::MachineProperties::default());
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
    fn sync_entries(&mut self, entries: Vec<ContainerEntry>) {
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
        if let Some(wizard) = &mut self.ui.wizard {
            wizard.draft.entries = self.data.entries.clone();
            wizard.draft.images = self.data.images.clone();
        }
    }

    /// Apply the independent machine/image snapshot returned by the backend.
    fn sync_snapshot(&mut self, snapshot: RuntimeSnapshot) {
        let RuntimeSnapshot { machines, images } = snapshot;
        let running: Vec<_> = machines
            .into_iter()
            .filter(|e| e.state.is_running())
            .collect();
        self.sync_entries(running);

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
        self.request_detail_refresh();
        if let Some(wizard) = &mut self.ui.wizard {
            wizard.draft.images = self.data.images.clone();
        }
    }

    fn sync_runtime_query(&mut self, query: crate::application::RuntimeQuery<RuntimeSnapshot>) {
        self.data.dbus_active = query.route.is_dbus();
        if let Some(fallback) = query.fallback {
            self.set_status(
                format!(
                    "{} unavailable: {}; using {}",
                    fallback.from.label(),
                    fallback.reason,
                    fallback.to.label()
                ),
                crate::tui::StatusLevel::Warn,
            );
        }
        self.sync_snapshot(query.value);
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
            AppEvent::WizardHardwareDiscoveryFinished { wizard_id, result } => {
                if self.ui.wizard.as_ref().map(Wizard::id) != Some(wizard_id) {
                    return;
                }
                let action = self
                    .ui
                    .wizard
                    .as_mut()
                    .map(|wizard| wizard.finish_hardware_discovery(result));
                if let Some(action) = action {
                    self.handle_wizard_action(action).await;
                }
            }
            AppEvent::WizardInterfaceValidationFinished { wizard_id, result } => {
                if self.ui.wizard.as_ref().map(Wizard::id) != Some(wizard_id) {
                    return;
                }
                let action = self
                    .ui
                    .wizard
                    .as_mut()
                    .map(|wizard| wizard.finish_interface_validation(result));
                if let Some(action) = action {
                    self.handle_wizard_action(action).await;
                }
            }
            AppEvent::DeploymentPreflightFinished {
                preflight_id,
                request,
                result,
            } => {
                if self.ui.pending_deployment_preflight != Some(preflight_id) {
                    return;
                }
                self.ui.pending_deployment_preflight = None;
                let action = self
                    .ui
                    .wizard
                    .as_mut()
                    .map(|wizard| wizard.finish_preflight(request, result));
                if let Some(action) = action {
                    self.handle_wizard_action(action).await;
                }
            }
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
        crate::tui::theme::init_theme(crate::tui::theme::load_theme(self.config.theme.as_ref()));

        let mut events = EventHandler::new(100);
        let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::channel::<RuntimeUpdate>(4);
        let (detail_refresh_tx, mut detail_refresh_rx) =
            tokio::sync::mpsc::channel::<detail_refresh::DetailRefreshCompletion>(1);

        self.ui.app_tx = Some(events.tx.clone());

        // Quit signal — the oneshot fires in the select! below so we
        // break out of the event loop immediately when the user confirms
        // quit, instead of blocking until a background task sends an event.
        let (quit_tx, mut quit_rx) = tokio::sync::oneshot::channel::<()>();
        self.ui.quit_tx = Some(quit_tx);

        // Start nspawn metrics collection engine
        crate::tui::effects::metrics::spawn_collector(
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
                        self.sync_runtime_query(snapshot);
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
                                crate::tui::StatusLevel::Warn,
                                std::time::Duration::from_secs(6),
                            );
                        }
                    }
                    Err(_) => break,
                }
            }

            // Drain per-buffer log channels before rendering
            self.data.log_manager.drain_all();

            // Detail reads are scheduled after input/observer batches have
            // coalesced, and never execute on the event-handler call stack.
            self.start_detail_refresh(&detail_refresh_tx);

            // Render a frame
            terminal.draw(|f| crate::tui::draw(f, self))?;

            tokio::select! {
                _ = &mut quit_rx => {
                    log::info!("[lasper] select!: quit_rx fired");
                    break;
                }
                Some(event) = events.rx.recv() => {
                    self.handle_event(event).await;
                    // Batch a bounded number of events so a busy PTY cannot
                    // starve rendering, keyboard input, or the quit signal.
                    for _ in 1..MAX_EVENTS_PER_FRAME {
                        let Ok(event) = events.rx.try_recv() else { break };
                        self.handle_event(event).await;
                    }
                }
                Some(completion) = detail_refresh_rx.recv() => {
                    self.apply_detail_refresh(completion);
                }
                changed = events.mouse_motion_rx.changed() => {
                    if changed.is_ok() {
                        let mouse = *events.mouse_motion_rx.borrow_and_update();
                        if let Some(mouse) = mouse {
                            self.handle_mouse(mouse).await;
                        }
                    }
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
    outcome: crate::application::MachineLifecycleOutcome,
) -> (String, crate::tui::StatusLevel) {
    use crate::application::MachineLifecycleResult;

    let fallback = outcome
        .fallback
        .map(|fallback| format!(" (CLI fallback: {})", fallback.reason))
        .unwrap_or_default();
    let machine = outcome.machine.as_str();
    match outcome.result {
        MachineLifecycleResult::Succeeded => (
            format!("{} {}{}", outcome.action.success_label(), machine, fallback),
            crate::tui::StatusLevel::Success,
        ),
        MachineLifecycleResult::NotAttempted(reason) => (
            format!(
                "{} {} was not attempted: {}{}",
                outcome.action.audit_label(),
                machine,
                reason,
                fallback
            ),
            crate::tui::StatusLevel::Error,
        ),
        MachineLifecycleResult::Rejected { reason, .. } => (
            format!(
                "{} {} was rejected: {}{}",
                outcome.action.audit_label(),
                machine,
                reason,
                fallback
            ),
            crate::tui::StatusLevel::Warn,
        ),
        MachineLifecycleResult::Failed(reason) => (
            format!(
                "{} {} failed: {}{}",
                outcome.action.audit_label(),
                machine,
                reason,
                fallback
            ),
            crate::tui::StatusLevel::Error,
        ),
        MachineLifecycleResult::OutcomeUnknown(reason) => (
            format!(
                "{} {} outcome is unknown: {}{}",
                outcome.action.audit_label(),
                machine,
                reason,
                fallback
            ),
            crate::tui::StatusLevel::Warn,
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
        let permissions = std::sync::Arc::new(crate::composition::DefaultPermissionManager::new());
        let exec_ctx = std::sync::Arc::new(
            crate::composition::ExecutionContext::new(
                crate::composition::PermissionLevel::User,
                None,
            )
            .unwrap(),
        );
        let services = crate::composition::compose_application_services(
            crate::composition::PermissionLevel::User,
            false,
            &exec_ctx,
        );
        App::new(
            permissions,
            false, // cli_mode
            0,
            services,
            exec_ctx,
            std::sync::Arc::new(crate::config::AppConfig::default()),
        )
    }

    #[tokio::test]
    async fn stale_background_result_cannot_mutate_a_reopened_wizard() {
        let mut app = make_app();
        let current_id = crate::tui::wizard::WizardInstanceId::new(2);
        app.ui.wizard = Some(crate::tui::wizard::Wizard::new(
            current_id,
            Vec::new(),
            Vec::new(),
            crate::composition::PermissionLevel::User,
            app.config.clone(),
            Default::default(),
            app.data.provisioning_preparation.clone(),
        ));
        assert!(
            app.ui
                .wizard
                .as_ref()
                .unwrap()
                .draft
                .passthrough
                .hardware_scanning
        );

        app.handle_event(AppEvent::WizardHardwareDiscoveryFinished {
            wizard_id: crate::tui::wizard::WizardInstanceId::new(1),
            result: Err(crate::application::provisioning::DeploymentError::failed(
                "stale result",
            )),
        })
        .await;

        let wizard = app.ui.wizard.as_ref().unwrap();
        assert_eq!(wizard.id(), current_id);
        assert!(wizard.draft.passthrough.hardware_scanning);
        assert!(app.ui.status_message.is_none());
    }

    mod image_start_transitions {
        use super::*;
        use crate::application::machine_lifecycle::{
            MachineControlOutcome, MockMachineControl, MockMachineObservation,
            MockMachineStartDiagnostics, MockMachineStartPreparation, RoutedMachineControlOutcome,
        };
        use crate::application::operations::route::ExecutionRoute;
        use crate::application::{MachineLifecycleResult, OperationRegistry};
        use crate::tui::events::AppEvent;

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
            app.ui.focus = WorkspaceFocus::Images;
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
        use crate::application::image_lifecycle::{
            ArtifactCleanupReport, ImageControlOutcome, MockImageControl, MockImageRuntime,
            MockManagedArtifactCleanup, UnitDisableReport,
        };
        use crate::application::OperationRegistry;
        use crate::tui::events::AppEvent;

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
            app.ui.focus = WorkspaceFocus::Images;
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
            app.ui.focus = WorkspaceFocus::Machines;
            app.action_remove();

            let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("remove task should finish")
                .expect("remove task should report a result");
            assert!(matches!(
                event,
                AppEvent::ActionDone(message, crate::tui::StatusLevel::Success)
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
            assert!(matches!(result, crate::tui::core::EventResult::Consumed));
            app.action_remove();

            let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("remove task should finish")
                .expect("remove task should report a result");
            assert!(matches!(
                event,
                AppEvent::ActionDone(message, crate::tui::StatusLevel::Success)
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
                AppEvent::ActionDone(message, crate::tui::StatusLevel::Warn)
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
                AppEvent::ActionDone(message, crate::tui::StatusLevel::Warn)
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
                AppEvent::ActionDone(message, crate::tui::StatusLevel::Error)
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
        use crate::application::provisioning::{
            DeploymentError, DeploymentExecutor, DeploymentJobContext, DeploymentPreflight,
            DeploymentSubmission, ProvisioningService, RemoteTarSafety, SourcePreflight,
        };
        use crate::composition::PermissionLevel;
        use crate::tui::core::{AppMessage, WizardMessage};
        use crate::tui::wizard::{Wizard, WizardStep};
        use async_trait::async_trait;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use std::sync::Arc;

        struct TarConfirmationPort;

        #[async_trait]
        impl SourcePreflight for TarConfirmationPort {
            async fn inspect_remote_tar(&self) -> Result<RemoteTarSafety, DeploymentError> {
                Ok(RemoteTarSafety::Risk(
                    "GNU tar 1.34 lacks hard-link confinement".into(),
                ))
            }
        }

        #[async_trait]
        impl DeploymentExecutor for TarConfirmationPort {
            async fn run(
                &self,
                _submission: DeploymentSubmission,
                _context: DeploymentJobContext,
            ) -> Result<(), DeploymentError> {
                std::future::pending().await
            }
        }

        async fn prepare_confirmation() -> (
            App,
            tokio::sync::mpsc::Receiver<crate::tui::events::AppEvent>,
        ) {
            let mut app = make_app();
            app.data.provisioning = Arc::new(ProvisioningService::new(
                Arc::new(TarConfirmationPort),
                Arc::new(TarConfirmationPort),
            ));
            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(4);
            app.ui.app_tx = Some(event_tx);
            let mut wizard = Wizard::new(
                crate::tui::wizard::WizardInstanceId::new(1),
                vec![],
                vec![],
                PermissionLevel::User,
                app.config.clone(),
                Default::default(),
                app.data.provisioning_preparation.clone(),
            );
            wizard.step = WizardStep::Review;
            wizard.draft.source.kind = crate::tui::wizard::draft::SourceKind::Pull;
            wizard.draft.source.pull_url = "https://example.test/rootfs.tar".into();
            wizard.draft.source.is_pull_raw = false;
            wizard.draft.basic.name = "tar-test".into();
            wizard.draft.user.root_password = "root-secret".into();
            wizard
                .draft
                .user
                .users
                .push(crate::tui::wizard::draft::UserDraft {
                    username: "alice".into(),
                    password: "user-secret".into(),
                    sudoer: false,
                    shell: "/bin/bash".into(),
                    wayland: None,
                });
            wizard.active_view = None;
            app.ui.wizard = Some(wizard);
            app.ui.show_wizard = true;
            let action = app
                .ui
                .wizard
                .as_mut()
                .unwrap()
                .process_message(AppMessage::Wizard(WizardMessage::Submit));
            app.handle_wizard_action(action).await;
            let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap();
            app.handle_event(event).await;
            (app, event_rx)
        }

        #[tokio::test]
        async fn topmost_tar_dialog_consumes_keys_and_decline_never_submits() {
            let (mut app, mut event_rx) = prepare_confirmation().await;
            assert!(app.ui.active_dialog.is_some());

            app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
                .await;
            assert!(app.ui.active_dialog.is_some());
            assert_eq!(app.ui.wizard.as_ref().unwrap().step, WizardStep::Review);
            assert!(event_rx.try_recv().is_err());
            assert_eq!(
                app.ui.wizard.as_ref().unwrap().draft.user.root_password,
                "root-secret"
            );
            assert_eq!(
                app.ui.wizard.as_ref().unwrap().draft.user.users[0].password,
                "user-secret"
            );

            app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))
                .await;
            assert!(app.ui.active_dialog.is_none());
            assert_eq!(app.ui.wizard.as_ref().unwrap().step, WizardStep::Review);
            assert!(event_rx.try_recv().is_err());
        }

        #[tokio::test]
        async fn accepting_tar_risk_submits_once_and_enter_does_not_cancel_deployment() {
            let (mut app, mut event_rx) = prepare_confirmation().await;

            app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
                .await;
            assert!(app.ui.active_dialog.is_none());
            let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap();
            let crate::tui::events::AppEvent::DeploymentPreflightFinished { request, .. } = &event
            else {
                panic!("confirmation must run deployment preflight");
            };
            assert!(request.allow_unsafe_remote_tar);
            app.handle_event(event).await;
            assert!(event_rx.try_recv().is_err());

            assert_eq!(app.ui.wizard.as_ref().unwrap().step, WizardStep::Deploy);
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .await;

            let wizard = app.ui.wizard.as_ref().unwrap();
            assert_eq!(wizard.step, WizardStep::Deploy);
            assert!(wizard.draft.user.root_password.is_empty());
            assert!(wizard.draft.user.users[0].password.is_empty());
            assert!(event_rx.try_recv().is_err());
        }

        #[tokio::test]
        async fn stale_preflight_completion_cannot_consume_wizard_secrets() {
            let (mut app, mut event_rx) = prepare_confirmation().await;
            app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
                .await;
            let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap();
            let (preflight_id, request) = match &event {
                crate::tui::events::AppEvent::DeploymentPreflightFinished {
                    preflight_id,
                    request,
                    ..
                } => (*preflight_id, request.clone()),
                _ => panic!("confirmation must run deployment preflight"),
            };

            app.handle_event(crate::tui::events::AppEvent::DeploymentPreflightFinished {
                preflight_id: preflight_id.wrapping_add(1),
                request,
                result: Ok(DeploymentPreflight::Ready),
            })
            .await;

            let wizard = app.ui.wizard.as_ref().unwrap();
            assert_eq!(wizard.step, WizardStep::Review);
            assert_eq!(wizard.draft.user.root_password, "root-secret");
            assert_eq!(wizard.draft.user.users[0].password, "user-secret");
            assert_eq!(app.ui.pending_deployment_preflight, Some(preflight_id));

            app.handle_event(event).await;
            assert_eq!(app.ui.wizard.as_ref().unwrap().step, WizardStep::Deploy);
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
            app.ui.focus = WorkspaceFocus::Images;

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
            app.ui.focus = WorkspaceFocus::Images;
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
            app.set_focus(WorkspaceFocus::Machines);
            app
        }

        #[test]
        fn modal_layer_reports_the_topmost_rendered_overlay() {
            use crate::tui::widgets::dialogs::confirmation::ConfirmationDialog;

            let mut ui = AppUi::new();
            ui.power_menu = Some(crate::tui::widgets::power_menu::PowerMenu::new(0));
            assert_eq!(ui.modal_layer(), Some(ModalLayer::PowerMenu));

            ui.show_wizard = true;
            assert_eq!(ui.modal_layer(), Some(ModalLayer::Wizard));
            ui.show_help = true;
            assert_eq!(ui.modal_layer(), Some(ModalLayer::Help));

            ui.quit_dialog = Some(ConfirmationDialog::new("Quit", "Confirm"));
            assert_eq!(ui.modal_layer(), Some(ModalLayer::QuitConfirmation));
            ui.delete_dialog = Some(PendingImageRemoval::new(
                ImageName::new("image").unwrap(),
                ConfirmationDialog::new("Remove", "Confirm"),
            ));
            assert_eq!(ui.modal_layer(), Some(ModalLayer::DeleteConfirmation));

            ui.active_dialog = Some(Box::new(ConfirmationDialog::new("Dialog", "Confirm")));
            assert_eq!(ui.modal_layer(), Some(ModalLayer::Dialog));
        }

        #[test]
        fn tab_cycle_pairs_each_list_with_its_inspector() {
            let mut app = app_with_machine_and_image();

            app.cycle_main_focus(true);
            assert_eq!(app.ui.focus, WorkspaceFocus::MachineInspector);
            assert_eq!(
                app.data.detail_target,
                DetailTarget::Machine("machine".into())
            );

            app.cycle_main_focus(true);
            assert_eq!(app.ui.focus, WorkspaceFocus::Images);
            assert!(matches!(app.data.detail_target, DetailTarget::Image { .. }));

            app.cycle_main_focus(true);
            assert_eq!(app.ui.focus, WorkspaceFocus::ImageInspector);
            assert!(matches!(app.data.detail_target, DetailTarget::Image { .. }));

            app.cycle_main_focus(true);
            assert_eq!(app.ui.focus, WorkspaceFocus::Machines);
        }

        #[test]
        fn reverse_tab_cycle_is_the_exact_inverse() {
            let mut app = app_with_machine_and_image();

            app.cycle_main_focus(false);
            assert_eq!(app.ui.focus, WorkspaceFocus::ImageInspector);

            app.cycle_main_focus(false);
            assert_eq!(app.ui.focus, WorkspaceFocus::Images);

            app.cycle_main_focus(false);
            assert_eq!(app.ui.focus, WorkspaceFocus::MachineInspector);

            app.cycle_main_focus(false);
            assert_eq!(app.ui.focus, WorkspaceFocus::Machines);
        }

        #[test]
        fn terminal_joins_the_cycle_and_maximized_mode_skips_inspectors() {
            let mut app = app_with_machine_and_image();
            app.data.terminal.show = true;
            app.set_focus(WorkspaceFocus::ImageInspector);

            app.cycle_main_focus(true);
            assert_eq!(app.ui.focus, WorkspaceFocus::Terminal);
            app.cycle_main_focus(true);
            assert_eq!(app.ui.focus, WorkspaceFocus::Machines);

            app.data.terminal.maximized = true;
            app.set_focus(WorkspaceFocus::Machines);
            app.cycle_main_focus(true);
            assert_eq!(app.ui.focus, WorkspaceFocus::Images);
            app.cycle_main_focus(true);
            assert_eq!(app.ui.focus, WorkspaceFocus::Terminal);
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
            app.set_focus(WorkspaceFocus::Images);
            app.set_focus(WorkspaceFocus::Terminal);
            assert_eq!(app.ui.prev_focus, WorkspaceFocus::Images);

            app.set_focus(WorkspaceFocus::Machines);
            app.set_focus(WorkspaceFocus::Terminal);
            assert_eq!(app.ui.prev_focus, WorkspaceFocus::Machines);
            app.restore_non_terminal_focus();
            assert_eq!(app.ui.focus, WorkspaceFocus::Machines);
        }

        #[test]
        fn inspector_keeps_the_last_image_as_its_terminal_resource() {
            let mut app = make_app();
            app.data.images = vec![make_image("workstation")];
            app.data.entries = vec![make_entry("workstation", ContainerState::Running)];

            app.set_focus(WorkspaceFocus::Images);
            app.set_focus(WorkspaceFocus::ImageInspector);

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
            app.set_focus(WorkspaceFocus::Images);

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
            app.ui.focus = WorkspaceFocus::ImageInspector;
            app.ui.detail_panel.active_pane =
                crate::tui::views::detail_panel::DetailPane::ImageOverview;

            app.refresh_detail_now().await;

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
            app.ui.focus = WorkspaceFocus::ImageInspector;
            app.ui.detail_panel.active_pane =
                crate::tui::views::detail_panel::DetailPane::ImageUnit;

            app.refresh_detail_now().await;

            assert!(app.data.unit_name.is_none());
            assert!(app.data.unit_drop_ins.is_empty());
            assert!(app.data.properties.as_ref().unwrap().groups.is_empty());
        }

        #[test]
        fn completed_detail_read_cannot_overwrite_a_new_selection() {
            use crate::tui::app::detail_refresh::{DetailRefreshCompletion, DetailRefreshResult};

            let mut app = make_app();
            app.data.entries = vec![
                make_entry("first", ContainerState::Running),
                make_entry("second", ContainerState::Running),
            ];
            app.set_focus(WorkspaceFocus::Machines);
            app.request_detail_refresh();
            let stale_ticket = app.data.detail_refresh.take_pending().unwrap();

            app.data.selected = 1;
            app.request_detail_refresh();
            assert_eq!(
                app.data.detail_target,
                DetailTarget::Machine("second".into())
            );

            let mut stale_properties = crate::nspawn::models::MachineProperties::default();
            stale_properties.insert("Machine", "Name".into(), "first".into());
            app.apply_detail_refresh(DetailRefreshCompletion {
                ticket: stale_ticket,
                result: DetailRefreshResult::ImageOverview(stale_properties),
            });

            assert!(app.data.properties.as_ref().unwrap().groups.is_empty());
            assert!(app.data.detail_refresh.has_pending());
        }

        #[tokio::test]
        async fn visible_help_consumes_mouse_before_background_focus() {
            let mut app = make_app();
            app.ui.focus = WorkspaceFocus::MachineInspector;
            app.ui.show_help = true;
            app.ui.panel_layout.machines = Rect::new(0, 0, 20, 20);

            app.handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 2,
                modifiers: KeyModifiers::NONE,
            })
            .await;

            assert_eq!(app.ui.focus, WorkspaceFocus::MachineInspector);
        }
    }

    #[test]
    fn image_refresh_sorts_by_name_and_preserves_selection() {
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
        ));

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
            app.set_status("hello".into(), crate::tui::StatusLevel::Info);

            let (msg, level) = app.ui.status_message.as_ref().unwrap();
            assert_eq!(msg, "hello");
            assert_eq!(*level, crate::tui::StatusLevel::Info);
            assert!(app.ui.status_expiry.is_some());
        }
    }
}
