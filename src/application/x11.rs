use std::path::PathBuf;
use std::sync::Arc;

use crate::application::sessions::{
    MappedGuestIdentity, SessionError, SessionService, ShellTarget, X11ProjectionContext,
    X11SessionContext,
};
use crate::domain::x11::{HostX11Socket, X11SocketRevision};

const SERVER_INTERPRETED_FAMILY: u8 = 5;
const LOCAL_USER_KIND: &[u8] = b"localuser";

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct X11EndpointCatalog {
    pub sockets: Vec<HostX11Socket>,
    /// Observations of configured sources, including sources that could not
    /// become authenticated endpoints. Absence from `sockets` is not absence
    /// from the filesystem.
    pub sources: Vec<X11SourceObservation>,
    pub preferred_display: Option<u16>,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct X11SourceObservation {
    pub source: PathBuf,
    pub state: X11SourceState,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum X11SourceState {
    Observed,
    Missing,
    Invalid(String),
    Unverified(String),
}

#[async_trait::async_trait]
pub(crate) trait X11EndpointDiscoveryPort: Send + Sync {
    async fn discover(&self, configured_sources: &[PathBuf]) -> X11EndpointCatalog;
}

pub(crate) struct X11EndpointDiscoveryService {
    port: Arc<dyn X11EndpointDiscoveryPort>,
}

impl X11EndpointDiscoveryService {
    pub(crate) fn new(port: Arc<dyn X11EndpointDiscoveryPort>) -> Self {
        Self { port }
    }

    pub async fn discover(&self, configured_sources: &[PathBuf]) -> X11EndpointCatalog {
        self.port.discover(configured_sources).await
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

    pub(crate) const fn family(&self) -> u8 {
        self.family
    }

    pub(crate) fn address(&self) -> &[u8] {
        &self.address
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

    pub(crate) fn contains(&self, entry: &X11AclEntry) -> bool {
        self.entries.contains(entry)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum X11GrantRecordPhase {
    Pending,
    ConfirmedAdded,
    Revoked,
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
    purpose: X11AuthorizationPurpose,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum X11AuthorizationPurpose {
    Manual,
    ExplicitSession,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct X11RevokeRequest {
    target: ShellTarget,
    projection: X11ProjectionContext,
    record_id: String,
}

impl X11RevokeRequest {
    pub(crate) fn new(
        target: ShellTarget,
        projection: X11ProjectionContext,
        record_id: String,
    ) -> Self {
        Self {
            target,
            projection,
            record_id,
        }
    }

    pub(crate) fn target(&self) -> &ShellTarget {
        &self.target
    }

    pub(crate) fn projection(&self) -> &X11ProjectionContext {
        &self.projection
    }

    pub(crate) fn record_id(&self) -> &str {
        &self.record_id
    }
}

impl X11AuthorizationRequest {
    pub(crate) fn new(target: ShellTarget, projection: X11ProjectionContext) -> Self {
        Self {
            target,
            projection,
            purpose: X11AuthorizationPurpose::Manual,
        }
    }

    pub(crate) fn for_explicit_session(
        target: ShellTarget,
        projection: X11ProjectionContext,
    ) -> Self {
        Self {
            target,
            projection,
            purpose: X11AuthorizationPurpose::ExplicitSession,
        }
    }

    pub(crate) fn target(&self) -> &ShellTarget {
        &self.target
    }

    pub(crate) fn projection(&self) -> &X11ProjectionContext {
        &self.projection
    }

    pub(crate) const fn purpose(&self) -> X11AuthorizationPurpose {
        self.purpose
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum X11AuthorizationDisposition {
    AccessControlDisabled,
    PreExisting,
    Added { record_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum X11RevocationDisposition {
    Revoked { record_id: String },
    AlreadyAbsent { record_id: String },
}

/// Result of one bounded machine-lifecycle reconcile pass for one live X11
/// endpoint. A pending ID means the machine registration has disappeared but
/// the grace/revalidation state is not yet sufficient to revoke its ACL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct X11ReconcileReport {
    display: u16,
    revoked_record_ids: Vec<String>,
    pending_record_ids: Vec<String>,
    diagnostics: Vec<String>,
}

impl X11ReconcileReport {
    pub(crate) fn new(
        display: u16,
        revoked_record_ids: Vec<String>,
        pending_record_ids: Vec<String>,
        diagnostics: Vec<String>,
    ) -> Self {
        Self {
            display,
            revoked_record_ids,
            pending_record_ids,
            diagnostics,
        }
    }

    pub(crate) const fn display(&self) -> u16 {
        self.display
    }

    pub(crate) fn revoked_record_ids(&self) -> &[String] {
        &self.revoked_record_ids
    }

    pub(crate) fn pending_record_ids(&self) -> &[String] {
        &self.pending_record_ids
    }

    pub(crate) fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }
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
pub(crate) struct X11DesktopRevocation {
    observation: X11DesktopObservation,
    disposition: X11RevocationDisposition,
}

impl X11DesktopRevocation {
    pub(crate) fn new(
        observation: X11DesktopObservation,
        disposition: X11RevocationDisposition,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum X11SessionSelection {
    Current,
    Display(u16),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11SessionPreview {
    target: ShellTarget,
    check: X11AccessCheck,
}

impl X11SessionPreview {
    pub fn target(&self) -> &ShellTarget {
        &self.target
    }

    pub fn check(&self) -> &X11AccessCheck {
        &self.check
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11SessionPreparation {
    context: X11SessionContext,
    check: X11AccessCheck,
    disposition: X11AuthorizationDisposition,
}

impl X11SessionPreparation {
    pub fn context(&self) -> &X11SessionContext {
        &self.context
    }

    pub fn check(&self) -> &X11AccessCheck {
        &self.check
    }

    pub fn disposition(&self) -> &X11AuthorizationDisposition {
        &self.disposition
    }

    pub fn into_context(self) -> X11SessionContext {
        self.context
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11Revocation {
    check: X11AccessCheck,
    disposition: X11RevocationDisposition,
}

impl X11Revocation {
    pub fn check(&self) -> &X11AccessCheck {
        &self.check
    }

    pub fn disposition(&self) -> &X11RevocationDisposition {
        &self.disposition
    }
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
                X11GrantRecordPhase::Revoked if !entry_present => (
                    X11GrantHistoryStatus::Absent,
                    "the exact ACL entry was revoked by Lasper and is absent".to_owned(),
                ),
                X11GrantRecordPhase::Revoked => (
                    X11GrantHistoryStatus::Historical,
                    "the record was revoked, but the exact ACL entry is present again".to_owned(),
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
                    if endpoint_changed {
                        differences.push("selected X11 endpoint changed");
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
    #[error("X11 display selection failed: {0}")]
    Selection(String),
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

    async fn revoke(
        &self,
        request: &X11RevokeRequest,
    ) -> Result<X11DesktopRevocation, X11DesktopAccessError>;

    async fn reconcile(
        &self,
        _socket: &HostX11Socket,
    ) -> Result<X11ReconcileReport, X11DesktopAccessError> {
        Err(X11DesktopAccessError::new(
            "X11 lifecycle reconcile is not available on this desktop access adapter",
        ))
    }
}

/// Combines runtime namespace evidence with a caller-owned X server query.
/// The desktop query always stays in the invoking user's process, even when
/// the projection half is routed through the elevated daemon.
pub struct X11AccessService {
    sessions: Arc<SessionService>,
    endpoints: Arc<X11EndpointDiscoveryService>,
    desktop: Arc<dyn X11DesktopAccessPort>,
}

impl X11AccessService {
    pub(crate) fn new(
        sessions: Arc<SessionService>,
        endpoints: Arc<X11EndpointDiscoveryService>,
        desktop: Arc<dyn X11DesktopAccessPort>,
    ) -> Self {
        Self {
            sessions,
            endpoints,
            desktop,
        }
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

    /// Inspect the exact projection and desktop ACL that an explicit Host X11
    /// session would use, without changing the X server.  Interactive callers
    /// use this evidence to present an informed confirmation before invoking
    /// [`Self::prepare_previewed_session`]. The latter deliberately resolves
    /// and probes the endpoint again after the human pause rather than treating
    /// this preview as authorization evidence.
    pub async fn preview_session(
        &self,
        target: ShellTarget,
        selection: X11SessionSelection,
    ) -> Result<X11SessionPreview, X11AccessError> {
        let projection = self
            .resolve_session_projection(target.clone(), selection)
            .await?;
        let observation = self
            .desktop
            .snapshot(projection.host_socket())
            .await
            .map_err(X11AccessError::Desktop)?;
        let check =
            X11AccessCheck::from_desktop_observation(target.clone(), projection, observation);
        Ok(X11SessionPreview { target, check })
    }

    /// Prepare one explicitly requested Host X11 session. Endpoint discovery
    /// and projection probing are read-only; the desktop ACL is considered
    /// only after one startup-configured projection has been proven usable.
    pub async fn prepare_session(
        &self,
        target: ShellTarget,
        selection: X11SessionSelection,
    ) -> Result<X11SessionPreparation, X11AccessError> {
        let projection = self
            .resolve_session_projection(target.clone(), selection)
            .await?;

        self.prepare_resolved_session(target, projection).await
    }

    /// Complete an interactive request after displaying a read-only preview.
    /// The target, display, machine instance, endpoint and mapped identity must
    /// still be exactly the evidence the user confirmed. The desktop adapter
    /// performs its own fresh ACL query under the operation lock afterwards.
    pub async fn prepare_previewed_session(
        &self,
        preview: X11SessionPreview,
    ) -> Result<X11SessionPreparation, X11AccessError> {
        let display = preview.check.projection().host_socket().display();
        let projection = self
            .resolve_session_projection(
                preview.target.clone(),
                X11SessionSelection::Display(display),
            )
            .await?;
        if projection != *preview.check.projection() {
            return Err(X11AccessError::Projection(SessionError::with_hint(
                "the X11 projection changed while confirmation was pending",
                "Review the current Host X11 details and confirm the request again.",
            )));
        }

        self.prepare_resolved_session(preview.target, projection)
            .await
    }

    async fn prepare_resolved_session(
        &self,
        target: ShellTarget,
        projection: X11ProjectionContext,
    ) -> Result<X11SessionPreparation, X11AccessError> {
        let request =
            X11AuthorizationRequest::for_explicit_session(target.clone(), projection.clone());
        let desktop = self
            .desktop
            .ensure(&request)
            .await
            .map_err(X11AccessError::Desktop)?;
        let check = X11AccessCheck::from_desktop_observation(
            target.clone(),
            projection.clone(),
            desktop.observation,
        );
        Ok(X11SessionPreparation {
            context: X11SessionContext::prepared(target, projection),
            check,
            disposition: desktop.disposition,
        })
    }

    async fn resolve_session_projection(
        &self,
        target: ShellTarget,
        selection: X11SessionSelection,
    ) -> Result<X11ProjectionContext, X11AccessError> {
        let catalog = self.endpoints.discover(&[]).await;
        let (display, candidates) =
            select_session_endpoints(catalog, selection).map_err(X11AccessError::Selection)?;

        let mut failures = Vec::new();
        let mut selected = None;
        for socket in candidates {
            match self
                .sessions
                .test_x11_projection(target.clone(), socket.clone())
                .await
            {
                Ok(projection) => {
                    selected = Some(projection);
                    break;
                }
                Err(error) => failures.push(format!("{}: {error}", socket.source().display())),
            }
        }
        selected.ok_or_else(|| {
            X11AccessError::Projection(SessionError::with_hint(
                format!(
                    "no usable startup-configured projection was found for X11 display :{display}: {}",
                    failures.join("; ")
                ),
                "Configure the selected X11 socket while the machine is stopped, then start or restart it.",
            ))
        })
    }

    pub async fn revoke(
        &self,
        target: ShellTarget,
        socket: HostX11Socket,
        record_id: String,
    ) -> Result<X11Revocation, X11AccessError> {
        let projection = self
            .sessions
            .test_x11_projection(target.clone(), socket)
            .await
            .map_err(X11AccessError::Projection)?;
        let request = X11RevokeRequest::new(target.clone(), projection.clone(), record_id);
        let desktop = self
            .desktop
            .revoke(&request)
            .await
            .map_err(X11AccessError::Desktop)?;
        Ok(X11Revocation {
            check: X11AccessCheck::from_desktop_observation(
                target,
                projection,
                desktop.observation,
            ),
            disposition: desktop.disposition,
        })
    }

    /// Run one bounded, system-scope machine claim reconcile pass. The
    /// endpoint catalog is discovered in the invoking desktop process; the
    /// adapter only receives verified live X11 endpoints and never receives a
    /// target user-scope machine selector. Endpoint-local failures are
    /// returned as diagnostics so one broken display cannot prevent cleanup on
    /// another display.
    pub(crate) async fn reconcile(&self) -> Result<Vec<X11ReconcileReport>, X11AccessError> {
        let catalog = self.endpoints.discover(&[]).await;
        if catalog.sockets.is_empty() {
            return Err(X11AccessError::Selection(
                catalog
                    .diagnostics
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "no live local X11 endpoint was discovered".into()),
            ));
        }
        let mut reports = Vec::with_capacity(catalog.sockets.len());
        for socket in catalog.sockets {
            let report = match self.desktop.reconcile(&socket).await {
                Ok(report) => report,
                Err(error) => X11ReconcileReport::new(
                    socket.display(),
                    Vec::new(),
                    Vec::new(),
                    vec![error.to_string()],
                ),
            };
            reports.push(report);
        }
        Ok(reports)
    }
}

fn select_session_endpoints(
    catalog: X11EndpointCatalog,
    selection: X11SessionSelection,
) -> Result<(u16, Vec<HostX11Socket>), String> {
    let display = match selection {
        X11SessionSelection::Current => catalog.preferred_display.ok_or_else(|| {
            "the current DISPLAY does not identify a local X11 server; select one explicitly with --with-x11=:N"
                .to_owned()
        })?,
        X11SessionSelection::Display(display) => display,
    };
    let mut candidates = catalog
        .sockets
        .iter()
        .filter(|socket| socket.display() == display)
        .cloned()
        .collect::<Vec<_>>();
    candidates.sort_by_key(HostX11Socket::alternate);
    if !candidates.is_empty() {
        return Ok((display, candidates));
    }

    let mut available = catalog
        .sockets
        .iter()
        .map(HostX11Socket::display)
        .collect::<Vec<_>>();
    available.sort_unstable();
    available.dedup();
    let available = if available.is_empty() {
        "none".to_owned()
    } else {
        available
            .into_iter()
            .map(|display| format!(":{display}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let diagnostic = catalog
        .diagnostics
        .first()
        .map(|message| format!("; discovery: {message}"))
        .unwrap_or_default();
    Err(format!(
        "X11 display :{display} was not discovered (available: {available}){diagnostic}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::sessions::{
        JournalSessionHandle, JournalSessionRequest, MappedGuestIdentity, ObservedGuestIdentity,
        ObservedMachineInstance, ObservedNamespaceIdentity, SessionPort, TerminalSessionHandle,
        TerminalSessionRequest, ValidatedGuestUserName, WaylandPreparationRequest,
        WaylandSessionContext, X11FilesystemAccess, X11ProjectionProbeRequest,
    };
    use crate::domain::machine::MachineName;
    use crate::domain::wayland::HostWaylandSocket;
    use crate::domain::x11::X11SocketRevision;
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    struct ChangingProjectionPort {
        probes: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl SessionPort for ChangingProjectionPort {
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
            panic!("X11 confirmation must not open a user terminal")
        }

        async fn prepare_wayland(
            &self,
            _request: WaylandPreparationRequest,
        ) -> Result<WaylandSessionContext, SessionError> {
            panic!("X11 confirmation must not prepare Wayland")
        }

        async fn probe_x11_projection(
            &self,
            request: X11ProjectionProbeRequest,
        ) -> Result<X11ProjectionContext, SessionError> {
            let generation = self.probes.fetch_add(1, Ordering::Relaxed) as u32;
            Ok(projection_for_socket(
                request.host_socket,
                1_437_402_088 + generation,
            ))
        }

        async fn open_journal(
            &self,
            _request: JournalSessionRequest,
        ) -> Result<JournalSessionHandle, SessionError> {
            panic!("X11 confirmation must not open a journal")
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
        let service = X11AccessService::new(sessions, endpoints, desktop.clone());

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
        let service = X11AccessService::new(sessions, endpoints, desktop.clone());

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

    #[tokio::test]
    async fn session_preview_is_read_only_and_uses_the_selected_projection() {
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
        let service = X11AccessService::new(sessions, endpoints, desktop.clone());

        let preview = service
            .preview_session(target(), X11SessionSelection::Current)
            .await
            .unwrap();

        assert_eq!(preview.target(), &target());
        assert_eq!(preview.check().projection().host_socket().display(), 0);
        assert!(preview.check().projection().host_socket().alternate());
        assert_eq!(*projection_port.probes.lock(), [false, true]);
        assert_eq!(desktop.snapshots.load(Ordering::Relaxed), 1);
        assert_eq!(desktop.ensures.load(Ordering::Relaxed), 0);
        assert!(desktop.purposes.lock().is_empty());
    }

    #[tokio::test]
    async fn previewed_session_rejects_changed_identity_before_acl_mutation() {
        let projection_port = Arc::new(ChangingProjectionPort {
            probes: AtomicUsize::new(0),
        });
        let sessions = Arc::new(SessionService::new(projection_port.clone()));
        let endpoints = Arc::new(X11EndpointDiscoveryService::new(Arc::new(
            StaticEndpointPort(X11EndpointCatalog {
                sockets: vec![host_socket(0, false, 2)],
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
        let service = X11AccessService::new(sessions, endpoints, desktop.clone());
        let preview = service
            .preview_session(target(), X11SessionSelection::Current)
            .await
            .unwrap();

        let error = service
            .prepare_previewed_session(preview)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("changed while confirmation"));
        assert_eq!(projection_port.probes.load(Ordering::Relaxed), 2);
        assert_eq!(desktop.snapshots.load(Ordering::Relaxed), 1);
        assert_eq!(desktop.ensures.load(Ordering::Relaxed), 0);
    }
}
