//! Reconcile X11 grants with system-scope machine lifecycle evidence.
//!
//! Lifecycle owns machine registration observation, claim state transitions,
//! grouping of shared grants, and cleanup decisions. It does not perform
//! endpoint discovery; cleanup mutations are made only after it revalidates
//! the endpoint and server generation through the transport boundary.

use std::fs;

use crate::adapters::runtime::registration;
use crate::application::sessions::ObservedMachineInstance;
use crate::application::x11::{
    X11AclEntry, X11GrantRecordPhase, X11ReconcileReport, X11ReconcileRetry,
};
use crate::domain::x11::HostX11Socket;

use super::common::{CLAIM_RECONCILE_GRACE_MILLIS, MAX_GRANT_DIAGNOSTICS};
use super::ownership::GrantOwnership;
use super::state::{
    MachineClaimCatalog, MachineClaimPhase, ManagedX11GrantKey, ManagedX11MachineClaim,
    X11RuntimeState,
};
use super::transport::{host_boot_id, unix_millis, ConnectedDesktopAcl, DesktopAcl};

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
            let leader_pid = match registration::leader_pid_at(&path, machine) {
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
                    ))
                }
            };
            match registration::machine_instance_at(&path, machine) {
                Ok(instance) => SystemMachineRegistration::Present {
                    leader_pid: instance.leader_pid(),
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
    CleanupPending {
        since_unix_millis: u64,
    },
    CleanupReady,
    NeedsReview(String),
    /// A failed read in this pass, not an uncertain ACL mutation.
    Unavailable(String),
    /// A persisted ACL outcome whose ownership cannot be inferred by retrying.
    Blocked(String),
}

pub(super) fn observe_machine_claim(
    claim: &ManagedX11MachineClaim,
    current_boot_id: Option<&str>,
    registration: SystemMachineRegistration,
    now_unix_millis: u64,
) -> Option<MachineClaimObservation> {
    // A machine registration change is lifecycle evidence, not an uncertain
    // ACL mutation. Recover the review states written by the old transition
    // logic, but never reinterpret an unknown ACL outcome as a stopped guest.
    let phase = match &claim.phase {
        MachineClaimPhase::Ended => return None,
        MachineClaimPhase::NeedsReview { reason } if !claim.phase.tracks_machine_lifecycle() => {
            return Some(MachineClaimObservation::NeedsReview(reason.clone()));
        }
        MachineClaimPhase::Unknown { reason } if !claim.phase.tracks_machine_lifecycle() => {
            return Some(MachineClaimObservation::Blocked(reason.clone()));
        }
        phase => phase,
    };
    let Some(current_boot_id) = current_boot_id else {
        return Some(MachineClaimObservation::Unavailable(
            "host boot identity is unavailable".into(),
        ));
    };
    if claim.boot_id != current_boot_id {
        return Some(MachineClaimObservation::NeedsReview(
            "claim belongs to an earlier host boot".into(),
        ));
    }

    match registration {
        SystemMachineRegistration::Present {
            leader_pid,
            instance,
        } => {
            let replaced =
                match instance.and_then(|instance| claim.compare_machine_instance(instance)) {
                    Some(same) => !same,
                    None => leader_pid != claim.machine_leader_pid,
                };
            if !replaced {
                // Retain when the instance matches, or the leader PID is the
                // same and comparable generation evidence is unavailable.
                // This does not authorize a replacement instance.
                return Some(MachineClaimObservation::Active);
            }
            // A different registered instance proves this claim's consumer
            // ended. It does not inherit the claim; a freshly prepared consumer
            // must register its own claim under the same ACL lock.
        }
        SystemMachineRegistration::Absent => {}
        SystemMachineRegistration::Unknown(reason) => {
            return Some(MachineClaimObservation::Unavailable(reason));
        }
    }
    Some(match phase {
        MachineClaimPhase::CleanupPending { since_unix_millis } => {
            if now_unix_millis.saturating_sub(*since_unix_millis) >= CLAIM_RECONCILE_GRACE_MILLIS {
                MachineClaimObservation::CleanupReady
            } else {
                MachineClaimObservation::CleanupPending {
                    since_unix_millis: *since_unix_millis,
                }
            }
        }
        _ => MachineClaimObservation::EndedCandidate,
    })
}

impl MachineClaimObservation {
    pub(super) fn next_phase(
        &self,
        claim: &ManagedX11MachineClaim,
        now_unix_millis: u64,
    ) -> Option<MachineClaimPhase> {
        let next = match self {
            Self::Active | Self::Unavailable(_) => MachineClaimPhase::Active,
            Self::EndedCandidate => MachineClaimPhase::CleanupPending {
                since_unix_millis: now_unix_millis,
            },
            Self::NeedsReview(reason) => MachineClaimPhase::NeedsReview {
                reason: reason.clone(),
            },
            Self::CleanupPending { .. } | Self::CleanupReady | Self::Blocked(_) => return None,
        };
        // A transient read failure blocks this pass only. In particular, reset
        // any pending timestamp so the next successful absence starts a full
        // grace period. Never persist the observation as an ACL-outcome Unknown.
        (next != claim.phase).then_some(next)
    }

    fn into_reconcile_status(self) -> ReconcileClaimStatus {
        match self {
            Self::Active => ReconcileClaimStatus::Active,
            Self::EndedCandidate | Self::CleanupPending { .. } => ReconcileClaimStatus::Pending,
            Self::CleanupReady => ReconcileClaimStatus::Ready,
            Self::Unavailable(reason) => ReconcileClaimStatus::Unavailable(reason),
            Self::NeedsReview(reason) | Self::Blocked(reason) => {
                ReconcileClaimStatus::Blocked(reason)
            }
        }
    }
}

/// Report claim failures, not the ordinary lifecycle progress of every
/// machine sharing the desktop catalog. Callers surface these as warnings.
pub(super) fn machine_claim_diagnostics(
    claims: &MachineClaimCatalog,
    current_boot_id: Option<&str>,
) -> Vec<String> {
    let mut diagnostics = claims.diagnostics.clone();
    let now = unix_millis().unwrap_or(0);
    for claim in &claims.claims {
        let registration = system_machine_registration(&claim.machine);
        let Some(observation) = observe_machine_claim(claim, current_boot_id, registration, now)
        else {
            continue;
        };
        let proposed = observation.next_phase(claim, now);
        let mut detail = match observation {
            MachineClaimObservation::Active
            | MachineClaimObservation::EndedCandidate
            | MachineClaimObservation::CleanupPending { .. }
            | MachineClaimObservation::CleanupReady => continue,
            MachineClaimObservation::NeedsReview(reason)
            | MachineClaimObservation::Unavailable(reason)
            | MachineClaimObservation::Blocked(reason) => reason,
        };
        if let Some(phase) = proposed {
            detail.push_str(match phase {
                MachineClaimPhase::CleanupPending { .. } => "; next state: cleanup pending",
                MachineClaimPhase::NeedsReview { .. } => "; next state: needs review",
                MachineClaimPhase::Unknown { .. } => "; next state: unknown",
                MachineClaimPhase::Active => "; next state: active (observation will be retried)",
                MachineClaimPhase::Ended => "",
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ReconcileClaimStatus {
    Active,
    Pending,
    Ready,
    /// A transient machine-state read failure. It blocks ACL mutation for
    /// this pass but remains eligible for the bounded observation retry.
    Unavailable(String),
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
                ReconcileClaimStatus::Active
                    | ReconcileClaimStatus::Unavailable(_)
                    | ReconcileClaimStatus::Blocked(_)
            )
        })
    }

    pub(super) fn pending(&self) -> bool {
        self.claims
            .iter()
            .any(|claim| matches!(&claim.status, ReconcileClaimStatus::Pending))
    }

    pub(super) fn ready(&self) -> bool {
        self.claims
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

/// Use one transition path for normal passes, retries of old observations, and
/// the last machine check before removing an ACL entry. Persistence failures
/// always block mutation and leave the watcher responsible for the claim.
pub(super) fn reconcile_claim(
    state: &X11RuntimeState,
    claim: &ManagedX11MachineClaim,
    current_boot_id: &str,
    registration: SystemMachineRegistration,
    now: u64,
) -> Option<ReconcileClaimStatus> {
    let observation = observe_machine_claim(claim, Some(current_boot_id), registration, now)?;
    if let Some(phase) = observation.next_phase(claim, now) {
        if let Err(error) = mark_claim_phase_sync(state, claim, phase) {
            return Some(ReconcileClaimStatus::Blocked(format!(
                "could not persist machine claim transition: {error}"
            )));
        }
    }
    Some(observation.into_reconcile_status())
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
    reconcile_with_state(
        &state,
        socket,
        ConnectedDesktopAcl::connect,
        system_machine_registration,
        unix_millis,
    )
}

/// Run the same locked operation with explicit observation boundaries. Time
/// and registration are read again before mutation; a test can exercise that
/// race without installing a container or touching the desktop ACL.
pub(super) fn reconcile_with_state<D: DesktopAcl>(
    state: &X11RuntimeState,
    socket: &HostX11Socket,
    connect: impl FnOnce(&HostX11Socket) -> Result<D, String>,
    mut observe: impl FnMut(&str) -> SystemMachineRegistration,
    mut clock: impl FnMut() -> Result<u64, String>,
) -> Result<X11ReconcileReport, String> {
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
    if !super::state::claims_need_activation(&claims, &records)? {
        return Ok(X11ReconcileReport::new(
            socket.display(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ));
    }

    let desktop = connect(socket)?;
    let server_peer_start_time = desktop.server_start_time();
    let current_boot_id = host_boot_id()?;
    let now = clock()?;
    let mut acl = desktop.snapshot()?;
    if !matches!(
        acl.mode(),
        crate::application::x11::X11AccessControlMode::Enabled
    ) {
        return Err("X11 ACL lifecycle reconcile requires enabled access control".into());
    }

    let mut diagnostics = Vec::new();
    let mut groups = Vec::<ReconcileGroup>::new();
    for record in &records.records {
        if matches!(record.phase, X11GrantRecordPhase::ConfirmedAdded)
            && claim_key_matches_socket(
                &ManagedX11GrantKey::from_evidence(record),
                socket,
                server_peer_start_time,
            )
        {
            let key = ManagedX11GrantKey::from_evidence(record);
            if !groups.iter().any(|group| group.key == key) {
                groups.push(ReconcileGroup {
                    key,
                    claims: Vec::new(),
                });
            }
        }
    }
    for claim in claims.claims.iter() {
        if !claim.phase.requires_reconcile() {
            continue;
        }
        if !claim_key_matches_socket(&claim.key, socket, server_peer_start_time) {
            continue;
        }
        let Some(status) =
            reconcile_claim(state, claim, &current_boot_id, observe(&claim.machine), now)
        else {
            continue;
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
    let mut retry = X11ReconcileRetry::None;
    for group in groups {
        for claim in &group.claims {
            if let ReconcileClaimStatus::Unavailable(reason) = &claim.status {
                push_reconcile_diagnostic(
                    &mut diagnostics,
                    format!(
                        "claim {} machine observation unavailable: {reason}",
                        claim.claim_id
                    ),
                );
            } else if let ReconcileClaimStatus::Blocked(reason) = &claim.status {
                push_reconcile_diagnostic(
                    &mut diagnostics,
                    format!(
                        "claim {} not eligible for cleanup: {reason}",
                        claim.claim_id
                    ),
                );
            }
            if matches!(&claim.status, ReconcileClaimStatus::Pending)
                && !pending_record_ids.contains(&claim.record_id)
            {
                pending_record_ids.push(claim.record_id.clone());
            }
            match &claim.status {
                ReconcileClaimStatus::Active => {
                    retry = X11ReconcileRetry::RegisteredMachine;
                }
                ReconcileClaimStatus::Pending | ReconcileClaimStatus::Unavailable(_)
                    if retry == X11ReconcileRetry::None =>
                {
                    retry = X11ReconcileRetry::PendingObservation;
                }
                _ => {}
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

        let acl_entry = X11AclEntry::from_wire(group.key.acl_family, group.key.acl_address.clone());
        let ownership = match GrantOwnership::inspect(
            &records,
            &claims,
            &group.key,
            &current_boot_id,
        )
        .and_then(|ownership| {
            ownership.require_lifecycle_cleanup(acl.contains(&acl_entry))?;
            Ok(ownership)
        }) {
            Ok(ownership) => ownership,
            Err(reason) => {
                push_reconcile_diagnostic(&mut diagnostics, reason);
                continue;
            }
        };

        // An earlier group may have involved X11 I/O. Recheck every consumer
        // immediately before revoking: a matching or inconclusive registration
        // still retains the grant; a replaced instance does not inherit it.
        let mut still_ready = true;
        for member in &group.claims {
            let claim = claims
                .claims
                .iter()
                .find(|claim| claim.claim_id == member.claim_id)
                .expect("reconcile claim came from catalog");
            let status = reconcile_claim(
                state,
                claim,
                &current_boot_id,
                observe(&claim.machine),
                clock()?,
            );
            if status != Some(ReconcileClaimStatus::Ready) {
                still_ready = false;
                match status {
                    Some(ReconcileClaimStatus::Unavailable(reason)) => {
                        retry = X11ReconcileRetry::PendingObservation;
                        push_reconcile_diagnostic(
                            &mut diagnostics,
                            format!(
                                "claim {} machine observation unavailable before cleanup: {reason}",
                                claim.claim_id
                            ),
                        );
                    }
                    Some(ReconcileClaimStatus::Blocked(reason)) => {
                        push_reconcile_diagnostic(
                            &mut diagnostics,
                            format!("claim {} changed before cleanup: {reason}", claim.claim_id),
                        );
                    }
                    Some(ReconcileClaimStatus::Active) => {
                        retry = X11ReconcileRetry::RegisteredMachine;
                    }
                    Some(ReconcileClaimStatus::Pending) => {
                        if retry == X11ReconcileRetry::None {
                            retry = X11ReconcileRetry::PendingObservation;
                        }
                        if !pending_record_ids.contains(&member.record_id) {
                            pending_record_ids.push(member.record_id.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
        if !still_ready {
            continue;
        }

        if acl.contains(&acl_entry) {
            let (change, observed) = desktop.remove(group.key.host_uid);
            let endpoint_result = desktop.verify();
            if change.is_err() || endpoint_result.is_err() {
                let reason = [
                    change.err(),
                    observed
                        .err()
                        .map(|error| format!("ACL confirmation: {error}")),
                    endpoint_result
                        .err()
                        .map(|error| format!("endpoint/server revalidation: {error}")),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("; ");
                for claim in &group.claims {
                    let _ = mark_claim_phase_sync(
                        state,
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

        if let Err(error) = state.finish_revocation(&ownership.records) {
            push_reconcile_diagnostic(
                &mut diagnostics,
                format!("the ACL entry is absent, but its records could not be finalized: {error}"),
            );
            if retry == X11ReconcileRetry::None {
                retry = X11ReconcileRetry::PendingObservation;
            }
            continue;
        }
        for record in ownership.records {
            revoked_record_ids.push(record.record_id.clone());
        }
    }

    Ok(X11ReconcileReport::new(
        socket.display(),
        revoked_record_ids,
        pending_record_ids,
        diagnostics,
    )
    .with_retry(retry))
}
