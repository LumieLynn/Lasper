use crate::ui::core::{Component, EventResult};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState},
    Frame,
};
use std::collections::HashSet;

pub struct Checklist<T> {
    items: Vec<T>,
    state: ListState,
    checked_indices: HashSet<usize>,
    label: String,
    focused: bool,
    enabled: bool,
    display_fn: Box<dyn Fn(&T) -> String>,
}

impl<T> Checklist<T> {
    pub fn new(
        label: impl Into<String>,
        items: Vec<T>,
        display_fn: impl Fn(&T) -> String + 'static,
    ) -> Self {
        let mut state = ListState::default();
        if !items.is_empty() {
            state.select(Some(0));
        }
        Self {
            items,
            state,
            checked_indices: HashSet::new(),
            label: label.into(),
            focused: false,
            enabled: true,
            display_fn: Box::new(display_fn),
        }
    }

    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn items(&self) -> &[T] {
        &self.items
    }

    pub fn checked_indices(&self) -> &HashSet<usize> {
        &self.checked_indices
    }

    pub fn checked_items(&self) -> Vec<&T> {
        self.checked_indices
            .iter()
            .filter_map(|&idx| self.items.get(idx))
            .collect()
    }

    pub fn set_checked(&mut self, indices: Vec<usize>) {
        self.checked_indices = indices.into_iter().collect();
    }

    pub fn toggle_active(&mut self) {
        if let Some(idx) = self.state.selected() {
            if self.checked_indices.contains(&idx) {
                self.checked_indices.remove(&idx);
            } else {
                self.checked_indices.insert(idx);
            }
        }
    }

    pub fn next(&mut self) {
        let i = match self.state.selected() {
            Some(i) => {
                if i >= self.items.len().saturating_sub(1) {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        if !self.items.is_empty() {
            self.state.select(Some(i));
        }
    }

    pub fn previous(&mut self) {
        let i = match self.state.selected() {
            Some(i) => {
                if i == 0 {
                    self.items.len().saturating_sub(1)
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        if !self.items.is_empty() {
            self.state.select(Some(i));
        }
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        let style = if !self.enabled {
            Style::default().fg(Color::DarkGray)
        } else if self.focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::White)
        };

        let items: Vec<ListItem> = self
            .items
            .iter()
            .enumerate()
            .map(|(idx, item)| {
                let prefix = if self.checked_indices.contains(&idx) {
                    "[x] "
                } else {
                    "[ ] "
                };
                ListItem::new(format!("{}{}", prefix, (self.display_fn)(item)))
            })
            .collect();

        let mut list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(self.label.as_str())
                .border_style(style),
        );

        if self.enabled {
            list = list
                .highlight_style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol(">> ");
        }

        f.render_stateful_widget(list, area, &mut self.state);
    }
}

impl<T> Component for Checklist<T> {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        Self::render(self, f, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if !self.enabled {
            return EventResult::Ignored;
        }

        match key.code {
            KeyCode::Tab => EventResult::FocusNext,
            KeyCode::BackTab => EventResult::FocusPrev,
            KeyCode::Down | KeyCode::Char('j') => {
                self.next();
                EventResult::Consumed
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.previous();
                EventResult::Consumed
            }
            KeyCode::Char(' ') => {
                self.toggle_active();
                EventResult::Consumed
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
