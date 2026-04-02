use crate::nspawn::ContainerEntry;
use crate::ui::core::{AppMessage, EventResult, WizardMessage};
use crate::ui::wizard::context::{SourceKind, WizardContext};
use crate::ui::wizard::steps::{self, StepComponent};
use crate::ui::wizard::{StepAction, WizardStep};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    widgets::{Block, Borders, Clear},
    Frame,
};

pub struct Wizard {
    pub step: WizardStep,
    pub context: WizardContext,

    /// The active view for the current step.
    /// Recreated on step transitions to ensure fresh data from context.
    pub active_view: Option<Box<dyn StepComponent>>,

    pub command_tx: tokio::sync::mpsc::Sender<crate::ui::core::BackendCommand>,
    pub loading: bool,
}

impl Wizard {
    pub fn new(
        entries: Vec<ContainerEntry>,
        nvidia_toolkit_installed: bool,
        command_tx: tokio::sync::mpsc::Sender<crate::ui::core::BackendCommand>,
    ) -> Self {
        let mut context = WizardContext::new(entries);
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
            vec![
                WizardStep::Source,
                WizardStep::Basic,
                WizardStep::Storage,
                WizardStep::User,
                WizardStep::Network,
                WizardStep::Passthrough,
                WizardStep::Devices,
                WizardStep::Review,
                WizardStep::Deploy,
            ]
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
            EventResult::FocusNext => self.handle_action(StepAction::Next),
            EventResult::FocusPrev => self.handle_action(StepAction::Prev),
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
                WizardMessage::NextStep => self.handle_action(StepAction::Next),
                WizardMessage::PrevStep => self.handle_action(StepAction::Prev),
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
                                crate::nspawn::StatusLevel::Error,
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
                            crate::nspawn::StatusLevel::Error,
                        );
                    }

                    self.loading = true;
                    let tx = self.command_tx.clone();
                    let cmd = crate::ui::core::BackendCommand::SubmitConfig(Box::new(
                        self.context.clone(),
                    ));
                    if tx.try_send(cmd).is_err() {
                        self.loading = false;
                        return StepAction::Status(
                            "Internal error: Backend channel busy or closed".into(),
                            crate::nspawn::StatusLevel::Error,
                        );
                    }
                    StepAction::None
                }
                _ => StepAction::None, // Rest handled by context mutations which we'll keep as-is for now
            },

            AppMessage::Backend(res) => {
                self.loading = false;
                match res {
                    crate::ui::core::BackendResponse::ValidationSuccess => {
                        self.handle_action(StepAction::Next)
                    }
                    crate::ui::core::BackendResponse::ValidationError(e) => StepAction::Status(
                        format!("Error: {}", e),
                        crate::nspawn::StatusLevel::Error,
                    ),
                    crate::ui::core::BackendResponse::DeployStarted => {
                        self.handle_action(StepAction::Next)
                    }
                    crate::ui::core::BackendResponse::DeployFailed(e) => StepAction::Status(
                        format!("Deploy Failed: {}", e),
                        crate::nspawn::StatusLevel::Error,
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
                        return StepAction::Status(e, crate::nspawn::StatusLevel::Error);
                    }
                    view.commit_to_context(&mut self.context);
                }

                if let Some(next_step) = self.resolve_next_step(self.step) {
                    self.step = next_step;
                    // Evict view so it's recreated with fresh context
                    self.active_view = None;
                }
                StepAction::None
            }
            StepAction::Prev => {
                if let Some(view) = &mut self.active_view {
                    // Try to save HEARTBEAT data, but don't block navigation if invalid
                    if view.validate().is_ok() {
                        view.commit_to_context(&mut self.context);
                    }
                }
                if let Some(prev_step) = self.resolve_prev_step(self.step) {
                    self.step = prev_step;
                    self.active_view = None;
                }
                StepAction::None
            }
            _ => action,
        }
    }
}
