use crate::application::sessions::{
    ValidatedGuestUserName, WaylandSessionContext, WaylandShellRequest,
};
use crate::domain::machine::MachineName;
use crate::domain::wayland::HostWaylandSocket;
use crate::tui::core::{AppMessage, Component, EventResult, FocusTracker, SessionMessage};
use crate::tui::widgets::inputs::button::Button;
use crate::tui::widgets::inputs::text_box::TextBox;
use crate::tui::widgets::lists::selectable_list::SelectableList;
use crate::tui::widgets::selectors::checkbox::Checkbox;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};

pub struct ShellDialog {
    machine: MachineName,
    username: TextBox,
    wayland_sockets: SelectableList<HostWaylandSocket>,
    wayland_probe: WaylandProbeStatus,
    wayland: Checkbox,
    test_wayland: Button,
    open: Button,
    cancel: Button,
    focus: FocusTracker,
    root_confirmation_pending: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WaylandProbeStatus {
    Untested,
    Verified(WaylandSessionContext),
    Failed(String),
}

impl ShellDialog {
    pub fn new(
        machine: MachineName,
        initial_user: String,
        wayland_sockets: Vec<HostWaylandSocket>,
    ) -> Self {
        let wayland_available = !wayland_sockets.is_empty();
        let mut dialog = Self {
            machine,
            username: TextBox::new(" Guest user ", initial_user)
                .with_validator(validate_guest_user),
            wayland_sockets: SelectableList::new(
                " Host display ",
                wayland_sockets,
                wayland_socket_label,
            )
            .with_enabled(wayland_available),
            wayland_probe: WaylandProbeStatus::Untested,
            wayland: Checkbox::new("Enable Wayland for this shell", false)
                .with_enabled(wayland_available),
            test_wayland: Button::new("Test Wayland", || {
                AppMessage::Session(SessionMessage::DialogTestWayland)
            })
            .with_enabled(wayland_available),
            open: Button::new("Open shell", || {
                AppMessage::Session(SessionMessage::DialogSubmit)
            }),
            cancel: Button::new("Cancel", || {
                AppMessage::Session(SessionMessage::DialogCancel)
            }),
            focus: FocusTracker::new(),
            root_confirmation_pending: false,
        };
        dialog.update_focus();
        dialog
    }

    pub fn with_selected_wayland_socket(mut self, selected: &HostWaylandSocket) -> Self {
        if let Some(index) = self
            .wayland_sockets
            .items()
            .iter()
            .position(|socket| socket == selected)
        {
            self.wayland_sockets.select(index);
        }
        self
    }

    pub fn with_probe_result(mut self, result: Result<WaylandSessionContext, String>) -> Self {
        self.wayland_probe = match result {
            Ok(identity) => WaylandProbeStatus::Verified(identity),
            Err(error) => WaylandProbeStatus::Failed(safe_single_line(&error)),
        };
        self.wayland.set_checked(true);
        self
    }

    fn update_focus(&mut self) {
        let mut components: Vec<&mut dyn Component> = vec![
            &mut self.username,
            &mut self.wayland,
            &mut self.wayland_sockets,
            &mut self.test_wayland,
            &mut self.open,
            &mut self.cancel,
        ];
        self.focus.update_focus(&mut components, true);
    }

    fn move_focus(&mut self, forward: bool) {
        let components: Vec<&mut dyn Component> = vec![
            &mut self.username,
            &mut self.wayland,
            &mut self.wayland_sockets,
            &mut self.test_wayland,
            &mut self.open,
            &mut self.cancel,
        ];
        if forward {
            self.focus.next(&components);
        } else {
            self.focus.prev(&components);
        }
        self.update_focus();
    }

