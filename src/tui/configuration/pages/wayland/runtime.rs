//! Wayland runtime-access dialog adaptation.

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::Frame;

use super::{WaylandChecklistItem, WaylandPageState};
use crate::application::configuration::{ConfigurationSnapshot, ConfigurationTarget};
use crate::domain::wayland::HostWaylandSocket;
use crate::tui::widgets::dialogs::wayland_session::{
    WaylandSessionDialog, WaylandSessionDialogAction,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WaylandPageAction {
    Check {
        generation: u64,
        target: crate::application::sessions::ShellTarget,
        host_socket: HostWaylandSocket,
    },
    EnterShell {
        target: crate::application::sessions::ShellTarget,
        host_socket: HostWaylandSocket,
    },
}

pub(super) enum WaylandRuntimeRequest {
    None,
    Action(WaylandPageAction),
}

impl WaylandPageState {
    pub(super) fn access_dialog_is_open(&self) -> bool {
        self.access_dialog.is_some()
    }

    pub(super) fn handle_access_key(&mut self, key: KeyEvent) -> Option<WaylandRuntimeRequest> {
        let action = self.access_dialog.as_mut()?.handle_key(key);
        Some(self.handle_access_action(action))
    }

    pub(super) fn open_access_dialog(
        &mut self,
        target: &ConfigurationTarget,
        snapshot: &ConfigurationSnapshot,
    ) {
        let ConfigurationTarget::Machine(machine) = target else {
            return;
        };
        let selected = self.selected_host_socket(snapshot);
        self.access_dialog = Some(WaylandSessionDialog::new(
            machine.clone(),
            snapshot.host_wayland.sockets.clone(),
            snapshot.host_wayland.preferred_display.clone(),
            selected.as_ref(),
        ));
    }

    pub(super) fn block_shell(&mut self, message: impl Into<String>) {
        if let Some(dialog) = &mut self.access_dialog {
            dialog.block_shell(message);
        }
    }

    pub(super) fn track_check(&mut self, task: tokio::task::JoinHandle<()>) {
        if let Some(dialog) = &mut self.access_dialog {
            dialog.track_check(task);
        } else {
            task.abort();
        }
    }

    pub(super) fn render_access_dialog(&mut self, frame: &mut Frame, area: Rect) {
        if let Some(dialog) = &mut self.access_dialog {
            dialog.render(frame, area);
        }
    }

    fn selected_host_socket(&self, snapshot: &ConfigurationSnapshot) -> Option<HostWaylandSocket> {
        let selected = self
            .checklist
            .selected()
            .and_then(|index| self.checklist.items.get(index));
        let selected = match selected {
            Some(WaylandChecklistItem::Available(source)) => snapshot
                .host_wayland
                .sockets
                .iter()
                .find(|socket| socket.canonical_path() == source),
            Some(WaylandChecklistItem::Declaration(line)) => snapshot
                .wayland_bindings
                .iter()
                .find(|binding| binding.line == *line)
                .and_then(|binding| {
                    snapshot.host_wayland.sockets.iter().find(|socket| {
                        socket.canonical_path() == binding.source
                            || socket.display() == &binding.display
                    })
                }),
            None => None,
        };
        selected
            .or_else(|| {
                snapshot
                    .host_wayland
                    .preferred_display
                    .as_ref()
                    .and_then(|preferred| {
                        snapshot
                            .host_wayland
                            .sockets
                            .iter()
                            .find(|socket| socket.display() == preferred)
                    })
            })
            .or_else(|| snapshot.host_wayland.sockets.first())
            .cloned()
    }

    fn handle_access_action(
        &mut self,
        action: WaylandSessionDialogAction,
    ) -> WaylandRuntimeRequest {
        match action {
            WaylandSessionDialogAction::None => WaylandRuntimeRequest::None,
            WaylandSessionDialogAction::Close => {
                self.access_dialog = None;
                WaylandRuntimeRequest::None
            }
            WaylandSessionDialogAction::Check {
                generation,
                target,
                host_socket,
            } => WaylandRuntimeRequest::Action(WaylandPageAction::Check {
                generation,
                target,
                host_socket,
            }),
            WaylandSessionDialogAction::EnterShell {
                target,
                host_socket,
            } => WaylandRuntimeRequest::Action(WaylandPageAction::EnterShell {
                target,
                host_socket,
            }),
        }
    }
}
