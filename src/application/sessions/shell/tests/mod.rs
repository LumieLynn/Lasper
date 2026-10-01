use super::*;
use crate::application::sessions::{
    journal_session_channel, terminal_session_channel, InteractiveShellEnvironment,
    JournalSessionHandle, JournalSessionRequest, MappedGuestIdentity, ObservedGuestIdentity,
    ObservedMachineInstance, ObservedNamespaceIdentity, SessionPort, TerminalLaunch,
    TerminalSessionRequest, ValidatedGuestUserName, WaylandPreparationRequest,
    WaylandSessionContext, X11FilesystemAccess, X11ProjectionContext,
};
use crate::application::x11::{
    X11AccessCheck, X11AclEntry, X11AclSnapshot, X11AuthorizationDisposition, X11DesktopAccessError,
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
struct RecordingX11Preparation {
    calls: parking_lot::Mutex<Vec<(ShellTarget, X11SessionSelection)>>,
    fail_projection: bool,
    fail_authorization: bool,
}

fn x11_projection(display: u16) -> X11ProjectionContext {
    let path = PathBuf::from(format!("/tmp/.X11-unix/X{display}"));
    let socket = HostX11Socket::from_verified_parts(
        display,
        false,
        path.clone(),
        path.clone(),
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
    .unwrap();
    let namespace = ObservedNamespaceIdentity::new(1, 2);
    X11ProjectionContext::verified(
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
    )
}

#[async_trait]
impl X11SessionPreparationPort for RecordingX11Preparation {
    async fn prepare_session(
        &self,
        target: ShellTarget,
        selection: X11SessionSelection,
    ) -> Result<X11SessionPreparation, X11AccessError> {
        self.calls.lock().push((target.clone(), selection));
        if self.fail_projection {
            return Err(X11AccessError::Projection(SessionError::new(
                "simulated X11 projection failure",
            )));
        }
        if self.fail_authorization {
            return Err(X11AccessError::Desktop(X11DesktopAccessError::new(
                "simulated X11 authorization failure",
            )));
        }
        let display = match selection {
            X11SessionSelection::Current => 0,
            X11SessionSelection::Display(display) => display,
        };
        let check = X11AccessCheck::from_observations(
            target.clone(),
            x11_projection(display),
            X11AclSnapshot::from_wire(
                1,
                vec![X11AclEntry::from_wire(
                    5,
                    b"localuser\0#1437402088".to_vec(),
                )],
            ),
        );
        Ok(X11SessionPreparation::new(
            target,
            check,
            X11AuthorizationDisposition::Added {
                record_id: "test-grant".into(),
            },
        ))
    }
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

mod launch;
mod prepared;
mod wayland;
