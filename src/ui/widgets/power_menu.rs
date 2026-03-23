use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState},
    Frame,
};

pub struct PowerMenu {
    pub selected: usize,
}

impl PowerMenu {
    pub fn new(selected: usize) -> Self {
        Self { selected }
    }

    pub fn render(self, f: &mut Frame, area: Rect) {
        let items = vec![
            ListItem::new(Line::from("  Start")),
            ListItem::new(Line::from("  Poweroff")),
            ListItem::new(Line::from("  Reboot")),
            ListItem::new(Line::from("  Terminate")),
            ListItem::new(Line::from("  Kill (SIGTERM)")),
            ListItem::new(Line::from("  Enable (at boot)")),
            ListItem::new(Line::from("  Disable (at boot)")),
        ];

        let list = List::new(items)
            .block(
                Block::default()
                    .title(" Actions ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::Rgb(50, 50, 80))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        let mut state = ListState::default();
        state.select(Some(self.selected));

        // Center the menu using a fixed width and height that fits the items
        let area = centered_rect(30, 9, area);

        f.render_widget(Clear, area);
        f.render_stateful_widget(list, area, &mut state);
    }
}

fn centered_rect(width: u16, height: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(height),
            Constraint::Fill(1),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(width),
            Constraint::Fill(1),
        ])
        .split(popup_layout[1])[1]
}
