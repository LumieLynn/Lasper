use crate::tui::core::{AppMessage, Component, EventResult, WizardMessage};
use crate::tui::widgets::display::text_block::TextBlock;
use crate::tui::wizard::draft::WizardDraft;
use crate::tui::wizard::steps::StepComponent;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};

pub struct ReviewStepView {
    preview: TextBlock,
}

impl ReviewStepView {
    pub fn new(content: String) -> Self {
        Self {
            preview: TextBlock::new(" Deployment Preview ", content),
        }
    }
}

impl Component for ReviewStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let area = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([Constraint::Min(0)])
            .split(area)[0];
        self.preview.set_focus(true);
        self.preview.render(f, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if key.code == KeyCode::Enter {
            return EventResult::Message(AppMessage::Wizard(WizardMessage::Submit));
        }

        self.preview.handle_key(key)
    }

    fn set_focus(&mut self, focused: bool) {
        self.preview.set_focus(focused);
    }

    fn is_focused(&self) -> bool {
        self.preview.is_focused()
    }

    fn validate(&mut self) -> Result<(), String> {
        Ok(())
    }
}

impl StepComponent for ReviewStepView {
    fn commit_to_draft(&self, _ctx: &mut WizardDraft) {
        // Preview is read-only view of context
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardDraft) {
        self.render(f, area);
    }
}
