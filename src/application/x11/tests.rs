use super::acl::SERVER_INTERPRETED_FAMILY;
use super::service::MACHINE_LIFECYCLE_RETRY_DELAY;
use super::session::select_session_endpoints;
use super::*;
use crate::application::sessions::{
    JournalSessionHandle, JournalSessionRequest, MappedGuestIdentity, ObservedGuestIdentity,
    ObservedMachineInstance, ObservedNamespaceIdentity, SessionError, SessionPort, SessionService,
    ShellTarget, TerminalSessionHandle, TerminalSessionRequest, ValidatedGuestUserName,
    WaylandPreparationRequest, WaylandSessionContext, X11FilesystemAccess, X11ProjectionContext,
    X11ProjectionProbeRequest,
};
use crate::domain::machine::MachineName;
use crate::domain::wayland::HostWaylandSocket;
use crate::domain::x11::{HostX11Socket, X11SocketRevision};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct SessionProjectionPort {
    session: Arc<SessionService>,
}

#[async_trait::async_trait]
impl X11ProjectionPort for SessionProjectionPort {
    async fn probe(
        &self,
        target: ShellTarget,
        host_socket: HostX11Socket,
    ) -> Result<X11ProjectionContext, SessionError> {
        self.session.test_x11_projection(target, host_socket).await
    }
}

fn session_projection_port(session: Arc<SessionService>) -> Arc<dyn X11ProjectionPort> {
    Arc::new(SessionProjectionPort { session })
}

fn projection(host_uid: u32) -> X11ProjectionContext {
    projection_for_socket(host_socket(0, false, 2), host_uid)
}

fn host_socket(display: u16, alternate: bool, inode: u64) -> HostX11Socket {
    HostX11Socket::from_verified_parts(
        display,
        alternate,
        format!(
            "/tmp/.X11-unix/X{display}{}",
            if alternate { "_" } else { "" }
        )
        .into(),
        format!(
            "/tmp/.X11-unix/X{display}{}",
            if alternate { "_" } else { "" }
        )
        .into(),
        1000,
        1000,
        0o755,
        42,
        1000,
        1000,
        X11SocketRevision {
            device: 1,
            inode,
            ctime_seconds: 3,
            ctime_nanoseconds: 4,
        },
    )
    .unwrap()
}

fn projection_for_socket(socket: HostX11Socket, host_uid: u32) -> X11ProjectionContext {
    let namespace = ObservedNamespaceIdentity::new(1, 2);
    X11ProjectionContext::verified(
        socket,
        "/mnt/host-x11/X0".into(),
        "/tmp/.X11-unix/X0".into(),
        X11FilesystemAccess::observed(true, true),
        MappedGuestIdentity::verified(
            ObservedGuestIdentity::new(1000, 1000),
            host_uid,
            host_uid,
            ObservedMachineInstance::new(42, namespace, namespace),
        ),
    )
}

fn target() -> ShellTarget {
    ShellTarget::new(
        MachineName::new("archlinux").unwrap(),
        ValidatedGuestUserName::new("alice").unwrap(),
    )
}

fn record(phase: X11GrantRecordPhase, host_uid: u32) -> X11GrantRecordEvidence {
    let projection = projection(host_uid);
    X11GrantRecordEvidence {
        record_id: "0123456789abcdef0123456789abcdef".into(),
        phase,
        created_unix_millis: 42,
        caller_uid: 1000,
        boot_id: "11111111-1111-4111-8111-111111111111".into(),
        target: target(),
        identity: projection.identity(),
        display: 0,
        alternate_endpoint: false,
        source: projection.host_socket().source().to_path_buf(),
        canonical_source: projection.host_socket().canonical_path().to_path_buf(),
        socket_revision: projection.host_socket().revision(),
        server_peer: projection.host_socket().peer_identity(),
        server_peer_start_time: 77,
        acl_entry: X11AclEntry::from_wire(
            SERVER_INTERPRETED_FAMILY,
            format!("localuser\0#{host_uid}").into_bytes(),
        ),
    }
}

