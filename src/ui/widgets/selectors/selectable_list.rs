use crate::ui::core::{Component, EventResult};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState},
    Frame,
};

pub struct SelectableList<T> {
    items: Vec<T>,
    state: ListState,
    label: String,
    focused: bool,
    enabled: bool,
    display_fn: Box<dyn Fn(&T) -> String>,
}


impl<T> SelectableList<T> {
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


    pub fn selected_idx(&self) -> Option<usize> {
        self.state.selected()
    }

    pub fn selected_item(&self) -> Option<&T> {
        self.state.selected().and_then(|idx| self.items.get(idx))
    }

    pub fn select(&mut self, index: usize) {
        if index < self.items.len() {
            self.state.select(Some(index));
        }
    }

    pub fn set_items(&mut self, items: Vec<T>) {
        self.items = items;
        if let Some(i) = self.state.selected() {
            if i >= self.items.len() {
                self.state
                    .select(if self.items.is_empty() { None } else { Some(0) });
            }
        } else if !self.items.is_empty() {
            self.state.select(Some(0));
        }
    }

    pub fn add_item(&mut self, item: T) {
        self.items.push(item);
        if self.state.selected().is_none() {
            self.state.select(Some(0));
        }
    }

    pub fn remove_item(&mut self, idx: usize) {
        if idx < self.items.len() {
            self.items.remove(idx);
            if let Some(selected) = self.state.selected() {
                if selected >= self.items.len() && !self.items.is_empty() {
                    self.state.select(Some(self.items.len() - 1));
                } else if self.items.is_empty() {
                    self.state.select(None);
                }
            }
        }
    }

    pub fn select_last(&mut self) {
        if !self.items.is_empty() {
            self.state.select(Some(self.items.len() - 1));
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
        self.state.select(Some(i));
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
        self.state.select(Some(i));
    }

    /// Inherent method — callers don't need the `Component` trait in scope.
    pub fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Inherent render method — callers don't need the `Component` trait in scope.
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
            .map(|item| ListItem::new((self.display_fn)(item)))
            .collect();

        let mut list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
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

impl<T> Component for SelectableList<T> {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        // Delegate to the inherent method — single implementation.
        SelectableList::render(self, f, area);
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

            _ => EventResult::Ignored,
        }
    }

    fn set_focus(&mut self, focused: bool) {
        SelectableList::set_focus(self, focused);
    }

    fn is_focused(&self) -> bool {
        self.focused
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }
}
