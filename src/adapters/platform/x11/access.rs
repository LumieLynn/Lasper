//! Authorization, observation, and revocation orchestration.
//!
//! This module coordinates discovery evidence, transport operations, and
//! trusted state. Explicit access operations live here; stopped-machine
//! cleanup lives in lifecycle and uses the same wire/state primitives.

use x11rb::protocol::xproto::Family;

use crate::application::sessions::{GuestTarget, X11ProjectionContext};
use crate::application::x11::{
    X11AclEntry, X11AclSnapshot, X11AuthorizationDisposition, X11AuthorizationPurpose,
    X11AuthorizationRequest, X11DesktopAuthorization, X11DesktopObservation, X11DesktopRevocation,
    X11GrantRecordCatalog, X11GrantRecordEvidence, X11GrantRecordPhase, X11RecordRevocationCheck,
    X11RevocationDisposition, X11RevokeRequest,
};
use crate::domain::x11::HostX11Socket;

use super::common::MAX_GRANT_RECORD_BYTES;
use super::lifecycle::machine_claim_diagnostics;
use super::ownership::GrantOwnership;
use super::state::{
    load_existing_grant_records, load_existing_machine_claims, valid_record_id, GrantRecordPhase,
    MachineClaimCatalog, ManagedX11GrantKey, ManagedX11GrantRecord, ManagedX11MachineClaim,
    X11RuntimeState,
};
use super::transport::{
    authorization_was_confirmed, connect_current_socket, host_boot_id, insert_and_observe,
    numeric_local_user_address, read_acl, require_current_socket, x11_peer_start_time,
    ConnectedDesktopAcl, DesktopAcl,
};

pub(super) fn snapshot_desktop_sync(
    target: &GuestTarget,
    projection: &X11ProjectionContext,
) -> Result<X11DesktopObservation, String> {
    let socket = projection.host_socket();
    let connection = connect_current_socket(socket)?;
    let acl = read_acl(&connection, socket.display())?;
    require_current_socket(socket, "while querying its ACL")?;
    let observation = desktop_observation(target, projection, acl, None);
    require_current_socket(socket, "while assessing its grant records")?;
    Ok(observation)
}
pub(super) fn desktop_observation(
    target: &GuestTarget,
    projection: &X11ProjectionContext,
    acl: X11AclSnapshot,
    state: Option<&X11RuntimeState>,
) -> X11DesktopObservation {
    let socket = projection.host_socket();
    let mut diagnostics = Vec::new();
    let host_boot_id = match host_boot_id() {
        Ok(value) => Some(value),
        Err(error) => {
            diagnostics.push(format!("X11 server continuity: {error}"));
            None
        }
    };
    let server_peer_start_time = if socket.peer_identity().is_local_process() {
        match x11_peer_start_time(socket) {
            Ok(value) => Some(value),
            Err(error) => {
                diagnostics.push(format!("X11 server continuity: {error}"));
                None
            }
        }
    } else {
        diagnostics.push(
            "X11 server peer is outside the current PID namespace; automatic ACL lifecycle management is unavailable"
                .into(),
        );
        None
    };
    let records = state
        .map(X11RuntimeState::load_records)
        .unwrap_or_else(load_existing_grant_records);
    let claims = state
        .map(X11RuntimeState::load_claims)
        .unwrap_or_else(load_existing_machine_claims);
    diagnostics.extend(machine_claim_diagnostics(&claims, host_boot_id.as_deref()));
    let machine_claims = claims
        .claims
        .iter()
        .filter_map(|claim| {
            let record = records
                .records
                .iter()
                .find(|record| record.record_id == claim.grant_record_id)?;
            claim.active_evidence(record)
        })
        .collect();
    let revocation_checks = revocation_checks(
        target,
        projection,
        &records,
        &claims,
        host_boot_id.as_deref(),
        server_peer_start_time,
    );
    X11DesktopObservation::new(
        acl,
        uzers::get_effective_uid(),
        host_boot_id,
        server_peer_start_time,
        records,
        diagnostics,
    )
    .with_machine_claims(machine_claims, claims.complete)
    .with_revocation_checks(revocation_checks)
}

