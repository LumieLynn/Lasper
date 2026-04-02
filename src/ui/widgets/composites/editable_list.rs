use crate::ui::core::{AppMessage, Component, EventResult};
use crate::ui::widgets::selectors::selectable_list::SelectableList;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{layout::Rect, Frame};

pub struct EditableList<T> {
    list: SelectableList<T>,
    focused: bool,
    on_remove: Box<dyn Fn(usize) -> AppMessage>,
}

impl<T> EditableList<T> {
    pub fn new(
        label: impl Into<String>,
        items: Vec<T>,
        display_fn: impl Fn(&T) -> String + 'static,
        on_remove: impl Fn(usize) -> AppMessage + 'static,
    ) -> Self {
        Self {
            list: SelectableList::new(label, items, display_fn),
            focused: false,
            on_remove: Box::new(on_remove),
        }
    }

    pub fn add_item(&mut self, item: T) {
        self.list.add_item(item);
    }

    pub fn remove_item(&mut self, idx: usize) {
        self.list.remove_item(idx);
    }
}

impl<T> Component for EditableList<T> {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        self.list.render(f, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Delete | KeyCode::Char('d') | KeyCode::Char('D') => {
                if let Some(idx) = self.list.selected_idx() {
                    let msg = (self.on_remove)(idx);
                    self.list.remove_item(idx);
                    return EventResult::Message(msg);
                }
            }
            _ => {}
        }
        self.list.handle_key(key)
    }

    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
        self.list.set_focus(focused);
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}
