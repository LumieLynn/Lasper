//! Selected-user shell selection and safe pre-launch fallback.

use crate::application::sessions::{
    SessionError, SessionService, ShellOpenError, ShellOpenIntent, ShellTarget,
    TerminalSessionHandle, WaylandShellRequest,
};
use crate::domain::wayland::{HostWaylandSocket, WaylandDisplay};

#[derive(Debug, thiserror::Error)]
pub enum ShellAttemptError {
    #[error("{0}")]
    Initial(#[source] ShellOpenError),
    #[error("{cause}; terminal-only fallback failed: {error}")]
    Fallback {
        cause: WaylandFallbackCause,
        #[source]
        error: ShellOpenError,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum WaylandFallbackCause {
    #[error("Wayland socket selection failed: {0}")]
    SocketSelection(#[source] SessionError),
    #[error("Wayland validation failed: {0}")]
    Validation(#[source] SessionError),
}

impl WaylandFallbackCause {
    pub fn context(&self) -> &'static str {
        match self {
            Self::SocketSelection(_) => "Wayland socket selection failed",
            Self::Validation(_) => "Wayland validation failed",
        }
    }

    pub fn error(&self) -> &SessionError {
        match self {
            Self::SocketSelection(error) | Self::Validation(error) => error,
        }
    }
}

impl SessionService {
    /// Retry only failures before terminal creation. X11 context, command,
    /// environment and size remain attached to the original intent.
    pub async fn open_shell_with_fallback(
        &self,
        intent: ShellOpenIntent,
        allow_fallback: bool,
    ) -> Result<(TerminalSessionHandle, Option<WaylandFallbackCause>), ShellAttemptError> {
        // Explicit display requests never silently select a different session.
        let allow_fallback =
            allow_fallback && matches!(intent.wayland(), WaylandShellRequest::Automatic);
        let (socket, mut cause) = match self
            .resolve_shell_wayland(intent.target(), intent.wayland())
            .await
        {
            Ok(socket) => (socket, None),
            Err(error) if allow_fallback => {
                (None, Some(WaylandFallbackCause::SocketSelection(error)))
            }
            Err(error) => {
                return Err(ShellAttemptError::Initial(
                    ShellOpenError::WaylandSelection(error),
                ))
            }
        };
        let has_socket = socket.is_some();
        let result = match self.open_prepared_shell(&intent, socket).await {
            Err(ShellOpenError::WaylandPreparation(error)) if allow_fallback && has_socket => {
                cause = Some(WaylandFallbackCause::Validation(error));
                self.open_prepared_shell(&intent, None).await
            }
            result => result,
        };
        match result {
            Ok(handle) => Ok((handle, cause)),
            Err(error) => Err(match cause {
                Some(cause) => ShellAttemptError::Fallback { cause, error },
                None => ShellAttemptError::Initial(error),
            }),
        }
    }

    pub(super) async fn resolve_shell_wayland(
        &self,
        target: &ShellTarget,
        selection: &WaylandShellRequest,
    ) -> Result<Option<HostWaylandSocket>, SessionError> {
        match selection {
            WaylandShellRequest::Disabled => Ok(None),
            WaylandShellRequest::Automatic => self.automatic_wayland(target.machine()).await,
            WaylandShellRequest::SelectedHostDisplay(socket) => Ok(Some(socket.clone())),
            WaylandShellRequest::Display(display) => {
                select_wayland_socket(self.discover_host_wayland_sockets().await, display)
                    .map(Some)
                    .map_err(SessionError::new)
            }
        }
    }
}

fn select_wayland_socket(
    mut sockets: Vec<HostWaylandSocket>,
    display: &WaylandDisplay,
) -> Result<HostWaylandSocket, String> {
    if sockets.is_empty() {
        return Err("no usable host Wayland socket was discovered".into());
    }
    let Some(index) = sockets
        .iter()
        .position(|socket| socket.display() == display)
    else {
        let available = sockets
            .iter()
            .map(|socket| socket.display().as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "Wayland display {display} was not discovered (available: {available})"
        ));
    };
    Ok(sockets.remove(index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::sessions::{
        journal_session_channel, terminal_session_channel, InteractiveShellEnvironment,
        JournalSessionHandle, JournalSessionRequest, SessionPort, TerminalLaunch,
        TerminalSessionRequest, ValidatedGuestUserName, WaylandPreparationRequest,
        WaylandSessionContext,
    };
    use crate::domain::machine::MachineName;
    use crate::domain::session::TerminalAttachmentKind;
    use crate::domain::wayland::SocketRevision;
    use async_trait::async_trait;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn socket(display: &str, inode: u64) -> HostWaylandSocket {
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
                inode,
                ctime_seconds: 1,
                ctime_nanoseconds: 0,
            },
        )
        .unwrap()
    }

    #[derive(Default)]
    struct CountingSessionPort {
        automatic_calls: AtomicUsize,
        discovery_calls: AtomicUsize,
    }

    #[async_trait]
    impl SessionPort for CountingSessionPort {
        async fn automatic_wayland(
            &self,
            _machine: &MachineName,
        ) -> Result<Option<HostWaylandSocket>, SessionError> {
            self.automatic_calls.fetch_add(1, Ordering::Relaxed);
            Ok(None)
        }

        async fn discover_host_wayland_sockets(&self) -> Vec<HostWaylandSocket> {
            self.discovery_calls.fetch_add(1, Ordering::Relaxed);
            Vec::new()
        }

        async fn open_terminal(
            &self,
            _request: TerminalSessionRequest,
        ) -> Result<TerminalSessionHandle, SessionError> {
            panic!("CLI preparation must not open a terminal")
        }

        async fn prepare_wayland(
            &self,
            _request: WaylandPreparationRequest,
        ) -> Result<WaylandSessionContext, SessionError> {
            panic!("disabled Wayland must not run a probe")
        }

        async fn probe_x11_projection(
            &self,
            _request: crate::application::sessions::X11ProjectionProbeRequest,
        ) -> Result<crate::application::sessions::X11ProjectionContext, SessionError> {
            panic!("CLI preparation must not run an X11 probe")
        }

        async fn open_journal(
            &self,
            _request: JournalSessionRequest,
        ) -> Result<JournalSessionHandle, SessionError> {
            panic!("CLI preparation must not open a journal")
        }
    }

    struct FallbackSessionPort {
        prepare_calls: AtomicUsize,
        open_calls: AtomicUsize,
        open_wayland: parking_lot::Mutex<Vec<bool>>,
        fail_fallback: bool,
        fail_selection: bool,
        probe_succeeds: bool,
    }

    impl FallbackSessionPort {
        fn new(fail_fallback: bool) -> Self {
            Self {
                prepare_calls: AtomicUsize::new(0),
                open_calls: AtomicUsize::new(0),
                open_wayland: parking_lot::Mutex::new(Vec::new()),
                fail_fallback,
                fail_selection: false,
                probe_succeeds: false,
            }
        }
    }

    #[async_trait]
    impl SessionPort for FallbackSessionPort {
        async fn automatic_wayland(
            &self,
            _machine: &MachineName,
        ) -> Result<Option<HostWaylandSocket>, SessionError> {
            if self.fail_selection {
                return Err(SessionError::new("simulated socket selection failure"));
            }
            Ok(Some(socket("wayland-0", 1)))
        }

        async fn discover_host_wayland_sockets(&self) -> Vec<HostWaylandSocket> {
            vec![socket("wayland-0", 1)]
        }

        async fn open_terminal(
            &self,
            request: TerminalSessionRequest,
        ) -> Result<TerminalSessionHandle, SessionError> {
            self.open_calls.fetch_add(1, Ordering::Relaxed);
            let has_wayland = matches!(
                &request.launch,
                TerminalLaunch::SelectedUserShell { environment, .. }
                    if environment.wayland_context().is_some()
            );
            self.open_wayland.lock().push(has_wayland);
            if self.fail_fallback {
                return Err(SessionError::new("simulated terminal open failure"));
            }
            Ok(terminal_session_channel(request.id, TerminalAttachmentKind::Login).0)
        }

        async fn prepare_wayland(
            &self,
            request: WaylandPreparationRequest,
        ) -> Result<WaylandSessionContext, SessionError> {
            self.prepare_calls.fetch_add(1, Ordering::Relaxed);
            if self.probe_succeeds {
                return Ok(WaylandSessionContext::verified(
                    request.host_socket,
                    PathBuf::from("/run/lasper/wayland/1000/wayland-0"),
                    crate::application::sessions::ObservedGuestIdentity::new(1000, 1000),
                ));
            }
            Err(SessionError::with_hint(
                "simulated Wayland probe failure",
                "simulated probe hint",
            ))
        }

        async fn probe_x11_projection(
            &self,
            _request: crate::application::sessions::X11ProjectionProbeRequest,
        ) -> Result<crate::application::sessions::X11ProjectionContext, SessionError> {
            panic!("Wayland fallback tests must not run an X11 probe")
        }

        async fn open_journal(
            &self,
            request: JournalSessionRequest,
        ) -> Result<JournalSessionHandle, SessionError> {
            Ok(journal_session_channel(request.id).0)
        }
    }

    fn shell_intent(wayland: WaylandShellRequest) -> ShellOpenIntent {
        ShellOpenIntent::new(
            ShellTarget::new(
                MachineName::new("demo").unwrap(),
                ValidatedGuestUserName::new("alice").unwrap(),
            ),
            wayland,
            InteractiveShellEnvironment::default(),
            crate::domain::session::SessionSize::new(80, 24).unwrap(),
        )
    }

    #[tokio::test]
    async fn interactive_wayland_probe_retries_once_without_wayland() {
        let port = Arc::new(FallbackSessionPort::new(false));
        let service = SessionService::new(port.clone());
        let (mut handle, used_fallback) = service
            .open_shell_with_fallback(shell_intent(WaylandShellRequest::Automatic), true)
            .await
            .unwrap();

        assert!(matches!(
            used_fallback,
            Some(WaylandFallbackCause::Validation(_))
        ));
        assert_eq!(port.prepare_calls.load(Ordering::Relaxed), 1);
        assert_eq!(port.open_calls.load(Ordering::Relaxed), 1);
        assert_eq!(*port.open_wayland.lock(), [false]);
        handle.close();
    }

    #[tokio::test]
    async fn explicit_wayland_failure_does_not_retry() {
        let port = Arc::new(FallbackSessionPort::new(false));
        let service = SessionService::new(port.clone());
        let error = match service
            .open_shell_with_fallback(
                shell_intent(WaylandShellRequest::Display(
                    WaylandDisplay::new("wayland-0").unwrap(),
                )),
                true,
            )
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("explicit Wayland failure unexpectedly opened a shell"),
        };

        assert!(matches!(
            error,
            ShellAttemptError::Initial(ShellOpenError::WaylandPreparation(_))
        ));
        assert_eq!(port.prepare_calls.load(Ordering::Relaxed), 1);
        assert_eq!(port.open_calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn wayland_fallback_failure_is_reported_as_fallback_error() {
        let port = Arc::new(FallbackSessionPort::new(true));
        let service = SessionService::new(port.clone());
        let error = match service
            .open_shell_with_fallback(shell_intent(WaylandShellRequest::Automatic), true)
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("Wayland fallback unexpectedly succeeded"),
        };

        assert!(matches!(
            error,
            ShellAttemptError::Fallback {
                cause: WaylandFallbackCause::Validation(_),
                error: ShellOpenError::Terminal(_),
            }
        ));
        assert_eq!(port.prepare_calls.load(Ordering::Relaxed), 1);
        assert_eq!(port.open_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn exact_display_selection_uses_all_discovered_sockets() {
        let selected = select_wayland_socket(
            vec![socket("wayland-0", 1), socket("wayland-1", 2)],
            &WaylandDisplay::new("wayland-1").unwrap(),
        )
        .unwrap();

        assert_eq!(selected.display().as_str(), "wayland-1");
        assert!(select_wayland_socket(
            vec![socket("wayland-0", 1)],
            &WaylandDisplay::new("wayland-2").unwrap(),
        )
        .unwrap_err()
        .contains("available: wayland-0"));
    }

    #[tokio::test]
    async fn automatic_launcher_does_not_fall_back() {
        let port = Arc::new(FallbackSessionPort::new(false));
        let service = SessionService::new(port.clone());
        assert!(matches!(
            service
                .open_shell_with_fallback(shell_intent(WaylandShellRequest::Automatic), false)
                .await,
            Err(ShellAttemptError::Initial(
                ShellOpenError::WaylandPreparation(_)
            ))
        ));
        assert_eq!(port.open_calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn terminal_failure_is_never_retried() {
        let port = Arc::new(FallbackSessionPort {
            probe_succeeds: true,
            ..FallbackSessionPort::new(true)
        });
        let service = SessionService::new(port.clone());
        assert!(matches!(
            service
                .open_shell_with_fallback(shell_intent(WaylandShellRequest::Automatic), true)
                .await,
            Err(ShellAttemptError::Initial(ShellOpenError::Terminal(_)))
        ));
        assert_eq!(port.open_calls.load(Ordering::Relaxed), 1);
        assert_eq!(*port.open_wayland.lock(), [true]);
    }

    #[tokio::test]
    async fn selection_failure_preserves_its_cause_through_fallback() {
        for fail_fallback in [false, true] {
            let port = Arc::new(FallbackSessionPort {
                fail_selection: true,
                ..FallbackSessionPort::new(fail_fallback)
            });
            let service = SessionService::new(port.clone());
            let result = service
                .open_shell_with_fallback(shell_intent(WaylandShellRequest::Automatic), true)
                .await;
            if fail_fallback {
                assert!(matches!(
                    result,
                    Err(ShellAttemptError::Fallback {
                        cause: WaylandFallbackCause::SocketSelection(_),
                        ..
                    })
                ));
            } else {
                assert!(matches!(
                    result,
                    Ok((_, Some(WaylandFallbackCause::SocketSelection(_))))
                ));
            }
            assert_eq!(port.prepare_calls.load(Ordering::Relaxed), 0);
            assert_eq!(port.open_calls.load(Ordering::Relaxed), 1);
        }
    }

    #[tokio::test]
    async fn disabled_wayland_skips_discovery_and_probe() {
        let port = Arc::new(CountingSessionPort::default());
        let sessions = SessionService::new(port.clone());
        let target = ShellTarget::new(
            MachineName::new("demo").unwrap(),
            ValidatedGuestUserName::new("alice").unwrap(),
        );
        let request = sessions
            .resolve_shell_wayland(&target, &WaylandShellRequest::Disabled)
            .await
            .unwrap();

        assert!(request.is_none());
        assert_eq!(port.automatic_calls.load(Ordering::Relaxed), 0);
        assert_eq!(port.discovery_calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn automatic_wayland_uses_machine_aware_selection_only() {
        let port = Arc::new(CountingSessionPort::default());
        let sessions = SessionService::new(port.clone());
        let target = ShellTarget::new(
            MachineName::new("demo").unwrap(),
            ValidatedGuestUserName::new("alice").unwrap(),
        );

        let request = sessions
            .resolve_shell_wayland(&target, &WaylandShellRequest::Automatic)
            .await
            .unwrap();

        assert!(request.is_none());
        assert_eq!(port.automatic_calls.load(Ordering::Relaxed), 1);
        assert_eq!(port.discovery_calls.load(Ordering::Relaxed), 0);
    }
}
