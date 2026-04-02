use crate::ui::core::{AppMessage, Component, EventResult};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

pub struct Button {
    label: String,
    focused: bool,
    msg: AppMessage,
}

impl Button {
    pub fn new(label: impl Into<String>, msg: AppMessage) -> Self {
        Self {
            label: label.into(),
            focused: false,
            msg,
        }
    }
}

impl Component for Button {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let style = if self.focused {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::White)
        };

        let label = format!(" {} ", self.label);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(if self.focused {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            });

        let p = Paragraph::new(label)
            .style(style)
            .block(block)
            .alignment(ratatui::layout::Alignment::Center);
        f.render_widget(p, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Enter | KeyCode::Char(' ') => EventResult::Message(self.msg.clone()),
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
