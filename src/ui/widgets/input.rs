use ratatui::{
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph},
    Frame, layout::Rect,
};

/// A simple text input widget with a label and optional focus.
pub struct Input<'a> {
    label: &'a str,
    value: &'a str,
    focused: bool,
    scroll: u16,
}

impl<'a> Input<'a> {
    pub fn new(label: &'a str, value: &'a str) -> Self {
        Self {
            label,
            value,
            focused: false,
            scroll: 0,
        }
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    pub fn scroll(mut self, scroll: u16) -> Self {
        self.scroll = scroll;
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
            .title(self.label)
            .border_style(style);

        let p = Paragraph::new(self.value)
            .block(block)
            .scroll((self.scroll, 0));
        f.render_widget(p, area);
    }
}
