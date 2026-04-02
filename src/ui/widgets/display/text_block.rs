use crate::ui::core::{Component, EventResult};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

pub struct TextBlock {
    label: String,
    content: String,
    focused: bool,
    scroll: u16,
}

impl TextBlock {
    pub fn new(label: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            content: content.into(),
            focused: false,
            scroll: 0,
        }
    }

    pub fn set_content(&mut self, content: impl Into<String>) {
        self.content = content.into();
    }
}

impl Component for TextBlock {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let style = if self.focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::White)
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.label.as_str())
            .border_style(style);

        let paragraph = Paragraph::new(self.content.as_str())
            .block(block)
            .wrap(Wrap { trim: true })
            .scroll((self.scroll, 0));

        f.render_widget(paragraph, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_add(1);
                return EventResult::Consumed;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
                return EventResult::Consumed;
            }
            _ => {}
        }
        EventResult::Ignored
    }

    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}