fn assessed_check(
    phase: Option<X11GrantRecordPhase>,
    acl_present: bool,
    complete: bool,
    boot_id: Option<&str>,
) -> X11AccessCheck {
    let host_uid = 1_437_402_088;
    let entries = acl_present
        .then(|| {
            X11AclEntry::from_wire(
                SERVER_INTERPRETED_FAMILY,
                format!("localuser\0#{host_uid}").into_bytes(),
            )
        })
        .into_iter()
        .collect();
    let records = phase
        .map(|phase| record(phase, host_uid))
        .into_iter()
        .collect();
    X11AccessCheck::from_desktop_observation(
        target(),
        projection(host_uid),
        X11DesktopObservation::new(
            X11AclSnapshot::from_wire(1, entries),
            1000,
            boot_id.map(str::to_owned),
            Some(77),
            X11GrantRecordCatalog {
                records,
                diagnostics: (!complete)
                    .then(|| "record catalog is incomplete".to_owned())
                    .into_iter()
                    .collect(),
                complete,
            },
            Vec::new(),
        ),
    )
}

#[test]
fn acl_keeps_raw_entries_and_only_matches_exact_numeric_local_user() {
    let uid = 1_437_402_088;
    let exact = X11AclEntry::from_wire(5, format!("localuser\0#{uid}").into_bytes());
    let named = X11AclEntry::from_wire(5, b"localuser\0Lumie".to_vec());
    let unknown = X11AclEntry::from_wire(250, vec![0, 255, 128]);
    let snapshot = X11AclSnapshot::from_wire(1, vec![named, unknown.clone(), exact]);
    let check = X11AccessCheck::from_observations(target(), projection(uid), snapshot);

    assert_eq!(
        check.mapped_uid_status(),
        X11MappedUidAclStatus::ExactNumericEntryPresent
    );
    assert_eq!(check.acl().entries()[1], unknown);
}

#[test]
fn disabled_and_unknown_modes_are_not_reported_as_managed_access() {
    let disabled = X11AccessCheck::from_observations(
        target(),
        projection(1000),
        X11AclSnapshot::from_wire(0, vec![]),
    );
    assert_eq!(
        disabled.mapped_uid_status(),
        X11MappedUidAclStatus::AccessControlDisabled
    );

    let unknown = X11AccessCheck::from_observations(
        target(),
        projection(1000),
        X11AclSnapshot::from_wire(
            7,
            vec![X11AclEntry::from_wire(5, b"localuser\0#1000".to_vec())],
        ),
    );
    assert_eq!(
        unknown.mapped_uid_status(),
        X11MappedUidAclStatus::UnknownMode {
            exact_numeric_entry_present: true,
        }
    );
}

#[test]
fn confirmed_record_is_managed_only_with_current_server_and_instance_evidence() {
    let managed = assessed_check(
        Some(X11GrantRecordPhase::ConfirmedAdded),
        true,
        true,
        Some("11111111-1111-4111-8111-111111111111"),
    );
    assert!(matches!(
        managed.grant_assessment().status(),
        X11GrantAssessmentStatus::Managed { record_id }
            if record_id == "0123456789abcdef0123456789abcdef"
    ));
    assert_eq!(
        managed.grant_assessment().history()[0].status(),
        X11GrantHistoryStatus::Managed
    );

    let historical = assessed_check(
        Some(X11GrantRecordPhase::ConfirmedAdded),
        true,
        true,
        Some("22222222-2222-4222-8222-222222222222"),
    );
    assert_eq!(
        historical.grant_assessment().status(),
        &X11GrantAssessmentStatus::Historical
    );
    assert_eq!(
        historical.grant_assessment().history()[0].status(),
        X11GrantHistoryStatus::Historical
    );
}

#[test]
fn current_acl_and_operation_history_remain_separate_facts() {
    let preexisting = assessed_check(None, true, true, None);
    assert_eq!(
        preexisting.grant_assessment().status(),
        &X11GrantAssessmentStatus::PreExisting
    );
    assert!(preexisting.grant_assessment().history().is_empty());

    let absent = assessed_check(
        Some(X11GrantRecordPhase::ConfirmedAdded),
        false,
        true,
        Some("11111111-1111-4111-8111-111111111111"),
    );
    assert_eq!(
        absent.grant_assessment().status(),
        &X11GrantAssessmentStatus::Absent
    );
    assert_eq!(
        absent.grant_assessment().history()[0].status(),
        X11GrantHistoryStatus::Absent
    );

    let revoked = assessed_check(
        Some(X11GrantRecordPhase::Revoked),
        true,
        true,
        Some("11111111-1111-4111-8111-111111111111"),
    );
    assert_eq!(
        revoked.grant_assessment().status(),
        &X11GrantAssessmentStatus::Historical
    );
    assert_eq!(
        revoked.grant_assessment().history()[0].status(),
        X11GrantHistoryStatus::Historical
    );
}

