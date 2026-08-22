use crate::domain::wayland::HostWaylandSocket;
use crate::nspawn::errors::{NspawnError, Result as NspawnResult};
use crate::nspawn::models::{
    validate_chpasswd_secret, validate_login_shell, validate_login_username,
};
use crate::tui::core::{AppMessage, Component, EventResult, FocusTracker, WizardMessage};
use crate::tui::widgets::dialogs::wayland_access::WaylandAccessDialog;
use crate::tui::widgets::inputs::button::Button;
use crate::tui::widgets::inputs::password_box::PasswordBox;
use crate::tui::widgets::inputs::text_box::TextBox;
use crate::tui::widgets::selectors::checkbox::Checkbox;
use crate::tui::wizard::draft::{UserDraft, WaylandAccessDraft};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    widgets::{Block, BorderType, Borders, Clear},
    Frame,
};

macro_rules! active_comps {
    ($self:ident) => {{
        let comps: Vec<&mut dyn Component> = vec![
            &mut $self.username,
            &mut $self.password,
            &mut $self.shell,
            &mut $self.sudoer,
            &mut $self.wayland_toggle,
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
    wayland_toggle: Checkbox,
    wayland_sockets: Vec<HostWaylandSocket>,
    wayland: Option<WaylandAccessDraft>,
    wayland_dialog: Option<WaylandAccessDialog>,
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
    pub fn new(
        wayland_sockets: Vec<HostWaylandSocket>,
        wayland_owner: Option<&str>,
        on_submit: impl Fn(UserDraft) -> AppMessage + 'static,
    ) -> Self {
        let available = !wayland_sockets.is_empty() && wayland_owner.is_none();
        let label = match (wayland_sockets.is_empty(), wayland_owner) {
            (true, _) => "Wayland access (no displays)".to_string(),
            (false, Some(owner)) => format!("Wayland: used by {owner}"),
            (false, None) => "Wayland access".to_string(),
        };
        let mut editor = Self {
            username: TextBox::new(" Username ", String::new()).with_validator(validate_username),
            password: PasswordBox::new(" Password (optional) ", String::new())
                .with_validator(validate_password),
            shell: TextBox::new(" Shell ", "/bin/bash".to_string()).with_validator(validate_shell),
            sudoer: Checkbox::new("Add to sudo/wheel group", false),
            wayland_toggle: Checkbox::new(label, false).with_enabled(available),
            wayland_sockets,
            wayland: None,
            wayland_dialog: None,
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
        self.sudoer = Checkbox::new("Add to sudo/wheel group", user.sudoer);
        self.wayland = user.wayland.clone();
        self.wayland_toggle.set_checked(self.wayland.is_some());
        if self.wayland.is_some() {
            self.wayland_toggle.set_enabled(true);
        }
        self.update_focus();
        self
    }

    fn update_focus(&mut self) {
        let mut comps = active_comps!(self);
        self.focus.update_focus(&mut comps, true);
    }

    fn next(&mut self) {
        let comps = active_comps!(self);
        self.focus.next(&comps);
        self.update_focus();
    }

    fn prev(&mut self) {
        let comps = active_comps!(self);
        self.focus.prev(&comps);
        self.update_focus();
    }

    fn open_wayland_dialog(&mut self) {
        let mut dialog =
            WaylandAccessDialog::new(self.wayland_sockets.clone(), self.wayland.as_ref());
        dialog.set_focus(true);
        for component in active_comps!(self) {
            component.set_focus(false);
        }
        self.wayland_dialog = Some(dialog);
    }

    fn try_submit(&mut self) -> Option<AppMessage> {
        if self.username.validate().is_err()
            || self.password.validate().is_err()
            || self.shell.validate().is_err()
            || (self.wayland_toggle.checked() && self.wayland.is_none())
        {
            return None;
        }
        let user = UserDraft {
            username: self.username.value().to_string(),
            password: self.password.value().to_string(),
            shell: self.shell.value().to_string(),
            sudoer: self.sudoer.checked(),
            wayland: self.wayland.clone(),
        };
        Some((self.on_submit)(user))
    }

    fn handle_wayland_message(&mut self, result: EventResult) -> EventResult {
        match result {
            EventResult::Message(AppMessage::Wizard(WizardMessage::WaylandAccessConfigured(
                access,
            ))) => {
                self.wayland = Some(access);
                self.wayland_toggle.set_checked(true);
                self.wayland_dialog = None;
                self.update_focus();
                EventResult::Consumed
            }
            EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel)) => {
                self.wayland_dialog = None;
                self.wayland_toggle.set_checked(self.wayland.is_some());
                self.update_focus();
                EventResult::Consumed
            }
            _ => EventResult::Consumed,
        }
    }
}

impl Component for UserEditor {
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let dialog_area = crate::tui::centered_rect(70, 85, area);
        frame.render_widget(Clear, dialog_area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Add/Edit User ")
            .border_style(Style::default().fg(crate::tui::theme::theme().dialog_border));
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(3),
            ])
            .split(inner);
        self.username.render(frame, chunks[0]);
        self.password.render(frame, chunks[1]);
        self.shell.render(frame, chunks[2]);
        self.sudoer.render(frame, chunks[3]);
        self.wayland_toggle.render(frame, chunks[4]);

        let buttons = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[6]);
        self.btn_ok.render(frame, buttons[0]);
        self.btn_cancel.render(frame, buttons[1]);

        if let Some(dialog) = &mut self.wayland_dialog {
            dialog.render(frame, area);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if let Some(dialog) = &mut self.wayland_dialog {
            let result = dialog.handle_key(key);
            return self.handle_wayland_message(result);
        }

        if key.code == KeyCode::Esc {
            return EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel));
        }
        if self.wayland_toggle.is_focused() {
            if key.code == KeyCode::Char(' ') {
                if self.wayland_toggle.checked() {
                    self.wayland_toggle.set_checked(false);
                    self.wayland = None;
                } else {
                    self.wayland_toggle.set_checked(true);
                    self.open_wayland_dialog();
                }
                return EventResult::Consumed;
            }
            if key.code == KeyCode::Enter && self.wayland_toggle.is_enabled() {
                self.wayland_toggle.set_checked(true);
                self.open_wayland_dialog();
                return EventResult::Consumed;
            }
        }

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
                return self
                    .try_submit()
                    .map_or(EventResult::Consumed, EventResult::Message);
            }
            _ => {}
        }

        let mut comps = active_comps!(self);
        let result = comps[self.focus.active_idx].handle_key(key);
        match result {
            EventResult::Message(AppMessage::Wizard(WizardMessage::DialogSubmit)) => self
                .try_submit()
                .map_or(EventResult::Consumed, EventResult::Message),
            EventResult::FocusNext => {
                self.next();
                EventResult::Consumed
            }
            EventResult::FocusPrev => {
                self.prev();
                EventResult::Consumed
            }
            other => other,
        }
    }

    fn set_focus(&mut self, focused: bool) {
        if focused {
            if let Some(dialog) = &mut self.wayland_dialog {
                dialog.set_focus(true);
            } else {
                self.update_focus();
            }
        } else {
            for component in active_comps!(self) {
                component.set_focus(false);
            }
            if let Some(dialog) = &mut self.wayland_dialog {
                dialog.set_focus(false);
            }
        }
    }

    fn is_focused(&self) -> bool {
        self.wayland_dialog
            .as_ref()
            .is_some_and(Component::is_focused)
            || self.username.is_focused()
            || self.password.is_focused()
            || self.shell.is_focused()
            || self.sudoer.is_focused()
            || self.wayland_toggle.is_focused()
            || self.btn_ok.is_focused()
            || self.btn_cancel.is_focused()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::wayland::{SocketRevision, WaylandDisplay};
    use crossterm::event::KeyModifiers;
    use ratatui::{backend::TestBackend, Terminal};

    fn socket() -> HostWaylandSocket {
        HostWaylandSocket::from_verified_parts(
            WaylandDisplay::new("wayland-0").unwrap(),
            "/run/user/1001".into(),
            "/run/user/1001/wayland-0".into(),
            1001,
            1001,
            1001,
            0o700,
            SocketRevision {
                device: 1,
                inode: 2,
                ctime_seconds: 3,
                ctime_nanoseconds: 4,
            },
        )
        .unwrap()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn another_users_grant_disables_wayland_access() {
        let editor = UserEditor::new(vec![socket()], Some("alice"), |_| {
            AppMessage::Wizard(WizardMessage::DialogCancel)
        });

        assert!(!editor.wayland_toggle.is_enabled());
        assert!(!editor.wayland_toggle.checked());
    }

    #[test]
    fn escape_closes_only_the_nested_wayland_dialog() {
        let mut editor = UserEditor::new(vec![socket()], None, |_| {
            AppMessage::Wizard(WizardMessage::DialogCancel)
        });
        editor.focus.active_idx = 4;
        editor.update_focus();

        assert_eq!(
            editor.handle_key(key(KeyCode::Char(' '))),
            EventResult::Consumed
        );
        assert!(editor.wayland_dialog.is_some());
        assert_eq!(editor.handle_key(key(KeyCode::Esc)), EventResult::Consumed);
        assert!(editor.wayland_dialog.is_none());
        assert!(!editor.wayland_toggle.checked());
    }

    #[test]
    fn compact_terminal_keeps_wayland_control_and_buttons_visible() {
        crate::tui::theme::init_theme(crate::tui::theme::Theme::dark());
        let mut editor = UserEditor::new(vec![socket()], None, |_| {
            AppMessage::Wizard(WizardMessage::DialogCancel)
        });
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| editor.render(frame, frame.area()))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("Wayland access"));
        assert!(rendered.contains("OK"));
        assert!(rendered.contains("Cancel"));
    }
}
