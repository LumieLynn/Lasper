use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState},
    Frame,
};

/// A reusable scrollable list widget.
pub struct ScrollableList<'a> {
    title: &'a str,
    items: Vec<ListItem<'a>>,
    selected: Option<usize>,
}

impl<'a> ScrollableList<'a> {
    pub fn new(title: &'a str, items: Vec<ListItem<'a>>) -> Self {
        Self {
            title,
            items,
            selected: None,
        }
    }

    pub fn selected(mut self, selected: Option<usize>) -> Self {
        self.selected = selected;
        self
    }

    pub fn render(self, f: &mut Frame, area: Rect) {
        let list = List::new(self.items)
            .block(
                Block::default()
                    .title(format!(" {} ", self.title))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .style(Style::default().fg(Color::White)),
            )
            .highlight_style(
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");

        let mut state = ListState::default();
        state.select(self.selected);

        f.render_stateful_widget(list, area, &mut state);
    }
}
