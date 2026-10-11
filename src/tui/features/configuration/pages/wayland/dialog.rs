use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::application::sessions::{
    SessionError, ShellTarget, ValidatedGuestUserName, WaylandSessionContext,
};
use crate::domain::machine::MachineName;
use crate::domain::wayland::HostWaylandSocket;
use crate::domain::wayland::WaylandDisplay;
use crate::tui::foundation::components::confirmation::ConfirmationDialog;
use crate::tui::foundation::components::form::{Form, FormItem, FormOutcome, FormState};
use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::foundation::theme;
use crate::tui::foundation::widgets::display::content::ContentControl;
use crate::tui::foundation::widgets::inputs::{
    button::ButtonControl,
    text_input::{Input, TextInput},
};
use crate::tui::foundation::widgets::lists::list::{ListControl, ListControlState, ListEntry};
use crate::tui::protocol::Component;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionControl {
    Sockets,
    Guest,
    Status,
    Check,
    EnterShell,
    Close,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WaylandSessionDialogAction {
    None,
    Close,
    Check {
        generation: u64,
        target: ShellTarget,
        host_socket: HostWaylandSocket,
    },
    EnterShell {
        target: ShellTarget,
        host_socket: HostWaylandSocket,
    },
}

enum CheckState {
    Untested,
    Loading {
        generation: u64,
        user: ValidatedGuestUserName,
    },
    Ready {
        generation: u64,
        context: WaylandSessionContext,
    },
    Failed {
        generation: u64,
        message: String,
    },
}

pub(crate) struct WaylandSessionDialog {
    machine: MachineName,
    sockets: Vec<HostWaylandSocket>,
    selected: ListControlState,
    preferred_display: Option<WaylandDisplay>,
    guest_user: Input,
    user_error: Option<String>,
    form: FormState<SessionControl>,
    generation: u64,
    state: CheckState,
    root_confirmation: Option<(ShellTarget, HostWaylandSocket)>,
    pending_check: Option<tokio::task::JoinHandle<()>>,
}

impl WaylandSessionDialog {
    pub(crate) fn new(
        machine: MachineName,
        sockets: Vec<HostWaylandSocket>,
        preferred_display: Option<WaylandDisplay>,
        initially_selected: Option<&HostWaylandSocket>,
    ) -> Self {
        let selected = initially_selected
            .and_then(|selected| sockets.iter().position(|socket| socket == selected))
            .unwrap_or(0);
        let mut dialog = Self {
            machine,
            sockets,
            selected: ListControlState::new(selected),
            preferred_display,
            guest_user: Input::default(),
            user_error: None,
            form: FormState::new(SessionControl::Sockets),
            generation: 0,
            state: CheckState::Untested,
            root_confirmation: None,
            pending_check: None,
        };
        dialog.form.set_focus(true);
        dialog.with_form(|_| ());
        dialog
    }

    fn with_form<R>(&mut self, visit: impl FnOnce(&mut Form<'_, SessionControl>) -> R) -> R {
        let available = !self.sockets.is_empty();
        let entries: Vec<_> = self
            .sockets
            .iter()
            .map(|socket| ListEntry::new(socket_label(socket, self.preferred_display.as_ref())))
            .collect();
        let mut status = ContentControl::new(self.status_widget());
        let mut sockets = ListControl::new(&mut self.selected, " Wayland displays ", &entries);
        let mut guest = TextInput::new(
            &mut self.guest_user,
            " Guest user ",
            self.user_error.as_deref(),
        );
        let mut check = ButtonControl::new("Check access");
        let mut enter = ButtonControl::new("Enter shell");
        let mut close = ButtonControl::new("Close");
        visit(&mut Form::new(
            &mut self.form,
            [
                FormItem::field(SessionControl::Sockets, &mut sockets).height(5),
                FormItem::field(SessionControl::Guest, &mut guest),
                FormItem::field(SessionControl::Status, &mut status).fill(3),
                FormItem::row([
                    FormItem::field(SessionControl::Check, &mut check).with_enabled(available),
                    FormItem::field(SessionControl::EnterShell, &mut enter).with_enabled(available),
                    FormItem::field(SessionControl::Close, &mut close),
                ]),
            ],
        ))
    }

    pub(crate) fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        if self.root_confirmation.is_some() {
            return;
        }
        self.form.register_footer_hints(footer);
        footer.register(
            FooterHint::new("Esc", "close")
                .primary()
                .on_key(KeyCode::Esc),
        );
        if !self.form.captures_text_input() && !self.sockets.is_empty() {
            footer.register(FooterHint::new("c", "check").on_key(KeyCode::Char('c')));
            footer.register(FooterHint::new("e", "enter shell").on_key(KeyCode::Char('e')));
        }
    }

    pub(crate) fn render_layer(&mut self, frame: &mut Frame, area: Rect, foreground: bool) {
        self.form
            .set_focus(foreground && self.root_confirmation.is_none());
        let width = area.width.min(68);
        let height = area.height.min(17);
        let dialog = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, dialog);
        let block = Block::default()
            .title(format!(" Wayland session: {} ", self.machine))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme::theme().dialog_border));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        self.with_form(|form| form.render(frame, inner));

        if self.root_confirmation.is_some() {
            self.render_root_confirmation(frame, area);
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> WaylandSessionDialogAction {
        if self.root_confirmation.is_some() {
            return self.handle_root_confirmation(key);
        }
        match key.code {
            KeyCode::Esc => return WaylandSessionDialogAction::Close,
            KeyCode::Char('c') if !self.form.captures_text_input() => return self.request_check(),
            KeyCode::Char('e') if !self.form.captures_text_input() => {
                return self.request_enter_shell()
            }
            _ => {}
        }

        let outcome = self.with_form(|form| form.handle_key(key));
        self.finish_control_input(outcome)
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) -> WaylandSessionDialogAction {
        if self.root_confirmation.is_some() {
            return WaylandSessionDialogAction::None;
        }
        let outcome = self.with_form(|form| form.handle_mouse(mouse));
        self.finish_control_input(outcome)
    }

    fn finish_control_input(
        &mut self,
        outcome: FormOutcome<SessionControl>,
    ) -> WaylandSessionDialogAction {
        match outcome {
            FormOutcome::Changed(SessionControl::Sockets | SessionControl::Guest) => {
                self.invalidate_check()
            }
            FormOutcome::Activated(SessionControl::Check) => return self.request_check(),
            FormOutcome::Activated(SessionControl::EnterShell) => {
                return self.request_enter_shell()
            }
            FormOutcome::Activated(SessionControl::Close) => {
                return WaylandSessionDialogAction::Close
            }
            _ => {}
        }
        WaylandSessionDialogAction::None
    }

    pub(crate) fn track_check(&mut self, task: tokio::task::JoinHandle<()>) {
        if let Some(previous) = self.pending_check.replace(task) {
            previous.abort();
        }
    }

    pub(crate) fn finish_check(
        &mut self,
        generation: u64,
        result: Result<WaylandSessionContext, SessionError>,
    ) {
        if generation != self.generation {
            return;
        }
        self.pending_check.take();
        self.state = match result {
            Ok(context) => CheckState::Ready {
                generation,
                context,
            },
            Err(error) => CheckState::Failed {
                generation,
                message: error.to_string(),
            },
        };
    }

    pub(crate) fn block_shell(&mut self, message: impl Into<String>) {
        self.state = CheckState::Failed {
            generation: self.generation,
            message: message.into(),
        };
    }

    fn selected_socket(&self) -> Option<HostWaylandSocket> {
        self.selected
            .selected()
            .and_then(|index| self.sockets.get(index))
            .cloned()
    }

    fn target(&mut self) -> Result<ShellTarget, String> {
        let result = validate_guest_user(self.guest_user.value());
        self.user_error = result.as_ref().err().cloned();
        result?;
        let user = ValidatedGuestUserName::new(self.guest_user.value())
            .map_err(|error| error.to_string())?;
        Ok(ShellTarget::new(self.machine.clone(), user))
    }

    fn request_check(&mut self) -> WaylandSessionDialogAction {
        let Some(host_socket) = self.selected_socket() else {
            self.block_shell("No host Wayland display is available");
            return WaylandSessionDialogAction::None;
        };
        let target = match self.target() {
            Ok(target) => target,
            Err(error) => {
                self.block_shell(error);
                return WaylandSessionDialogAction::None;
            }
        };
        if let Some(previous) = self.pending_check.take() {
            previous.abort();
        }
        self.generation = self
            .generation
            .checked_add(1)
            .expect("Wayland check generation exhausted");
        let generation = self.generation;
        self.state = CheckState::Loading {
            generation,
            user: target.user().clone(),
        };
        WaylandSessionDialogAction::Check {
            generation,
            target,
            host_socket,
        }
    }

    fn request_enter_shell(&mut self) -> WaylandSessionDialogAction {
        let Some(host_socket) = self.selected_socket() else {
            self.block_shell("No host Wayland display is available");
            return WaylandSessionDialogAction::None;
        };
        let target = match self.target() {
            Ok(target) => target,
            Err(error) => {
                self.block_shell(error);
                return WaylandSessionDialogAction::None;
            }
        };
        if matches!(target.user().as_str(), "root" | "0") {
            self.root_confirmation = Some((target, host_socket));
            return WaylandSessionDialogAction::None;
        }
        WaylandSessionDialogAction::EnterShell {
            target,
            host_socket,
        }
    }

    fn invalidate_check(&mut self) {
        if let Some(previous) = self.pending_check.take() {
            previous.abort();
        }
        self.generation = self.generation.saturating_add(1);
        self.state = CheckState::Untested;
    }

    fn handle_root_confirmation(&mut self, key: KeyEvent) -> WaylandSessionDialogAction {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let (target, host_socket) = self
                    .root_confirmation
                    .take()
                    .expect("root confirmation is visible");
                WaylandSessionDialogAction::EnterShell {
                    target,
                    host_socket,
                }
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.root_confirmation = None;
                WaylandSessionDialogAction::None
            }
            _ => WaylandSessionDialogAction::None,
        }
    }

    fn status_widget(&self) -> Paragraph<'static> {
        let t = theme::theme();
        let (state, detail, style) = match &self.state {
            CheckState::Untested if self.sockets.is_empty() => (
                "UNAVAILABLE",
                "No host Wayland socket was discovered.".to_owned(),
                Style::default().fg(t.text_dim),
            ),
            CheckState::Untested => (
                "NOT CHECKED",
                "Check validates the running machine; Enter shell repeats that validation."
                    .to_owned(),
                Style::default().fg(t.text_secondary),
            ),
            CheckState::Loading { generation, user } => (
                "CHECKING",
                format!("Probe #{generation} is validating {user}."),
                Style::default().fg(t.warning),
            ),
            CheckState::Ready {
                generation,
                context,
            } => (
                "READY",
                format!(
                    "Check #{generation}: guest uid {} gid {} can use {}.",
                    context.identity().uid(),
                    context.identity().gid(),
                    context.guest_socket().display()
                ),
                Style::default().fg(t.success),
            ),
            CheckState::Failed {
                generation,
                message,
            } => (
                "FAILED",
                format!("Check #{generation}: {message}"),
                Style::default().fg(t.error),
            ),
        };
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {state}  "), style.add_modifier(Modifier::BOLD)),
            Span::styled(detail, style),
        ]))
        .wrap(Wrap { trim: true })
    }

    fn render_root_confirmation(&self, frame: &mut Frame, area: Rect) {
        ConfirmationDialog::new("Root shell", "Open a root shell with Wayland access?")
            .with_hints(&[("y", "open root shell"), ("n/Esc", "cancel")])
            .render(frame, area);
    }
}

impl Drop for WaylandSessionDialog {
    fn drop(&mut self) {
        if let Some(task) = self.pending_check.take() {
            task.abort();
        }
    }
}

fn socket_label(socket: &HostWaylandSocket, preferred: Option<&WaylandDisplay>) -> String {
    let current = if preferred.is_some_and(|display| display == socket.display()) {
        "  (current WAYLAND_DISPLAY)"
    } else {
        ""
    };
    format!(
        "{}{}  {}",
        socket.display(),
        current,
        socket.canonical_path().display()
    )
}

fn validate_guest_user(value: &str) -> Result<(), String> {
    ValidatedGuestUserName::new(value)
        .map(|_| ())
        .map_err(|error| error.to_string())
}
