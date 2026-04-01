use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::app::App;
use crate::nspawn::ContainerState;
use crate::ui::core::Component;
use crate::ui::widgets::selectors::selectable_list::SelectableList;

pub fn render(f: &mut Frame, app: &mut App, area: Rect) {
    let items = app.data.entries.clone();
    
    let mut list = SelectableList::new(
        " Containers ",
        items,
        |e| {
            let (icon, _) = match &e.state {
                ContainerState::Running  => ("● ", "green"),
                ContainerState::Starting => ("◑ ", "yellow"),
                ContainerState::Exiting  => ("◐ ", "yellow"),
                ContainerState::Off      => ("○ ", "gray"),
            };
            format!("{} {} ({})", icon, e.name, e.state.label())
        }
    );
    list.select(app.data.selected);
    
    list.render(f, area);

    if app.data.entries.is_empty() {
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
