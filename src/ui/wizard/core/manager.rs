use crate::nspawn::ContainerEntry;
use crate::ui::core::{AppMessage, EventResult, WizardMessage};
use crate::ui::wizard::core::context::{SourceKind, WizardContext};
use crate::ui::wizard::steps::{self, StepComponent};
use crate::ui::wizard::{StepAction, WizardStep};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    widgets::{Block, BorderType, Borders, Clear},
    Frame,
};

pub struct Wizard {
    pub step: WizardStep,
    pub context: WizardContext,

    /// The active view for the current step.
    /// Recreated on step transitions to ensure fresh data from context.
    pub active_view: Option<Box<dyn StepComponent>>,

    pub command_tx: tokio::sync::mpsc::Sender<crate::nspawn::ops::BackendCommand>,
    pub loading: bool,
}

impl Wizard {
    pub async fn new(
        entries: Vec<ContainerEntry>,
        nvidia_toolkit_installed: bool,
        command_tx: tokio::sync::mpsc::Sender<crate::nspawn::ops::BackendCommand>,
    ) -> Self {
        let mut context = WizardContext::new(entries).await;
        context.passthrough.nvidia_toolkit_installed = nvidia_toolkit_installed;

        Self {
            step: WizardStep::Source,
            context,
            active_view: None,
            command_tx,
            loading: false,
        }
    }

    /// Look for builded view.
    fn sync_view(&mut self) {
        if self.active_view.is_none() {
            self.active_view = Some(steps::build_view(self.step, &self.context));
        }
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        self.sync_view();

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
            .border_type(BorderType::Rounded)
            .border_style(ratatui::style::Style::default().fg(ratatui::style::Color::Cyan));

        let inner = block.inner(area);
        f.render_widget(block, area);

        if self.loading {
            let loading_area = crate::ui::centered_rect(30, 10, inner);
            let spinner = ratatui::widgets::Paragraph::new("\n  Processing... Please wait  ")
                .alignment(ratatui::layout::Alignment::Center)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .title(" Working "),
                );
            f.render_widget(spinner, loading_area);
            return;
        }

