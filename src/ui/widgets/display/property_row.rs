use crossterm::event::KeyEvent;
use ratatui::{
    layout::Rect,
    style::{Color, Style, Modifier},
    text::{Line, Span},
    widgets::{Paragraph},
    Frame,
};
use crate::ui::core::{Component, EventResult};

pub struct PropertyRow {
    key: String,
    value: String,
}

impl PropertyRow {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }
}

impl Component for PropertyRow {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let line = Line::from(vec![
            Span::styled(format!("{}: ", self.key), Style::default().fg(Color::DarkGray)),
            Span::styled(self.value.as_str(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        ]);

        let paragraph = Paragraph::new(line);
        f.render_widget(paragraph, area);
    }

    fn handle_key(&mut self, _key: KeyEvent) -> EventResult {
        EventResult::Ignored
    }
}
