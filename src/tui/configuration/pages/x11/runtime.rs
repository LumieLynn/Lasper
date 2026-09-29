//! X11 runtime-access dialog adaptation.

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::Frame;

use super::{X11ChecklistItem, X11PageState};
use crate::application::configuration::{
    ConfigurationSnapshot, ConfigurationTarget, X11BindingScope,
};
use crate::domain::x11::HostX11Socket;
use crate::tui::widgets::dialogs::x11_authorization::{
    X11AuthorizationDialog, X11AuthorizationDialogAction,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum X11PageAction {
    Check {
        generation: u64,
        target: crate::application::sessions::ShellTarget,
        host_socket: HostX11Socket,
    },
    Authorize {
        generation: u64,
        target: crate::application::sessions::ShellTarget,
        host_socket: HostX11Socket,
    },
    Revoke {
        generation: u64,
        target: crate::application::sessions::ShellTarget,
        host_socket: HostX11Socket,
        record_id: String,
    },
}

pub(super) enum X11RuntimeRequest {
    None,
    Action(X11PageAction),
}

impl X11PageState {
    pub(super) fn access_dialog_is_open(&self) -> bool {
        self.access_dialog.is_some()
    }

    pub(super) fn handle_access_key(&mut self, key: KeyEvent) -> Option<X11RuntimeRequest> {
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
        self.access_dialog = Some(X11AuthorizationDialog::new(
            machine.clone(),
            snapshot.host_x11.sockets.clone(),
            selected.as_ref(),
        ));
    }

    pub(super) fn render_access_dialog(&mut self, frame: &mut Frame, area: Rect) {
        if let Some(dialog) = &mut self.access_dialog {
            dialog.render(frame, area);
        }
    }

    pub(super) fn track_check(&mut self, task: tokio::task::JoinHandle<()>) {
        if let Some(dialog) = &mut self.access_dialog {
            dialog.track_check(task);
        } else {
            task.abort();
        }
    }

    pub(super) fn track_authorization(&mut self, task: tokio::task::JoinHandle<()>) {
        if let Some(dialog) = &mut self.access_dialog {
            dialog.track_authorization(task);
        } else {
            task.abort();
        }
    }

    pub(super) fn track_revocation(&mut self, task: tokio::task::JoinHandle<()>) {
        if let Some(dialog) = &mut self.access_dialog {
            dialog.track_revocation(task);
        } else {
            task.abort();
        }
    }

    fn selected_host_socket(&self, snapshot: &ConfigurationSnapshot) -> Option<HostX11Socket> {
        let item = self
            .checklist
            .selected()
            .and_then(|index| self.checklist.items.get(index));
        match item {
            Some(X11ChecklistItem::Available(source)) => snapshot
                .host_x11
                .sockets
                .iter()
                .find(|socket| socket.source() == source)
                .cloned(),
            Some(X11ChecklistItem::Declaration(line)) => {
                let binding = snapshot
                    .x11_bindings
                    .iter()
                    .find(|binding| binding.line == *line)?;
                match binding.scope {
                    X11BindingScope::Socket { .. } => snapshot
                        .host_x11
                        .sockets
                        .iter()
                        .find(|socket| socket.source() == binding.source)
                        .cloned(),
                    X11BindingScope::Directory => preferred_standard_socket(snapshot),
                }
            }
            None => preferred_standard_socket(snapshot),
        }
    }

    fn handle_access_action(&mut self, action: X11AuthorizationDialogAction) -> X11RuntimeRequest {
        match action {
            X11AuthorizationDialogAction::None => X11RuntimeRequest::None,
            X11AuthorizationDialogAction::Close => {
                self.access_dialog = None;
                X11RuntimeRequest::None
            }
            X11AuthorizationDialogAction::Check {
                generation,
                target,
                host_socket,
            } => X11RuntimeRequest::Action(X11PageAction::Check {
                generation,
                target,
                host_socket,
            }),
            X11AuthorizationDialogAction::Authorize {
                generation,
                target,
                host_socket,
            } => X11RuntimeRequest::Action(X11PageAction::Authorize {
                generation,
                target,
                host_socket,
            }),
            X11AuthorizationDialogAction::Revoke {
                generation,
                target,
                host_socket,
                record_id,
            } => X11RuntimeRequest::Action(X11PageAction::Revoke {
                generation,
                target,
                host_socket,
                record_id,
            }),
        }
    }
}

fn preferred_standard_socket(snapshot: &ConfigurationSnapshot) -> Option<HostX11Socket> {
    snapshot
        .host_x11
        .preferred_display
        .and_then(|display| {
            snapshot
                .host_x11
                .sockets
                .iter()
                .find(|socket| socket.display() == display && !socket.alternate())
        })
        .or_else(|| {
            snapshot
                .host_x11
                .sockets
                .iter()
                .find(|socket| !socket.alternate())
        })
        .cloned()
}
