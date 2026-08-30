use crate::domain::runtime::ImageEntry;
use crate::tui::core::{Component, EventResult};
use crate::tui::widgets::lists::selectable_list::SelectableList;
use crate::tui::wizard::draft::WizardDraft;
use crate::tui::wizard::steps::StepComponent;

use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, Frame};

pub struct CopySelectStepView {
    list: SelectableList<ImageEntry>,
    focused: bool,
}

impl CopySelectStepView {
    pub fn new(images: &[ImageEntry], initial_cursor: usize) -> Self {
        let mut list = SelectableList::new(
            " Select image to clone ",
            images.to_vec(),
            |image: &ImageEntry| {
                format!(
                    "◆ {} ({}){}",
                    image.name,
                    image.image_type,
                    if image.readonly { " [ro]" } else { "" }
                )
            },
        );
        list.select(initial_cursor.min(images.len().saturating_sub(1)));
        list.set_focus(true);
        Self {
            list,
            focused: true,
        }
    }
}

impl Component for CopySelectStepView {
    fn render(&mut self, _f: &mut Frame, _area: Rect) {
        // This view uses render_step for reactive rendering with context
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        self.list.handle_key(key)
    }

    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
        self.list.set_focus(focused);
    }

    fn is_focused(&self) -> bool {
        self.focused
    }

    fn validate(&mut self) -> Result<(), String> {
        if self.list.items().is_empty() {
            return Err("No images available to clone".to_string());
        }
        if self.list.selected_item().is_none() {
            return Err("Please select an image".to_string());
        }
        Ok(())
    }
}

impl StepComponent for CopySelectStepView {
    fn commit_to_draft(&self, ctx: &mut WizardDraft) {
        if let Some(idx) = self.list.selected_idx() {
            ctx.source.copy_idx = idx;
            if let Some(image) = self.list.selected_item() {
                ctx.source.clone_source = image.name.clone();
            }
        }
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardDraft) {
        let chunks = ratatui::layout::Layout::default()
            .direction(ratatui::layout::Direction::Vertical)
            .margin(1)
            .constraints([ratatui::layout::Constraint::Min(0)])
            .split(area);

        self.list.render(f, chunks[0]);
    }
}
