use crate::domain::provisioning::{validate_login_shell, validate_login_username};
use crate::domain::secret::validate_chpasswd_secret;
use crate::domain::wayland::{HostWaylandSocket, WaylandDisplay};
use crate::tui::features::wizard::dialogs::wayland_bindings::WaylandBindingsDialog;
use crate::tui::features::wizard::draft::{UserDraft, WaylandAccessDraft};
use crate::tui::foundation::components::form::{Form, FormItem, FormOutcome, FormState};
use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::foundation::widgets::inputs::button::ButtonControl;
use crate::tui::foundation::widgets::inputs::text_input::{Input, TextInput};
use crate::tui::foundation::widgets::selectors::checkbox::CheckboxControl;
use crate::tui::protocol::{AppMessage, Component, EventResult, WizardMessage};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    layout::Rect,
    style::Style,
    widgets::{Block, BorderType, Borders, Clear},
    Frame,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum UserControl {
    Username,
    Password,
    Shell,
    Sudoer,
    Wayland,
    Submit,
    Cancel,
}

#[derive(Default)]
struct UserErrors {
    username: Option<String>,
    password: Option<String>,
    shell: Option<String>,
}

pub struct UserEditor {
    username: Input,
    password: Input,
    shell: Input,
    errors: UserErrors,
    sudoer: bool,
    wayland_selected: bool,
    wayland_available: bool,
    wayland_label: String,
    wayland_sockets: Vec<HostWaylandSocket>,
    preferred_wayland_display: Option<WaylandDisplay>,
    wayland: Option<WaylandAccessDraft>,
    wayland_dialog: Option<WaylandBindingsDialog>,
    form: FormState<UserControl>,
    on_submit: Box<dyn Fn(UserDraft) -> AppMessage>,
}

fn validate_username(name: &str) -> Result<(), String> {
    validate_login_username(name).map_err(|error| error.to_string())
}

fn validate_password(password: &str) -> Result<(), String> {
    validate_chpasswd_secret(password).map_err(|error| error.message("Password"))
}

fn validate_shell(shell: &str) -> Result<(), String> {
    validate_login_shell(shell).map_err(|error| error.to_string())
}

impl UserEditor {
    pub fn new(
        wayland_sockets: Vec<HostWaylandSocket>,
        preferred_wayland_display: Option<WaylandDisplay>,
        wayland_owner: Option<&str>,
        on_submit: impl Fn(UserDraft) -> AppMessage + 'static,
    ) -> Self {
        let available = !wayland_sockets.is_empty() && wayland_owner.is_none();
        let label = match (wayland_sockets.is_empty(), wayland_owner) {
            (true, _) => "Wayland access (no sockets found)".to_string(),
            (false, Some(owner)) => format!("Wayland: used by {owner}"),
            (false, None) => "Wayland access".to_string(),
        };
        let mut editor = Self {
            username: Input::default(),
            password: Input::default(),
            shell: Input::from("/bin/bash"),
            errors: UserErrors::default(),
            sudoer: false,
            wayland_selected: false,
            wayland_available: available,
            wayland_label: label,
            wayland_sockets,
            preferred_wayland_display,
            wayland: None,
            wayland_dialog: None,
            form: FormState::new(UserControl::Username),
            on_submit: Box::new(on_submit),
        };
        editor.with_form(|_| ());
        editor.set_focus(true);
        editor
    }

    pub fn with_user(mut self, user: &UserDraft) -> Self {
        self.username = Input::from(user.username.clone());
        self.password = Input::from(user.password.clone());
        self.shell = Input::from(user.shell.clone());
        self.sudoer = user.sudoer;
        self.wayland = user.wayland.clone();
        self.wayland_selected = self.wayland.is_some();
        if self.wayland.is_some() {
            self.wayland_available = true;
        }
        self.with_form(|_| ());
        self
    }

