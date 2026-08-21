use crate::ui::core::{AppMessage, Component, EventResult};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::Style,
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};

pub struct Button {
    label: String,
    focused: bool,
    message: Box<dyn Fn() -> AppMessage>,
}

impl Button {
    pub fn new(label: impl Into<String>, message: impl Fn() -> AppMessage + 'static) -> Self {
        Self {
            label: label.into(),
            focused: false,
            message: Box::new(message),
        }
    }
}

impl Component for Button {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let t = crate::ui::theme::theme();
        let style = if self.focused {
            Style::default()
                .fg(t.button_focused_fg)
                .bg(t.button_focused_bg)
        } else {
            Style::default().fg(t.button_unfocused_fg)
        };

        let label = format!(" {} ", self.label);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(if self.focused {
                Style::default().fg(t.button_border_focused)
            } else {
                Style::default().fg(t.button_border_unfocused)
            });

        let p = Paragraph::new(label)
            .style(style)
            .block(block)
            .alignment(ratatui::layout::Alignment::Center);
        f.render_widget(p, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Enter | KeyCode::Char(' ') => EventResult::Message((self.message)()),
            _ => EventResult::Ignored,
        }
    }

    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}