pub(super) fn revocation_checks(
    target: &GuestTarget,
    projection: &X11ProjectionContext,
    records: &X11GrantRecordCatalog,
    claims: &MachineClaimCatalog,
    boot_id: Option<&str>,
    server_start_time: Option<u64>,
) -> Vec<X11RecordRevocationCheck> {
    records
        .records
        .iter()
        .filter(|record| {
            record.display == projection.host_socket().display()
                && matches!(record.phase, X11GrantRecordPhase::ConfirmedAdded)
        })
        .map(|record| {
            let request =
                X11RevokeRequest::new(target.clone(), projection.clone(), record.record_id.clone());
            let eligibility = (|| {
                let boot_id = boot_id.ok_or("host boot identity is unavailable")?;
                if record.boot_id != boot_id
                    || Some(record.server_peer_start_time) != server_start_time
                {
                    return Err("X11 server continuity is not confirmed".into());
                }
                let ownership = GrantOwnership::inspect(
                    records,
                    claims,
                    &ManagedX11GrantKey::from_evidence(record),
                    boot_id,
                )?;
                ownership.require_request(&request, uzers::get_effective_uid())?;
                ownership.require_exclusive_consumer(&request)
            })();
            X11RecordRevocationCheck {
                record_id: record.record_id.clone(),
                eligibility,
            }
        })
        .collect()
}

