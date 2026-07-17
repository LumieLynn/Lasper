use crate::nspawn::errors::NspawnError;
use crate::nspawn::models::{validate_chpasswd_secret, CreateUser};
use crate::ui::core::{AppMessage, Component, EventResult, FocusTracker, WizardMessage};
use crate::ui::widgets::inputs::password_box::PasswordBox;
use crate::ui::widgets::lists::editable_list::EditableList;
use crate::ui::wizard::context::{UserConfig, WizardContext};
use crate::ui::wizard::steps::StepComponent;
use crate::ui::wizard::StepAction;
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
    user_list: EditableList<CreateUser>,

    focus: FocusTracker,
}

fn validate_root_password(password: &str) -> Result<(), String> {
    validate_chpasswd_secret("Root password", password).map_err(|error| match error {
        NspawnError::Validation(message) => message,
        other => other.to_string(),
    })
}

impl UserStepView {
    pub fn new(initial_data: &UserConfig) -> Self {
        let users = initial_data.users.clone();

        let mut view = Self {
            root_password: PasswordBox::new(
                " Root Password (optional) ",
                initial_data.root_password.clone().unwrap_or_default(),
            )
            .with_validator(validate_root_password),
            user_list: EditableList::new(
                " Regular Users ",
                users,
                |u| format!("  {} {}", u.username, if u.sudoer { "[sudo]" } else { "" }),
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
                .style(Style::default().fg(crate::ui::theme::theme().wizard_footer)),
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
                            WizardMessage::OpenUserEditDialog(idx, user.clone()),
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
    fn commit_to_context(&self, ctx: &mut WizardContext) {
        ctx.user.root_password = self.root_password.value().to_string();
        ctx.user.users = self.user_list.items().to_vec();
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardContext) {
        self.render(f, area);
    }

    fn handle_message(&mut self, msg: &AppMessage) -> StepAction {
        match msg {
            AppMessage::Wizard(WizardMessage::UserAdded(user)) => {
                self.user_list.add_item(user.clone());
                self.update_focus();
                StepAction::CloseDialog
            }
            AppMessage::Wizard(WizardMessage::UserUpdated(idx, user)) => {
                self.user_list.update_item(*idx, user.clone());
                self.update_focus();
                StepAction::CloseDialog
            }
            AppMessage::Wizard(WizardMessage::DialogCancel) => StepAction::CloseDialog,
            _ => StepAction::None,
        }
    }
}
