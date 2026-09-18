use std::path::PathBuf;
use std::sync::Arc;

use crate::application::sessions::{
    MappedGuestIdentity, SessionError, SessionService, ShellTarget, X11ProjectionContext,
};
use crate::domain::x11::{HostX11Socket, X11SocketRevision};

const SERVER_INTERPRETED_FAMILY: u8 = 5;
const LOCAL_USER_KIND: &[u8] = b"localuser";

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct X11EndpointCatalog {
    pub sockets: Vec<HostX11Socket>,
    pub preferred_display: Option<u16>,
    pub diagnostics: Vec<String>,
}

#[async_trait::async_trait]
pub(crate) trait X11EndpointDiscoveryPort: Send + Sync {
    async fn discover(&self) -> X11EndpointCatalog;
}

pub(crate) struct X11EndpointDiscoveryService {
    port: Arc<dyn X11EndpointDiscoveryPort>,
}

impl X11EndpointDiscoveryService {
    pub(crate) fn new(port: Arc<dyn X11EndpointDiscoveryPort>) -> Self {
        Self { port }
    }

    pub async fn discover(&self) -> X11EndpointCatalog {
        self.port.discover().await
    }
}

/// Access-control mode reported by one X server's `ListHosts` reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum X11AccessControlMode {
    Enabled,
    Disabled,
    Unknown(u8),
}

impl X11AccessControlMode {
    pub(crate) const fn from_wire(value: u8) -> Self {
        match value {
            0 => Self::Disabled,
            1 => Self::Enabled,
            value => Self::Unknown(value),
        }
    }
}

/// One exact X11 host ACL entry. Raw protocol bytes are retained so a future
/// mutation path can only remove the representation that Lasper actually saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11AclEntry {
    family: u8,
    address: Vec<u8>,
}

impl X11AclEntry {
    pub(crate) fn from_wire(family: u8, address: Vec<u8>) -> Self {
        Self { family, address }
    }

    pub fn server_interpreted(&self) -> Option<(&str, &str)> {
        if self.family != SERVER_INTERPRETED_FAMILY {
            return None;
        }
        let separator = self.address.iter().position(|byte| *byte == 0)?;
        let kind = std::str::from_utf8(&self.address[..separator]).ok()?;
        let value = std::str::from_utf8(&self.address[separator + 1..]).ok()?;
        Some((kind, value))
    }

    fn is_numeric_local_user(&self, uid: u32) -> bool {
        let Some((kind, value)) = self.server_interpreted() else {
            return false;
        };
        kind.as_bytes() == LOCAL_USER_KIND && value == format!("#{uid}")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11AclSnapshot {
    mode: X11AccessControlMode,
    entries: Vec<X11AclEntry>,
}

impl X11AclSnapshot {
    pub(crate) fn from_wire(mode: u8, entries: Vec<X11AclEntry>) -> Self {
        Self {
            mode: X11AccessControlMode::from_wire(mode),
            entries,
        }
    }

    pub const fn mode(&self) -> X11AccessControlMode {
        self.mode
    }

    pub fn entries(&self) -> &[X11AclEntry] {
        &self.entries
    }

    pub(crate) fn has_numeric_local_user(&self, uid: u32) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.is_numeric_local_user(uid))
    }

