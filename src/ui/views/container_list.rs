use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::nspawn::{ContainerEntry, ContainerState};
use crate::ui::core::{AppMessage, EventResult};
use crate::ui::widgets::selectors::selectable_list::SelectableList;

pub struct ContainerListComponent;

impl ContainerListComponent {
    pub fn new() -> Self {
        Self
    }

    pub fn render_with_data(
        &mut self,
        f: &mut Frame,
        area: Rect,
        entries: &[ContainerEntry],
        selected: usize,
        is_root: bool,
        focused: bool,
    ) {
        let mut list = SelectableList::new(" Containers ", entries.to_vec(), |e| {
            let (icon, _) = match &e.state {
                ContainerState::Running => ("● ", "green"),
                ContainerState::Starting => ("◑ ", "yellow"),
                ContainerState::Exiting => ("◐ ", "yellow"),
                ContainerState::Off => ("○ ", "gray"),
            };
            format!("{} {} ({})", icon, e.name, e.state.label())
        });
        list.select(selected);
        list.set_focus(focused);
        list.render(f, area);

        if entries.is_empty() {
            let hint = if is_root {
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

    /// Handles navigation keys and returns the corresponding AppMessage.
    /// j/↓ → ListNext, k/↑ → ListPrev. All other keys are Ignored.
    pub fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => EventResult::Message(AppMessage::ListNext),
            KeyCode::Char('k') | KeyCode::Up => EventResult::Message(AppMessage::ListPrev),
            _ => EventResult::Ignored,
        }
    }
}
