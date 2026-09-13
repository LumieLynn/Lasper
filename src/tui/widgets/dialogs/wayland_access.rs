use crate::domain::wayland::HostWaylandSocket;
use crate::tui::core::{AppMessage, Component, EventResult, FocusTracker, WizardMessage};
use crate::tui::widgets::inputs::button::Button;
use crate::tui::widgets::lists::checklist::Checklist;
use crate::tui::wizard::draft::WaylandAccessDraft;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

macro_rules! active_comps {
    ($self:ident) => {{
        let comps: Vec<&mut dyn Component> =
            vec![&mut $self.sockets, &mut $self.btn_ok, &mut $self.btn_cancel];
        comps
    }};
}

pub struct WaylandAccessDialog {
    sockets: Checklist<HostWaylandSocket>,
    btn_ok: Button,
    btn_cancel: Button,
    focus: FocusTracker,
}

impl WaylandAccessDialog {
    pub fn new(available: Vec<HostWaylandSocket>, initial: Option<&WaylandAccessDraft>) -> Self {
        let mut sockets = Checklist::new("Wayland Displays", available.clone(), socket_label);
        if let Some(initial) = initial {
            sockets.set_checked(
                available
                    .iter()
                    .enumerate()
                    .filter_map(|(index, socket)| initial.sockets.contains(socket).then_some(index))
                    .collect(),
            );
        }

        let mut dialog = Self {
            sockets,
            btn_ok: Button::new("OK", || AppMessage::Wizard(WizardMessage::DialogSubmit)),
            btn_cancel: Button::new("Cancel", || AppMessage::Wizard(WizardMessage::DialogCancel)),
            focus: FocusTracker::new(),
        };
        dialog.update_focus();
        dialog
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

    fn try_submit(&self) -> Option<WaylandAccessDraft> {
        let sockets = checked_sockets(&self.sockets);
        WaylandAccessDraft::new(sockets).ok()
    }
}

fn checked_sockets(checklist: &Checklist<HostWaylandSocket>) -> Vec<HostWaylandSocket> {
    let mut indices: Vec<_> = checklist.checked_indices().iter().copied().collect();
    indices.sort_unstable();
    indices
        .into_iter()
        .filter_map(|index| checklist.items().get(index).cloned())
        .collect()
}

fn socket_label(socket: &HostWaylandSocket) -> String {
    format!(
        "{}  uid {}  mode {:04o}",
        socket.display().as_str(),
        socket.owner_uid(),
        socket.mode(),
    )
}

impl Component for WaylandAccessDialog {
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let dialog_area = crate::tui::centered_rect(65, 70, area);
        frame.render_widget(Clear, dialog_area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Wayland Access ")
            .border_style(Style::default().fg(crate::tui::theme::theme().dialog_border));
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(4),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(inner);
        self.sockets.render(frame, chunks[0]);
        frame.render_widget(
            Paragraph::new(" [Space] select displays  [Tab] switch ")
                .style(Style::default().fg(crate::tui::theme::theme().wizard_footer)),
            chunks[1],
        );

        let buttons = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[2]);
        self.btn_ok.render(frame, buttons[0]);
        self.btn_cancel.render(frame, buttons[1]);
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
            KeyCode::Esc => {
                return EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel));
            }
            KeyCode::Enter if !self.btn_ok.is_focused() && !self.btn_cancel.is_focused() => {
                return self.try_submit().map_or(EventResult::Consumed, |access| {
                    EventResult::Message(AppMessage::Wizard(
                        WizardMessage::WaylandAccessConfigured(access),
                    ))
                });
            }
            _ => {}
        }

        let mut comps = active_comps!(self);
        let result = comps[self.focus.active_idx].handle_key(key);
        drop(comps);
        match result {
            EventResult::Message(AppMessage::Wizard(WizardMessage::DialogSubmit)) => {
                self.try_submit().map_or(EventResult::Consumed, |access| {
                    EventResult::Message(AppMessage::Wizard(
                        WizardMessage::WaylandAccessConfigured(access),
                    ))
                })
            }
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
            self.update_focus();
        } else {
            for component in active_comps!(self) {
                component.set_focus(false);
            }
        }
    }

    fn is_focused(&self) -> bool {
        self.sockets.is_focused() || self.btn_ok.is_focused() || self.btn_cancel.is_focused()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::wayland::{SocketRevision, WaylandDisplay};

    fn socket(display: &str, inode: u64) -> HostWaylandSocket {
        HostWaylandSocket::from_verified_parts(
            WaylandDisplay::new(display).unwrap(),
            "/run/user/1001".into(),
            format!("/run/user/1001/{display}").into(),
            1001,
            1001,
            1001,
            0o700,
            SocketRevision {
                device: 1,
                inode,
                ctime_seconds: 3,
                ctime_nanoseconds: 4,
            },
        )
        .unwrap()
    }

    #[test]
    fn multiple_selected_sockets_are_preserved_without_a_guest_default() {
        let first = socket("wayland-0", 1);
        let second = socket("wayland-1", 2);
        let initial = WaylandAccessDraft::new(vec![first.clone(), second.clone()]).unwrap();
        let dialog = WaylandAccessDialog::new(vec![first, second], Some(&initial));

        let result = dialog.try_submit().unwrap();
        assert_eq!(result.sockets.len(), 2);
        assert_eq!(result.sockets[1].display().as_str(), "wayland-1");
    }
}