    fn with_form<R>(&mut self, visit: impl FnOnce(&mut Form<'_, UserControl>) -> R) -> R {
        let mut username = TextInput::new(
            &mut self.username,
            " Username ",
            self.errors.username.as_deref(),
        );
        let mut password = TextInput::new(
            &mut self.password,
            " Password (optional) ",
            self.errors.password.as_deref(),
        )
        .masked();
        let mut shell = TextInput::new(&mut self.shell, " Shell ", self.errors.shell.as_deref());
        let mut sudoer = CheckboxControl::new(&mut self.sudoer, "Add to sudo/wheel group");
        let mut wayland = CheckboxControl::new(&mut self.wayland_selected, &self.wayland_label);
        let mut submit = ButtonControl::new("OK");
        let mut cancel = ButtonControl::new("Cancel");
        let mut form = Form::new(
            &mut self.form,
            [
                FormItem::field(UserControl::Username, &mut username),
                FormItem::field(UserControl::Password, &mut password),
                FormItem::field(UserControl::Shell, &mut shell),
                FormItem::field(UserControl::Sudoer, &mut sudoer),
                FormItem::field(UserControl::Wayland, &mut wayland)
                    .with_enabled(self.wayland_available)
                    .on_enter("Wayland bindings"),
                FormItem::spacer(),
                FormItem::row([
                    FormItem::field(UserControl::Submit, &mut submit),
                    FormItem::field(UserControl::Cancel, &mut cancel),
                ]),
            ],
        )
        .with_actions(UserControl::Submit, UserControl::Cancel);
        visit(&mut form)
    }

    fn open_wayland_dialog(&mut self) {
        let mut dialog = WaylandBindingsDialog::new(
            self.wayland_sockets.clone(),
            self.preferred_wayland_display.clone(),
            self.wayland.as_ref(),
        );
        dialog.set_focus(true);
        self.form.set_focus(false);
        self.wayland_dialog = Some(dialog);
    }

    fn try_submit(&mut self) -> Option<AppMessage> {
        self.errors.username = validate_username(self.username.value()).err();
        self.errors.password = validate_password(self.password.value()).err();
        self.errors.shell = validate_shell(self.shell.value()).err();
        if self.errors.username.is_some()
            || self.errors.password.is_some()
            || self.errors.shell.is_some()
            || (self.wayland_selected && self.wayland.is_none())
        {
            return None;
        }
        let user = UserDraft {
            username: self.username.value().to_string(),
            password: self.password.value().to_string(),
            shell: self.shell.value().to_string(),
            sudoer: self.sudoer,
            wayland: self.wayland.clone(),
        };
        Some((self.on_submit)(user))
    }

    fn finish_control_input(&mut self, outcome: FormOutcome<UserControl>) -> EventResult {
        match outcome {
            FormOutcome::Changed(UserControl::Wayland) => {
                if self.wayland_selected {
                    self.open_wayland_dialog();
                } else {
                    self.wayland = None;
                }
                EventResult::Consumed
            }
            FormOutcome::Activated(UserControl::Wayland) => {
                self.wayland_selected = true;
                self.open_wayland_dialog();
                EventResult::Consumed
            }
            FormOutcome::Activated(UserControl::Submit) => self
                .try_submit()
                .map_or(EventResult::Consumed, EventResult::Message),
            FormOutcome::Activated(UserControl::Cancel) => {
                EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel))
            }
            FormOutcome::Ignored => EventResult::Ignored,
            _ => EventResult::Consumed,
        }
    }

    fn handle_wayland_message(&mut self, result: EventResult) -> EventResult {
        match result {
            EventResult::Message(AppMessage::Wizard(WizardMessage::WaylandAccessConfigured(
                access,
            ))) => {
                self.wayland = Some(access);
                self.wayland_selected = true;
                self.wayland_dialog = None;
                self.form.set_focus(true);
                EventResult::Consumed
            }
            EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel)) => {
                self.wayland_dialog = None;
                self.wayland_selected = self.wayland.is_some();
                self.form.set_focus(true);
                EventResult::Consumed
            }
            _ => EventResult::Consumed,
        }
    }
}