    fn submit(&mut self) -> EventResult {
        if self.username.validate().is_err() {
            return EventResult::Consumed;
        }
        let user = ValidatedGuestUserName::new(self.username.value())
            .expect("the username validator and value object share a contract");
        if matches!(user.as_str(), "root" | "0") && !self.root_confirmation_pending {
            self.root_confirmation_pending = true;
            return EventResult::Consumed;
        }
        EventResult::Message(AppMessage::Session(SessionMessage::OpenShell {
            machine: self.machine.clone(),
            user,
            wayland: if self.wayland.checked() {
                WaylandShellRequest::SelectedHostDisplay(
                    self.wayland_sockets
                        .selected_item()
                        .cloned()
                        .expect("an enabled Wayland checkbox has a selected display"),
                )
            } else {
                WaylandShellRequest::Disabled
            },
        }))
    }

    fn test_wayland(&mut self) -> EventResult {
        if self.username.validate().is_err() {
            return EventResult::Consumed;
        }
        let Some(host_socket) = self.wayland_sockets.selected_item().cloned() else {
            return EventResult::Consumed;
        };
        let user = ValidatedGuestUserName::new(self.username.value())
            .expect("the username validator and value object share a contract");
        EventResult::Message(AppMessage::Session(SessionMessage::TestWayland {
            machine: self.machine.clone(),
            user,
            host_socket,
            available_sockets: self.wayland_sockets.items().to_vec(),
        }))
    }

