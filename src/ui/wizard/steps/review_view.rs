use crate::ui::core::{AppMessage, Component, EventResult};
use crate::ui::widgets::display::text_block::TextBlock;
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
            KeyCode::Enter => return EventResult::Message(AppMessage::Submit),
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
}