        // Layout: Remove the extra Length(1) phantom row
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0)])
            .split(inner);

        if let Some(view) = &mut self.active_view {
            // Use the NEW reactive render_step with context
            view.render_step(f, chunks[0], &self.context);
        }
    }

    pub fn active_flow(&self) -> Vec<WizardStep> {
        let is_copy = self.context.source.kind == SourceKind::Copy;

        if is_copy {
            vec![
                WizardStep::Source,
                WizardStep::CopySelect,
                WizardStep::Basic,
                WizardStep::Review,
                WizardStep::Deploy,
            ]
        } else {
            let mut flow = vec![
                WizardStep::Source,
                WizardStep::Basic,
                WizardStep::User,
                WizardStep::Network,
                WizardStep::Passthrough,
                WizardStep::Devices,
                WizardStep::Review,
                WizardStep::Deploy,
            ];

            if !self.context.source.is_storage_managed_externally() {
                flow.insert(2, WizardStep::Storage);
            }
            flow
        }
    }

    fn total_steps(&self) -> usize {
        self.active_flow().len()
    }

    fn step_index(&self) -> usize {
        self.active_flow()
            .iter()
            .position(|&s| s == self.step)
            .unwrap_or(0)
    }

    fn resolve_next_step(&self, current: WizardStep) -> Option<WizardStep> {
        let flow = self.active_flow();
        let idx = flow.iter().position(|&s| s == current)?;
        flow.get(idx + 1).copied()
    }

    fn resolve_prev_step(&self, current: WizardStep) -> Option<WizardStep> {
        let flow = self.active_flow();
        let idx = flow.iter().position(|&s| s == current)?;
        if idx > 0 {
            Some(flow[idx - 1])
        } else {
            None
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> StepAction {
        if self.loading {
            return StepAction::None;
        }

        let res = if let Some(view) = &mut self.active_view {
            let result = view.handle_key(key);
            view.commit_to_context(&mut self.context);
            result
        } else {
            EventResult::Ignored
        };

        match res {
            EventResult::Message(msg) => self.process_message(msg),
            EventResult::Consumed => StepAction::None,
            EventResult::FocusNext | EventResult::FocusPrev => StepAction::None,
            EventResult::Ignored => match key.code {
                KeyCode::Esc => self.handle_action(StepAction::Prev),
                KeyCode::Char('q') => StepAction::Close,
                KeyCode::Enter => self.handle_action(StepAction::Next),
                _ => StepAction::None,
            },
        }
    }

    pub fn process_message(&mut self, msg: AppMessage) -> StepAction {
        match msg {
            AppMessage::Wizard(wiz_msg) => match wiz_msg {
                WizardMessage::Close => StepAction::Close,
                WizardMessage::Submit => {
                    if self.context.source.kind == SourceKind::Copy {
                        let source_cfg = self.context.source.clone_source.clone();
                        if !self.context.entries.iter().any(|e| e.name == source_cfg) {
                            return StepAction::Status(
                                format!(
                                    "Validation Error: Source container '{}' no longer exists",
                                    source_cfg
                                ),
                                crate::ui::StatusLevel::Error,
                            );
                        }
                    }
                    let target_name = self.context.basic.name.clone();
                    if self.context.entries.iter().any(|e| e.name == target_name) {
                        return StepAction::Status(
                            format!(
                                "Validation Error: Container '{}' already exists",
                                target_name
                            ),
                            crate::ui::StatusLevel::Error,
                        );
                    }

                    self.loading = true;
                    let tx = self.command_tx.clone();
                    let cmd = crate::nspawn::ops::BackendCommand::SubmitConfig(Box::new(
                        self.context.clone(),
                    ));
                    if tx.try_send(cmd).is_err() {
                        self.loading = false;
                        return StepAction::Status(
                            "Internal error: Backend channel busy or closed".into(),
                            crate::ui::StatusLevel::Error,
                        );
                    }
                    StepAction::None
                }
                _ => StepAction::None, // Rest handled by context mutations which we'll keep as-is for now
            },

            AppMessage::Backend(res) => {
                self.loading = false;
                match res {
                    crate::nspawn::ops::BackendResponse::ValidationSuccess => {
                        self.move_next();
                        StepAction::None
                    }
                    crate::nspawn::ops::BackendResponse::ValidationWarning(w) => {
                        self.move_next();
                        StepAction::Status(format!("Warning: {}", w), crate::ui::StatusLevel::Warn)
                    }
                    crate::nspawn::ops::BackendResponse::ValidationError(e) => {
                        StepAction::Status(format!("Error: {}", e), crate::ui::StatusLevel::Error)
                    }
                    crate::nspawn::ops::BackendResponse::DeployStarted => {
                        self.move_next();
                        StepAction::None
                    }
                    crate::nspawn::ops::BackendResponse::DeployFailed(e) => StepAction::Status(
                        format!("Deploy Failed: {}", e),
                        crate::ui::StatusLevel::Error,
                    ),
                }
            }
            _ => StepAction::None,
        }
    }

    pub fn handle_action(&mut self, action: StepAction) -> StepAction {
        match action {
            StepAction::Next => {
                if let Some(view) = &mut self.active_view {
                    if let Err(e) = view.validate() {
                        return StepAction::Status(e, crate::ui::StatusLevel::Error);
                    }
                    view.commit_to_context(&mut self.context);
                }

                // Trigger backend validation for network modes with interfaces
                if self.step == WizardStep::Network {
                    let mode = self.context.network.network_mode();
                    let (name, is_bridge) = match mode {
                        Some(crate::nspawn::models::NetworkMode::Bridge(n)) => (Some(n), true),
                        Some(crate::nspawn::models::NetworkMode::MacVlan(n))
                        | Some(crate::nspawn::models::NetworkMode::IpVlan(n))
                        | Some(crate::nspawn::models::NetworkMode::Interface(n)) => (Some(n), false),
                        _ => (None, false),
                    };

                    if let Some(name) = name {
                        self.loading = true;
                        let tx = self.command_tx.clone();
                        let _ = tx.try_send(crate::nspawn::ops::BackendCommand::ValidateInterface {
                            name,
                            is_bridge_mode: is_bridge,
                        });
                        return StepAction::None;
                    }
                }

                self.move_next();
                StepAction::None
            }
            StepAction::Prev => {
                if let Some(view) = &mut self.active_view {
                    // Try to save HEARTBEAT data, but don't block navigation if invalid
                    if view.validate().is_ok() {
                        view.commit_to_context(&mut self.context);
                    }
                }
                self.move_prev();
                StepAction::None
            }
            _ => action,
        }
    }

    fn move_next(&mut self) {
        if let Some(next_step) = self.resolve_next_step(self.step) {
            self.step = next_step;
            // Evict view so it's recreated with fresh context
            self.active_view = None;
        }
    }

    fn move_prev(&mut self) {
        if let Some(prev_step) = self.resolve_prev_step(self.step) {
            self.step = prev_step;
            self.active_view = None;
        }
    }
}
