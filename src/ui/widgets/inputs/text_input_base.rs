use crate::ui::core::EventResult;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::Style,
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};
use tui_input::backend::crossterm::EventHandler;
pub use tui_input::Input;

pub struct TextInputBase {
    pub input: Input,
    pub label: String,
    pub focused: bool,
    pub enabled: bool,
    pub error_msg: Option<String>,
}

impl TextInputBase {
    pub fn new(label: impl Into<String>, initial_value: String) -> Self {
        Self {
            input: Input::from(initial_value),
            label: label.into(),
            focused: false,
            enabled: true,
            error_msg: None,
        }
    }

    pub fn render_base(&mut self, f: &mut Frame, area: Rect) {
        let style = if self.error_msg.is_some() {
            Style::default().fg(crate::ui::theme::theme().editor_error)
        } else {
            Style::default().fg(crate::ui::widget_border_color(self.focused, self.enabled))
        };

        let mut title = self.label.clone();
        if let Some(error) = &self.error_msg {
            title.push_str(&format!(" [ {} ]", error));
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(title)
            .border_style(style);

        let value = self.input.value();
        let paragraph = Paragraph::new(value).block(block);
        f.render_widget(paragraph, area);

        if self.focused && self.enabled {
            let width = area.width.saturating_sub(2);
            let scroll = self.input.visual_scroll(width as usize);
            let cursor_pos = self.input.visual_cursor().saturating_sub(scroll);

            f.set_cursor_position((area.x + 1 + cursor_pos as u16, area.y + 1));
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if !self.enabled {
            return EventResult::Ignored;
        }

        match key.code {
            KeyCode::Tab => EventResult::FocusNext,
            KeyCode::BackTab => EventResult::FocusPrev,
            KeyCode::Esc => EventResult::Ignored,
            _ => {
                if self
                    .input
                    .handle_event(&crossterm::event::Event::Key(key))
                    .is_some()
                {
                    EventResult::Consumed
                } else {
                    EventResult::Ignored
                }
            }
        }
    }
}