#[test]
fn uncertain_records_never_create_managed_ownership() {
    let pending = assessed_check(Some(X11GrantRecordPhase::Pending), true, true, None);
    assert_eq!(
        pending.grant_assessment().status(),
        &X11GrantAssessmentStatus::OutcomeUnknown
    );
    assert_eq!(
        pending.grant_assessment().history()[0].status(),
        X11GrantHistoryStatus::OutcomeUnknown
    );

    let incomplete = assessed_check(None, true, false, None);
    assert_eq!(
        incomplete.grant_assessment().status(),
        &X11GrantAssessmentStatus::OutcomeUnknown
    );
    assert!(!incomplete.grant_assessment().records_complete());
}

#[test]
fn session_endpoint_selection_is_local_exact_and_standard_first() {
    let catalog = X11EndpointCatalog {
        sockets: vec![
            host_socket(2, true, 23),
            host_socket(1, false, 11),
            host_socket(2, false, 22),
        ],
        preferred_display: Some(2),
        ..Default::default()
    };
    let (display, sockets) =
        select_session_endpoints(catalog.clone(), X11SessionSelection::Current).unwrap();
    assert_eq!(display, 2);
    assert_eq!(sockets.len(), 2);
    assert!(!sockets[0].alternate());
    assert!(sockets[1].alternate());

    let (display, sockets) =
        select_session_endpoints(catalog, X11SessionSelection::Display(1)).unwrap();
    assert_eq!(display, 1);
    assert_eq!(sockets.len(), 1);
    assert!(!sockets[0].alternate());
}

#[test]
fn current_session_selection_requires_a_local_display() {
    let error =
        select_session_endpoints(X11EndpointCatalog::default(), X11SessionSelection::Current)
            .unwrap_err();
    assert!(error.contains("current DISPLAY"));

    let error = select_session_endpoints(
        X11EndpointCatalog {
            sockets: vec![host_socket(1, false, 11)],
            diagnostics: vec!["bounded diagnostic".into()],
            ..Default::default()
        },
        X11SessionSelection::Display(2),
    )
    .unwrap_err();
    assert!(error.contains("available: :1"));
    assert!(error.contains("bounded diagnostic"));
}

struct StaticEndpointPort(X11EndpointCatalog);

#[async_trait::async_trait]
impl X11EndpointDiscoveryPort for StaticEndpointPort {
    async fn discover(&self, configured_sources: &[PathBuf]) -> X11EndpointCatalog {
        assert!(configured_sources.is_empty());
        self.0.clone()
    }
}

struct ProjectionPort {
    probes: Mutex<Vec<bool>>,
}

#[async_trait::async_trait]
impl SessionPort for ProjectionPort {
    async fn discover_host_wayland_sockets(&self) -> Vec<HostWaylandSocket> {
        Vec::new()
    }

    async fn automatic_wayland(
        &self,
        _machine: &MachineName,
    ) -> Result<Option<HostWaylandSocket>, SessionError> {
        Ok(None)
    }

    async fn open_terminal(
        &self,
        _request: TerminalSessionRequest,
    ) -> Result<TerminalSessionHandle, SessionError> {
        panic!("X11 preparation must not open a user terminal")
    }

    async fn prepare_wayland(
        &self,
        _request: WaylandPreparationRequest,
    ) -> Result<WaylandSessionContext, SessionError> {
        panic!("X11 preparation must not prepare Wayland")
    }

    async fn probe_x11_projection(
        &self,
        request: X11ProjectionProbeRequest,
    ) -> Result<X11ProjectionContext, SessionError> {
        self.probes.lock().push(request.host_socket.alternate());
        if !request.host_socket.alternate() {
            return Err(SessionError::new("standard endpoint is not projected"));
        }
        Ok(projection_for_socket(request.host_socket, 1_437_402_088))
    }

