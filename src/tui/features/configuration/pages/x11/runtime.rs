//! X11 runtime-access dialog adaptation.

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::layout::Rect;
use ratatui::Frame;

use super::request::X11PageAction;
use super::{X11ChecklistItem, X11PageState};
use crate::application::configuration::{
    ConfigurationSnapshot, ConfigurationTarget, X11BindingScope,
};
use crate::domain::x11::HostX11Socket;
use crate::tui::features::configuration::page::PageRequest;
use crate::tui::features::configuration::pages::x11::dialog::{
    X11AuthorizationDialog, X11AuthorizationDialogAction,
};

pub(super) enum X11RuntimeRequest {
    None,
    Action(X11PageAction),
}

impl From<X11RuntimeRequest> for PageRequest<X11PageAction> {
    fn from(request: X11RuntimeRequest) -> Self {
        match request {
            X11RuntimeRequest::None => Self::None,
            X11RuntimeRequest::Action(action) => Self::Action(action),
        }
    }
}

impl X11PageState {
    pub(super) fn access_dialog_is_open(&self) -> bool {
        self.access_dialog.is_some()
    }

    pub(super) fn handle_access_key(&mut self, key: KeyEvent) -> Option<X11RuntimeRequest> {
        let action = self.access_dialog.as_mut()?.handle_key(key);
        Some(self.handle_access_action(action))
    }

    pub(super) fn handle_access_mouse(&mut self, mouse: MouseEvent) -> Option<X11RuntimeRequest> {
        let action = self.access_dialog.as_mut()?.handle_mouse(mouse);
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

    pub(super) fn render_access_dialog(&mut self, frame: &mut Frame, area: Rect, foreground: bool) {
        if let Some(dialog) = &mut self.access_dialog {
            dialog.render_layer(frame, area, foreground);
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
        // Dropping an unclaimed mutation handle detaches it, just as closing
        // the dialog does. The accepted operation still owns its completion.
        if let Some(dialog) = &mut self.access_dialog {
            dialog.track_authorization(task);
        }
    }

    pub(super) fn track_revocation(&mut self, task: tokio::task::JoinHandle<()>) {
        if let Some(dialog) = &mut self.access_dialog {
            dialog.track_revocation(task);
        }
    }

    fn selected_host_socket(&self, snapshot: &ConfigurationSnapshot) -> Option<HostX11Socket> {
        let item = self
            .checklist
            .selected()
            .and_then(|index| self.checklist.items.get(index));
        match item {
            Some(X11ChecklistItem::Directory(_)) => preferred_standard_socket(snapshot),
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn missing_dialog_cancels_reads_but_allows_both_mutations_to_complete() {
        struct ReadResource(Option<oneshot::Sender<()>>);
        impl Drop for ReadResource {
            fn drop(&mut self) {
                let _ = self.0.take().unwrap().send(());
            }
        }
        let mut page = X11PageState::default();
        let (read_dropped_tx, read_dropped_rx) = oneshot::channel();
        let resource = ReadResource(Some(read_dropped_tx));
        page.track_check(tokio::spawn(async move {
            let _resource = resource;
            std::future::pending::<()>().await;
        }));
        read_dropped_rx.await.unwrap();

        let (authorize_tx, authorize_rx) = oneshot::channel();
        let (authorized_tx, authorized_rx) = oneshot::channel();
        page.track_authorization(tokio::spawn(async move {
            authorize_rx.await.unwrap();
            let _ = authorized_tx.send(());
        }));
        let (revoke_tx, revoke_rx) = oneshot::channel();
        let (revoked_tx, revoked_rx) = oneshot::channel();
        page.track_revocation(tokio::spawn(async move {
            revoke_rx.await.unwrap();
            let _ = revoked_tx.send(());
        }));
        drop(page);
        tokio::task::yield_now().await;
        authorize_tx.send(()).unwrap();
        revoke_tx.send(()).unwrap();
        authorized_rx.await.unwrap();
        revoked_rx.await.unwrap();
    }
}
