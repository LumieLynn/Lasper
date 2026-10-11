use crate::domain::secret::{replace_secret_string, validate_chpasswd_secret};
use crate::tui::features::wizard::draft::{UserDraft, UserState, WizardDraft};
use crate::tui::features::wizard::steps::{field_event, register_step_form, StepComponent};
use crate::tui::features::wizard::StepAction;
use crate::tui::foundation::components::form::{Form, FormItem, FormState};
use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::foundation::widgets::inputs::text_input::{Input, TextInput};
use crate::tui::foundation::widgets::lists::list::{ListControl, ListControlState, ListEntry};
use crate::tui::protocol::{AppMessage, Component, EventResult, WizardMessage};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    layout::{Margin, Rect},
    Frame,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum UserControl {
    Password,
    Users,
}
pub struct UserStepView {
    root_password: Input,
    password_error: Option<String>,
    users: Vec<UserDraft>,
    selected: ListControlState,
    form: FormState<UserControl>,
}
fn validate_root_password(password: &str) -> Result<(), String> {
    validate_chpasswd_secret(password).map_err(|error| error.message("Root password"))
}
impl UserStepView {
    pub fn new(initial: &UserState) -> Self {
        let mut view = Self {
            root_password: Input::from(initial.root_password.clone()),
            password_error: None,
            users: initial.users.iter().map(UserDraft::editing_copy).collect(),
            selected: ListControlState::new(0),
            form: FormState::new(UserControl::Password),
        };
        view.form.set_focus(true);
        view.with_form(|_| ());
        view
    }
    fn with_form<R>(&mut self, visit: impl FnOnce(&mut Form<'_, UserControl>) -> R) -> R {
        let entries: Vec<_> = self
            .users
            .iter()
            .map(|user| {
                let sudo = if user.sudoer { " [sudo]" } else { "" };
                let wayland = user.wayland.as_ref().map_or(String::new(), |access| {
                    format!(" [wayland:{}]", access.sockets.len())
                });
                ListEntry::new(format!("  {}{sudo}{wayland}", user.username))
            })
            .collect();
        let mut users = ListControl::new(&mut self.selected, " Regular Users ", &entries);
        let mut password = TextInput::new(
            &mut self.root_password,
            " Root Password (optional) ",
            self.password_error.as_deref(),
        )
        .masked();
        visit(&mut Form::new(
            &mut self.form,
            [
                FormItem::field(UserControl::Password, &mut password),
                FormItem::field(UserControl::Users, &mut users).fill(4),
            ],
        ))
    }
}
impl Component for UserStepView {
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        register_step_form(footer, &self.form);
        if self.form.is_selected(UserControl::Users) {
            footer.register(FooterHint::new("a", "add user").on_key(KeyCode::Char('a')));
            if self.selected.selected().is_some() {
                footer.register(FooterHint::new("e", "edit user").on_key(KeyCode::Char('e')));
                footer.register(FooterHint::new("d", "delete").on_key(KeyCode::Char('d')));
            }
        }
    }
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.with_form(|form| form.render(frame, area.inner(Margin::new(1, 1))));
    }
    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if self.form.is_selected(UserControl::Users) {
            match key.code {
                KeyCode::Char('a' | 'A') => {
                    return EventResult::Message(AppMessage::Wizard(WizardMessage::OpenUserDialog))
                }
                KeyCode::Char('e' | 'E') => {
                    if let Some(index) = self.selected.selected() {
                        if let Some(user) = self.users.get(index) {
                            return EventResult::Message(AppMessage::Wizard(
                                WizardMessage::OpenUserEditDialog(index, user.editing_copy()),
                            ));
                        }
                    }
                }
                KeyCode::Delete | KeyCode::Char('d' | 'D') => {
                    if let Some(index) = self
                        .selected
                        .selected()
                        .filter(|index| *index < self.users.len())
                    {
                        self.users.remove(index);
                        self.selected.removed(index, self.users.len());
                        return EventResult::Message(AppMessage::Wizard(
                            WizardMessage::UserRemoved(index),
                        ));
                    }
                }
                _ => {}
            }
        }
        field_event(self.with_form(|form| form.handle_key(key)))
    }
    fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        field_event(self.with_form(|form| form.handle_mouse(mouse)))
    }
    fn set_focus(&mut self, focused: bool) {
        self.form.set_focus(focused);
    }
    fn validate(&mut self) -> Result<(), String> {
        let result = validate_root_password(self.root_password.value());
        self.password_error = result.as_ref().err().cloned();
        result
    }
}
impl StepComponent for UserStepView {
    fn commit_to_draft(&self, context: &mut WizardDraft) {
        replace_secret_string(&mut context.user.root_password, self.root_password.value());
        context.user.users = self.users.iter().map(UserDraft::editing_copy).collect();
    }
    fn render_step(&mut self, frame: &mut Frame, area: Rect, _: &WizardDraft) {
        self.render(frame, area);
    }
    fn handle_message(&mut self, message: &AppMessage) -> StepAction {
        match message {
            AppMessage::Wizard(WizardMessage::UserAdded(user)) => {
                self.users.push(user.editing_copy());
                self.with_form(|_| ());
                StepAction::CloseDialog
            }
            AppMessage::Wizard(WizardMessage::UserUpdated(index, user)) => {
                if let Some(current) = self.users.get_mut(*index) {
                    *current = user.editing_copy();
                }
                self.with_form(|_| ());
                StepAction::CloseDialog
            }
            AppMessage::Wizard(WizardMessage::DialogCancel) => StepAction::CloseDialog,
            _ => StepAction::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    #[test]
    fn repeated_commits_keep_the_root_password_free_of_zeroized_prefixes() {
        let mut draft = WizardDraft::new(
            Vec::new(),
            Vec::new(),
            crate::domain::nvidia::NvidiaCdiSource::Generate,
            crate::application::provisioning::ProvisioningHostSnapshot::default(),
            crate::config::AppConfig::default().bootstrap.resolve(),
        );
        let mut view = UserStepView::new(&draft.user);

        for character in "Password123".chars() {
            assert_eq!(
                view.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE,)),
                EventResult::Consumed,
            );
            view.commit_to_draft(&mut draft);
            assert!(!draft.user.root_password.chars().any(char::is_control));
        }

        assert_eq!(draft.user.root_password, "Password123");
        assert!(validate_root_password(&draft.user.root_password).is_ok());
    }
}
