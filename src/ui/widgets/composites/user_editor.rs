use crate::nspawn::models::CreateUser;
use crate::ui::core::{AppMessage, Component, EventResult, FocusTracker, WizardMessage};

use crate::ui::widgets::inputs::button::Button;
use crate::ui::widgets::inputs::password_box::PasswordBox;
use crate::ui::widgets::inputs::text_box::TextBox;
use crate::ui::widgets::selectors::checkbox::Checkbox;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};

pub struct UserEditor {
    username: TextBox,
    password: PasswordBox,
    shell: TextBox,
    sudoer: Checkbox,

    btn_ok: Button,
    btn_cancel: Button,
    focus: FocusTracker,
    on_submit: Box<dyn Fn(CreateUser) -> AppMessage>,
}

fn validate_username(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Username cannot be empty".into());
    }
    if name.len() > 32 {
        return Err("Username is too long".into());
    }
    let bytes = name.as_bytes();
    let first = bytes[0];
    if !first.is_ascii_alphabetic() && first != b'_' {
        return Err("Must start with a letter or '_'".into());
    }
    for (i, &b) in bytes.iter().enumerate().skip(1) {
        if !b.is_ascii_alphanumeric() && b != b'_' && b != b'-' {
            if i == bytes.len() - 1 && b == b'$' {
                continue;
            }
            return Err("Contains invalid characters".into());
        }
    }
    Ok(())
}

impl UserEditor {
    pub fn new(on_submit: impl Fn(CreateUser) -> AppMessage + 'static) -> Self {
        let mut editor = Self {
            username: TextBox::new(" Username ", String::new()).with_validator(validate_username),
            password: PasswordBox::new(" Password (optional) ", String::new()),
            shell: TextBox::new(" Shell ", "/bin/bash".to_string()),

            sudoer: Checkbox::new(" Add to sudo/wheel group ", false),
            btn_ok: Button::new("OK", AppMessage::Wizard(WizardMessage::DialogSubmit)),
            btn_cancel: Button::new("Cancel", AppMessage::Wizard(WizardMessage::DialogCancel)),

            focus: FocusTracker::new(),
            on_submit: Box::new(on_submit),
        };
        editor.update_focus();
        editor
    }

    pub fn with_user(mut self, user: &CreateUser) -> Self {
        self.username =
            TextBox::new(" Username ", user.username.clone()).with_validator(validate_username);
        self.password = PasswordBox::new(" Password (optional) ", user.password.clone());
        self.shell = TextBox::new(" Shell ", user.shell.clone());

        self.sudoer = Checkbox::new(" Add to sudo/wheel group ", user.sudoer);
        self.update_focus();
        self
    }

    fn update_focus(&mut self) {
        let mut components: Vec<&mut dyn Component> = vec![
            &mut self.username,
            &mut self.password,
            &mut self.shell,
            &mut self.sudoer,
            &mut self.btn_ok,
            &mut self.btn_cancel,
        ];
        self.focus.update_focus(&mut components, true);
    }

    fn next(&mut self) {
        let comps: Vec<&dyn Component> = vec![
            &self.username,
            &self.password,
            &self.shell,
            &self.sudoer,
            &self.btn_ok,
            &self.btn_cancel,
        ];
        self.focus.next(&comps);
        self.update_focus();
    }

    fn prev(&mut self) {
        let comps: Vec<&dyn Component> = vec![
            &self.username,
            &self.password,
            &self.shell,
            &self.sudoer,
            &self.btn_ok,
            &self.btn_cancel,
        ];
        self.focus.prev(&comps);
        self.update_focus();
    }
}

impl Component for UserEditor {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(3),
            ])
            .split(area);

        self.username.render(f, chunks[0]);
        self.password.render(f, chunks[1]);
        self.shell.render(f, chunks[2]);
        self.sudoer.render(f, chunks[3]);

        let btn_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[5]);

        let ok_area = crate::ui::centered_rect(60, 100, btn_chunks[0]);
        let cancel_area = crate::ui::centered_rect(60, 100, btn_chunks[1]);
        self.btn_ok.render(f, ok_area);
        self.btn_cancel.render(f, cancel_area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Tab => {
                self.next();
                return EventResult::Consumed;
            }
            KeyCode::BackTab => {
                self.prev();
                return EventResult::Consumed;
            }
            KeyCode::Enter if !self.btn_ok.is_focused() && !self.btn_cancel.is_focused() => {
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
                    return EventResult::Consumed;
                }

                let user = CreateUser {
                    username: self.username.value().to_string(),
                    password: self.password.value().to_string(),
                    shell: self.shell.value().to_string(),
                    sudoer: self.sudoer.checked(),
                };
                return EventResult::Message((self.on_submit)(user));

            }
            _ => {}
        }

        let mut comps: Vec<&mut dyn Component> = vec![
            &mut self.username,
            &mut self.password,
            &mut self.shell,
            &mut self.sudoer,
            &mut self.btn_ok,
            &mut self.btn_cancel,
        ];

        let res = comps[self.focus.active_idx].handle_key(key);
        match res {
            EventResult::Message(AppMessage::Wizard(WizardMessage::DialogSubmit)) => {
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
                    return EventResult::Consumed;
                }

                let user = CreateUser {
                    username: self.username.value().to_string(),
                    password: self.password.value().to_string(),
                    shell: self.shell.value().to_string(),
                    sudoer: self.sudoer.checked(),
                };
                EventResult::Message((self.on_submit)(user))

            }
            EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel)) => {
                EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel))
            }

            EventResult::FocusNext => {
                self.next();
                EventResult::Consumed
            }
            EventResult::FocusPrev => {
                self.prev();
                EventResult::Consumed
            }
            _ => res,
        }
    }

    fn set_focus(&mut self, focused: bool) {
        if focused {
            self.update_focus();
        } else {
            self.username.set_focus(false);
            self.password.set_focus(false);
            self.shell.set_focus(false);
            self.sudoer.set_focus(false);
            self.btn_ok.set_focus(false);
            self.btn_cancel.set_focus(false);
        }
    }
}
