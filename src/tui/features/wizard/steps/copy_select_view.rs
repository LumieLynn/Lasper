use crate::domain::runtime::ImageEntry;
use crate::tui::features::wizard::draft::WizardDraft;
use crate::tui::features::wizard::steps::{field_event, register_step_form, StepComponent};
use crate::tui::foundation::components::form::{Form, FormItem, FormState};
use crate::tui::foundation::footer::FooterRegistry;
use crate::tui::foundation::widgets::lists::list::{ListControl, ListControlState, ListEntry};
use crate::tui::protocol::{Component, EventResult};
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{
    layout::{Margin, Rect},
    Frame,
};

pub struct CopySelectStepView {
    images: Vec<ImageEntry>,
    selected: ListControlState,
    form: FormState<()>,
}
impl CopySelectStepView {
    pub fn new(images: &[ImageEntry], initial_cursor: usize) -> Self {
        let mut view = Self {
            images: images.to_vec(),
            selected: ListControlState::new(initial_cursor.min(images.len().saturating_sub(1))),
            form: FormState::new(()),
        };
        view.form.set_focus(true);
        view.with_form(|_| ());
        view
    }
    fn with_form<R>(&mut self, visit: impl FnOnce(&mut Form<'_, ()>) -> R) -> R {
        let entries: Vec<_> = self
            .images
            .iter()
            .map(|image| {
                ListEntry::new(format!(
                    "◆ {} ({}){}",
                    image.name,
                    image.image_type,
                    if image.readonly { " [ro]" } else { "" }
                ))
            })
            .collect();
        let mut list = ListControl::new(&mut self.selected, " Select image to clone ", &entries);
        visit(&mut Form::new(
            &mut self.form,
            [FormItem::field((), &mut list).fill(4)],
        ))
    }
}
impl Component for CopySelectStepView {
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        register_step_form(footer, &self.form);
    }
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.with_form(|form| form.render(frame, area.inner(Margin::new(1, 1))));
    }
    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        field_event(self.with_form(|form| form.handle_key(key)))
    }
    fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        field_event(self.with_form(|form| form.handle_mouse(mouse)))
    }
    fn set_focus(&mut self, focused: bool) {
        self.form.set_focus(focused);
    }
    fn is_focused(&self) -> bool {
        self.form.is_focused()
    }
    fn validate(&mut self) -> Result<(), String> {
        if self.images.is_empty() {
            Err("No images available to clone".into())
        } else if self.selected.selected().is_none() {
            Err("Please select an image".into())
        } else {
            Ok(())
        }
    }
}
impl StepComponent for CopySelectStepView {
    fn commit_to_draft(&self, context: &mut WizardDraft) {
        if let Some(index) = self.selected.selected() {
            context.source.copy_idx = index;
            if let Some(image) = self.images.get(index) {
                context.source.clone_source = image.name.clone();
            }
        }
    }
    fn render_step(&mut self, frame: &mut Frame, area: Rect, _: &WizardDraft) {
        self.render(frame, area);
    }
}
