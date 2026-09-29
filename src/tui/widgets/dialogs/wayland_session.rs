use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
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
use crate::tui::core::{AppMessage, Component, ConfigurationMessage, EventResult, FocusTracker};
use crate::tui::theme;
use crate::tui::widgets::inputs::button::Button;
use crate::tui::widgets::inputs::text_box::TextBox;
use crate::tui::widgets::lists::selectable_list::SelectableList;

macro_rules! active_components {
    ($self:ident) => {{
        let components: Vec<&mut dyn Component> = vec![
            &mut $self.sockets,
            &mut $self.guest_user,
            &mut $self.check,
            &mut $self.enter_shell,
            &mut $self.close,
        ];
        components
    }};
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
    sockets: SelectableList<HostWaylandSocket>,
    guest_user: TextBox,
    check: Button,
    enter_shell: Button,
    close: Button,
    focus: FocusTracker,
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
        let mut sockets = SelectableList::new(" Wayland displays ", sockets, move |socket| {
            socket_label(socket, preferred_display.as_ref())
        });
        if let Some(selected) = initially_selected {
            if let Some(index) = sockets.items().iter().position(|socket| socket == selected) {
                sockets.select(index);
            }
        }
        let available = !sockets.items().is_empty();
        let mut dialog = Self {
            machine,
            sockets,
            guest_user: TextBox::new(" Guest user ", String::new())
                .with_validator(validate_guest_user),
            check: Button::new("Check access", || {
                AppMessage::Configuration(ConfigurationMessage::CheckWayland)
            })
            .with_enabled(available),
            enter_shell: Button::new("Enter shell", || {
                AppMessage::Configuration(ConfigurationMessage::EnterWaylandShell)
            })
            .with_enabled(available),
            close: Button::new("Close", || {
                AppMessage::Configuration(ConfigurationMessage::CloseWaylandSession)
            }),
            focus: FocusTracker::new(),
            generation: 0,
            state: CheckState::Untested,
            root_confirmation: None,
            pending_check: None,
        };
        dialog.update_focus();
        dialog
    }

    pub(crate) fn render(&mut self, frame: &mut Frame, area: Rect) {
        let width = area.width.min(68);
        let height = area.height.min(18);
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
        let rows = Layout::vertical([
            Constraint::Length(5),
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .split(inner);
        self.sockets.render(frame, rows[0]);
        self.guest_user.render(frame, rows[1]);
        self.render_status(frame, rows[2]);
        frame.render_widget(
            Paragraph::new(
                " Tab/Shift+Tab focus, j/k select display, c check, e enter shell, Esc close ",
            )
            .style(Style::default().fg(theme::theme().hint_fg)),
            rows[3],
        );
        let buttons = Layout::horizontal([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(rows[4]);
        self.check.render(frame, buttons[0]);
        self.enter_shell.render(frame, buttons[1]);
        self.close.render(frame, buttons[2]);

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
            KeyCode::Tab => {
                self.next_focus();
                return WaylandSessionDialogAction::None;
            }
            KeyCode::BackTab => {
                self.previous_focus();
                return WaylandSessionDialogAction::None;
            }
            KeyCode::Char('c') if !self.guest_user.is_focused() => return self.request_check(),
            KeyCode::Char('e') if !self.guest_user.is_focused() => {
                return self.request_enter_shell()
            }
            _ => {}
        }

        let selected_before = self.sockets.selected_idx();
        let user_before = self.guest_user.value().to_owned();
        let result = {
            let mut components = active_components!(self);
            components[self.focus.active_idx].handle_key(key)
        };
        if self.sockets.selected_idx() != selected_before || self.guest_user.value() != user_before
        {
            self.invalidate_check();
        }
        match result {
            EventResult::FocusNext => self.next_focus(),
            EventResult::FocusPrev => self.previous_focus(),
            EventResult::Message(AppMessage::Configuration(message)) => {
                return self.handle_message(message)
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

    fn update_focus(&mut self) {
        let mut components = active_components!(self);
        self.focus.update_focus(&mut components, true);
    }

    fn next_focus(&mut self) {
        let components = active_components!(self);
        self.focus.next(&components);
        drop(components);
        self.update_focus();
    }

    fn previous_focus(&mut self) {
        let components = active_components!(self);
        self.focus.prev(&components);
        drop(components);
        self.update_focus();
    }

    fn selected_socket(&self) -> Option<HostWaylandSocket> {
        self.sockets.selected_item().cloned()
    }

    fn target(&mut self) -> Result<ShellTarget, String> {
        self.guest_user.validate()?;
        let user = ValidatedGuestUserName::new(self.guest_user.value())
            .map_err(|error| error.to_string())?;
        Ok(ShellTarget::new(self.machine.clone(), user))
    }

    fn handle_message(&mut self, message: ConfigurationMessage) -> WaylandSessionDialogAction {
        match message {
            ConfigurationMessage::CheckWayland => self.request_check(),
            ConfigurationMessage::EnterWaylandShell => self.request_enter_shell(),
            ConfigurationMessage::CloseWaylandSession => WaylandSessionDialogAction::Close,
            _ => WaylandSessionDialogAction::None,
        }
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

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let t = theme::theme();
        let (state, detail, style) = match &self.state {
            CheckState::Untested if self.sockets.items().is_empty() => (
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
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!(" {state}  "), style.add_modifier(Modifier::BOLD)),
                Span::styled(detail, style),
            ]))
            .wrap(Wrap { trim: true }),
            area,
        );
    }

    fn render_root_confirmation(&self, frame: &mut Frame, area: Rect) {
        let width = 58.min(area.width);
        let height = 8.min(area.height);
        let dialog = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, dialog);
        frame.render_widget(
            Paragraph::new("Open a root shell with Wayland access?\n\n[y] Open    [n/Esc] Cancel")
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .title(" Root shell ")
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(theme::theme().dialog_border_warn)),
                ),
            dialog,
        );
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
