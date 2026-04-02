use crate::ui::core::{Component, EventResult};
use crate::ui::widgets::inputs::text_input_base::TextInputBase;
use crossterm::event::KeyEvent;
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

pub struct PasswordBox {
    base: TextInputBase,
    validator: Option<Box<dyn Fn(&str) -> Result<(), String>>>,
}


impl PasswordBox {
    pub fn new(label: impl Into<String>, initial_value: String) -> Self {
        Self {
            base: TextInputBase::new(label, initial_value),
            validator: None,
        }
    }


    pub fn value(&self) -> &str {
        self.base.input.value()
    }

    pub fn set_value(&mut self, value: String) {
        self.base.input = tui_input::Input::from(value);
    }


    pub fn with_validator<F>(mut self, f: F) -> Self
    where
        F: Fn(&str) -> Result<(), String> + 'static,
    {
        self.validator = Some(Box::new(f));
        self
    }
}

impl Component for PasswordBox {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let style = if !self.base.enabled {
            Style::default().fg(Color::DarkGray)
        } else if self.base.error_msg.is_some() {
            Style::default().fg(Color::Red)
        } else if self.base.focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::White)
        };

        let title = if let Some(err) = &self.base.error_msg {
            let label = self.base.label.trim();
            format!(" {} [{}] ", label, err)
        } else {
            self.base.label.clone()
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(style);

        let width = area.width.saturating_sub(2);
        let scroll = self.base.input.visual_scroll(width as usize);

        let total_len = self.base.input.value().len();
        let visible_len = total_len.saturating_sub(scroll);
        let visible_masked = "*".repeat(visible_len);

        let paragraph = Paragraph::new(visible_masked).block(block);

        f.render_widget(paragraph, area);

        if self.base.focused && self.base.enabled {
            let cursor_pos = self.base.input.visual_cursor().saturating_sub(scroll);
            f.set_cursor_position((area.x + 1 + cursor_pos as u16, area.y + 1));
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        self.base.handle_key(key)
    }


    fn set_focus(&mut self, focused: bool) {
        self.base.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.base.focused
    }

    fn validate(&mut self) -> Result<(), String> {
        let val = self.base.input.value().to_string();
        if let Some(validator) = &self.validator {
            if let Err(e) = validator(&val) {
                self.base.error_msg = Some(e.clone());
                return Err(e);
            }
        }
        self.base.error_msg = None;
        Ok(())
    }
}
