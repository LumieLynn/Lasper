use crate::domain::secret::replace_secret_string;
use crate::nspawn::errors::NspawnError;
use crate::nspawn::models::validate_chpasswd_secret;
use crate::tui::core::{AppMessage, Component, EventResult, FocusTracker, WizardMessage};
use crate::tui::widgets::inputs::password_box::PasswordBox;
use crate::tui::widgets::lists::editable_list::EditableList;
use crate::tui::wizard::draft::{UserDraft, UserState, WizardDraft};
use crate::tui::wizard::steps::StepComponent;
use crate::tui::wizard::StepAction;
use crate::{delegate_wizard_navigation, impl_wizard_nav, wizard_set_focus};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    widgets::Paragraph,
    Frame,
};

macro_rules! active_comps {
    ($self:ident) => {{
        let comps: Vec<&mut dyn Component> = vec![&mut $self.root_password, &mut $self.user_list];
        comps
    }};
}

impl_wizard_nav!(UserStepView, active_comps);

pub struct UserStepView {
    root_password: PasswordBox,
    user_list: EditableList<UserDraft>,

    focus: FocusTracker,
}

fn validate_root_password(password: &str) -> Result<(), String> {
    validate_chpasswd_secret("Root password", password).map_err(|error| match error {
        NspawnError::Validation(message) => message,
        other => other.to_string(),
    })
}

impl UserStepView {
    pub fn new(initial_data: &UserState) -> Self {
        let users = initial_data
            .users
            .iter()
            .map(UserDraft::editing_copy)
            .collect();

        let mut view = Self {
            root_password: PasswordBox::new(
                " Root Password (optional) ",
                initial_data.root_password.clone(),
            )
            .with_validator(validate_root_password),
            user_list: EditableList::new(
                " Regular Users ",
                users,
                |u| {
                    let sudo = if u.sudoer { " [sudo]" } else { "" };
                    let wayland = u.wayland.as_ref().map_or(String::new(), |access| {
                        format!(" [wayland:{}]", access.sockets.len())
                    });
                    format!("  {}{sudo}{wayland}", u.username)
                },
                |idx| AppMessage::Wizard(WizardMessage::UserRemoved(idx)),
            ),

            focus: FocusTracker::new(),
        };
        view.update_focus();
        view
    }
}

impl Component for UserStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(3), // Root password
                Constraint::Min(0),    // List
                Constraint::Length(1), // Hint
            ])
            .split(area);

        self.root_password.render(f, chunks[0]);
        self.user_list.render(f, chunks[1]);

        let hint = " [Tab] switch, [A]dd user, [E]dit user, [D]elete user, [Enter] next ";
        f.render_widget(
            Paragraph::new(hint)
                .style(Style::default().fg(crate::tui::theme::theme().wizard_footer)),
            chunks[2],
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if self.user_list.is_focused() {
            match key.code {
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    return EventResult::Message(AppMessage::Wizard(WizardMessage::OpenUserDialog));
                }
                KeyCode::Char('e') | KeyCode::Char('E') => {
                    if let Some(user) = self.user_list.selected_item() {
                        let idx = self.user_list.selected();
                        return EventResult::Message(AppMessage::Wizard(
                            WizardMessage::OpenUserEditDialog(idx, user.editing_copy()),
                        ));
                    }
                }
                _ => {}
            }
        }

        let res = delegate_wizard_navigation!(self, key, active_comps);
        if let EventResult::Message(AppMessage::Wizard(WizardMessage::UserRemoved(_))) = res {
            self.update_focus();
        }
        res
    }

    fn set_focus(&mut self, focused: bool) {
        wizard_set_focus!(self, focused, active_comps);
    }

    fn validate(&mut self) -> Result<(), String> {
        self.root_password.validate()?;
        Ok(())
    }
}

impl StepComponent for UserStepView {
    fn commit_to_draft(&self, ctx: &mut WizardDraft) {
        replace_secret_string(&mut ctx.user.root_password, self.root_password.value());
        ctx.user.users = self
            .user_list
            .items()
            .iter()
            .map(UserDraft::editing_copy)
            .collect();
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardDraft) {
        self.render(f, area);
    }

    fn handle_message(&mut self, msg: &AppMessage) -> StepAction {
        match msg {
            AppMessage::Wizard(WizardMessage::UserAdded(user)) => {
                self.user_list.add_item(user.editing_copy());
                self.update_focus();
                StepAction::CloseDialog
            }
            AppMessage::Wizard(WizardMessage::UserUpdated(idx, user)) => {
                self.user_list.update_item(*idx, user.editing_copy());
                self.update_focus();
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
    use std::sync::Arc;

    #[test]
    fn repeated_commits_keep_the_root_password_free_of_zeroized_prefixes() {
        let mut draft = WizardDraft::new(
            Vec::new(),
            Vec::new(),
            Arc::new(crate::config::AppConfig::default()),
            crate::application::provisioning::ProvisioningHostSnapshot::default(),
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
