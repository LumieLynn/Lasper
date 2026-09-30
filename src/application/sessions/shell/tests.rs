use super::*;
use crate::application::sessions::{
    journal_session_channel, terminal_session_channel, InteractiveShellEnvironment,
    JournalSessionHandle, JournalSessionRequest, MappedGuestIdentity, ObservedGuestIdentity,
    ObservedMachineInstance, ObservedNamespaceIdentity, SessionPort, TerminalLaunch,
    TerminalSessionRequest, ValidatedGuestUserName, WaylandPreparationRequest,
    WaylandSessionContext, X11FilesystemAccess, X11ProjectionContext,
};
use crate::application::x11::{
    X11AclEntry, X11AclSnapshot, X11AuthorizationDisposition, X11AuthorizationRequest,
    X11DesktopAccessError, X11DesktopAccessPort, X11DesktopAuthorization, X11DesktopObservation,
    X11DesktopRevocation, X11EndpointCatalog, X11EndpointDiscoveryPort,
    X11EndpointDiscoveryService, X11GrantRecordCatalog, X11ProjectionPort, X11RevokeRequest,
};
use crate::domain::machine::MachineName;
use crate::domain::session::TerminalAttachmentKind;
use crate::domain::wayland::SocketRevision;
use crate::domain::x11::{HostX11Socket, X11SocketRevision};
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
    requests: parking_lot::Mutex<Vec<TerminalSessionRequest>>,
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
            requests: parking_lot::Mutex::new(Vec::new()),
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
        self.requests.lock().push(request.clone());
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

#[derive(Default)]
struct X11Ports {
    discoveries: AtomicUsize,
    probes: AtomicUsize,
    ensures: AtomicUsize,
    fail_projection: bool,
    fail_authorization: bool,
}

#[async_trait]
impl X11EndpointDiscoveryPort for X11Ports {
    async fn discover(&self, configured_sources: &[PathBuf]) -> X11EndpointCatalog {
        assert!(configured_sources.is_empty());
        self.discoveries.fetch_add(1, Ordering::Relaxed);
        X11EndpointCatalog {
            sockets: (0..=1)
                .map(|display| {
                    let path = PathBuf::from(format!("/tmp/.X11-unix/X{display}"));
                    HostX11Socket::from_verified_parts(
                        display,
                        false,
                        path.clone(),
                        path,
                        1000,
                        1000,
                        0o755,
                        42,
                        1000,
                        1000,
                        X11SocketRevision {
                            device: 1,
                            inode: u64::from(display) + 2,
                            ctime_seconds: 3,
                            ctime_nanoseconds: 4,
                        },
                    )
                    .unwrap()
                })
                .collect(),
            preferred_display: Some(0),
            ..Default::default()
        }
    }
}

#[async_trait]
impl X11ProjectionPort for X11Ports {
    async fn probe(
        &self,
        _target: ShellTarget,
        socket: HostX11Socket,
    ) -> Result<X11ProjectionContext, SessionError> {
        self.probes.fetch_add(1, Ordering::Relaxed);
        if self.fail_projection {
            return Err(SessionError::new("simulated X11 projection failure"));
        }
        let path = PathBuf::from(format!("/tmp/.X11-unix/X{}", socket.display()));
        let namespace = ObservedNamespaceIdentity::new(1, 2);
        Ok(X11ProjectionContext::verified(
            socket,
            path.clone(),
            path,
            X11FilesystemAccess::observed(true, true),
            MappedGuestIdentity::verified(
                ObservedGuestIdentity::new(1000, 1000),
                1_437_402_088,
                1_437_402_088,
                ObservedMachineInstance::new(42, namespace, namespace),
            ),
        ))
    }
}

#[async_trait]
impl X11DesktopAccessPort for X11Ports {
    async fn snapshot(
        &self,
        _socket: &HostX11Socket,
    ) -> Result<X11DesktopObservation, X11DesktopAccessError> {
        panic!("shell preparation must use the authorization observation")
    }

    async fn ensure(
        &self,
        request: &X11AuthorizationRequest,
    ) -> Result<X11DesktopAuthorization, X11DesktopAccessError> {
        self.ensures.fetch_add(1, Ordering::Relaxed);
        if self.fail_authorization {
            return Err(X11DesktopAccessError::new(
                "simulated X11 authorization failure",
            ));
        }
        let uid = request.projection().identity().host_uid();
        Ok(X11DesktopAuthorization::new(
            X11DesktopObservation::new(
                X11AclSnapshot::from_wire(
                    1,
                    vec![X11AclEntry::from_wire(
                        5,
                        format!("localuser\0#{uid}").into_bytes(),
                    )],
                ),
                1000,
                None,
                None,
                X11GrantRecordCatalog::empty(),
                Vec::new(),
            ),
            X11AuthorizationDisposition::Added {
                record_id: "test-grant".into(),
            },
        ))
    }

