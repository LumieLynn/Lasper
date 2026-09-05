use super::{
    JournalSessionHandle, JournalSessionRequest, SessionError, SessionPort, ShellOpenError,
    ShellOpenIntent, ShellTarget, TerminalSessionHandle, TerminalSessionRequest,
    TypedSessionEnvironment, WaylandPreparationRequest, WaylandSessionContext, WaylandShellRequest,
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

    pub async fn automatic_wayland(
        &self,
        machine: &MachineName,
    ) -> Result<WaylandShellRequest, SessionError> {
        self.port.automatic_wayland(machine).await.map(|socket| {
            socket
                .map(WaylandShellRequest::SelectedHostDisplay)
                .unwrap_or(WaylandShellRequest::Disabled)
        })
    }

    pub async fn open_shell(
        &self,
        intent: ShellOpenIntent,
    ) -> Result<TerminalSessionHandle, ShellOpenError> {
        let terminal_environment = intent.terminal_environment().clone();
        let command = intent.command().cloned();
        let environment = match intent.wayland() {
            WaylandShellRequest::Disabled => {
                TypedSessionEnvironment::terminal(terminal_environment)
            }
            WaylandShellRequest::SelectedHostDisplay(socket) => TypedSessionEnvironment::wayland(
                terminal_environment,
                self.prepare_wayland(intent.target().clone(), socket.clone())
                    .await
                    .map_err(ShellOpenError::WaylandPreparation)?,
            ),
        };
        let target = intent.target();
        self.port
            .open_terminal(TerminalSessionRequest::selected_user_shell_with_command(
                self.allocate_id(),
                target.machine().clone(),
                target.user().clone(),
                environment,
                command,
                intent.size(),
            ))
            .await
            .map_err(ShellOpenError::Terminal)
    }

    pub async fn test_wayland(
        &self,
        target: ShellTarget,
        host_socket: crate::domain::wayland::HostWaylandSocket,
    ) -> Result<WaylandSessionContext, SessionError> {
        self.prepare_wayland(target, host_socket).await
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
    use crate::application::sessions::{
        journal_session_channel, terminal_session_channel, JournalSessionRequest, SessionPort,
        TerminalLaunch, TerminalSessionRequest, WaylandPreparationRequest, WaylandSessionContext,
    };
    use crate::domain::session::TerminalAttachmentKind;
    use crate::domain::wayland::{HostWaylandSocket, SocketRevision, WaylandDisplay};
    use parking_lot::Mutex;
    use std::path::PathBuf;

    #[derive(Default)]
    struct RecordingPort {
        ids: Mutex<Vec<SessionId>>,
        terminal_wayland_contexts: Mutex<Vec<bool>>,
        terminal_terms: Mutex<Vec<String>>,
        terminal_commands: Mutex<Vec<Option<String>>>,
    }

    #[async_trait::async_trait]
    impl SessionPort for RecordingPort {
        async fn automatic_wayland(
            &self,
            _machine: &MachineName,
        ) -> Result<Option<HostWaylandSocket>, SessionError> {
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
            self.terminal_wayland_contexts.lock().push(matches!(
                &request.launch,
                TerminalLaunch::SelectedUserShell { environment, .. }
                    if environment.wayland_context().is_some()
            ));
            if let TerminalLaunch::SelectedUserShell { environment, .. } = &request.launch {
                self.terminal_terms
                    .lock()
                    .push(environment.terminal_environment().term().to_string());
            }
            if let TerminalLaunch::SelectedUserShell { command, .. } = &request.launch {
                self.terminal_commands.lock().push(
                    command
                        .as_ref()
                        .map(|command| command.program().to_string()),
                );
            }
            Ok(terminal_session_channel(request.id, TerminalAttachmentKind::Login).0)
        }

        async fn prepare_wayland(
            &self,
            request: WaylandPreparationRequest,
        ) -> Result<WaylandSessionContext, SessionError> {
            self.ids.lock().push(request.probe_id);
            Ok(WaylandSessionContext::verified(
                request.host_socket,
                PathBuf::from("/run/lasper/wayland/1000/wayland-0"),
                crate::application::sessions::ObservedGuestIdentity::new(1000, 1000),
            ))
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

    #[tokio::test]
    async fn wayland_shell_prepares_before_opening_with_verified_context() {
        let port = Arc::new(RecordingPort::default());
        let service = SessionService::new(port.clone());
        let socket = HostWaylandSocket::from_verified_parts(
            WaylandDisplay::new("wayland-0").unwrap(),
            PathBuf::from("/run/user/1000"),
            PathBuf::from("/run/user/1000/wayland-0"),
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
        .unwrap();

        let _terminal = service
            .open_shell(ShellOpenIntent::new(
                ShellTarget::new(
                    MachineName::new("test").unwrap(),
                    crate::application::sessions::ValidatedGuestUserName::new("alice").unwrap(),
                ),
                WaylandShellRequest::SelectedHostDisplay(socket),
                crate::application::sessions::InteractiveShellEnvironment::default(),
                SessionSize::new(80, 24).unwrap(),
            ))
            .await
            .unwrap();

        let ids = port.ids.lock();
        assert_eq!(ids.iter().map(|id| id.get()).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(*port.terminal_wayland_contexts.lock(), [true]);
        assert_eq!(*port.terminal_terms.lock(), ["dumb"]);
    }

    #[tokio::test]
    async fn shell_command_is_carried_after_wayland_preparation() {
        let port = Arc::new(RecordingPort::default());
        let service = SessionService::new(port.clone());
        let command = crate::application::sessions::GuestCommand::new(
            "/usr/bin/kitty",
            vec!["--single-instance".into()],
        )
        .unwrap();
        let _terminal = service
            .open_shell(
                ShellOpenIntent::new(
                    ShellTarget::new(
                        MachineName::new("test").unwrap(),
                        crate::application::sessions::ValidatedGuestUserName::new("alice").unwrap(),
                    ),
                    WaylandShellRequest::Disabled,
                    crate::application::sessions::InteractiveShellEnvironment::default(),
                    SessionSize::new(80, 24).unwrap(),
                )
                .with_command(command),
            )
            .await
            .unwrap();

        assert_eq!(
            *port.terminal_commands.lock(),
            [Some("/usr/bin/kitty".into())]
        );
    }
}
