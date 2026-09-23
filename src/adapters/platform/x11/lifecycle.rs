//! Reconcile X11 grants with system-scope machine lifecycle evidence.
//!
//! Lifecycle owns machine registration observation, claim state transitions,
//! grouping of shared grants, and cleanup decisions. It does not perform
//! endpoint discovery; cleanup mutations are made only after it revalidates
//! the endpoint and server generation through the transport boundary.

use std::fs;

use crate::application::sessions::ObservedMachineInstance;
use crate::application::x11::{X11AclEntry, X11GrantRecordPhase, X11ReconcileReport};
use crate::domain::x11::HostX11Socket;

use super::common::{CLAIM_RECONCILE_GRACE_MILLIS, MAX_GRANT_DIAGNOSTICS, MAX_GRANT_RECORD_BYTES};
use super::state::{
    GrantRecordPhase, MachineClaimCatalog, MachineClaimPhase, ManagedX11GrantKey,
    ManagedX11GrantRecord, ManagedX11MachineClaim, X11RuntimeState,
};
use super::transport::{
    authenticated_connection, host_boot_id, inspect_endpoint_peer_only, read_acl,
    remove_and_observe, require_current_socket, unix_millis, x11_peer_start_time,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SystemMachineRegistration {
    Present {
        leader_pid: u32,
        instance: Option<ObservedMachineInstance>,
    },
    Absent,
    Unknown(String),
}

/// Observe only the system machined registration directory. This helper
/// never consults the desktop user's runtime machine directory: claims on
/// this branch belong to system-scope nspawn machines.
pub(super) fn system_machine_registration(machine: &str) -> SystemMachineRegistration {
    let state_dir = crate::paths::runtime_machines_dir();
    match fs::symlink_metadata(&state_dir) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return SystemMachineRegistration::Unknown(format!(
                "system machined runtime path is not a directory: {}",
                state_dir.display()
            ));
        }
        Err(error) => {
            return SystemMachineRegistration::Unknown(format!(
                "cannot inspect system machined runtime directory {}: {error}",
                state_dir.display()
            ));
        }
    }
    let path = crate::paths::runtime_machine_state(machine);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            let leader_pid = match crate::adapters::runtime::state::leader_pid_at(&path, machine) {
                Ok(leader_pid) => leader_pid,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return match fs::symlink_metadata(&path) {
                        Err(current) if current.kind() == std::io::ErrorKind::NotFound => {
                            SystemMachineRegistration::Absent
                        }
                        _ => SystemMachineRegistration::Unknown(format!(
                            "system machine registration changed during leader inspection {}: {error}",
                            path.display()
                        )),
                    };
                }
                Err(error) => {
                    return SystemMachineRegistration::Unknown(format!(
                        "cannot inspect system machine leader {}: {error}",
                        path.display()
                    ));
                }
            };
            match crate::adapters::runtime::state::machine_instance_at(&path, machine) {
                Ok(instance) => SystemMachineRegistration::Present {
                    leader_pid,
                    instance: Some(instance),
                },
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    SystemMachineRegistration::Present {
                        leader_pid,
                        instance: None,
                    }
                }
                Err(error) => SystemMachineRegistration::Unknown(format!(
                    "cannot inspect system machine instance {}: {error}",
                    path.display()
                )),
            }
        }
        Ok(_) => SystemMachineRegistration::Unknown(format!(
            "system machine registration is not a regular file: {}",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            SystemMachineRegistration::Absent
        }
        Err(error) => SystemMachineRegistration::Unknown(format!(
            "cannot inspect system machine registration {}: {error}",
            path.display()
        )),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum MachineClaimObservation {
    Active,
    EndedCandidate,
    CleanupPending { since_unix_millis: u64 },
    CleanupReady,
    NeedsReview(String),
    Unknown(String),
}

pub(super) fn observe_machine_claim(
    claim: &ManagedX11MachineClaim,
    current_boot_id: Option<&str>,
    registration: SystemMachineRegistration,
    now_unix_millis: u64,
) -> Option<MachineClaimObservation> {
    if !matches!(
        &claim.phase,
        MachineClaimPhase::Active | MachineClaimPhase::CleanupPending { .. }
    ) {
        return None;
    }
    let Some(current_boot_id) = current_boot_id else {
        return Some(MachineClaimObservation::Unknown(
            "host boot identity is unavailable".into(),
        ));
    };
    if claim.boot_id != current_boot_id {
        return Some(MachineClaimObservation::NeedsReview(
            "claim belongs to an earlier host boot".into(),
        ));
    }
    match (&claim.phase, registration) {
        (
            MachineClaimPhase::Active,
            SystemMachineRegistration::Present {
                leader_pid: _,
                instance: Some(instance),
            },
        ) if claim.matches_machine_instance(instance) => Some(MachineClaimObservation::Active),
        (
            MachineClaimPhase::Active,
            SystemMachineRegistration::Present {
                leader_pid: _,
                instance: Some(_),
            },
        ) => Some(MachineClaimObservation::NeedsReview(
            "machine registration belongs to a different machine instance".into(),
        )),
        (
            MachineClaimPhase::Active,
            SystemMachineRegistration::Present {
                leader_pid,
                instance: None,
            },
        ) if leader_pid == claim.machine_leader_pid => Some(MachineClaimObservation::Unknown(
            "machine namespace identity is unavailable; cleanup is blocked".into(),
        )),
        (
            MachineClaimPhase::Active,
            SystemMachineRegistration::Present {
                leader_pid: _,
                instance: None,
            },
        ) => Some(MachineClaimObservation::NeedsReview(
            "machine leader changed but namespace identity is unavailable".into(),
        )),
        (MachineClaimPhase::Active, SystemMachineRegistration::Absent) => {
            Some(MachineClaimObservation::EndedCandidate)
        }
        (MachineClaimPhase::Active, SystemMachineRegistration::Unknown(reason)) => {
            Some(MachineClaimObservation::Unknown(reason))
        }
        (
            MachineClaimPhase::CleanupPending { .. },
            SystemMachineRegistration::Present {
                leader_pid,
                instance,
            },
        ) => {
            let reason = if instance
                .is_some_and(|instance| claim.matches_machine_instance(instance))
            {
                "machine registration reappeared before cleanup; explicit preparation is required"
            } else if leader_pid == claim.machine_leader_pid && instance.is_none() {
                "machine registration reappeared but namespace identity is unavailable; explicit preparation is required"
            } else {
                "a different machine instance reappeared before cleanup; explicit preparation is required"
            };
            Some(MachineClaimObservation::NeedsReview(reason.into()))
        }
        (
            MachineClaimPhase::CleanupPending { since_unix_millis },
            SystemMachineRegistration::Absent,
        ) => Some(
            if now_unix_millis.saturating_sub(*since_unix_millis) >= CLAIM_RECONCILE_GRACE_MILLIS {
                MachineClaimObservation::CleanupReady
            } else {
                MachineClaimObservation::CleanupPending {
                    since_unix_millis: *since_unix_millis,
                }
            },
        ),
        (
            MachineClaimPhase::CleanupPending {
                since_unix_millis: _,
            },
            SystemMachineRegistration::Unknown(reason),
        ) => Some(MachineClaimObservation::Unknown(reason)),
        _ => None,
    }
}

pub(super) fn proposed_claim_phase(
    claim: &ManagedX11MachineClaim,
    current_boot_id: Option<&str>,
    registration: SystemMachineRegistration,
    now_unix_millis: u64,
) -> Option<MachineClaimPhase> {
    match observe_machine_claim(claim, current_boot_id, registration, now_unix_millis)? {
        MachineClaimObservation::Active
        | MachineClaimObservation::CleanupPending { .. }
        | MachineClaimObservation::CleanupReady => None,
        MachineClaimObservation::EndedCandidate => Some(MachineClaimPhase::CleanupPending {
            since_unix_millis: now_unix_millis,
        }),
        MachineClaimObservation::NeedsReview(reason) => {
            Some(MachineClaimPhase::NeedsReview { reason })
        }
        MachineClaimObservation::Unknown(reason) => Some(MachineClaimPhase::Unknown { reason }),
    }
}

pub(super) fn machine_claim_diagnostics(
    claims: &MachineClaimCatalog,
    current_boot_id: Option<&str>,
) -> Vec<String> {
    let mut diagnostics = claims.diagnostics.clone();
    let now = unix_millis().unwrap_or(0);
    for claim in &claims.claims {
        let registration = system_machine_registration(&claim.machine);
        let proposed = proposed_claim_phase(claim, current_boot_id, registration.clone(), now);
        let Some(observation) = observe_machine_claim(claim, current_boot_id, registration, now)
        else {
            continue;
        };
        let mut detail = match observation {
            MachineClaimObservation::Active => "system machine registration is present".to_owned(),
            MachineClaimObservation::EndedCandidate => {
                "system machine registration is absent; cleanup is only a candidate".to_owned()
            }
            MachineClaimObservation::CleanupPending { since_unix_millis } => {
                format!("cleanup pending since unix millisecond {since_unix_millis}")
            }
            MachineClaimObservation::CleanupReady => {
                "machine registration stayed absent; cleanup is ready for a guarded reconcile"
                    .to_owned()
            }
            MachineClaimObservation::NeedsReview(reason)
            | MachineClaimObservation::Unknown(reason) => reason,
        };
        if let Some(phase) = proposed {
            detail.push_str(match phase {
                MachineClaimPhase::CleanupPending { .. } => "; next state: cleanup pending",
                MachineClaimPhase::NeedsReview { .. } => "; next state: needs review",
                MachineClaimPhase::Unknown { .. } => "; next state: unknown",
                MachineClaimPhase::Active | MachineClaimPhase::Ended => "",
            });
        }
        if diagnostics.len() < MAX_GRANT_DIAGNOSTICS {
            diagnostics.push(format!(
                "X11 machine claim {} for {}: {detail}",
                claim.claim_id, claim.machine
            ));
        }
    }
    diagnostics
}

#[derive(Clone, Debug)]
pub(super) enum ReconcileClaimStatus {
    Active,
    Pending,
    Ready,
    Blocked(String),
}

#[derive(Clone, Debug)]
pub(super) struct ReconcileClaim {
    pub(super) claim_id: String,
    pub(super) record_id: String,
    pub(super) key: ManagedX11GrantKey,
    pub(super) status: ReconcileClaimStatus,
}

#[derive(Clone, Debug)]
pub(super) struct ReconcileGroup {
    pub(super) key: ManagedX11GrantKey,
    pub(super) claims: Vec<ReconcileClaim>,
}

impl ReconcileGroup {
    pub(super) fn new(claim: ReconcileClaim) -> Self {
        Self {
            key: claim.key.clone(),
            claims: vec![claim],
        }
    }

    pub(super) fn add(&mut self, claim: ReconcileClaim) {
        self.claims.push(claim);
    }

    pub(super) fn blocked(&self) -> bool {
        self.claims.iter().any(|claim| {
            matches!(
                &claim.status,
                ReconcileClaimStatus::Active | ReconcileClaimStatus::Blocked(_)
            )
        })
    }

    pub(super) fn pending(&self) -> bool {
        self.claims
            .iter()
            .any(|claim| matches!(&claim.status, ReconcileClaimStatus::Pending))
    }

    pub(super) fn ready(&self) -> bool {
        !self.claims.is_empty()
            && self
                .claims
                .iter()
                .all(|claim| matches!(&claim.status, ReconcileClaimStatus::Ready))
    }
}

pub(super) fn claim_key_matches_socket(
    key: &ManagedX11GrantKey,
    socket: &HostX11Socket,
    server_peer_start_time: u64,
) -> bool {
    key.display == socket.display()
        && key.alternate_endpoint == socket.alternate()
        && key.source == socket.source()
        && key.canonical_source == socket.canonical_path()
        && key.socket_revision == socket.revision()
        && key.server_peer == socket.peer_identity().legacy_tuple()
        && key.server_peer_start_time == server_peer_start_time
}

pub(super) fn mark_claim_phase_sync(
    state: &X11RuntimeState,
    claim: &ManagedX11MachineClaim,
    phase: MachineClaimPhase,
) -> Result<(), String> {
    let mut updated = claim.clone();
    updated.phase = phase;
    state.write_claim(&updated)
}

pub(super) fn mark_record_phase_sync(
    state: &X11RuntimeState,
    record_id: &str,
    phase: GrantRecordPhase,
) -> Result<(), String> {
    let file_name = format!("grant-{record_id}.json");
    let file = state
        .grants
        .read_bounded(&file_name, MAX_GRANT_RECORD_BYTES)
        .map_err(|error| format!("read X11 grant record {record_id}: {error}"))?
        .ok_or_else(|| format!("X11 grant record {record_id} does not exist"))?;
    if file.uid != uzers::get_effective_uid() || file.mode & 0o077 != 0 {
        return Err(format!(
            "X11 grant record {record_id} is not owned by the invoking user"
        ));
    }
    let mut record: ManagedX11GrantRecord = serde_json::from_slice(&file.bytes)
        .map_err(|error| format!("invalid X11 grant record {record_id}: {error}"))?;
    record
        .clone()
        .into_evidence(&file_name)
        .map_err(|error| format!("invalid X11 grant record {record_id}: {error}"))?;
    record.phase = phase;
    state.write(&file_name, &record)
}

pub(super) fn push_reconcile_diagnostic(diagnostics: &mut Vec<String>, message: impl Into<String>) {
    if diagnostics.len() < MAX_GRANT_DIAGNOSTICS {
        diagnostics.push(message.into());
    }
}

pub(super) fn reconcile_sync(socket: &HostX11Socket) -> Result<X11ReconcileReport, String> {
    let Some(state) = X11RuntimeState::open_existing()? else {
        return Ok(X11ReconcileReport::new(
            socket.display(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ));
    };
    let _lock = state.lock()?;
    let claims = state.load_claims();
    if !claims.complete {
        return Err(claims
            .diagnostics
            .first()
            .cloned()
            .unwrap_or_else(|| "X11 machine claim set is incomplete".into()));
    }
    let records = state.load_records();
    if !records.complete {
        return Err(records
            .diagnostics
            .first()
            .cloned()
            .unwrap_or_else(|| "X11 grant record set is incomplete".into()));
    }
    if claims.claims.is_empty() {
        return Ok(X11ReconcileReport::new(
            socket.display(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ));
    }

    let current_socket = inspect_endpoint_peer_only(
        socket.display(),
        socket.alternate(),
        socket.source().to_path_buf(),
    )?;
    if current_socket != *socket {
        return Err(format!(
            "{} changed before X11 lifecycle reconcile",
            socket.source().display()
        ));
    }
    let server_peer_start_time = x11_peer_start_time(socket)?;
    let current_boot_id = host_boot_id()?;
    let now = unix_millis()?;
    let (connection, _) = authenticated_connection(socket.source(), socket.display())?;
    let mut acl = read_acl(&connection, socket.display())?;
    require_current_socket(socket, "before X11 lifecycle reconcile")?;
    if !matches!(
        acl.mode(),
        crate::application::x11::X11AccessControlMode::Enabled
    ) {
        return Err("X11 ACL lifecycle reconcile requires enabled access control".into());
    }

    let mut diagnostics = Vec::new();
    let mut groups = Vec::<ReconcileGroup>::new();
    for claim in claims.claims.iter() {
        if !claim_key_matches_socket(&claim.key, socket, server_peer_start_time) {
            continue;
        }
        let status = match &claim.phase {
            MachineClaimPhase::Ended => continue,
            MachineClaimPhase::NeedsReview { reason } => {
                ReconcileClaimStatus::Blocked(reason.clone())
            }
            MachineClaimPhase::Unknown { reason } => ReconcileClaimStatus::Blocked(reason.clone()),
            MachineClaimPhase::Active | MachineClaimPhase::CleanupPending { .. } => {
                let registration = system_machine_registration(&claim.machine);
                match observe_machine_claim(
                    claim,
                    Some(current_boot_id.as_str()),
                    registration,
                    now,
                ) {
                    Some(MachineClaimObservation::Active) => ReconcileClaimStatus::Active,
                    Some(MachineClaimObservation::EndedCandidate) => {
                        let phase = MachineClaimPhase::CleanupPending {
                            since_unix_millis: now,
                        };
                        if let Err(error) = mark_claim_phase_sync(&state, claim, phase) {
                            ReconcileClaimStatus::Blocked(format!(
                                "could not persist cleanup-pending state: {error}"
                            ))
                        } else {
                            ReconcileClaimStatus::Pending
                        }
                    }
                    Some(MachineClaimObservation::CleanupPending { .. }) => {
                        ReconcileClaimStatus::Pending
                    }
                    Some(MachineClaimObservation::CleanupReady) => ReconcileClaimStatus::Ready,
                    Some(MachineClaimObservation::NeedsReview(reason)) => {
                        let phase = MachineClaimPhase::NeedsReview {
                            reason: reason.clone(),
                        };
                        if let Err(error) = mark_claim_phase_sync(&state, claim, phase) {
                            ReconcileClaimStatus::Blocked(format!(
                                "{reason}; could not persist review state: {error}"
                            ))
                        } else {
                            ReconcileClaimStatus::Blocked(reason)
                        }
                    }
                    Some(MachineClaimObservation::Unknown(reason)) => {
                        let phase = MachineClaimPhase::Unknown {
                            reason: reason.clone(),
                        };
                        if let Err(error) = mark_claim_phase_sync(&state, claim, phase) {
                            ReconcileClaimStatus::Blocked(format!(
                                "{reason}; could not persist unknown state: {error}"
                            ))
                        } else {
                            ReconcileClaimStatus::Blocked(reason)
                        }
                    }
                    None => ReconcileClaimStatus::Blocked(
                        "machine claim lifecycle could not be observed".into(),
                    ),
                }
            }
        };
        let reconcile_claim = ReconcileClaim {
            claim_id: claim.claim_id.clone(),
            record_id: claim.grant_record_id.clone(),
            key: claim.key.clone(),
            status,
        };
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group.key == reconcile_claim.key)
        {
            group.add(reconcile_claim);
        } else {
            groups.push(ReconcileGroup::new(reconcile_claim));
        }
    }

    let mut revoked_record_ids = Vec::new();
    let mut pending_record_ids = Vec::new();
    for group in groups {
        for claim in &group.claims {
            if let ReconcileClaimStatus::Blocked(reason) = &claim.status {
                push_reconcile_diagnostic(
                    &mut diagnostics,
                    format!(
                        "claim {} not eligible for cleanup: {reason}",
                        claim.claim_id
                    ),
                );
            }
            if matches!(&claim.status, ReconcileClaimStatus::Pending) {
                pending_record_ids.push(claim.record_id.clone());
            }
        }
        if group.blocked() {
            continue;
        }
        if group.pending() {
            continue;
        }
        if !group.ready() {
            continue;
        }

        let mut matching_records = Vec::new();
        let mut invalid_record = None;
        for claim in &group.claims {
            let Some(record) = records.records.iter().find(|record| {
                record.record_id == claim.record_id
                    && matches!(&record.phase, X11GrantRecordPhase::ConfirmedAdded)
                    && ManagedX11GrantKey::from_evidence(record) == group.key
            }) else {
                invalid_record = Some(format!(
                    "claim {} has no matching confirmed grant record",
                    claim.claim_id
                ));
                break;
            };
            matching_records.push(record);
        }
        if let Some(reason) = invalid_record {
            push_reconcile_diagnostic(&mut diagnostics, reason);
            continue;
        }

        let acl_entry = X11AclEntry::from_wire(group.key.acl_family, group.key.acl_address.clone());
        if acl.contains(&acl_entry) {
            let (change, observed) =
                remove_and_observe(&connection, socket.display(), group.key.host_uid);
            let socket_result = require_current_socket(socket, "while revoking a stopped machine");
            let server_result = x11_peer_start_time(socket).and_then(|current| {
                (current == server_peer_start_time)
                    .then_some(())
                    .ok_or_else(|| {
                        "X11 server process changed while revoking a stopped machine".to_owned()
                    })
            });
            if change.is_err() || socket_result.is_err() || server_result.is_err() {
                let reason = [
                    change.err(),
                    observed
                        .err()
                        .map(|error| format!("ACL confirmation: {error}")),
                    socket_result.err(),
                    server_result
                        .err()
                        .map(|error| format!("server generation: {error}")),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("; ");
                for claim in &group.claims {
                    let _ = mark_claim_phase_sync(
                        &state,
                        claims
                            .claims
                            .iter()
                            .find(|candidate| candidate.claim_id == claim.claim_id)
                            .expect("reconcile claim came from catalog"),
                        MachineClaimPhase::Unknown {
                            reason: reason.clone(),
                        },
                    );
                }
                push_reconcile_diagnostic(
                    &mut diagnostics,
                    format!("X11 revoke outcome is unknown: {reason}"),
                );
                continue;
            }
            acl = observed?;
            if acl.contains(&acl_entry) {
                push_reconcile_diagnostic(
                    &mut diagnostics,
                    "exact X11 ACL entry remained after stopped-machine revoke".to_owned(),
                );
                continue;
            }
        }

        for record in matching_records {
            mark_record_phase_sync(&state, &record.record_id, GrantRecordPhase::Revoked)?;
            revoked_record_ids.push(record.record_id.clone());
        }
        for claim in &group.claims {
            let original = claims
                .claims
                .iter()
                .find(|candidate| candidate.claim_id == claim.claim_id)
                .expect("reconcile claim came from catalog");
            mark_claim_phase_sync(&state, original, MachineClaimPhase::Ended)?;
        }
    }

    Ok(X11ReconcileReport::new(
        socket.display(),
        revoked_record_ids,
        pending_record_ids,
        diagnostics,
    ))
}
