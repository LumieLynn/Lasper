use crate::ui::core::{AppMessage, Component, EventResult, WizardMessage};
use crate::ui::widgets::display::text_block::TextBlock;
use crate::ui::wizard::context::WizardContext;
use crate::ui::wizard::steps::StepComponent;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{layout::Rect, Frame};

pub struct ReviewStepView {
    preview: TextBlock,
}

impl ReviewStepView {
    pub fn new(content: String) -> Self {
        Self {
            preview: TextBlock::new(" Preview .nspawn configuration ", content),
        }
    }
}

impl Component for ReviewStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        self.preview.set_focus(true);
        self.preview.render(f, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Enter => {
                return EventResult::Message(AppMessage::Wizard(WizardMessage::Submit))
            }
            _ => {}
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
    fn commit_to_context(&self, _ctx: &mut WizardContext) {
        // Preview is read-only view of context
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, context: &WizardContext) {
        // Reactive update: ensure preview reflects current context before rendering
        self.preview.set_content(context.build_preview_nspawn());
        self.render(f, area);
    }
}