pub(super) fn ensure_access_sync(
    request: &X11AuthorizationRequest,
) -> Result<X11DesktopAuthorization, String> {
    let projection = request.projection();
    let socket = projection.host_socket();
    let connection = connect_current_socket(socket)?;
    let before = read_acl(&connection, socket.display())?;
    require_current_socket(socket, "before changing its ACL")?;

    match before.mode() {
        crate::application::x11::X11AccessControlMode::Disabled => {
            return Ok(X11DesktopAuthorization::new(
                desktop_observation(request.target(), projection, before, None),
                X11AuthorizationDisposition::AccessControlDisabled,
            ));
        }
        crate::application::x11::X11AccessControlMode::Unknown(mode) => {
            return Err(format!(
                "X server returned unsupported access-control mode {mode}; no ACL change was attempted"
            ));
        }
        crate::application::x11::X11AccessControlMode::Enabled => {}
    }

    let state = X11RuntimeState::open()?;
    let _lock = state.lock()?;
    let existing_claims = state.load_claims();
    if !existing_claims.complete {
        let detail = existing_claims
            .diagnostics
            .first()
            .map(String::as_str)
            .unwrap_or("the machine claim set is incomplete");
        return Err(format!(
            "X11 authorization was not attempted because Lasper cannot safely inspect its machine claims: {detail}"
        ));
    }
    // Another Lasper process may have changed the ACL while the lock was
    // being acquired. Re-observe it before deciding whether a mutation is
    // needed; a concurrent switch to disabled access remains a read-only path.
    let before = read_acl(&connection, socket.display())?;
    require_current_socket(socket, "before changing its ACL")?;
    match before.mode() {
        crate::application::x11::X11AccessControlMode::Disabled => {
            return Ok(X11DesktopAuthorization::new(
                desktop_observation(request.target(), projection, before, Some(&state)),
                X11AuthorizationDisposition::AccessControlDisabled,
            ));
        }
        crate::application::x11::X11AccessControlMode::Unknown(mode) => {
            return Err(format!(
                "X server returned unsupported access-control mode {mode}; no ACL change was attempted"
            ));
        }
        crate::application::x11::X11AccessControlMode::Enabled => {}
    }
    let server_peer_start_time = x11_peer_start_time(socket)?;

    let existing_records = state.load_records();
    if !existing_records.complete {
        let detail = existing_records
            .diagnostics
            .first()
            .map(String::as_str)
            .unwrap_or("the grant record set is incomplete");
        return Err(format!(
            "X11 authorization was not attempted because Lasper cannot safely inspect its existing grant records: {detail}"
        ));
    }
    let boot_id = host_boot_id()?;
    let host_uid = projection.identity().host_uid();
    if before.has_numeric_local_user(host_uid) {
        let disposition = match reuse_managed_grant_sync(
            &state,
            &existing_records,
            &existing_claims,
            request,
            server_peer_start_time,
            &boot_id,
        )? {
            Some(reused) => {
                return Ok(X11DesktopAuthorization::new(
                    desktop_observation(request.target(), projection, before, Some(&state))
                        .with_diagnostics(reused.diagnostics),
                    X11AuthorizationDisposition::ReusedManaged {
                        record_id: reused.record_id,
                    },
                ));
            }
            None => X11AuthorizationDisposition::PreExisting,
        };
        return Ok(X11DesktopAuthorization::new(
            desktop_observation(request.target(), projection, before, Some(&state)),
            disposition,
        ));
    }
    if request.purpose() == X11AuthorizationPurpose::ExplicitSession {
        if let Some(record) =
            current_generation_record(&existing_records, request, server_peer_start_time, &boot_id)
        {
            let phase = match &record.phase {
                X11GrantRecordPhase::Pending => "pending",
                X11GrantRecordPhase::ConfirmedAdded => "confirmed but now absent",
                X11GrantRecordPhase::Revoked => "revoked",
                X11GrantRecordPhase::OutcomeUnknown { .. } => "outcome unknown",
            };
            return Err(format!(
                "X11 session authorization was not recreated because grant record {} for this machine instance and X server is {phase}; review the record and authorize access explicitly from Configure > Host Integration > X11",
                record.record_id
            ));
        }
    }

    let record_id = uuid::Uuid::new_v4().simple().to_string();
    let file_name = format!("grant-{record_id}.json");
    let mut record =
        ManagedX11GrantRecord::pending(request, record_id.clone(), server_peer_start_time)?;
    state.write(&file_name, &record)?;

    let (change_result, observed) = insert_and_observe(&connection, socket.display(), host_uid);

    let socket_result = require_current_socket(socket, "while authorizing access");
    let server_result = x11_peer_start_time(socket).and_then(|current| {
        (current == server_peer_start_time)
            .then_some(())
            .ok_or_else(|| "X server process changed while authorizing access".to_owned())
    });
    if authorization_was_confirmed(
        &change_result,
        &observed,
        &socket_result,
        &server_result,
        host_uid,
    ) {
        let after = observed.expect("confirmed observation is successful");
        record.phase = GrantRecordPhase::ConfirmedAdded;
        state.write(&file_name, &record).map_err(|error| {
            format!(
                "X11 access was added, but its operation record could not be finalized ({error}); the pending record was preserved"
            )
        })?;
        let diagnostics = match state.register_machine_claim(
            ManagedX11MachineClaim::active_from_record(&record),
            &existing_claims,
        ) {
            Ok(diagnostics) => diagnostics,
            Err(claim_error) => {
                let reason = format!(
                "X11 access was confirmed, but its machine claim could not be persisted: {claim_error}"
            );
                record.phase = GrantRecordPhase::OutcomeUnknown {
                    reason: reason.clone(),
                };
                let _ = state.write(&file_name, &record);
                return Err(reason);
            }
        };
        return Ok(X11DesktopAuthorization::new(
            desktop_observation(request.target(), projection, after, Some(&state))
                .with_diagnostics(diagnostics),
            X11AuthorizationDisposition::Added { record_id },
        ));
    }

    let reason = [
        change_result.err(),
        observed
            .err()
            .map(|error| format!("confirmation query: {error}")),
        socket_result
            .err()
            .map(|error| format!("endpoint revalidation: {error}")),
        server_result
            .err()
            .map(|error| format!("server generation: {error}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("; ");
    let reason = if reason.is_empty() {
        format!("localuser:#{host_uid} was absent after the X server round trip")
    } else {
        reason
    };
    record.phase = GrantRecordPhase::OutcomeUnknown {
        reason: reason.clone(),
    };
    let record_result = state.write(&file_name, &record);
    Err(match record_result {
        Ok(()) => format!(
            "X11 authorization outcome is unknown: {reason}; operation record {record_id} was preserved"
        ),
        Err(record_error) => format!(
            "X11 authorization outcome is unknown: {reason}; additionally, the pending operation record could not be updated: {record_error}"
        ),
    })
}

#[derive(Debug)]
pub(super) struct ManagedGrantReuse {
    pub(super) record_id: String,
    pub(super) diagnostics: Vec<String>,
}

pub(super) fn reuse_managed_grant_sync(
    state: &X11RuntimeState,
    records: &X11GrantRecordCatalog,
    claims: &MachineClaimCatalog,
    request: &X11AuthorizationRequest,
    server_peer_start_time: u64,
    boot_id: &str,
) -> Result<Option<ManagedGrantReuse>, String> {
    let key = ManagedX11GrantKey::from_projection(request.projection(), server_peer_start_time);
    let Some(record) = records.records.iter().find(|record| {
        record.caller_uid == uzers::get_effective_uid()
            && record.boot_id == boot_id
            && matches!(record.phase, X11GrantRecordPhase::ConfirmedAdded)
            && ManagedX11GrantKey::from_evidence(record) == key
    }) else {
        return Ok(None);
    };
    // Reuse renews this consumer's lifecycle, not the original ACL mutation.
    let claim = ManagedX11MachineClaim::active_for_grant(request, record)?;
    let diagnostics = state
        .register_machine_claim(claim, claims)
        .map_err(|error| format!("X11 grant reuse could not be completed: {error}"))?;
    Ok(Some(ManagedGrantReuse {
        record_id: record.record_id.clone(),
        diagnostics,
    }))
}

pub(super) fn current_generation_record<'a>(
    catalog: &'a X11GrantRecordCatalog,
    request: &X11AuthorizationRequest,
    server_peer_start_time: u64,
    boot_id: &str,
) -> Option<&'a X11GrantRecordEvidence> {
    let projection = request.projection();
    let desired_entry = X11AclEntry::from_wire(
        Family::SERVER_INTERPRETED.into(),
        numeric_local_user_address(projection.identity().host_uid()),
    );
    let caller_uid = uzers::get_effective_uid();
    catalog.records.iter().find(|record| {
        record.target == *request.target()
            && record.display == projection.host_socket().display()
            && record.caller_uid == caller_uid
            && record.boot_id == boot_id
            && record.identity.matches(projection.identity())
            && record.server_peer == projection.host_socket().peer_identity().legacy_tuple()
            && record.server_peer_start_time == server_peer_start_time
            && record.acl_entry == desired_entry
    })
}

pub(super) fn revoke_access_sync(
    request: &X11RevokeRequest,
) -> Result<X11DesktopRevocation, String> {
    let state = X11RuntimeState::open_existing()?
        .ok_or_else(|| "X11 grant record storage does not exist".to_owned())?;
    let _lock = state.lock()?;
    revoke_access_with_state(&state, request, ConnectedDesktopAcl::connect)
}

/// The caller holds the ACL lock. Separating runtime-directory selection from
/// the operation also lets tests exercise the complete revoke flow in isolation.
pub(super) fn revoke_access_with_state<D: DesktopAcl>(
    state: &X11RuntimeState,
    request: &X11RevokeRequest,
    connect: impl FnOnce(&HostX11Socket) -> Result<D, String>,
) -> Result<X11DesktopRevocation, String> {
    let records = state.load_records();
    if !records.complete {
        let detail = records
            .diagnostics
            .first()
            .map(String::as_str)
            .unwrap_or("the grant record set is incomplete");
        return Err(format!(
            "X11 revocation was not attempted because Lasper cannot safely inspect its grant records: {detail}"
        ));
    }
    let claims = state.load_claims();
    if !claims.complete {
        let detail = claims
            .diagnostics
            .first()
            .map(String::as_str)
            .unwrap_or("the machine claim set is incomplete");
        return Err(format!(
            "X11 revocation was not attempted because Lasper cannot safely inspect its machine claims: {detail}"
        ));
    }
    let record_id = request.record_id();
    if !valid_record_id(record_id) {
        return Err("X11 revocation record ID is invalid".to_owned());
    }
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
    let evidence = record
        .clone()
        .into_evidence(&file_name)
        .map_err(|error| format!("invalid X11 grant record {record_id}: {error}"))?;
    if !matches!(evidence.phase, X11GrantRecordPhase::ConfirmedAdded) {
        return Err(format!(
            "X11 grant record {record_id} is not an active confirmed grant"
        ));
    }

    let projection = request.projection();
    let socket = projection.host_socket();
    let current_boot_id = host_boot_id()?;
    if evidence.boot_id != current_boot_id {
        return Err("X11 grant record belongs to another host boot".to_owned());
    }
    let key = ManagedX11GrantKey::from_evidence(&evidence);
    let ownership = GrantOwnership::inspect(&records, &claims, &key, &current_boot_id)?;
    ownership.require_request(request, uzers::get_effective_uid())?;
    let desktop = connect(socket)?;
    if desktop.server_start_time() != evidence.server_peer_start_time {
        return Err("X11 server generation changed since the grant was created".to_owned());
    }
    let before = desktop.snapshot()?;
    match before.mode() {
        crate::application::x11::X11AccessControlMode::Disabled => {
            return Err("X11 access control is disabled; no ACL revoke was attempted".to_owned())
        }
        crate::application::x11::X11AccessControlMode::Unknown(mode) => {
            return Err(format!(
            "X server returned unsupported access-control mode {mode}; no ACL revoke was attempted"
        ))
        }
        crate::application::x11::X11AccessControlMode::Enabled => {}
    }

    if !before.contains(&evidence.acl_entry) {
        // Absence does not prove that other consumers ended. Retire only the
        // claims matched to this request, then finish shared records only if
        // no unresolved consumer or operation remains.
        state.end_consumer_claims(&ownership.consumer_claims(request)).map_err(|error| {
            format!("the exact ACL entry was already absent, but this consumer's claims could not be ended: {error}")
        })?;
        let remaining = state.load_claims();
        let ownership = GrantOwnership::inspect(&records, &remaining, &key, &current_boot_id)?;
        let diagnostics = match ownership.require_exclusive_consumer(request) {
            Ok(()) => {
                state.finish_revocation(&ownership.records).map_err(|error| {
                    format!("the exact ACL entry was already absent, but its records could not be finalized: {error}")
                })?;
                Vec::new()
            }
            Err(reason) => vec![format!(
                "the exact ACL entry is absent; shared records were retained: {reason}"
            )],
        };
        return Ok(X11DesktopRevocation::new(
            desktop_observation(request.target(), projection, before, Some(state))
                .with_diagnostics(diagnostics),
            X11RevocationDisposition::AlreadyAbsent {
                record_id: record_id.to_owned(),
            },
        ));
    }

    ownership.require_exclusive_consumer(request)?;

    let (change_result, observed) = desktop.remove(evidence.identity.host_uid());
    let endpoint_result = desktop.verify();
    let confirmed = change_result.is_ok()
        && endpoint_result.is_ok()
        && observed
            .as_ref()
            .is_ok_and(|snapshot| !snapshot.contains(&evidence.acl_entry));
    if confirmed {
        let after = observed.expect("confirmed observation is successful");
        state
            .finish_revocation(&ownership.records)
            .map_err(|error| {
                format!("X11 access was revoked, but its records could not be finalized: {error}")
            })?;
        return Ok(X11DesktopRevocation::new(
            desktop_observation(request.target(), projection, after, Some(state)),
            X11RevocationDisposition::Revoked {
                record_id: record_id.to_owned(),
            },
        ));
    }

    let reason = [
        change_result.err(),
        observed
            .err()
            .map(|error| format!("confirmation query: {error}")),
        endpoint_result
            .err()
            .map(|error| format!("endpoint revalidation: {error}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("; ");
    let reason = if reason.is_empty() {
        "the exact ACL entry was still present after the X server round trip".to_owned()
    } else {
        reason
    };
    record.phase = GrantRecordPhase::OutcomeUnknown {
        reason: reason.clone(),
    };
    let record_result = state.write(&file_name, &record);
    Err(match record_result {
        Ok(()) => format!(
            "X11 revocation outcome is unknown: {reason}; operation record {record_id} was preserved"
        ),
        Err(record_error) => format!(
            "X11 revocation outcome is unknown: {reason}; additionally, the operation record could not be updated: {record_error}"
        ),
    })
}
