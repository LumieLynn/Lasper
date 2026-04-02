use crate::nspawn::ContainerEntry;
use crate::ui::core::{Component, EventResult};
use crate::ui::widgets::selectors::selectable_list::SelectableList;
use crate::ui::wizard::context::WizardContext;
use crate::ui::wizard::steps::StepComponent;

use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, Frame};

pub struct CopySelectStepView {
    list: SelectableList<ContainerEntry>,
}

impl CopySelectStepView {
    pub fn new(entries: &[ContainerEntry], initial_cursor: usize) -> Self {
        let mut list = SelectableList::new(" Select container to clone ", entries.to_vec(), |e| {
            format!("  {} ({})", e.name, e.state.label())
        });


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
    fn validate(&mut self) -> Result<(), String> {
        Ok(())
    }
}

impl StepComponent for CopySelectStepView {
    fn commit_to_context(&self, ctx: &mut WizardContext) {
        if let Some(idx) = self.list.selected_idx() {
            ctx.source.copy_idx = idx;
            if let Some(entry) = self.list.selected_item() {
                ctx.source.clone_source = entry.name.clone();
            }
        }
    }
}

