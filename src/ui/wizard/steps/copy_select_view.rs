use crate::nspawn::ContainerEntry;
use crate::ui::core::{AppMessage, Component, EventResult};
use crate::ui::widgets::selectors::selectable_list::SelectableList;
use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, Frame};

pub struct CopySelectStepView {
    list: SelectableList<ContainerEntry>,
}

impl CopySelectStepView {
    pub fn new(entries: &[ContainerEntry], initial_cursor: usize) -> Self {
        let mut list = SelectableList::new(" Select container to clone ", entries.to_vec(), |e| {
            format!("  {} ({})", e.name, e.state.label())
        })
        .with_on_change(|idx| AppMessage::SourceCloneIdxUpdated(idx));

        list.select(initial_cursor);

        Self { list }
    }
}

impl Component for CopySelectStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        self.list.render(f, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        self.list.handle_key(key)
    }

    fn set_focus(&mut self, focused: bool) {
        self.list.set_focus(focused);
    }

    fn is_focused(&self) -> bool {
        self.list.is_focused()
    }
}