    async fn open_journal(
        &self,
        _request: JournalSessionRequest,
    ) -> Result<JournalSessionHandle, SessionError> {
        panic!("X11 preparation must not open a journal")
    }
}

struct RecordingDesktopPort {
    snapshots: AtomicUsize,
    ensures: AtomicUsize,
    reconciles: AtomicUsize,
    purposes: Mutex<Vec<X11AuthorizationPurpose>>,
}

#[async_trait::async_trait]
impl X11DesktopAccessPort for RecordingDesktopPort {
    async fn snapshot(
        &self,
        _socket: &HostX11Socket,
    ) -> Result<X11DesktopObservation, X11DesktopAccessError> {
        self.snapshots.fetch_add(1, Ordering::Relaxed);
        Ok(X11DesktopObservation::new(
            X11AclSnapshot::from_wire(1, Vec::new()),
            1000,
            None,
            None,
            X11GrantRecordCatalog::empty(),
            Vec::new(),
        ))
    }

    async fn ensure(
        &self,
        request: &X11AuthorizationRequest,
    ) -> Result<X11DesktopAuthorization, X11DesktopAccessError> {
        self.ensures.fetch_add(1, Ordering::Relaxed);
        self.purposes.lock().push(request.purpose());
        let uid = request.projection().identity().host_uid();
        Ok(X11DesktopAuthorization::new(
            X11DesktopObservation::new(
                X11AclSnapshot::from_wire(
                    1,
                    vec![X11AclEntry::from_wire(
                        SERVER_INTERPRETED_FAMILY,
                        format!("localuser\0#{uid}").into_bytes(),
                    )],
                ),
                1000,
                None,
                None,
                X11GrantRecordCatalog::empty(),
                Vec::new(),
            ),
            X11AuthorizationDisposition::PreExisting,
        ))
    }

    async fn revoke(
        &self,
        _request: &X11RevokeRequest,
    ) -> Result<X11DesktopRevocation, X11DesktopAccessError> {
        panic!("X11 preparation must not revoke access")
    }

