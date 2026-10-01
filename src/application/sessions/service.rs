use super::{
    GuestCommand, JournalSessionHandle, JournalSessionRequest, SessionError, SessionPort,
    ShellTarget, TerminalSessionHandle, TerminalSessionRequest, TypedSessionEnvironment,
    WaylandPreparationRequest, WaylandSessionContext, X11ProjectionContext,
    X11ProjectionProbeRequest,
};
use crate::domain::machine::MachineName;
use crate::domain::session::{SessionId, SessionSize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub struct SessionService {
    port: Arc<dyn SessionPort>,
    next_id: AtomicU64,
}

impl SessionService {
    pub fn new(port: Arc<dyn SessionPort>) -> Self {
        Self {
            port,
            next_id: AtomicU64::new(1),
        }
    }

    pub async fn open_terminal(
        &self,
        machine: MachineName,
        size: SessionSize,
    ) -> Result<TerminalSessionHandle, SessionError> {
        self.port
            .open_terminal(TerminalSessionRequest::login_prompt(
                self.allocate_id(),
                machine,
                size,
            ))
            .await
    }

    pub async fn discover_host_wayland_sockets(
        &self,
    ) -> Vec<crate::domain::wayland::HostWaylandSocket> {
        self.port.discover_host_wayland_sockets().await
    }

    pub(super) async fn automatic_wayland(
        &self,
        machine: &MachineName,
    ) -> Result<Option<crate::domain::wayland::HostWaylandSocket>, SessionError> {
        self.port.automatic_wayland(machine).await
    }

    pub async fn test_wayland(
        &self,
        target: ShellTarget,
        host_socket: crate::domain::wayland::HostWaylandSocket,
    ) -> Result<WaylandSessionContext, SessionError> {
        self.prepare_wayland(target, host_socket).await
    }

    pub async fn test_x11_projection(
        &self,
        target: ShellTarget,
        host_socket: crate::domain::x11::HostX11Socket,
    ) -> Result<X11ProjectionContext, SessionError> {
        self.port
            .probe_x11_projection(X11ProjectionProbeRequest {
                probe_id: self.allocate_id(),
                target,
                host_socket,
            })
            .await
    }

    pub async fn open_journal(
        &self,
        machine: MachineName,
    ) -> Result<JournalSessionHandle, SessionError> {
        self.port
            .open_journal(JournalSessionRequest {
                id: self.allocate_id(),
                machine,
            })
            .await
    }

    pub(super) async fn open_selected_user_terminal(
        &self,
        target: &ShellTarget,
        environment: TypedSessionEnvironment,
        command: Option<GuestCommand>,
        size: SessionSize,
    ) -> Result<TerminalSessionHandle, SessionError> {
        self.port
            .open_terminal(TerminalSessionRequest::selected_user_shell_with_command(
                self.allocate_id(),
                target.machine().clone(),
                target.user().clone(),
                environment,
                command,
                size,
            ))
            .await
    }

    pub(crate) fn allocate_id(&self) -> SessionId {
        loop {
            let value = self.next_id.fetch_add(1, Ordering::Relaxed);
            if let Ok(id) = SessionId::new(value) {
                return id;
            }
        }
    }

    async fn prepare_wayland(
        &self,
        target: ShellTarget,
        host_socket: crate::domain::wayland::HostWaylandSocket,
    ) -> Result<WaylandSessionContext, SessionError> {
        self.port
            .prepare_wayland(WaylandPreparationRequest {
                probe_id: self.allocate_id(),
                target,
                host_socket,
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::sessions::{journal_session_channel, terminal_session_channel};
    use crate::domain::session::TerminalAttachmentKind;
    use parking_lot::Mutex;

    #[derive(Default)]
    struct RecordingPort {
        ids: Mutex<Vec<SessionId>>,
    }

    #[async_trait::async_trait]
    impl SessionPort for RecordingPort {
        async fn automatic_wayland(
            &self,
            _machine: &MachineName,
        ) -> Result<Option<crate::domain::wayland::HostWaylandSocket>, SessionError> {
            Ok(None)
        }

        async fn discover_host_wayland_sockets(
            &self,
        ) -> Vec<crate::domain::wayland::HostWaylandSocket> {
            Vec::new()
        }

        async fn open_terminal(
            &self,
            request: TerminalSessionRequest,
        ) -> Result<TerminalSessionHandle, SessionError> {
            self.ids.lock().push(request.id);
            Ok(terminal_session_channel(request.id, TerminalAttachmentKind::Login).0)
        }

        async fn prepare_wayland(
            &self,
            _request: WaylandPreparationRequest,
        ) -> Result<WaylandSessionContext, SessionError> {
            panic!("session allocation must not prepare Wayland")
        }

        async fn probe_x11_projection(
            &self,
            _request: X11ProjectionProbeRequest,
        ) -> Result<X11ProjectionContext, SessionError> {
            panic!("session allocation must not probe X11")
        }

        async fn open_journal(
            &self,
            request: JournalSessionRequest,
        ) -> Result<JournalSessionHandle, SessionError> {
            self.ids.lock().push(request.id);
            Ok(journal_session_channel(request.id).0)
        }
    }

    #[tokio::test]
    async fn service_assigns_distinct_ids_across_session_kinds() {
        let port = Arc::new(RecordingPort::default());
        let service = SessionService::new(port.clone());
        let machine = MachineName::new("test").unwrap();
        let _terminal = service
            .open_terminal(machine.clone(), SessionSize::new(80, 24).unwrap())
            .await
            .unwrap();
        let _journal = service.open_journal(machine).await.unwrap();

        let ids = port.ids.lock();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
    }
}
