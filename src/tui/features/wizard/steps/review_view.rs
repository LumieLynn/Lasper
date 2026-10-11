use crate::tui::features::wizard::draft::WizardDraft;
use crate::tui::features::wizard::steps::{field_event, register_step_form, StepComponent};
use crate::tui::foundation::components::{
    form::{Form, FormItem, FormState},
    text_block::{TextBlockControl, TextBlockState},
};
use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::protocol::{AppMessage, Component, EventResult, WizardMessage};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    layout::{Margin, Rect},
    Frame,
};
pub struct ReviewStepView {
    content: String,
    preview: TextBlockState,
    form: FormState<()>,
}
impl ReviewStepView {
    pub fn new(content: String) -> Self {
        let mut view = Self {
            content,
            preview: TextBlockState::default(),
            form: FormState::new(()),
        };
        view.form.set_focus(true);
        view.with_form(|_| ());
        view
    }
    fn with_form<R>(&mut self, visit: impl FnOnce(&mut Form<'_, ()>) -> R) -> R {
        let mut preview =
            TextBlockControl::new(&mut self.preview, " Deployment Preview ", &self.content);
        visit(&mut Form::new(
            &mut self.form,
            [FormItem::field((), &mut preview).fill(4)],
        ))
    }
}
impl Component for ReviewStepView {
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        register_step_form(footer, &self.form);
        footer.register(
            FooterHint::new("Enter", "deploy")
                .primary()
                .on_key(KeyCode::Enter),
        );
    }
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.with_form(|form| form.render(frame, area.inner(Margin::new(1, 1))));
    }
    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if key.code == KeyCode::Enter {
            return EventResult::Message(AppMessage::Wizard(WizardMessage::Submit));
        }
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
}
impl StepComponent for ReviewStepView {
    fn commit_to_draft(&self, _: &mut WizardDraft) {}
    fn render_step(&mut self, frame: &mut Frame, area: Rect, _: &WizardDraft) {
        self.render(frame, area);
    }
}
