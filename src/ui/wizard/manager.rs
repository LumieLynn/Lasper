use crate::nspawn::ContainerEntry;
use crate::ui::core::{AppMessage, Component, EventResult};
use crate::ui::wizard::context::{SourceKind, WizardContext};
use crate::ui::wizard::steps;
use crate::ui::wizard::{StepAction, WizardStep};
use crossterm::event::{KeyEvent, KeyCode};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    widgets::{Block, Borders, Clear},
    Frame,
};
use std::collections::HashMap;

pub struct Wizard {
    pub step: WizardStep,
    pub context: WizardContext,
    pub views: HashMap<WizardStep, Box<dyn Component>>,
    pub command_tx: tokio::sync::mpsc::UnboundedSender<crate::ui::core::BackendCommand>,
    pub loading: bool,
}

impl Wizard {
    pub fn new(
        entries: Vec<ContainerEntry>,
        nvidia_toolkit_installed: bool,
        command_tx: tokio::sync::mpsc::UnboundedSender<crate::ui::core::BackendCommand>,
    ) -> Self {
        let mut context = WizardContext::new(entries);
        context.passthrough.nvidia_toolkit_installed = nvidia_toolkit_installed;

        let views = HashMap::new();

        Self {
            step: WizardStep::Source,
            context,
            views,
            command_tx,
            loading: false,
        }
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        self.ensure_view_exists();

        let area = crate::ui::centered_rect(80, 80, area);
        f.render_widget(Clear, area);

        let block = Block::default()
            .title(format!(
                " {} - Step {}/{} ",
                self.step.title(),
                self.step_index() + 1,
                self.total_steps()
            ))
            .borders(Borders::ALL)
            .border_style(ratatui::style::Style::default().fg(ratatui::style::Color::Cyan));

        let inner = block.inner(area);
        f.render_widget(block, area);

        if self.loading {
            let loading_area = crate::ui::centered_rect(30, 10, inner);
            let spinner = ratatui::widgets::Paragraph::new("\n  Processing... Please wait  ")
                .alignment(ratatui::layout::Alignment::Center)
                .block(Block::default().borders(Borders::ALL).title(" Working "));
            f.render_widget(spinner, loading_area);
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(inner);

        if let Some(view) = self.views.get_mut(&self.step) {
            view.render(f, chunks[0]);
        }
    }

    fn total_steps(&self) -> usize {
        if self.context.source.kind == SourceKind::Copy {
            5 // Source, CopySelect, Basic, Review, Deploy
        } else {
            10 // Full Flow
        }
    }

    fn step_index(&self) -> usize {
        let is_copy = self.context.source.kind == SourceKind::Copy;

        match self.step {
            WizardStep::Source => 0,
            WizardStep::CopySelect => 1,
            WizardStep::Basic => {
                if is_copy {
                    2
                } else {
                    1
                }
            }
            WizardStep::Storage => 2,
            WizardStep::User => 3,
            WizardStep::Network => 4,
            WizardStep::Passthrough => 5,
            WizardStep::Devices => 6,
            WizardStep::Review => {
                if is_copy {
                    3
                } else {
                    7
                }
            }
            WizardStep::Deploy => {
                if is_copy {
                    4
                } else {
                    8
                }
            }
        }
    }

    fn resolve_next_step(&self, current: WizardStep) -> Option<WizardStep> {
        let is_copy = self.context.source.kind == SourceKind::Copy;

        match current {
            WizardStep::Source => {
                if is_copy {
                    Some(WizardStep::CopySelect)
                } else {
                    Some(WizardStep::Basic)
                }
            }
            WizardStep::CopySelect => Some(WizardStep::Basic),
            WizardStep::Basic => {
                if is_copy {
                    Some(WizardStep::Review)
                } else {
                    Some(WizardStep::Storage)
                }
            }
            WizardStep::Storage => Some(WizardStep::User),
            WizardStep::User => Some(WizardStep::Network),
            WizardStep::Network => Some(WizardStep::Passthrough),
            WizardStep::Passthrough => Some(WizardStep::Devices),
            WizardStep::Devices => Some(WizardStep::Review),
            WizardStep::Review => Some(WizardStep::Deploy),
            WizardStep::Deploy => None,
        }
    }

    fn resolve_prev_step(&self, current: WizardStep) -> Option<WizardStep> {
        let is_copy = self.context.source.kind == SourceKind::Copy;

        match current {
            WizardStep::Deploy => Some(WizardStep::Review),
            WizardStep::Review => {
                if is_copy {
                    Some(WizardStep::Basic)
                } else {
                    Some(WizardStep::Devices)
                }
            }
            WizardStep::Devices => Some(WizardStep::Passthrough),
            WizardStep::Passthrough => Some(WizardStep::Network),
            WizardStep::Network => Some(WizardStep::User),
            WizardStep::User => Some(WizardStep::Storage),
            WizardStep::Storage => Some(WizardStep::Basic),
            WizardStep::Basic => {
                if is_copy {
                    Some(WizardStep::CopySelect)
                } else {
                    Some(WizardStep::Source)
                }
            }
            WizardStep::CopySelect => Some(WizardStep::Source),
            WizardStep::Source => None,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> StepAction {
        if self.loading {
            return StepAction::None;
        }

        let res = if let Some(view) = self.views.get_mut(&self.step) {
            view.handle_key(key)
        } else {
            EventResult::Ignored
        };

        match res {
            EventResult::Message(msg) => self.process_message(msg),
            EventResult::Consumed => StepAction::None,
            EventResult::FocusNext => self.handle_action(StepAction::Next),
            EventResult::FocusPrev => self.handle_action(StepAction::Prev),
            EventResult::Ignored => {
                match key.code {
                    KeyCode::Esc => self.handle_action(StepAction::Prev),
                    KeyCode::Char('q') => StepAction::Close,
                    KeyCode::Enter => self.handle_action(StepAction::Next),
                    _ => StepAction::None,
                }
            }
        }
    }

    fn ensure_view_exists(&mut self) {
        if self.views.contains_key(&self.step) {
            return;
        }

        match self.step {
            WizardStep::Source => {
                let initial = self.context.source.extract_config();
                self.views.insert(
                    self.step,
                    Box::new(steps::source_view::SourceStepView::new(&initial)),
                );
            }
            WizardStep::CopySelect => {
                self.views.insert(
                    self.step,
                    Box::new(steps::copy_select_view::CopySelectStepView::new(
                        &self.context.entries,
                        self.context.source.copy_idx,
                    )),
                );
            }
            WizardStep::Basic => {
                let initial = self.context.basic.extract_config();
                self.views.insert(
                    self.step,
                    Box::new(steps::basic_view::BasicStepView::new(&initial)),
                );
            }
            WizardStep::Storage => {
                let initial = self.context.storage.extract_config();
                self.views.insert(
                    self.step,
                    Box::new(steps::storage_view::StorageStepView::new(
                        &initial,
                        self.context.storage.info.clone(),
                    )),
                );
            }
            WizardStep::User => {
                let initial = self.context.user.extract_config();
                self.views.insert(
                    self.step,
                    Box::new(steps::user_view::UserStepView::new(&initial)),
                );
            }
            WizardStep::Network => {
                let initial = self.context.network.extract_config();
                self.views.insert(
                    self.step,
                    Box::new(steps::network_view::NetworkStepView::new(
                        &initial,
                        &self.context.network.bridge_list,
                    )),
                );
            }
            WizardStep::Passthrough => {
                let initial = self
                    .context
                    .passthrough
                    .extract_config(self.context.network.network_mode());
                let nw_mode = self.context.network.network_mode();
                self.views.insert(
                    self.step,
                    Box::new(steps::passthrough_view::PassthroughStepView::new(
                        &initial,
                        nw_mode,
                        self.context.passthrough.nvidia_toolkit_installed,
                    )),
                );
            }
            WizardStep::Devices => {
                let initial = self
                    .context
                    .passthrough
                    .extract_config(self.context.network.network_mode());
                self.views.insert(
                    self.step,
                    Box::new(steps::devices_view::DevicesStepView::new(&initial)),
                );
            }
            WizardStep::Review => {
                let preview = self.context.build_preview_nspawn();
                self.views.insert(
                    self.step,
                    Box::new(steps::review_view::ReviewStepView::new(preview)),
                );
            }
            WizardStep::Deploy => {
                self.views.insert(
                    self.step,
                    Box::new(steps::deploy_view::DeployStepView::new(
                        self.context.deploy.log_tx.clone(),
                        self.context.deploy.done.clone(),
                        self.context.deploy.success.clone(),
                    )),
                );
            }
        }
    }

    pub fn process_message(&mut self, msg: AppMessage) -> StepAction {
        match msg {
            AppMessage::StepNext => return self.handle_action(StepAction::Next),
            AppMessage::Submit => {
                self.loading = true;
                let _ = self
                    .command_tx
                    .send(crate::ui::core::BackendCommand::SubmitConfig(Box::new(
                        self.context.clone(),
                    )));
                return StepAction::None;
            }
            AppMessage::StepPrev => return self.handle_action(StepAction::Prev),
            AppMessage::Close => return StepAction::Close,

            AppMessage::BackendResult(res) => {
                self.loading = false;
                match res {
                    crate::ui::core::BackendResponse::ValidationSuccess => {
                        return self.handle_action(StepAction::Next);
                    }
                    crate::ui::core::BackendResponse::ValidationError(e) => {
                        return StepAction::Status(
                            format!("Error: {}", e),
                            crate::nspawn::StatusLevel::Error,
                        );
                    }
                    crate::ui::core::BackendResponse::DeployStarted => {
                        return self.handle_action(StepAction::Next);
                    }
                    crate::ui::core::BackendResponse::DeployFailed(e) => {
                        return StepAction::Status(
                            format!("Deploy Failed: {}", e),
                            crate::nspawn::StatusLevel::Error,
                        );
                    }
                }
            }

            // Source State
            AppMessage::SourceUrlUpdated(url) => self.context.source.oci_url = url,
            AppMessage::SourceKindUpdated(kind) => self.context.source.kind = kind,
            AppMessage::SourceMirrorUpdated(m) => self.context.source.deboot_mirror = m,
            AppMessage::SourceSuiteUpdated(s) => self.context.source.deboot_suite = s,
            AppMessage::SourcePkgsUpdated(p) => self.context.source.pacstrap_pkgs = p,
            AppMessage::SourceDiskPathUpdated(d) => self.context.source.disk_path = d,
            AppMessage::SourceCloneIdxUpdated(idx) => {
                self.context.source.copy_idx = idx;
                if let Some(entry) = self.context.entries.get(idx).cloned() {
                    self.context.source.clone_source = entry.name;
                }
            }

            // Basic State
            AppMessage::BaseNameUpdated(n) => self.context.basic.name = n,
            AppMessage::BaseHostnameUpdated(h) => self.context.basic.hostname = h,

            // Storage State
            AppMessage::StorageTypeUpdated(idx) => self.context.storage.type_idx = idx,
            AppMessage::StorageSizeUpdated(s) => self.context.storage.raw_size = s,
            AppMessage::StorageFsUpdated(f) => self.context.storage.raw_fs = f,
            AppMessage::StoragePartitionUpdated(p) => self.context.storage.raw_partition = p,

            // User State
            AppMessage::RootPasswordUpdated(p) => self.context.user.root_password = p,
            AppMessage::UserAdded(u) => self.context.user.users.push(u),
            AppMessage::UserRemoved(idx) => {
                if idx < self.context.user.users.len() {
                    self.context.user.users.remove(idx);
                }
            }

            // Network State
            AppMessage::NetworkModeUpdated(mode) => self.context.network.mode = mode,
            AppMessage::NetworkBridgeUpdated(b) => self.context.network.bridge_name = b,
            AppMessage::PortForwardAdded(p) => self.context.network.port_list.push(p),
            AppMessage::PortForwardRemoved(idx) => {
                if idx < self.context.network.port_list.len() {
                    self.context.network.port_list.remove(idx);
                }
            }

            // Passthrough State
            AppMessage::GenericGpuUpdated(g) => self.context.passthrough.full_capabilities = g,
            AppMessage::WaylandSocketUpdated(w) => self.context.passthrough.wayland_socket = w,
            AppMessage::NvidiaGpuUpdated(n) => self.context.passthrough.nvidia_gpu = n,
            AppMessage::BindMountAdded(b) => self.context.passthrough.bind_mounts.push(b),
            AppMessage::BindMountRemoved(idx) => {
                if idx < self.context.passthrough.bind_mounts.len() {
                    self.context.passthrough.bind_mounts.remove(idx);
                }
            }
            AppMessage::DialogSubmit | AppMessage::DialogCancel => {} // Handled by inline editors
        }
        StepAction::None
    }

    fn handle_action(&mut self, action: StepAction) -> StepAction {
        match action {
            StepAction::Next => {
                if let Some(view) = self.views.get_mut(&self.step) {
                    if let Err(_) = view.validate() {
                        return StepAction::None;
                    }
                }

                if let Some(next_step) = self.resolve_next_step(self.step) {
                    self.step = next_step;
                    if matches!(
                        self.step,
                        WizardStep::Passthrough | WizardStep::Devices | WizardStep::Review
                    ) {
                        self.views.remove(&self.step);
                    }
                }
                StepAction::None
            }
            StepAction::Prev => {
                if let Some(prev_step) = self.resolve_prev_step(self.step) {
                    self.step = prev_step;
                    if matches!(
                        self.step,
                        WizardStep::Passthrough | WizardStep::Devices | WizardStep::Review
                    ) {
                        self.views.remove(&self.step);
                    }
                }
                StepAction::None
            }
            _ => action,
        }
    }
}
