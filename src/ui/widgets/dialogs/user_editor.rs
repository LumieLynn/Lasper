use crate::nspawn::errors::{NspawnError, Result as NspawnResult};
use crate::nspawn::models::{
    validate_chpasswd_secret, validate_login_shell, validate_login_username,
};
use crate::ui::core::{AppMessage, Component, FocusTracker, WizardMessage};
use crate::ui::wizard::draft::UserDraft;

use crate::ui::widgets::inputs::button::Button;
use crate::ui::widgets::inputs::password_box::PasswordBox;
use crate::ui::widgets::inputs::text_box::TextBox;
use crate::ui::widgets::selectors::checkbox::Checkbox;

macro_rules! active_comps {
    ($self:ident) => {{
        let comps: Vec<&mut dyn Component> = vec![
            &mut $self.username,
            &mut $self.password,
            &mut $self.shell,
            &mut $self.sudoer,
            &mut $self.btn_ok,
            &mut $self.btn_cancel,
        ];
        comps
    }};
}

pub struct UserEditor {
    username: TextBox,
    password: PasswordBox,
    shell: TextBox,
    sudoer: Checkbox,

    btn_ok: Button,
    btn_cancel: Button,
    focus: FocusTracker,
    on_submit: Box<dyn Fn(UserDraft) -> AppMessage>,
}

fn validation_message(result: NspawnResult<()>) -> Result<(), String> {
    result.map_err(|error| match error {
        NspawnError::Validation(message) => message,
        other => other.to_string(),
    })
}

fn validate_username(name: &str) -> Result<(), String> {
    validation_message(validate_login_username(name))
}

fn validate_password(password: &str) -> Result<(), String> {
    validation_message(validate_chpasswd_secret("Password", password))
}

fn validate_shell(shell: &str) -> Result<(), String> {
    validation_message(validate_login_shell(shell))
}

impl UserEditor {
    pub fn new(on_submit: impl Fn(UserDraft) -> AppMessage + 'static) -> Self {
        let mut editor = Self {
            username: TextBox::new(" Username ", String::new()).with_validator(validate_username),
            password: PasswordBox::new(" Password (optional) ", String::new())
                .with_validator(validate_password),
            shell: TextBox::new(" Shell ", "/bin/bash".to_string()).with_validator(validate_shell),

            sudoer: Checkbox::new(" Add to sudo/wheel group ", false),
            btn_ok: Button::new("OK", || AppMessage::Wizard(WizardMessage::DialogSubmit)),
            btn_cancel: Button::new("Cancel", || AppMessage::Wizard(WizardMessage::DialogCancel)),

            focus: FocusTracker::new(),
            on_submit: Box::new(on_submit),
        };
        editor.update_focus();
        editor
    }

    pub fn with_user(mut self, user: &UserDraft) -> Self {
        self.username =
            TextBox::new(" Username ", user.username.clone()).with_validator(validate_username);
        self.password = PasswordBox::new(" Password (optional) ", user.password.clone())
            .with_validator(validate_password);
        self.shell = TextBox::new(" Shell ", user.shell.clone()).with_validator(validate_shell);

        self.sudoer = Checkbox::new(" Add to sudo/wheel group ", user.sudoer);
        self.update_focus();
        self
    }

    fn try_submit(&mut self) -> Option<AppMessage> {
        let mut valid = true;
        if self.username.validate().is_err() {
            valid = false;
        }
        if self.password.validate().is_err() {
            valid = false;
        }
        if self.shell.validate().is_err() {
            valid = false;
        }
        if !valid {
            return None;
        }
        let user = UserDraft {
            username: self.username.value().to_string(),
            password: self.password.value().to_string(),
            shell: self.shell.value().to_string(),
            sudoer: self.sudoer.checked(),
        };
        Some((self.on_submit)(user))
    }
}

form_dialog!(
    UserEditor,
    " Add/Edit User ",
    (40, 60),
    [username, password, shell, sudoer]
);