    async fn revoke(
        &self,
        _request: &X11RevokeRequest,
    ) -> Result<X11DesktopRevocation, X11DesktopAccessError> {
        panic!("shell opening must not revoke machine-lifetime access")
    }

    async fn synchronize_reconcile_activation(&self) -> Result<(), X11DesktopAccessError> {
        Ok(())
    }
}

fn x11_service(ports: Arc<X11Ports>) -> X11AccessService {
    X11AccessService::new(
        ports.clone(),
        Arc::new(X11EndpointDiscoveryService::new(ports.clone())),
        ports,
    )
}

fn launch_request(wayland: WaylandShellRequest) -> ShellLaunchRequest {
    let intent = shell_intent(wayland);
    ShellLaunchRequest::new(
        intent.target().clone(),
        intent.wayland().clone(),
        intent.terminal_environment().clone(),
        intent.size(),
    )
}

#[tokio::test]
async fn x11_is_not_prepared_without_an_explicit_selection() {
    let port = Arc::new(FallbackSessionPort::new(false));
    let sessions = SessionService::new(port.clone());
    let x11_ports = Arc::new(X11Ports::default());
    let x11 = x11_service(x11_ports.clone());
    let mut opened = sessions
        .launch_shell(launch_request(WaylandShellRequest::Disabled), Some(&x11))
        .await
        .unwrap();

    assert!(opened.x11.is_none());
    assert_eq!(x11_ports.discoveries.load(Ordering::Relaxed), 0);
    assert_eq!(x11_ports.probes.load(Ordering::Relaxed), 0);
    assert_eq!(x11_ports.ensures.load(Ordering::Relaxed), 0);
    assert_eq!(port.open_calls.load(Ordering::Relaxed), 1);
    opened.handle.close();
}

#[tokio::test]
async fn current_and_exact_x11_selections_reach_the_terminal() {
    for (selection, display) in [
        (X11SessionSelection::Current, 0),
        (X11SessionSelection::Display(1), 1),
    ] {
        let port = Arc::new(FallbackSessionPort::new(false));
        let sessions = SessionService::new(port.clone());
        let x11_ports = Arc::new(X11Ports::default());
        let x11 = x11_service(x11_ports.clone());
        let mut opened = sessions
            .launch_shell(
                launch_request(WaylandShellRequest::Disabled).with_x11(selection),
                Some(&x11),
            )
            .await
            .unwrap();

        let prepared = opened.x11.as_ref().unwrap();
        assert_eq!(prepared.context().display(), display);
        let requests = port.requests.lock();
        let TerminalLaunch::SelectedUserShell { environment, .. } = &requests[0].launch else {
            panic!("expected selected-user shell");
        };
        assert_eq!(environment.x11_context(), Some(prepared.context()));
        assert_eq!(x11_ports.probes.load(Ordering::Relaxed), 1);
        assert_eq!(x11_ports.ensures.load(Ordering::Relaxed), 1);
        opened.handle.close();
    }
}

#[tokio::test]
async fn explicit_x11_failure_never_opens_a_fallback_terminal() {
    for fail_projection in [true, false] {
        let port = Arc::new(FallbackSessionPort::new(false));
        let sessions = SessionService::new(port.clone());
        let x11_ports = Arc::new(X11Ports {
            fail_projection,
            fail_authorization: !fail_projection,
            ..Default::default()
        });
        let x11 = x11_service(x11_ports.clone());
        let result = sessions
            .launch_shell(
                launch_request(WaylandShellRequest::Automatic)
                    .with_x11(X11SessionSelection::Current)
                    .with_wayland_fallback(true),
                Some(&x11),
            )
            .await;

        assert!(matches!(result, Err(ShellLaunchError::X11(_))));
        assert_eq!(port.prepare_calls.load(Ordering::Relaxed), 0);
        assert_eq!(port.open_calls.load(Ordering::Relaxed), 0);
        assert_eq!(
            x11_ports.ensures.load(Ordering::Relaxed),
            usize::from(!fail_projection)
        );
    }
}