impl Component for UserEditor {
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        if let Some(dialog) = &self.wayland_dialog {
            dialog.register_footer_hints(footer);
            return;
        }
        if self.form.is_selected(UserControl::Wayland) && self.wayland_available {
            footer.register(FooterHint::new("Space", "select displays").on_key(KeyCode::Char(' ')));
        }
        self.form.register_footer_hints(footer);
    }

    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let dialog_area = crate::tui::foundation::geometry::centered_rect(70, 85, area);
        frame.render_widget(Clear, dialog_area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Add/Edit User ")
            .border_style(
                Style::default().fg(crate::tui::foundation::theme::theme().dialog_border),
            );
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);
        self.with_form(|form| form.render(frame, inner));

        if let Some(dialog) = &mut self.wayland_dialog {
            dialog.render(frame, area);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if let Some(dialog) = &mut self.wayland_dialog {
            let result = dialog.handle_key(key);
            return self.handle_wayland_message(result);
        }

        let outcome = self.with_form(|form| form.handle_key(key));
        self.finish_control_input(outcome)
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        if let Some(dialog) = &mut self.wayland_dialog {
            let result = dialog.handle_mouse(mouse);
            return self.handle_wayland_message(result);
        }
        let outcome = self.with_form(|form| form.handle_mouse(mouse));
        self.finish_control_input(outcome)
    }

    fn set_focus(&mut self, focused: bool) {
        if focused {
            if let Some(dialog) = &mut self.wayland_dialog {
                dialog.set_focus(true);
            } else {
                self.form.set_focus(true);
            }
        } else {
            self.form.set_focus(false);
            if let Some(dialog) = &mut self.wayland_dialog {
                dialog.set_focus(false);
            }
        }
    }

    fn is_focused(&self) -> bool {
        self.wayland_dialog
            .as_ref()
            .is_some_and(Component::is_focused)
            || self.form.is_focused()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::wayland::{SocketRevision, WaylandDisplay};
    use crossterm::event::{KeyCode, KeyModifiers};
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

    fn click_control(terminal: &Terminal<TestBackend>, label: &str) -> MouseEvent {
        use unicode_width::UnicodeWidthStr;
        let buffer = terminal.backend().buffer();
        for row in 0..buffer.area.height {
            let line: String = (0..buffer.area.width)
                .map(|column| buffer[(column, row)].symbol())
                .collect();
            if let Some(column) = line.find(label) {
                return MouseEvent {
                    kind: crossterm::event::MouseEventKind::Down(
                        crossterm::event::MouseButton::Left,
                    ),
                    column: line[..column].width() as u16,
                    row,
                    modifiers: KeyModifiers::NONE,
                };
            }
        }
        panic!("missing control: {label}");
    }

    fn render(editor: &mut UserEditor, terminal: &mut Terminal<TestBackend>) {
        terminal
            .draw(|frame| editor.render(frame, frame.area()))
            .unwrap();
    }

    #[test]
    fn another_users_grant_disables_wayland_access() {
        let editor = UserEditor::new(vec![socket()], None, Some("alice"), |_| {
            AppMessage::Wizard(WizardMessage::DialogCancel)
        });

        assert!(!editor.wayland_available);
        assert!(!editor.wayland_selected);
    }

    #[test]
    fn escape_closes_only_the_nested_wayland_dialog() {
        let mut editor = UserEditor::new(vec![socket()], None, None, |_| {
            AppMessage::Wizard(WizardMessage::DialogCancel)
        });
        for _ in 0..4 {
            editor.handle_key(key(KeyCode::Tab));
        }

        assert_eq!(
            editor.handle_key(key(KeyCode::Char(' '))),
            EventResult::Consumed
        );
        assert!(editor.wayland_dialog.is_some());
        assert_eq!(editor.handle_key(key(KeyCode::Esc)), EventResult::Consumed);
        assert!(editor.wayland_dialog.is_none());
        assert!(!editor.wayland_selected);
    }

    #[test]
    fn pointer_submission_keeps_validation_and_nested_dialog_input_ownership() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let mut editor = UserEditor::new(vec![socket()], None, None, |user| {
            AppMessage::Wizard(WizardMessage::UserAdded(user))
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
        render(&mut editor, &mut terminal);
        let parent_cancel = click_control(&terminal, "Cancel");
        // The empty username must fail validation rather than emitting submit.
        assert_eq!(
            editor.handle_mouse(click_control(&terminal, "OK")),
            EventResult::Consumed
        );
        assert!(editor.wayland_dialog.is_none());

        assert_eq!(
            editor.handle_mouse(click_control(&terminal, "Wayland access")),
            EventResult::Consumed
        );
        assert!(editor.wayland_dialog.is_some());
        terminal
            .draw(|frame| editor.render(frame, frame.area()))
            .unwrap();
        assert_eq!(editor.handle_mouse(parent_cancel), EventResult::Consumed);
        assert!(editor.wayland_dialog.is_some());
        assert!(editor.wayland_selected);
    }

    #[test]
    fn nested_dialog_preserves_editors_and_restores_parent_input() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let mut editor = UserEditor::new(vec![socket()], None, None, |user| {
            AppMessage::Wizard(WizardMessage::UserAdded(user))
        });
        for character in "alice".chars() {
            editor.handle_key(key(KeyCode::Char(character)));
        }
        editor.handle_key(key(KeyCode::Tab));
        for character in "密🙂pass".chars() {
            editor.handle_key(key(KeyCode::Char(character)));
        }
        editor.handle_key(key(KeyCode::Left));
        let cursor = editor.password.cursor();
        let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
        render(&mut editor, &mut terminal);
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(!rendered.contains("密🙂pass"));
        assert!(rendered.contains("******"));
        for _ in 0..3 {
            editor.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(
            editor.handle_key(key(KeyCode::Enter)),
            EventResult::Consumed
        );
        assert!(editor.wayland_dialog.is_some());
        editor.handle_key(key(KeyCode::Char('z')));
        assert_eq!(editor.password.value(), "密🙂pass");
        assert_eq!(editor.password.cursor(), cursor);
        assert_eq!(editor.handle_key(key(KeyCode::Esc)), EventResult::Consumed);
        assert!(editor.form.is_focused());
        assert!(!editor.wayland_selected);
        assert_eq!(
            editor.handle_key(key(KeyCode::Char(' '))),
            EventResult::Consumed
        );
        assert!(editor.wayland_dialog.is_some());
        editor.handle_key(key(KeyCode::Esc));
        editor.handle_key(key(KeyCode::BackTab));
        editor.handle_key(key(KeyCode::BackTab));
        editor.handle_key(key(KeyCode::BackTab));
        editor.handle_key(key(KeyCode::Char('!')));
        assert_eq!(editor.password.value(), "密🙂pas!s");
        assert_eq!(editor.username.value(), "alice");
    }

    #[test]
    fn disabled_wayland_is_skipped_and_mouse_submission_matches_keyboard() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let mut editor = UserEditor::new(Vec::new(), None, None, |user| {
            AppMessage::Wizard(WizardMessage::UserAdded(user))
        });
        editor.handle_key(key(KeyCode::Char('a')));
        // Password, shell, sudoer, then submit; the unavailable Wayland row is skipped.
        for _ in 0..4 {
            editor.handle_key(key(KeyCode::Tab));
        }
        editor.handle_key(key(KeyCode::Char(' ')));
        assert!(!editor.sudoer);
        assert!(!editor.wayland_selected);
        assert!(editor.wayland_dialog.is_none());
        let expected = editor.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            expected,
            EventResult::Message(AppMessage::Wizard(WizardMessage::UserAdded(_)))
        ));
        let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
        render(&mut editor, &mut terminal);
        assert_eq!(
            editor.handle_mouse(click_control(&terminal, "OK")),
            expected
        );
    }

    #[test]
    fn reediting_wayland_preserves_existing_grant_on_cancel_and_submit() {
        let user = UserDraft {
            username: "alice".into(),
            password: String::new(),
            shell: "/bin/bash".into(),
            sudoer: true,
            wayland: Some(WaylandAccessDraft::new(vec![socket()]).unwrap()),
        };
        let mut editor = UserEditor::new(vec![socket()], None, None, |user| {
            AppMessage::Wizard(WizardMessage::UserAdded(user))
        })
        .with_user(&user);
        for _ in 0..4 {
            editor.handle_key(key(KeyCode::Tab));
        }
        editor.handle_key(key(KeyCode::Enter));
        editor.handle_key(key(KeyCode::Esc));
        assert!(editor.wayland_selected);
        assert_eq!(editor.wayland, user.wayland);
        editor.handle_key(key(KeyCode::Enter));
        assert!(editor.wayland_dialog.is_some());
        editor.handle_key(key(KeyCode::Enter));
        assert!(editor.wayland_dialog.is_none());
        editor.handle_key(key(KeyCode::Tab));
        assert_eq!(
            editor.handle_key(key(KeyCode::Enter)),
            EventResult::Message(AppMessage::Wizard(WizardMessage::UserAdded(user)))
        );
    }

    #[test]
    fn compact_terminal_keeps_wayland_control_and_buttons_visible() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let mut editor = UserEditor::new(vec![socket()], None, None, |_| {
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
