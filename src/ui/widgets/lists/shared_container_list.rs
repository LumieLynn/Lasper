use crate::nspawn::{ContainerEntry, ContainerState};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState},
    Frame,
};

pub struct SharedContainerList {
    pub state: ListState,
    label: String,
    focused: bool,
}

impl SharedContainerList {
    pub fn new(label: impl Into<String>, initial_idx: usize) -> Self {
        let mut state = ListState::default();
        state.select(Some(initial_idx));
        Self {
            state,
            label: label.into(),
            focused: false,
        }
    }

    pub fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    pub fn selected_idx(&self) -> Option<usize> {
        self.state.selected()
    }

    pub fn select(&mut self, idx: usize) {
        self.state.select(Some(idx));
    }

    /// Renders the container list without taking ownership of data.
    /// Uses ContainerEntry references to build the UI list.
    pub fn render(&mut self, f: &mut Frame, area: Rect, entries: &[ContainerEntry]) {
        // If background data changes and current selection is out of bounds, clamp it.
        if let Some(current) = self.state.selected() {
            if entries.is_empty() {
                self.state.select(None);
            } else {
                let max_idx = entries.len().saturating_sub(1);
                if current > max_idx {
                    self.state.select(Some(max_idx));
                }
            }
        }

        let items: Vec<ListItem> = entries
            .iter()
            .map(|e| {
                let (icon, color) = match &e.state {
                    ContainerState::Running => ("● ", Color::Green),
                    ContainerState::Starting => ("◑ ", Color::Yellow),
                    ContainerState::Exiting => ("◐ ", Color::Yellow),
                    ContainerState::Off => ("○ ", Color::DarkGray),
                };
                ListItem::new(format!("{} {} ({})", icon, e.name, e.state.label()))
                    .style(Style::default().fg(color))
            })
            .collect();

        let border_color = if self.focused {
            Color::Cyan
        } else {
            Color::White
        };

        let mut list = List::new(items)
            .block(
                Block::default()
                    .title(self.label.as_str())
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(border_color)),
            )
            .highlight_symbol(">> ");

        if self.focused {
            list = list.highlight_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
        } else {
            list = list.highlight_style(Style::default().fg(Color::DarkGray));
        }

        f.render_stateful_widget(list, area, &mut self.state);
    }

    /// Generic list navigation logic.
    pub fn handle_key(&mut self, key: KeyEvent, items_len: usize) -> bool {
        if items_len == 0 {
            return false;
        }

        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                let i = self.state.selected().unwrap_or(0);
                self.state.select(Some(if i >= items_len.saturating_sub(1) {
                    0
                } else {
                    i + 1
                }));
                true
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let i = self.state.selected().unwrap_or(0);
                self.state.select(Some(if i == 0 {
                    items_len.saturating_sub(1)
                } else {
                    i - 1
                }));
                true
            }
            _ => false,
        }
    }
}
