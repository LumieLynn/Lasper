use crate::domain::nvidia::NvidiaFileCategory;
use crate::tui::features::wizard::core::draft::UnclassifiedFile;
use crate::tui::foundation::components::form::{Form, FormItem, FormOutcome, FormState};
use crate::tui::foundation::widgets::display::content::ContentControl;
use crate::tui::foundation::widgets::inputs::{
    button::ButtonControl,
    text_input::{Input, TextInput},
};
use crate::tui::foundation::widgets::lists::list::{ListControl, ListControlState, ListEntry};
use crate::tui::foundation::widgets::selectors::checkbox::CheckboxControl;
use crate::tui::foundation::{footer::FooterRegistry, geometry::centered_rect, theme};
use crate::tui::protocol::{AppMessage, Component, EventResult, WizardMessage};
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{
    layout::Rect,
    style::Style,
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileControl {
    HostPath,
    Destination,
    Category,
    Readonly,
    Submit,
    Cancel,
}
pub struct UnclassifiedFileDialog {
    host_path: String,
    destination: Input,
    error: Option<String>,
    categories: Vec<NvidiaFileCategory>,
    selected: ListControlState,
    readonly: bool,
    form: FormState<FileControl>,
    on_submit: Box<dyn Fn(UnclassifiedFile) -> AppMessage>,
}
impl UnclassifiedFileDialog {
    pub fn new(
        file: UnclassifiedFile,
        on_submit: impl Fn(UnclassifiedFile) -> AppMessage + 'static,
    ) -> Self {
        let categories = NvidiaFileCategory::all();
        let selected = file
            .assigned_category
            .as_ref()
            .and_then(|category| categories.iter().position(|item| item == category))
            .unwrap_or(categories.len() - 1);
        Self {
            host_path: file.host_path,
            destination: Input::from(if file.custom_destination.is_empty() {
                file.default_container_path
            } else {
                file.custom_destination
            }),
            error: None,
            categories,
            selected: ListControlState::new(selected),
            readonly: file.readonly,
            form: FormState::new(FileControl::Destination),
            on_submit: Box::new(on_submit),
        }
    }
    fn with_form<R>(&mut self, visit: impl FnOnce(&mut Form<'_, FileControl>) -> R) -> R {
        let entries: Vec<_> = self
            .categories
            .iter()
            .map(|category| ListEntry::new(format!("  {}", category.label())))
            .collect();
        let mut categories = ListControl::new(&mut self.selected, " Category ", &entries);
        let mut destination = TextInput::new(
            &mut self.destination,
            " Destination Path ",
            self.error.as_deref(),
        );
        let mut readonly = CheckboxControl::new(&mut self.readonly, " Read-only");
        let mut submit = ButtonControl::new("OK");
        let mut cancel = ButtonControl::new("Cancel");
        let mut host = ContentControl::new(
            Paragraph::new(self.host_path.as_str())
                .style(Style::default().fg(theme::theme().dialog_host_text))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .title(" Host Path ")
                        .border_style(Style::default().fg(theme::theme().dialog_host_border)),
                ),
        );
        visit(
            &mut Form::new(
                &mut self.form,
                [
                    FormItem::field(FileControl::HostPath, &mut host),
                    FormItem::field(FileControl::Destination, &mut destination),
                    FormItem::field(FileControl::Category, &mut categories).fill(4),
                    FormItem::field(FileControl::Readonly, &mut readonly),
                    FormItem::row([
                        FormItem::field(FileControl::Submit, &mut submit).with_width(60),
                        FormItem::field(FileControl::Cancel, &mut cancel).with_width(60),
                    ]),
                ],
            )
            .with_actions(FileControl::Submit, FileControl::Cancel),
        )
    }
    fn try_submit(&mut self) -> Option<AppMessage> {
        let destination = self.destination.value().trim();
        self.error = (destination.is_empty() || !std::path::Path::new(destination).is_absolute())
            .then(|| "Absolute destination path required".into());
        if self.error.is_some() {
            return None;
        }
        Some((self.on_submit)(UnclassifiedFile {
            host_path: self.host_path.clone(),
            default_container_path: String::new(),
            assigned_category: self
                .selected
                .selected()
                .and_then(|index| self.categories.get(index))
                .cloned(),
            custom_destination: destination.into(),
            readonly: self.readonly,
        }))
    }
    fn finish_input(&mut self, outcome: FormOutcome<FileControl>) -> EventResult {
        match outcome {
            FormOutcome::Activated(FileControl::Submit) => self
                .try_submit()
                .map_or(EventResult::Consumed, EventResult::Message),
            FormOutcome::Activated(FileControl::Cancel) => {
                EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel))
            }
            FormOutcome::Ignored => EventResult::Ignored,
            _ => EventResult::Consumed,
        }
    }
}
impl Component for UnclassifiedFileDialog {
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        self.form.register_footer_hints(footer);
    }
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let dialog = centered_rect(50, 60, area);
        frame.render_widget(Clear, dialog);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Reclassify File ")
            .border_style(Style::default().fg(theme::theme().dialog_border));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        self.with_form(|form| form.render(frame, inner));
    }
    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        let outcome = self.with_form(|form| form.handle_key(key));
        self.finish_input(outcome)
    }
    fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        let outcome = self.with_form(|form| form.handle_mouse(mouse));
        self.finish_input(outcome)
    }
    fn set_focus(&mut self, focused: bool) {
        self.form.set_focus(focused);
        self.with_form(|_| ());
    }
    fn is_focused(&self) -> bool {
        self.form.is_focused()
    }
}
