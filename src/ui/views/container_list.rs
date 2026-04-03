use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::nspawn::ContainerEntry;
use crate::ui::core::{AppMessage, EventResult, ListMessage};
use crate::ui::widgets::lists::shared_container_list::SharedContainerList;

pub struct ContainerListComponent {
    list: SharedContainerList,
}

impl ContainerListComponent {
    pub fn new() -> Self {
        Self {
            list: SharedContainerList::new(" Containers ", 0),
        }
    }

    pub fn render_with_data(
        &mut self,
        f: &mut Frame,
        area: Rect,
        entries: &[ContainerEntry],
        selected: usize,
        _is_root: bool, // is_root was used for hint, keeping it in signature for now
        focused: bool,
    ) {
        // Sync state from background data
        self.list.select(selected);
        self.list.set_focus(focused);

        // Zero-copy rendering
        self.list.render(f, area, entries);

        if entries.is_empty() {
            // Hint logic (simplified for now but preserving the spirit)
            let hint = "  No containers found";
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
            KeyCode::Char('j') | KeyCode::Down => {
                EventResult::Message(AppMessage::List(ListMessage::Next))
            }
            KeyCode::Char('k') | KeyCode::Up => {
                EventResult::Message(AppMessage::List(ListMessage::Prev))
            }
            _ => EventResult::Ignored,
        }
    }
}
