use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};
use crossterm::event::{KeyCode, KeyEvent};
use crate::ui::wizard::{IStep, StepAction, WizardContext, SourceKind};
use crate::ui::wizard::render_hint;
use crate::ui::widgets::input::Input;
use crate::nspawn::StatusLevel;
use async_trait::async_trait;

pub struct BasicStep;

impl BasicStep {
    pub fn new() -> Self { Self }
}

#[async_trait]
impl IStep for BasicStep {
    fn title(&self) -> String { "Basic Settings".into() }

    fn next_step(&self, context: &WizardContext) -> Option<Box<dyn IStep>> {
        if context.source.kind == crate::ui::wizard::context::SourceKind::Copy {
            Some(Box::new(crate::ui::wizard::steps::review::ReviewStep::new()))
        } else {
            Some(Box::new(crate::ui::wizard::steps::storage::StorageStep::new()))
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, context: &WizardContext) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(2)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);

        let name_val = if context.basic.field_idx == 0 { format!("{}_", context.basic.name) } else { context.basic.name.clone() };
        Input::new(" Container name (required) ", &name_val)
            .focused(context.basic.field_idx == 0)
            .render(f, chunks[0]);

        let host_val = if context.basic.field_idx == 1 { format!("{}_", context.basic.hostname) } else { context.basic.hostname.clone() };
        Input::new(" Hostname (optional, defaults to name) ", &host_val)
            .focused(context.basic.field_idx == 1)
            .render(f, chunks[2]);

        render_hint(f, chunks[4], &["[Tab] switch field", "[Enter] next", "[Esc] back"][..]);
    }

    async fn handle_key(&mut self, key: KeyEvent, context: &mut WizardContext) -> StepAction {
        match key.code {
            KeyCode::Esc => StepAction::Prev,
            KeyCode::Tab => { context.basic.field_idx = 1 - context.basic.field_idx; StepAction::None }
            KeyCode::Backspace => {
                if context.basic.field_idx == 0 { context.basic.name.pop(); }
                else { context.basic.hostname.pop(); }
                StepAction::None
            }
            KeyCode::Char(c) => {
                if context.basic.field_idx == 0 { context.basic.name.push(c); }
                else { context.basic.hostname.push(c); }
                StepAction::None
            }
            KeyCode::Enter => {
                if context.basic.name.is_empty() {
                    StepAction::Status("Container name is required".into(), StatusLevel::Error)
                } else {
                    context.basic.field_idx = 0;
                    if context.source.kind == SourceKind::Copy {
                        let cp = context.build_config();
                        context.review.preview = cp.preview;
                        context.review.preview_scroll = 0;
                    }
                    StepAction::Next
                }
            }
            _ => StepAction::None,
        }
    }
}