    fn contains(&self, entry: &X11AclEntry) -> bool {
        self.entries.contains(entry)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum X11GrantRecordPhase {
    Pending,
    ConfirmedAdded,
    OutcomeUnknown { reason: String },
}

/// Validated evidence loaded from one user-runtime grant record. The adapter
/// owns the on-disk format; the application owns how it relates to fresh ACL,
/// machine-instance, and server observations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct X11GrantRecordEvidence {
    pub(crate) record_id: String,
    pub(crate) phase: X11GrantRecordPhase,
    pub(crate) created_unix_millis: u64,
    pub(crate) caller_uid: u32,
    pub(crate) boot_id: String,
    pub(crate) target: ShellTarget,
    pub(crate) identity: MappedGuestIdentity,
    pub(crate) display: u16,
    pub(crate) alternate_endpoint: bool,
    pub(crate) source: PathBuf,
    pub(crate) canonical_source: PathBuf,
    pub(crate) socket_revision: X11SocketRevision,
    pub(crate) server_peer: (u32, u32, u32),
    pub(crate) server_peer_start_time: u64,
    pub(crate) acl_entry: X11AclEntry,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct X11GrantRecordCatalog {
    pub(crate) records: Vec<X11GrantRecordEvidence>,
    pub(crate) diagnostics: Vec<String>,
    pub(crate) complete: bool,
}

impl X11GrantRecordCatalog {
    pub(crate) fn empty() -> Self {
        Self {
            records: Vec::new(),
            diagnostics: Vec::new(),
            complete: true,
        }
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self {
            records: Vec::new(),
            diagnostics: vec![message.into()],
            complete: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct X11DesktopObservation {
    acl: X11AclSnapshot,
    caller_uid: u32,
    host_boot_id: Option<String>,
    server_peer_start_time: Option<u64>,
    records: X11GrantRecordCatalog,
    diagnostics: Vec<String>,
}

impl X11DesktopObservation {
    pub(crate) fn new(
        acl: X11AclSnapshot,
        caller_uid: u32,
        host_boot_id: Option<String>,
        server_peer_start_time: Option<u64>,
        records: X11GrantRecordCatalog,
        diagnostics: Vec<String>,
    ) -> Self {
        Self {
            acl,
            caller_uid,
            host_boot_id,
            server_peer_start_time,
            records,
            diagnostics,
        }
    }

    fn acl(&self) -> &X11AclSnapshot {
        &self.acl
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum X11GrantHistoryStatus {
    Managed,
    Historical,
    Absent,
    OutcomeUnknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11GrantHistoryEntry {
    record_id: String,
    created_unix_millis: u64,
    host_uid: u32,
    status: X11GrantHistoryStatus,
    detail: String,
}

impl X11GrantHistoryEntry {
    pub fn record_id(&self) -> &str {
        &self.record_id
    }

    pub const fn host_uid(&self) -> u32 {
        self.host_uid
    }

    pub const fn status(&self) -> X11GrantHistoryStatus {
        self.status
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum X11GrantAssessmentStatus {
    AccessControlDisabled,
    Managed { record_id: String },
    PreExisting,
    Historical,
    Absent,
    OutcomeUnknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11GrantAssessment {
    status: X11GrantAssessmentStatus,
    history: Vec<X11GrantHistoryEntry>,
    diagnostics: Vec<String>,
    records_complete: bool,
}

impl X11GrantAssessment {
    pub fn status(&self) -> &X11GrantAssessmentStatus {
        &self.status
    }

    pub fn history(&self) -> &[X11GrantHistoryEntry] {
        &self.history
    }

    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    pub const fn records_complete(&self) -> bool {
        self.records_complete
    }
}

/// What the queried ACL proves about the mapped host UID. This deliberately
/// does not infer access from cookies or similarly named ACL entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum X11MappedUidAclStatus {
    AccessControlDisabled,
    ExactNumericEntryPresent,
    ExactNumericEntryAbsent,
    UnknownMode { exact_numeric_entry_present: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11AccessCheck {
    projection: X11ProjectionContext,
    acl: X11AclSnapshot,
    mapped_uid_status: X11MappedUidAclStatus,
    grant_assessment: X11GrantAssessment,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct X11AuthorizationRequest {
    target: ShellTarget,
    projection: X11ProjectionContext,
}

impl X11AuthorizationRequest {
    pub(crate) fn new(target: ShellTarget, projection: X11ProjectionContext) -> Self {
        Self { target, projection }
    }

    pub(crate) fn target(&self) -> &ShellTarget {
        &self.target
    }

    pub(crate) fn projection(&self) -> &X11ProjectionContext {
        &self.projection
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum X11AuthorizationDisposition {
    AccessControlDisabled,
    PreExisting,
    Added { record_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct X11DesktopAuthorization {
    observation: X11DesktopObservation,
    disposition: X11AuthorizationDisposition,
}

impl X11DesktopAuthorization {
    pub(crate) fn new(
        observation: X11DesktopObservation,
        disposition: X11AuthorizationDisposition,
    ) -> Self {
        Self {
            observation,
            disposition,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11Authorization {
    check: X11AccessCheck,
    disposition: X11AuthorizationDisposition,
}

impl X11Authorization {
    pub fn check(&self) -> &X11AccessCheck {
        &self.check
    }

    pub fn disposition(&self) -> &X11AuthorizationDisposition {
        &self.disposition
    }
}

impl X11AccessCheck {
    #[cfg(test)]
    pub(crate) fn from_observations(
        target: ShellTarget,
        projection: X11ProjectionContext,
        acl: X11AclSnapshot,
    ) -> Self {
        Self::from_desktop_observation(
            target,
            projection,
            X11DesktopObservation::new(
                acl,
                uzers::get_effective_uid(),
                None,
                None,
                X11GrantRecordCatalog::empty(),
                Vec::new(),
            ),
        )
    }

    fn from_desktop_observation(
        target: ShellTarget,
        projection: X11ProjectionContext,
        observation: X11DesktopObservation,
    ) -> Self {
        let acl = observation.acl().clone();
        let exact_numeric_entry_present =
            acl.has_numeric_local_user(projection.identity().host_uid());
        let mapped_uid_status = match acl.mode() {
            X11AccessControlMode::Disabled => X11MappedUidAclStatus::AccessControlDisabled,
            X11AccessControlMode::Enabled if exact_numeric_entry_present => {
                X11MappedUidAclStatus::ExactNumericEntryPresent
            }
            X11AccessControlMode::Enabled => X11MappedUidAclStatus::ExactNumericEntryAbsent,
            X11AccessControlMode::Unknown(_) => X11MappedUidAclStatus::UnknownMode {
                exact_numeric_entry_present,
            },
        };
        let grant_assessment = assess_grants(&target, &projection, &observation);
        Self {
            projection,
            acl,
            mapped_uid_status,
            grant_assessment,
        }
    }

    pub fn projection(&self) -> &X11ProjectionContext {
        &self.projection
    }

    pub fn acl(&self) -> &X11AclSnapshot {
        &self.acl
    }

    pub const fn mapped_uid_status(&self) -> X11MappedUidAclStatus {
        self.mapped_uid_status
    }

    pub fn grant_assessment(&self) -> &X11GrantAssessment {
        &self.grant_assessment
    }
}

fn assess_grants(
    target: &ShellTarget,
    projection: &X11ProjectionContext,
    observation: &X11DesktopObservation,
) -> X11GrantAssessment {
    let display = projection.host_socket().display();
    let desired_entry = X11AclEntry::from_wire(
        SERVER_INTERPRETED_FAMILY,
        format!("localuser\0#{}", projection.identity().host_uid()).into_bytes(),
    );
    let mut matching_current_key = Vec::new();
    let mut history = observation
        .records
        .records
        .iter()
        .filter(|record| record.target == *target && record.display == display)
        .map(|record| {
            let entry_present = observation.acl.contains(&record.acl_entry);
            let current_key = record.acl_entry == desired_entry;
            let (status, detail) = match &record.phase {
                X11GrantRecordPhase::Pending => (
                    X11GrantHistoryStatus::OutcomeUnknown,
                    "authorization was recorded as pending and never confirmed".to_owned(),
                ),
                X11GrantRecordPhase::OutcomeUnknown { reason } => (
                    X11GrantHistoryStatus::OutcomeUnknown,
                    format!("authorization outcome is unknown: {reason}"),
                ),
                X11GrantRecordPhase::ConfirmedAdded if !entry_present => (
                    X11GrantHistoryStatus::Absent,
                    "the confirmed exact ACL entry is no longer present".to_owned(),
                ),
                X11GrantRecordPhase::ConfirmedAdded => {
                    let mut differences = Vec::new();
                    let endpoint_changed = record.alternate_endpoint
                        != projection.host_socket().alternate()
                        || record.source != projection.host_socket().source()
                        || record.canonical_source != projection.host_socket().canonical_path()
                        || record.socket_revision != projection.host_socket().revision();
                    if record.caller_uid != observation.caller_uid {
                        differences.push("calling user changed");
                    }
                    if observation.host_boot_id.as_deref() != Some(record.boot_id.as_str())
                        || observation.server_peer_start_time
                            != Some(record.server_peer_start_time)
                        || projection.host_socket().peer_identity() != record.server_peer
                    {
                        differences.push("X server continuity is not confirmed");
                    }
                    if record.identity != projection.identity() {
                        differences.push("machine instance or mapped identity changed");
                    }
                    if current_key && differences.is_empty() {
                        (
                            X11GrantHistoryStatus::Managed,
                            if endpoint_changed {
                                "confirmed for the current machine instance and X server; the selected listener changed"
                            } else {
                                "confirmed for the current machine instance and X server"
                            }
                            .to_owned(),
                        )
                    } else {
                        if !current_key {
                            differences.push("the mapped host UID changed");
                        }
                        (
                            X11GrantHistoryStatus::Historical,
                            differences.join("; "),
                        )
                    }
                }
            };
            if current_key {
                matching_current_key.push(status);
            }
            X11GrantHistoryEntry {
                record_id: record.record_id.clone(),
                created_unix_millis: record.created_unix_millis,
                host_uid: record.identity.host_uid(),
                status,
                detail,
            }
        })
        .collect::<Vec<_>>();
    history.sort_by(|left, right| {
        right
            .created_unix_millis
            .cmp(&left.created_unix_millis)
            .then_with(|| right.record_id.cmp(&left.record_id))
    });

    let status = match observation.acl.mode() {
        X11AccessControlMode::Disabled => X11GrantAssessmentStatus::AccessControlDisabled,
        X11AccessControlMode::Unknown(_) => X11GrantAssessmentStatus::OutcomeUnknown,
        X11AccessControlMode::Enabled
            if !observation
                .acl
                .has_numeric_local_user(projection.identity().host_uid()) =>
        {
            X11GrantAssessmentStatus::Absent
        }
        X11AccessControlMode::Enabled => {
            let managed = history.iter().find(|entry| {
                entry.status == X11GrantHistoryStatus::Managed
                    && entry.host_uid == projection.identity().host_uid()
            });
            if let Some(managed) = managed {
                X11GrantAssessmentStatus::Managed {
                    record_id: managed.record_id.clone(),
                }
            } else if matching_current_key.contains(&X11GrantHistoryStatus::OutcomeUnknown)
                || !observation.records.complete
            {
                X11GrantAssessmentStatus::OutcomeUnknown
            } else if matching_current_key.contains(&X11GrantHistoryStatus::Historical) {
                X11GrantAssessmentStatus::Historical
            } else {
                X11GrantAssessmentStatus::PreExisting
            }
        }
    };
    let diagnostics = observation
        .records
        .diagnostics
        .iter()
        .chain(&observation.diagnostics)
        .cloned()
        .collect();
    X11GrantAssessment {
        status,
        history,
        diagnostics,
        records_complete: observation.records.complete,
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct X11DesktopAccessError {
    message: String,
}

impl X11DesktopAccessError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum X11AccessError {
    #[error("{0}")]
    Projection(#[source] SessionError),
    #[error("X11 desktop access operation failed: {0}")]
    Desktop(#[source] X11DesktopAccessError),
}

#[async_trait::async_trait]
pub(crate) trait X11DesktopAccessPort: Send + Sync {
    async fn snapshot(
        &self,
        socket: &HostX11Socket,
    ) -> Result<X11DesktopObservation, X11DesktopAccessError>;

    async fn ensure(
        &self,
        request: &X11AuthorizationRequest,
    ) -> Result<X11DesktopAuthorization, X11DesktopAccessError>;
}

/// Combines runtime namespace evidence with a caller-owned X server query.
/// The desktop query always stays in the invoking user's process, even when
/// the projection half is routed through the elevated daemon.
pub struct X11AccessService {
    sessions: Arc<SessionService>,
    desktop: Arc<dyn X11DesktopAccessPort>,
}

impl X11AccessService {
    pub(crate) fn new(
        sessions: Arc<SessionService>,
        desktop: Arc<dyn X11DesktopAccessPort>,
    ) -> Self {
        Self { sessions, desktop }
    }

    pub async fn check(
        &self,
        target: ShellTarget,
        socket: HostX11Socket,
    ) -> Result<X11AccessCheck, X11AccessError> {
        let (projection, observation) = tokio::join!(
            self.sessions
                .test_x11_projection(target.clone(), socket.clone()),
            self.desktop.snapshot(&socket),
        );
        let projection = projection.map_err(X11AccessError::Projection)?;
        let observation = observation.map_err(X11AccessError::Desktop)?;
        Ok(X11AccessCheck::from_desktop_observation(
            target,
            projection,
            observation,
        ))
    }

    pub async fn authorize(
        &self,
        target: ShellTarget,
        socket: HostX11Socket,
    ) -> Result<X11Authorization, X11AccessError> {
        let projection = self
            .sessions
            .test_x11_projection(target.clone(), socket)
            .await
            .map_err(X11AccessError::Projection)?;
        let request = X11AuthorizationRequest::new(target.clone(), projection.clone());
        let desktop = self
            .desktop
            .ensure(&request)
            .await
            .map_err(X11AccessError::Desktop)?;
        Ok(X11Authorization {
            check: X11AccessCheck::from_desktop_observation(
                target,
                projection,
                desktop.observation,
            ),
            disposition: desktop.disposition,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::sessions::{
        MappedGuestIdentity, ObservedGuestIdentity, ObservedMachineInstance,
        ObservedNamespaceIdentity, ValidatedGuestUserName,
    };
    use crate::domain::machine::MachineName;
    use crate::domain::x11::X11SocketRevision;

    fn projection(host_uid: u32) -> X11ProjectionContext {
        let namespace = ObservedNamespaceIdentity::new(1, 2);
        X11ProjectionContext::verified(
            HostX11Socket::from_verified_parts(
                0,
                false,
                "/tmp/.X11-unix/X0".into(),
                "/tmp/.X11-unix/X0".into(),
                1000,
                1000,
                0o777,
                42,
                1000,
                1000,
                X11SocketRevision {
                    device: 1,
                    inode: 2,
                    ctime_seconds: 3,
                    ctime_nanoseconds: 4,
                },
            )
            .unwrap(),
            "/mnt/host-x11/X0".into(),
            "/tmp/.X11-unix/X0".into(),
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
}
