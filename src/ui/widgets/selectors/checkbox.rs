use crate::ui::core::{Component, EventResult};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};

pub struct Checkbox {
    label: String,
    checked: bool,
    focused: bool,
    enabled: bool,
}

impl Checkbox {
    pub fn new(label: impl Into<String>, initial_checked: bool) -> Self {
        Self {
            label: label.into(),
            checked: initial_checked,
            focused: false,
            enabled: true,
        }
    }

    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn set_checked(&mut self, checked: bool) {
        self.checked = checked;
    }

    pub fn checked(&self) -> bool {
        self.checked
    }
}

impl Component for Checkbox {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let style = if !self.enabled {
            Style::default().fg(Color::DarkGray)
        } else if self.focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::White)
        };

        let symbol = if self.checked { "[x]" } else { "[ ]" };
        let text = format!("{} {}", symbol, self.label);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(style);

        let paragraph = Paragraph::new(text).block(block);
        f.render_widget(paragraph, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if !self.enabled {
            return EventResult::Ignored;
        }

        match key.code {
            KeyCode::Tab => EventResult::FocusNext,
            KeyCode::BackTab => EventResult::FocusPrev,
            KeyCode::Char(' ') => {
                self.checked = !self.checked;
                EventResult::Consumed
            }

            _ => EventResult::Ignored,
        }
    }

    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }
}
