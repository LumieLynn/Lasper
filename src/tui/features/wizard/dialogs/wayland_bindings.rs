use crate::domain::wayland::{HostWaylandSocket, WaylandDisplay};
use crate::tui::features::wizard::draft::WaylandAccessDraft;
use crate::tui::foundation::components::form::{Form, FormItem, FormOutcome, FormState};
use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::foundation::widgets::inputs::button::ButtonControl;
use crate::tui::foundation::widgets::lists::list::{ListControl, ListControlState, ListEntry};
use crate::tui::foundation::{geometry::centered_rect, theme};
use crate::tui::protocol::{AppMessage, Component, EventResult, WizardMessage};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    layout::Rect,
    style::Style,
    widgets::{Block, BorderType, Borders, Clear},
    Frame,
};
use std::collections::HashSet;

#[derive(Clone, Copy, PartialEq, Eq)]
enum BindingControl {
    Sockets,
    Submit,
    Cancel,
}

pub struct WaylandBindingsDialog {
    sockets: Vec<HostWaylandSocket>,
    selected: ListControlState,
    checked: HashSet<usize>,
    preferred_display: Option<WaylandDisplay>,
    form: FormState<BindingControl>,
}
impl WaylandBindingsDialog {
    pub fn new(
        available: Vec<HostWaylandSocket>,
        preferred_display: Option<WaylandDisplay>,
        initial: Option<&WaylandAccessDraft>,
    ) -> Self {
        let checked = available
            .iter()
            .enumerate()
            .filter_map(|(index, socket)| {
                initial
                    .is_some_and(|access| access.sockets.contains(socket))
                    .then_some(index)
            })
            .collect();
        let mut dialog = Self {
            sockets: available,
            selected: ListControlState::new(0),
            checked,
            preferred_display,
            form: FormState::new(BindingControl::Sockets),
        };
        dialog.form.set_focus(true);
        dialog.with_form(|_| ());
        dialog
    }
    fn with_form<R>(&mut self, visit: impl FnOnce(&mut Form<'_, BindingControl>) -> R) -> R {
        let entries: Vec<_> = self
            .sockets
            .iter()
            .enumerate()
            .map(|(index, socket)| {
                ListEntry::new(socket_label(socket, self.preferred_display.as_ref()))
                    .checked(self.checked.contains(&index))
            })
            .collect();
        let mut sockets = ListControl::new(&mut self.selected, "Wayland Displays", &entries);
        let mut submit = ButtonControl::new("OK");
        let mut cancel = ButtonControl::new("Cancel");
        visit(
            &mut Form::new(
                &mut self.form,
                [
                    FormItem::field(BindingControl::Sockets, &mut sockets).fill(4),
                    FormItem::row([
                        FormItem::field(BindingControl::Submit, &mut submit),
                        FormItem::field(BindingControl::Cancel, &mut cancel),
                    ]),
                ],
            )
            .with_actions(BindingControl::Submit, BindingControl::Cancel),
        )
    }
    fn try_submit(&self) -> Option<WaylandAccessDraft> {
        let sockets = self
            .sockets
            .iter()
            .enumerate()
            .filter(|(index, _)| self.checked.contains(index))
            .map(|(_, socket)| socket.clone())
            .collect();
        WaylandAccessDraft::new(sockets).ok()
    }
    fn finish_input(&mut self, outcome: FormOutcome<BindingControl>) -> EventResult {
        match outcome {
            FormOutcome::Activated(BindingControl::Sockets) => {
                if let Some(index) = self.selected.selected() {
                    if !self.checked.insert(index) {
                        self.checked.remove(&index);
                    }
                }
                EventResult::Consumed
            }
            FormOutcome::Activated(BindingControl::Submit) => {
                self.try_submit().map_or(EventResult::Consumed, |access| {
                    EventResult::Message(AppMessage::Wizard(
                        WizardMessage::WaylandAccessConfigured(access),
                    ))
                })
            }
            FormOutcome::Activated(BindingControl::Cancel) => {
                EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel))
            }
            FormOutcome::Ignored => EventResult::Ignored,
            _ => EventResult::Consumed,
        }
    }
}
fn socket_label(socket: &HostWaylandSocket, preferred_display: Option<&WaylandDisplay>) -> String {
    let current_marker = if preferred_display.is_some_and(|display| display == socket.display()) {
        "  (current WAYLAND_DISPLAY)"
    } else {
        ""
    };
    format!(
        "{}{}  uid {}  mode {:04o}",
        socket.display().as_str(),
        current_marker,
        socket.owner_uid(),
        socket.mode()
    )
}
impl Component for WaylandBindingsDialog {
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        if self.form.is_selected(BindingControl::Sockets) && !self.sockets.is_empty() {
            footer.register(FooterHint::new("Space", "select displays").on_key(KeyCode::Char(' ')));
        }
        self.form.register_footer_hints(footer);
    }
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let dialog = centered_rect(65, 70, area);
        frame.render_widget(Clear, dialog);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Wayland Bindings ")
            .border_style(Style::default().fg(theme::theme().dialog_border));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        self.with_form(|form| form.render(frame, inner));
    }
    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        let outcome = self.with_form(|form| form.handle_key(key));
        self.finish_input(outcome)
    }
    fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        let outcome = self.with_form(|form| form.handle_mouse(mouse));
        self.finish_input(outcome)
    }
    fn set_focus(&mut self, focused: bool) {
        self.form.set_focus(focused);
        self.with_form(|_| ());
    }
    fn is_focused(&self) -> bool {
        self.form.is_focused()
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
        let dialog = WaylandBindingsDialog::new(
            vec![first, second],
            Some(WaylandDisplay::new("wayland-0").unwrap()),
            Some(&initial),
        );

        let result = dialog.try_submit().unwrap();
        assert_eq!(result.sockets.len(), 2);
        assert_eq!(result.sockets[1].display().as_str(), "wayland-1");
    }

    #[test]
    fn current_display_marker_follows_the_socket_name() {
        let socket = socket("wayland-0", 1);
        let current = WaylandDisplay::new("wayland-0").unwrap();
        let label = socket_label(&socket, Some(&current));

        assert!(label.starts_with("wayland-0  (current WAYLAND_DISPLAY)  uid "));
        assert!(!socket_label(&socket, None).contains("current WAYLAND_DISPLAY"));
    }
}
