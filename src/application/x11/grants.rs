//! X11 grant evidence and current access assessment.

use std::path::PathBuf;

use crate::application::sessions::{MappedGuestIdentity, ShellTarget, X11ProjectionContext};
use crate::domain::x11::X11SocketRevision;

use super::acl::SERVER_INTERPRETED_FAMILY;
use super::{X11AccessControlMode, X11AclEntry, X11AclSnapshot};

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

    pub(super) fn from_desktop_observation(
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

    /// Whether Lasper can bind ACL lifecycle evidence to a host X server
    /// process in the current PID namespace.  External peers (such as WSLg)
    /// may still be observed and used when access control is disabled, but
    /// they cannot safely receive a managed ACL mutation.
    pub fn server_identity_trackable(&self) -> bool {
        self.projection
            .host_socket()
            .peer_identity()
            .is_local_process()
    }

    pub fn grant_assessment(&self) -> &X11GrantAssessment {
        &self.grant_assessment
    }

    pub(crate) fn push_diagnostic(&mut self, diagnostic: impl Into<String>) {
        self.grant_assessment.diagnostics.push(diagnostic.into());
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
                        || projection.host_socket().peer_identity().legacy_tuple()
                            != record.server_peer
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