#[tokio::test]
async fn explicit_x11_without_an_access_service_is_an_error() {
    let port = Arc::new(FallbackSessionPort::new(false));
    let sessions = SessionService::new(port.clone());
    let result = sessions
        .launch_shell(
            launch_request(WaylandShellRequest::Disabled).with_x11(X11SessionSelection::Current),
            None,
        )
        .await;

    assert!(matches!(
        result,
        Err(ShellLaunchError::X11(X11AccessError::Selection(_)))
    ));
    assert_eq!(port.open_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn wayland_fallback_preserves_x11_command_environment_and_size() {
    let port = Arc::new(FallbackSessionPort::new(false));
    let sessions = SessionService::new(port.clone());
    let x11_ports = Arc::new(X11Ports::default());
    let x11 = x11_service(x11_ports.clone());
    let target = shell_intent(WaylandShellRequest::Automatic)
        .target()
        .clone();
    let terminal_environment = InteractiveShellEnvironment::new(
        "xterm-256color".into(),
        Some("truecolor".into()),
        Some(String::new()),
    )
    .unwrap();
    let command =
        GuestCommand::new("/usr/bin/kitty", vec!["--class".into(), "demo".into()]).unwrap();
    let size = SessionSize::new(111, 37).unwrap();
    let request = ShellLaunchRequest::new(
        target.clone(),
        WaylandShellRequest::Automatic,
        terminal_environment.clone(),
        size,
    )
    .with_command(command.clone())
    .with_x11(X11SessionSelection::Display(1))
    .with_wayland_fallback(true);
    let mut opened = sessions.launch_shell(request, Some(&x11)).await.unwrap();

    assert!(matches!(
        opened.wayland_fallback,
        Some(WaylandFallbackCause::Validation(_))
    ));
    assert_eq!(x11_ports.probes.load(Ordering::Relaxed), 1);
    assert_eq!(x11_ports.ensures.load(Ordering::Relaxed), 1);
    assert_eq!(port.prepare_calls.load(Ordering::Relaxed), 1);
    let requests = port.requests.lock();
    assert_eq!(requests.len(), 1);
    let terminal = &requests[0];
    assert_eq!(&terminal.machine, target.machine());
    assert_eq!(terminal.size, size);
    let TerminalLaunch::SelectedUserShell {
        user,
        environment,
        command: launched_command,
    } = &terminal.launch
    else {
        panic!("expected selected-user shell");
    };
    assert_eq!(user, target.user());
    assert_eq!(launched_command.as_ref(), Some(&command));
    assert_eq!(environment.terminal_environment(), &terminal_environment);
    assert!(environment.wayland_context().is_none());
    assert_eq!(
        environment.x11_context(),
        Some(opened.x11.as_ref().unwrap().context())
    );
    opened.handle.close();
}

#[tokio::test]
async fn wayland_and_x11_can_be_prepared_together() {
    let port = Arc::new(FallbackSessionPort {
        probe_succeeds: true,
        ..FallbackSessionPort::new(false)
    });
    let sessions = SessionService::new(port.clone());
    let x11_ports = Arc::new(X11Ports::default());
    let x11 = x11_service(x11_ports.clone());
    let mut opened = sessions
        .launch_shell(
            launch_request(WaylandShellRequest::Automatic)
                .with_x11(X11SessionSelection::Current)
                .with_wayland_fallback(true),
            Some(&x11),
        )
        .await
        .unwrap();

    assert!(opened.wayland_fallback.is_none());
    assert_eq!(x11_ports.ensures.load(Ordering::Relaxed), 1);
    let requests = port.requests.lock();
    let TerminalLaunch::SelectedUserShell { environment, .. } = &requests[0].launch else {
        panic!("expected selected-user shell");
    };
    assert!(environment.wayland_context().is_some());
    assert!(environment.x11_context().is_some());
    opened.handle.close();
}

#[tokio::test]
async fn terminal_failure_retains_the_x11_preparation_without_reauthorizing() {
    for probe_succeeds in [true, false] {
        let port = Arc::new(FallbackSessionPort {
            probe_succeeds,
            ..FallbackSessionPort::new(true)
        });
        let sessions = SessionService::new(port.clone());
        let x11_ports = Arc::new(X11Ports::default());
        let x11 = x11_service(x11_ports.clone());
        let result = sessions
            .launch_shell(
                launch_request(WaylandShellRequest::Automatic)
                    .with_x11(X11SessionSelection::Current)
                    .with_wayland_fallback(true),
                Some(&x11),
            )
            .await;

        let Err(ShellLaunchError::Open {
            error,
            x11: Some(prepared),
        }) = result
        else {
            panic!("expected terminal failure with prepared X11 access");
        };
        assert_eq!(prepared.context().display(), 0);
        assert!(matches!(
            prepared.disposition(),
            X11AuthorizationDisposition::Added { .. }
        ));
        assert_eq!(
            matches!(error, ShellAttemptError::Initial(_)),
            probe_succeeds
        );
        assert_eq!(x11_ports.ensures.load(Ordering::Relaxed), 1);
        assert_eq!(port.open_calls.load(Ordering::Relaxed), 1);
    }
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
