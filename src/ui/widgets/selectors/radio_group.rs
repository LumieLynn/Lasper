use crossterm::event::{KeyEvent, KeyCode};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use crate::ui::core::{AppMessage, Component, EventResult};

pub struct RadioGroup {
    label: String,
    options: Vec<String>,
    selected_idx: usize,
    focused: bool,
    enabled: bool,
    on_change: Option<Box<dyn Fn(usize) -> AppMessage>>,
}

impl RadioGroup {
    pub fn new(label: impl Into<String>, options: Vec<String>, initial_idx: usize) -> Self {
        Self {
            label: label.into(),
            options,
            selected_idx: initial_idx,
            focused: false,
            enabled: true,
            on_change: None,
        }
    }

    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn with_on_change<F>(mut self, f: F) -> Self 
    where F: Fn(usize) -> AppMessage + 'static 
    {
        self.on_change = Some(Box::new(f));
        self
    }

    pub fn selected_idx(&self) -> usize {
        self.selected_idx
    }
}

impl Component for RadioGroup {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let style = if !self.enabled {
            Style::default().fg(Color::DarkGray)
        } else if self.focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::White)
        };

        let mut spans = vec![];
        for (i, opt) in self.options.iter().enumerate() {
            let symbol = if i == self.selected_idx { "(●)" } else { "(○)" };
            spans.push(format!("{} {}", symbol, opt));
        }

        let text = spans.join("   ");
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.label.as_str())
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
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Up | KeyCode::Char('k') => {
                if self.selected_idx > 0 {
                    self.selected_idx -= 1;
                    if let Some(on_change) = &self.on_change {
                        return EventResult::Message(on_change(self.selected_idx));
                    }
                    return EventResult::Consumed;
                }
                EventResult::Ignored
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Down | KeyCode::Char('j') => {
                if self.selected_idx < self.options.len().saturating_sub(1) {
                    self.selected_idx += 1;
                    if let Some(on_change) = &self.on_change {
                        return EventResult::Message(on_change(self.selected_idx));
                    }
                    return EventResult::Consumed;
                }
                EventResult::Ignored
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