    fn render_wayland_status(&self, frame: &mut Frame, area: Rect) {
        let theme = crate::tui::theme::theme();
        let (state, detail, style) = match (
            self.wayland_sockets.items().is_empty(),
            &self.wayland_probe,
            self.wayland.checked(),
        ) {
            (true, _, _) => (
                "UNAVAILABLE",
                "No host Wayland socket was found.".to_string(),
                Style::default()
                    .fg(theme.border_disabled)
                    .add_modifier(Modifier::DIM),
            ),
            (false, WaylandProbeStatus::Verified(context), _) => (
                "READY",
                format!(
                    "Guest uid {} gid {} can access {}.",
                    context.identity().uid(),
                    context.identity().gid(),
                    context.guest_socket().display()
                ),
                Style::default().fg(theme.success),
            ),
            (false, WaylandProbeStatus::Failed(error), _) => (
                "TEST FAILED",
                error.clone(),
                Style::default().fg(theme.error),
            ),
            (false, WaylandProbeStatus::Untested, true) => (
                "NEEDS TEST",
                "Validate the startup projection for this guest account.".to_string(),
                Style::default().fg(theme.warning),
            ),
            (false, WaylandProbeStatus::Untested, false) => (
                "OFF",
                "This shell will not receive Wayland context.".to_string(),
                Style::default().fg(theme.text_secondary),
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
}

fn wayland_socket_label(socket: &HostWaylandSocket) -> String {
    format!(
        "{}  {}",
        socket.display(),
        socket.canonical_path().display()
    )
}

fn safe_single_line(value: &str) -> String {
    value
        .chars()
        .take(160)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn validate_guest_user(value: &str) -> Result<(), String> {
    ValidatedGuestUserName::new(value)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

impl Component for ShellDialog {
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let width = area.width.min(68);
        let height = area.height.min(18);
        let dialog_area = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, dialog_area);
        let block = Block::default()
            .title(format!(" Shell: {} ", self.machine))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(crate::tui::theme::theme().dialog_border));
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(2),
                Constraint::Length(3),
            ])
            .split(inner);
        self.username.render(frame, rows[0]);
        self.wayland.render(frame, rows[1]);
        self.wayland_sockets.render(frame, rows[2]);
        if self.root_confirmation_pending {
            frame.render_widget(
                Paragraph::new(
                    "Root grants full control inside this guest. Submit again to confirm.",
                )
                .style(Style::default().fg(crate::tui::theme::theme().dialog_border_warn)),
                rows[3],
            );
        } else {
            self.render_wayland_status(frame, rows[3]);
        }
        let buttons = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(34),
                Constraint::Percentage(33),
                Constraint::Percentage(33),
            ])
            .split(rows[4]);
        self.test_wayland.render(frame, buttons[0]);
        self.open.render(frame, buttons[1]);
        self.cancel.render(frame, buttons[2]);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Esc => {
                return EventResult::Message(AppMessage::Session(SessionMessage::DialogCancel));
            }
            KeyCode::Tab => {
                self.move_focus(true);
                return EventResult::Consumed;
            }
            KeyCode::BackTab => {
                self.move_focus(false);
                return EventResult::Consumed;
            }
            KeyCode::Enter if self.username.is_focused() => return self.submit(),
            KeyCode::Enter if self.wayland.is_focused() && self.wayland.is_enabled() => {
                self.wayland.set_checked(!self.wayland.checked());
                return EventResult::Consumed;
            }
            _ => {}
        }

        let username_focused = self.username.is_focused();
        let selected_wayland_socket = self.wayland_sockets.selected_item().cloned();
        let mut components: Vec<&mut dyn Component> = vec![
            &mut self.username,
            &mut self.wayland,
            &mut self.wayland_sockets,
            &mut self.test_wayland,
            &mut self.open,
            &mut self.cancel,
        ];
        let result = components[self.focus.active_idx].handle_key(key);
        drop(components);
        if self.wayland_sockets.selected_item() != selected_wayland_socket.as_ref() {
            self.wayland_probe = WaylandProbeStatus::Untested;
        }
        if username_focused
            && matches!(
                key.code,
                KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
            )
        {
            self.root_confirmation_pending = false;
            self.wayland_probe = WaylandProbeStatus::Untested;
        }
        match result {
            EventResult::Message(AppMessage::Session(SessionMessage::DialogSubmit)) => {
                self.submit()
            }
            EventResult::Message(AppMessage::Session(SessionMessage::DialogTestWayland)) => {
                self.test_wayland()
            }
            other => other,
        }
    }

    fn set_focus(&mut self, focused: bool) {
        if focused {
            self.update_focus();
        } else {
            self.username.set_focus(false);
            self.wayland.set_focus(false);
            self.wayland_sockets.set_focus(false);
            self.test_wayland.set_focus(false);
            self.open.set_focus(false);
            self.cancel.set_focus(false);
        }
    }

    fn is_focused(&self) -> bool {
        self.username.is_focused()
            || self.wayland.is_focused()
            || self.wayland_sockets.is_focused()
            || self.test_wayland.is_focused()
            || self.open.is_focused()
            || self.cancel.is_focused()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::sessions::ObservedGuestIdentity;
    use crate::domain::wayland::{SocketRevision, WaylandDisplay};
    use ratatui::{backend::TestBackend, Terminal};
    use std::path::PathBuf;

    fn host_socket(display: &str) -> HostWaylandSocket {
        HostWaylandSocket::from_verified_parts(
            WaylandDisplay::new(display).unwrap(),
            PathBuf::from("/run/user/1000"),
            PathBuf::from(format!("/run/user/1000/{display}")),
            1000,
            1000,
            1000,
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

    fn verified_context(socket: HostWaylandSocket) -> WaylandSessionContext {
        WaylandSessionContext::verified(
            socket,
            PathBuf::from("/run/lasper/wayland/1000/wayland-2"),
            ObservedGuestIdentity::new(1000, 1001),
        )
    }

    fn render_text(dialog: &mut ShellDialog, width: u16, height: u16) -> String {
        crate::tui::theme::init_theme(crate::tui::theme::Theme::dark());
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| dialog.render(frame, frame.area()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn root_requires_a_second_submit() {
        let mut dialog =
            ShellDialog::new(MachineName::new("demo").unwrap(), "root".into(), Vec::new());
        assert_eq!(dialog.submit(), EventResult::Consumed);
        assert!(matches!(
            dialog.submit(),
            EventResult::Message(AppMessage::Session(SessionMessage::OpenShell {
                wayland: WaylandShellRequest::Disabled,
                ..
            }))
        ));
    }

    #[test]
    fn selected_display_is_preserved_as_typed_intent() {
        let socket = host_socket("wayland-1");
        let mut dialog = ShellDialog::new(
            MachineName::new("demo").unwrap(),
            "alice".into(),
            vec![socket.clone()],
        );
        dialog.wayland.set_checked(true);

        let rendered = render_text(&mut dialog, 72, 18);
        assert!(rendered.contains("wayland-1"));
        assert!(rendered.contains("NEEDS TEST"));

        assert!(matches!(
            dialog.submit(),
            EventResult::Message(AppMessage::Session(SessionMessage::OpenShell {
                wayland: WaylandShellRequest::SelectedHostDisplay(found),
                ..
            })) if found == socket
        ));
    }

    #[test]
    fn test_action_preserves_the_selected_target_and_host_socket_evidence() {
        let first = host_socket("wayland-0");
        let socket = host_socket("wayland-1");
        let mut dialog = ShellDialog::new(
            MachineName::new("demo").unwrap(),
            "alice".into(),
            vec![first.clone(), socket.clone()],
        );
        dialog.wayland_sockets.select(1);

        assert!(matches!(
            dialog.test_wayland(),
            EventResult::Message(AppMessage::Session(SessionMessage::TestWayland {
                machine,
                user,
                host_socket: found,
                available_sockets,
            })) if machine.as_str() == "demo"
                && user.as_str() == "alice"
                && found == socket
                && available_sockets == vec![first, socket]
        ));
    }

    #[test]
    fn unavailable_display_cannot_be_selected() {
        let mut dialog = ShellDialog::new(
            MachineName::new("demo").unwrap(),
            "alice".into(),
            Vec::new(),
        );

        assert!(!dialog.wayland.is_enabled());
        assert!(!dialog.test_wayland.is_enabled());
        assert!(!dialog.wayland.checked());
        assert!(render_text(&mut dialog, 52, 15).contains("UNAVAILABLE"));
    }

    #[test]
    fn complete_probe_result_is_rendered_as_ready() {
        let socket = host_socket("wayland-2");
        let mut dialog = ShellDialog::new(
            MachineName::new("demo").unwrap(),
            "alice".into(),
            vec![socket.clone()],
        )
        .with_probe_result(Ok(verified_context(socket)));

        let rendered = render_text(&mut dialog, 72, 18);
        assert!(rendered.contains("READY"));
        assert!(rendered.contains("uid 1000"));
        assert!(rendered.contains("/run/lasper/wayland/1000/wayland-2"));
    }

    #[test]
    fn editing_username_invalidates_the_previous_probe_result() {
        let socket = host_socket("wayland-2");
        let mut dialog = ShellDialog::new(
            MachineName::new("demo").unwrap(),
            "alice".into(),
            vec![socket.clone()],
        )
        .with_probe_result(Ok(verified_context(socket)));

        assert_eq!(
            dialog.handle_key(KeyEvent::new(
                KeyCode::Char('2'),
                crossterm::event::KeyModifiers::NONE,
            )),
            EventResult::Consumed
        );

        let rendered = render_text(&mut dialog, 72, 18);
        assert!(rendered.contains("NEEDS TEST"));
        assert!(!rendered.contains("Guest uid 1000"));
    }

    #[test]
    fn changing_display_invalidates_the_previous_probe_result() {
        let first = host_socket("wayland-1");
        let second = host_socket("wayland-2");
        let mut dialog = ShellDialog::new(
            MachineName::new("demo").unwrap(),
            "alice".into(),
            vec![first.clone(), second],
        )
        .with_probe_result(Ok(verified_context(first)));
        dialog.move_focus(true);
        dialog.move_focus(true);

        assert_eq!(
            dialog.handle_key(KeyEvent::new(
                KeyCode::Down,
                crossterm::event::KeyModifiers::NONE,
            )),
            EventResult::Consumed
        );

        let rendered = render_text(&mut dialog, 72, 18);
        assert!(rendered.contains("NEEDS TEST"));
        assert!(!rendered.contains("Guest uid 1000"));
    }
}
