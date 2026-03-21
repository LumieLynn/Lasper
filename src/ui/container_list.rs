use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{ListItem, Paragraph},
    Frame,
};

use crate::app::App;
use crate::nspawn::ContainerState;
use crate::ui::widgets::list::ScrollableList;

pub fn render(f: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<ListItem> = app.entries.iter().map(|e| {
        let (icon, color) = match &e.state {
            ContainerState::Running  => ("● ", Color::Green),
            ContainerState::Starting => ("◑ ", Color::Yellow),
            ContainerState::Exiting  => ("◐ ", Color::Yellow),
            ContainerState::Stopped  => ("○ ", Color::DarkGray),
        };

        let type_hint = e.image_type.as_deref().unwrap_or("");
        let addr_hint = e.address.as_deref().unwrap_or("");

        let mut spans = vec![
            Span::styled(icon, Style::default().fg(color)),
            Span::raw(e.name.as_str()),
        ];
        if !type_hint.is_empty() {
            spans.push(Span::styled(
                format!("  {}", type_hint),
                Style::default().fg(Color::Rgb(80, 80, 100)),
            ));
        }
        if !addr_hint.is_empty() {
            spans.push(Span::styled(
                format!("  {}", addr_hint),
                Style::default().fg(Color::Rgb(60, 120, 100)),
            ));
        }

        ListItem::new(Line::from(spans))
    }).collect();

    let selected = if app.entries.is_empty() { None } else { Some(app.selected) };
    
    ScrollableList::new(" Containers ", items)
        .selected(selected)
        .render(f, area);

    if app.entries.is_empty() {
        let hint = if app.is_root {
            "  No containers in /var/lib/machines"
        } else {
            "  No running containers\n  (run with sudo to see all)"
        };
        f.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))),
            ]),
            area,
        );
    }
}
