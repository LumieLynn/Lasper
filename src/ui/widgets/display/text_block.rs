use crate::ui::core::{Component, EventResult};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
    Frame,
};

pub struct TextBlock {
    label: String,
    content: String,
    focused: bool,
    scroll: u16,
    max_scroll: u16,
}

impl TextBlock {
    pub fn new(label: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            content: content.into(),
            focused: false,
            scroll: 0,
            max_scroll: 0,
        }
    }

    pub fn set_content(&mut self, content: impl Into<String>) {
        self.content = content.into();
    }

    pub fn required_height(&self, width: u16) -> u16 {
        let inner_width = width.saturating_sub(2).max(1) as usize;
        let lines: usize = self
            .content
            .lines()
            .map(|line| {
                let count = line.chars().count();
                if count == 0 {
                    1
                } else {
                    (count + inner_width - 1) / inner_width
                }
            })
            .sum();
        (lines + 2) as u16 // add borders
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
            .border_type(BorderType::Rounded)
            .title(self.label.as_str())
            .border_style(style);

        let inner = block.inner(area);
        let inner_width = inner.width as usize;
        let inner_height = inner.height as usize;
        let safe_width = inner_width.max(1);

        let lines_count: usize = self
            .content
            .lines()
            .map(|line| {
                let count = line.chars().count();
                if count == 0 {
                    1
                } else {
                    (count + safe_width - 1) / safe_width
                }
            })
            .sum();
        self.max_scroll = lines_count.saturating_sub(inner_height) as u16;
        self.scroll = self.scroll.min(self.max_scroll);

        let paragraph = Paragraph::new(self.content.as_str())
            .block(block)
            .wrap(Wrap { trim: true })
            .scroll((self.scroll, 0));

        f.render_widget(paragraph, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if self.scroll < self.max_scroll {
                    self.scroll = self.scroll.saturating_add(1);
                }
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