    async fn reconcile(
        &self,
        socket: &HostX11Socket,
    ) -> Result<X11ReconcileReport, X11DesktopAccessError> {
        self.reconciles.fetch_add(1, Ordering::Relaxed);
        Ok(X11ReconcileReport::new(
            socket.display(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
    }
}

struct SequencedDesktopPort {
    reports: Mutex<VecDeque<X11ReconcileReport>>,
    reconciles: AtomicUsize,
}

#[async_trait::async_trait]
impl X11DesktopAccessPort for SequencedDesktopPort {
    async fn snapshot(
        &self,
        _socket: &HostX11Socket,
    ) -> Result<X11DesktopObservation, X11DesktopAccessError> {
        panic!("lifecycle retry test must not query desktop snapshots")
    }

    async fn ensure(
        &self,
        _request: &X11AuthorizationRequest,
    ) -> Result<X11DesktopAuthorization, X11DesktopAccessError> {
        panic!("lifecycle retry test must not authorize access")
    }

    async fn revoke(
        &self,
        _request: &X11RevokeRequest,
    ) -> Result<X11DesktopRevocation, X11DesktopAccessError> {
        panic!("lifecycle retry test must not revoke through the session API")
    }

    async fn reconcile(
        &self,
        _socket: &HostX11Socket,
    ) -> Result<X11ReconcileReport, X11DesktopAccessError> {
        self.reconciles.fetch_add(1, Ordering::Relaxed);
        self.reports
            .lock()
            .pop_front()
            .ok_or_else(|| X11DesktopAccessError::new("missing lifecycle test report"))
    }
}

#[tokio::test]
async fn explicit_session_tries_projected_endpoint_before_one_atomic_acl_ensure() {
    let projection_port = Arc::new(ProjectionPort {
        probes: Mutex::new(Vec::new()),
    });
    let sessions = Arc::new(SessionService::new(projection_port.clone()));
    let endpoints = Arc::new(X11EndpointDiscoveryService::new(Arc::new(
        StaticEndpointPort(X11EndpointCatalog {
            sockets: vec![host_socket(0, false, 2), host_socket(0, true, 3)],
            preferred_display: Some(0),
            ..Default::default()
        }),
    )));
    let desktop = Arc::new(RecordingDesktopPort {
        snapshots: AtomicUsize::new(0),
        ensures: AtomicUsize::new(0),
        reconciles: AtomicUsize::new(0),
        purposes: Mutex::new(Vec::new()),
    });
    let service = X11AccessService::new(
        session_projection_port(sessions),
        endpoints,
        desktop.clone(),
    );

    let prepared = service
        .prepare_session(target(), X11SessionSelection::Current)
        .await
        .unwrap();

    assert_eq!(prepared.context().display(), 0);
    assert!(prepared.context().projection().host_socket().alternate());
    assert_eq!(*projection_port.probes.lock(), [false, true]);
    assert_eq!(desktop.snapshots.load(Ordering::Relaxed), 0);
    assert_eq!(desktop.ensures.load(Ordering::Relaxed), 1);
    assert_eq!(
        *desktop.purposes.lock(),
        [X11AuthorizationPurpose::ExplicitSession]
    );
}

#[tokio::test]
async fn lifecycle_reconcile_routes_each_discovered_local_endpoint() {
    let sessions = Arc::new(SessionService::new(Arc::new(ProjectionPort {
        probes: Mutex::new(Vec::new()),
    })));
    let endpoints = Arc::new(X11EndpointDiscoveryService::new(Arc::new(
        StaticEndpointPort(X11EndpointCatalog {
            sockets: vec![host_socket(0, false, 2), host_socket(1, false, 4)],
            preferred_display: Some(0),
            ..Default::default()
        }),
    )));
    let desktop = Arc::new(RecordingDesktopPort {
        snapshots: AtomicUsize::new(0),
        ensures: AtomicUsize::new(0),
        reconciles: AtomicUsize::new(0),
        purposes: Mutex::new(Vec::new()),
    });
    let service = X11AccessService::new(
        session_projection_port(sessions),
        endpoints,
        desktop.clone(),
    );

    let reports = service.reconcile().await.unwrap();

    assert_eq!(
        reports
            .iter()
            .map(X11ReconcileReport::display)
            .collect::<Vec<_>>(),
        [0, 1]
    );
    assert_eq!(desktop.reconciles.load(Ordering::Relaxed), 2);
}

#[tokio::test(start_paused = true)]
async fn lifecycle_event_reconcile_covers_stop_pending_and_cleanup_ready() {
    let sessions = Arc::new(SessionService::new(Arc::new(ProjectionPort {
        probes: Mutex::new(Vec::new()),
    })));
    let endpoints = Arc::new(X11EndpointDiscoveryService::new(Arc::new(
        StaticEndpointPort(X11EndpointCatalog {
            sockets: vec![host_socket(0, false, 2)],
            preferred_display: Some(0),
            ..Default::default()
        }),
    )));
    let desktop = Arc::new(SequencedDesktopPort {
        reports: Mutex::new(VecDeque::from([
            X11ReconcileReport::new(0, Vec::new(), Vec::new(), Vec::new()),
            X11ReconcileReport::new(0, Vec::new(), vec!["pending".into()], Vec::new()),
            X11ReconcileReport::new(0, vec!["revoked".into()], Vec::new(), Vec::new()),
        ])),
        reconciles: AtomicUsize::new(0),
    });
    let service = X11AccessService::new(
        session_projection_port(sessions),
        endpoints,
        desktop.clone(),
    );

    let task = tokio::spawn(async move { service.reconcile_after_machine_event().await });
    tokio::task::yield_now().await;
    tokio::time::advance(MACHINE_LIFECYCLE_RETRY_DELAY).await;
    tokio::time::advance(MACHINE_LIFECYCLE_RETRY_DELAY).await;
    let reports = task.await.unwrap().unwrap();

    assert_eq!(desktop.reconciles.load(Ordering::Relaxed), 3);
    assert_eq!(reports[0].revoked_record_ids(), ["revoked"]);
    assert!(reports[0].pending_record_ids().is_empty());
}
