use crate::nspawn::ContainerEntry;
use crate::ui::core::{Component, EventResult};
use crate::ui::widgets::lists::shared_container_list::SharedContainerList;
use crate::ui::wizard::context::WizardContext;
use crate::ui::wizard::steps::StepComponent;

use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, Frame};

pub struct CopySelectStepView {
    list: SharedContainerList,
    items_len: usize,
    focused: bool,
}

impl CopySelectStepView {
    pub fn new(entries: &[ContainerEntry], initial_cursor: usize) -> Self {
        Self {
            list: SharedContainerList::new(" Select container to clone ", initial_cursor),
            items_len: entries.len(),
            focused: false,
        }
    }
}

impl Component for CopySelectStepView {
    fn render(&mut self, _f: &mut Frame, _area: Rect) {
        // This view uses render_step for reactive rendering with context
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if self.list.handle_key(key, self.items_len) {
            EventResult::Consumed
        } else {
            EventResult::Ignored
        }
    }

    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
        self.list.set_focus(focused);
    }

    fn is_focused(&self) -> bool {
        self.focused
    }

    fn validate(&mut self) -> Result<(), String> {
        if self.items_len == 0 {
            return Err("No containers available to clone".to_string());
        }
        if self.list.selected_idx().is_none() {
            return Err("Please select a container".to_string());
        }
        Ok(())
    }
}

impl StepComponent for CopySelectStepView {
    fn commit_to_context(&self, ctx: &mut WizardContext) {
        if let Some(idx) = self.list.selected_idx() {
            ctx.source.copy_idx = idx;
            if let Some(entry) = ctx.entries.get(idx) {
                ctx.source.clone_source = entry.name.clone();
            }
        }
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, context: &WizardContext) {
        self.items_len = context.entries.len();

        let chunks = ratatui::layout::Layout::default()
            .direction(ratatui::layout::Direction::Vertical)
            .margin(1)
            .constraints([ratatui::layout::Constraint::Min(0)])
            .split(area);

        // Zero-copy rendering!
        self.list.render(f, chunks[0], &context.entries);
    }
}
