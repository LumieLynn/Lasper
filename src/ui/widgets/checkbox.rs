use ratatui::{
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph},
    Frame, layout::Rect,
};

/// A simple checkbox widget with a label and optional focus.
pub struct Checkbox<'a> {
    label: &'a str,
    checked: bool,
    focused: bool,
}

impl<'a> Checkbox<'a> {
    pub fn new(label: &'a str, checked: bool) -> Self {
        Self {
            label,
            checked,
            focused: false,
        }
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    pub fn render(self, f: &mut Frame, area: Rect) {
        let style = if self.focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(style);

        let symbol = if self.checked { "[x] " } else { "[ ] " };
        let text = format!("{}{}", symbol, self.label);
        
        let p = Paragraph::new(text).block(block);
        f.render_widget(p, area);
    }
}
