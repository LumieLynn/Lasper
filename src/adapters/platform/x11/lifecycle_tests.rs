//! Exercise persisted claims across multiple worker invocations. These tests
//! never modify the desktop ACL or install units in the real user manager.

use super::access::{reuse_managed_grant_sync, revocation_checks, revoke_access_with_state};
use super::common::CLAIM_RECONCILE_GRACE_MILLIS;
use super::lifecycle::*;
use super::state::*;
use super::tests::{grant_request, grant_request_for_instance};
use super::transport::DesktopAcl;
use crate::adapters::trusted_state::TrustedDirectory;
use crate::application::sessions::ObservedMachineInstance;
use crate::application::x11::{
    X11AclSnapshot, X11GrantRecordPhase, X11ReconcileRetry, X11RevokeRequest,
};
use std::os::unix::fs::PermissionsExt;

struct Fixture {
    _directory: tempfile::TempDir,
    state: X11RuntimeState,
    record: ManagedX11GrantRecord,
}

impl Fixture {
    fn new() -> Self {
        Self::with_request(grant_request())
    }

    fn with_request(request: crate::application::x11::X11AuthorizationRequest) -> Self {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let access =
            TrustedDirectory::open_existing(directory.path(), uzers::get_effective_uid()).unwrap();
        let state = X11RuntimeState {
            grants: access.open_or_create_child("grants", 0o700).unwrap(),
            claims: Some(access.open_or_create_child("claims", 0o700).unwrap()),
            access,
        };
        let mut record =
            ManagedX11GrantRecord::pending(&request, "0123456789abcdef0123456789abcdef".into(), 77)
                .unwrap();
        record.phase = GrantRecordPhase::ConfirmedAdded;
        state
            .write(&format!("grant-{}.json", record.record_id), &record)
            .unwrap();
        state
            .write_claim(&ManagedX11MachineClaim::active_from_record(&record))
            .unwrap();
        Self {
            _directory: directory,
            state,
            record,
        }
    }

    fn claim(&self) -> ManagedX11MachineClaim {
        let catalog = self.state.load_claims();
        assert!(catalog.complete);
        assert_eq!(catalog.claims.len(), 1);
        catalog.claims.into_iter().next().unwrap()
    }

    fn phase(&self, phase: MachineClaimPhase) {
        let mut claim = self.claim();
        claim.phase = phase;
        self.state.write_claim(&claim).unwrap();
    }

    fn step(&self, registration: SystemMachineRegistration, now: u64) -> ReconcileClaimStatus {
        reconcile_claim(
            &self.state,
            &self.claim(),
            &self.record.boot_id,
            registration,
            now,
        )
        .unwrap()
    }

    fn registered(&self) -> SystemMachineRegistration {
        registered(self.claim().machine_instance().unwrap())
    }

    fn watcher_needed(&self) -> bool {
        claims_need_activation(&self.state.load_claims(), &self.state.load_records()).unwrap()
    }

    fn end_claims(&self) {
        let claims = self.state.load_claims();
        assert!(claims.complete);
        for mut claim in claims.claims {
            claim.phase = MachineClaimPhase::Ended;
            self.state.write_claim(&claim).unwrap();
        }
    }

    fn revocation_check(
        &self,
        request: &crate::application::x11::X11AuthorizationRequest,
    ) -> Result<(), String> {
        revocation_checks(
            request.target(),
            request.projection(),
            &self.state.load_records(),
            &self.state.load_claims(),
            Some(&self.record.boot_id),
            Some(77),
        )
        .into_iter()
        .find(|check| check.record_id == self.record.record_id)
        .expect("fixture has a confirmed grant record")
        .eligibility
    }
}

fn registered(instance: ObservedMachineInstance) -> SystemMachineRegistration {
    SystemMachineRegistration::Present {
        leader_pid: instance.leader_pid(),
        instance: Some(instance),
    }
}

struct Desktop {
    acl: std::cell::RefCell<X11AclSnapshot>,
    removals: std::cell::Cell<usize>,
}

impl Desktop {
    fn for_fixture(fixture: &Fixture) -> Self {
        let entry = fixture.state.load_records().records[0].acl_entry.clone();
        Self {
            acl: std::cell::RefCell::new(X11AclSnapshot::from_wire(1, vec![entry])),
            removals: std::cell::Cell::new(0),
        }
    }
}

impl DesktopAcl for &Desktop {
    fn server_start_time(&self) -> u64 {
        77
    }
    fn snapshot(&self) -> Result<X11AclSnapshot, String> {
        Ok(self.acl.borrow().clone())
    }
    fn remove(&self, uid: u32) -> (Result<(), String>, Result<X11AclSnapshot, String>) {
        assert!(self.acl.borrow().has_numeric_local_user(uid));
        self.removals.set(self.removals.get() + 1);
        *self.acl.borrow_mut() = X11AclSnapshot::from_wire(1, Vec::new());
        (Ok(()), self.snapshot())
    }
    fn verify(&self) -> Result<(), String> {
        Ok(())
    }
}

fn restarted_request() -> crate::application::x11::X11AuthorizationRequest {
    grant_request_for_instance(ObservedMachineInstance::from_process(
        43,
        std::num::NonZeroU64::new(88).unwrap(),
        None,
    ))
}

#[test]
fn current_claim_can_revoke_a_grant_record_from_before_restart() {
    let fixture = Fixture::new();
    let request = restarted_request();
    reuse_managed_grant_sync(
        &fixture.state,
        &fixture.state.load_records(),
        &fixture.state.load_claims(),
        &request,
        77,
        &fixture.record.boot_id,
    )
    .unwrap();
    let desktop = Desktop::for_fixture(&fixture);
    let revoke = X11RevokeRequest::new(
        request.target().clone(),
        request.projection().clone(),
        fixture.record.record_id.clone(),
    );
    let _lock = fixture.state.lock().unwrap();
    assert!(fixture.revocation_check(&request).is_ok());
    revoke_access_with_state(&fixture.state, &revoke, |_| Ok(&desktop)).unwrap();
    assert_eq!(desktop.removals.get(), 1);
    assert_eq!(
        fixture.state.load_records().records[0].phase,
        X11GrantRecordPhase::Revoked
    );
    assert!(!fixture.watcher_needed());
}

#[test]
fn manual_revoke_protects_consumers_across_records_and_a_record_without_its_own_claim() {
    for same_record in [true, false] {
        for original_ended in [false, true] {
            let fixture = Fixture::new();
            let request = grant_request();
            assert!(fixture.revocation_check(&request).is_ok());
            let mut other = fixture.record.clone();
            other.machine = "fedora".into();
            if !same_record {
                other.record_id = "11111111111111111111111111111111".into();
                fixture
                    .state
                    .write(&format!("grant-{}.json", other.record_id), &other)
                    .unwrap();
            }
            let mut other_claim = ManagedX11MachineClaim::active_from_record(&other);
            other_claim.claim_id = "22222222222222222222222222222222".into();
            fixture.state.write_claim(&other_claim).unwrap();
            if original_ended {
                let mut original = ManagedX11MachineClaim::active_from_record(&fixture.record);
                original.phase = MachineClaimPhase::Ended;
                fixture.state.write_claim(&original).unwrap();
            }
            let desktop = Desktop::for_fixture(&fixture);
            let revoke = X11RevokeRequest::new(
                request.target().clone(),
                request.projection().clone(),
                fixture.record.record_id.clone(),
            );
            let _lock = fixture.state.lock().unwrap();
            assert!(fixture
                .revocation_check(&request)
                .unwrap_err()
                .contains("shared with unresolved machine claim"));
            let error =
                revoke_access_with_state(&fixture.state, &revoke, |_| Ok(&desktop)).unwrap_err();
            assert!(
                error.contains("shared with unresolved machine claim"),
                "{error}"
            );
            assert_eq!(desktop.removals.get(), 0);
            assert!(fixture
                .state
                .load_records()
                .records
                .iter()
                .all(|record| record.phase == X11GrantRecordPhase::ConfirmedAdded));
        }
    }
}

#[test]
fn listener_change_is_blocked_by_both_inspection_and_revoke() {
    let fixture = Fixture::new();
    let request = grant_request();
    let old = request.projection();
    let socket = old.host_socket();
    let changed = crate::domain::x11::HostX11Socket::from_verified_parts(
        socket.display(),
        true,
        "/tmp/.X11-unix/X0_".into(),
        "/tmp/.X11-unix/X0_".into(),
        socket.owner_uid(),
        socket.owner_gid(),
        socket.mode(),
        socket.peer_identity().legacy_tuple().0,
        socket.peer_identity().legacy_tuple().1,
        socket.peer_identity().legacy_tuple().2,
        socket.revision(),
    )
    .unwrap();
    let projection = crate::application::sessions::X11ProjectionContext::verified(
        changed,
        old.guest_mount().to_path_buf(),
        old.guest_client_path().to_path_buf(),
        old.filesystem_access(),
        old.identity(),
    );
    let changed_request = crate::application::x11::X11AuthorizationRequest::new(
        request.target().clone(),
        projection.clone(),
    );
    let reason = fixture.revocation_check(&changed_request).unwrap_err();
    assert!(reason.contains("selected X11 endpoint differs"), "{reason}");
    let revoke = X11RevokeRequest::new(
        request.target().clone(),
        projection,
        fixture.record.record_id.clone(),
    );
    let _lock = fixture.state.lock().unwrap();
    let desktop = Desktop::for_fixture(&fixture);
    let error = revoke_access_with_state(&fixture.state, &revoke, |_| Ok(&desktop)).unwrap_err();
    assert_eq!(error, reason);
    assert_eq!(desktop.removals.get(), 0);
}

#[test]
fn absent_acl_retires_only_the_requested_consumer() {
    for same_record in [true, false] {
        for other_phase in [
            MachineClaimPhase::Active,
            MachineClaimPhase::Unknown {
                reason: "uncertain ACL change".into(),
            },
        ] {
            let fixture = Fixture::new();
            let request = grant_request();
            let mut other = fixture.record.clone();
            other.machine = "fedora".into();
            if !same_record {
                other.record_id = "11111111111111111111111111111111".into();
                fixture
                    .state
                    .write(&format!("grant-{}.json", other.record_id), &other)
                    .unwrap();
            }
            let mut claim = ManagedX11MachineClaim::active_from_record(&other);
            claim.claim_id = "22222222222222222222222222222222".into();
            claim.phase = other_phase.clone();
            fixture.state.write_claim(&claim).unwrap();
            let desktop = Desktop::for_fixture(&fixture);
            *desktop.acl.borrow_mut() = X11AclSnapshot::from_wire(1, Vec::new());
            let revoke = X11RevokeRequest::new(
                request.target().clone(),
                request.projection().clone(),
                fixture.record.record_id.clone(),
            );
            let _lock = fixture.state.lock().unwrap();
            revoke_access_with_state(&fixture.state, &revoke, |_| Ok(&desktop)).unwrap();
            let claims = fixture.state.load_claims();
            assert_eq!(
                claims
                    .claims
                    .iter()
                    .find(|claim| claim.claim_id == fixture.record.record_id)
                    .unwrap()
                    .phase,
                MachineClaimPhase::Ended
            );
            assert_eq!(
                claims
                    .claims
                    .iter()
                    .find(|current| current.claim_id == claim.claim_id)
                    .unwrap()
                    .phase,
                other_phase
            );
            assert_eq!(desktop.removals.get(), 0);
            assert!(fixture
                .state
                .load_records()
                .records
                .iter()
                .all(|record| record.phase == X11GrantRecordPhase::ConfirmedAdded));
            assert!(fixture.watcher_needed());
        }
    }
}

#[test]
fn absent_acl_preserves_uncertain_operations_even_after_this_consumer_ends() {
    let fixture = Fixture::new();
    let request = grant_request();
    let mut uncertain = fixture.record.clone();
    uncertain.record_id = "11111111111111111111111111111111".into();
    uncertain.phase = GrantRecordPhase::Pending;
    fixture
        .state
        .write(&format!("grant-{}.json", uncertain.record_id), &uncertain)
        .unwrap();
    let desktop = Desktop::for_fixture(&fixture);
    *desktop.acl.borrow_mut() = X11AclSnapshot::from_wire(1, Vec::new());
    let revoke = X11RevokeRequest::new(
        request.target().clone(),
        request.projection().clone(),
        fixture.record.record_id.clone(),
    );
    let _lock = fixture.state.lock().unwrap();
    revoke_access_with_state(&fixture.state, &revoke, |_| Ok(&desktop)).unwrap();
    assert_eq!(fixture.claim().phase, MachineClaimPhase::Ended);
    let records = fixture.state.load_records();
    assert!(records
        .records
        .iter()
        .any(|record| record.record_id == uncertain.record_id
            && record.phase == X11GrantRecordPhase::Pending));
    assert!(fixture.watcher_needed());
    assert_eq!(desktop.removals.get(), 0);
}

#[test]
fn absent_acl_finalizes_records_when_this_is_the_last_known_consumer() {
    let fixture = Fixture::new();
    let request = grant_request();
    let desktop = Desktop::for_fixture(&fixture);
    *desktop.acl.borrow_mut() = X11AclSnapshot::from_wire(1, Vec::new());
    let revoke = X11RevokeRequest::new(
        request.target().clone(),
        request.projection().clone(),
        fixture.record.record_id.clone(),
    );
    let _lock = fixture.state.lock().unwrap();
    revoke_access_with_state(&fixture.state, &revoke, |_| Ok(&desktop)).unwrap();
    assert_eq!(fixture.claim().phase, MachineClaimPhase::Ended);
    assert_eq!(
        fixture.state.load_records().records[0].phase,
        X11GrantRecordPhase::Revoked
    );
    assert!(!fixture.watcher_needed());
    assert_eq!(desktop.removals.get(), 0);
}

#[test]
fn cross_record_handoff_keeps_history_and_finishes_every_record_after_revoke() {
    let fixture = Fixture::new();
    let request = restarted_request();
    let mut newer =
        ManagedX11GrantRecord::pending(&request, "11111111111111111111111111111111".into(), 77)
            .unwrap();
    newer.phase = GrantRecordPhase::ConfirmedAdded;
    fixture
        .state
        .write(&format!("grant-{}.json", newer.record_id), &newer)
        .unwrap();
    fixture
        .state
        .register_machine_claim(
            ManagedX11MachineClaim::active_from_record(&newer),
            &fixture.state.load_claims(),
        )
        .unwrap();
    let claims = fixture.state.load_claims();
    assert_eq!(
        claims
            .claims
            .iter()
            .filter(|claim| claim.phase.requires_reconcile())
            .count(),
        1
    );
    assert_eq!(fixture.state.load_records().records.len(), 2);
    let desktop = Desktop::for_fixture(&fixture);
    let revoke = X11RevokeRequest::new(
        request.target().clone(),
        request.projection().clone(),
        newer.record_id,
    );
    let _lock = fixture.state.lock().unwrap();
    revoke_access_with_state(&fixture.state, &revoke, |_| Ok(&desktop)).unwrap();
    assert_eq!(desktop.removals.get(), 1);
    assert!(fixture
        .state
        .load_records()
        .records
        .iter()
        .all(|record| record.phase == X11GrantRecordPhase::Revoked));
    assert!(!fixture.watcher_needed());
}

#[test]
fn partial_claim_handoff_reports_saved_consumer_and_can_be_retried() {
    let fixture = Fixture::new();
    let catalog = fixture.state.load_claims();
    let request = restarted_request();
    let evidence = fixture.state.load_records().records.remove(0);
    let old_path = fixture
        ._directory
        .path()
        .join("claims")
        .join(format!("claim-{}.json", fixture.record.record_id));
    let backup = old_path.with_extension("backup");
    std::fs::rename(&old_path, &backup).unwrap();
    std::fs::create_dir(&old_path).unwrap();
    let current = ManagedX11MachineClaim::active_for_grant(&request, &evidence).unwrap();
    let warnings = fixture
        .state
        .register_machine_claim(current.clone(), &catalog)
        .unwrap();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("was saved, but predecessor"));
    let saved = fixture
        ._directory
        .path()
        .join("claims")
        .join(format!("claim-{}.json", current.claim_id));
    assert!(saved.is_file());
    std::fs::remove_dir(&old_path).unwrap();
    std::fs::rename(&backup, &old_path).unwrap();
    let claims = fixture.state.load_claims();
    assert!(fixture
        .state
        .register_machine_claim(current, &claims)
        .unwrap()
        .is_empty());
    let claims = fixture.state.load_claims();
    assert_eq!(claims.claims.len(), 2);
    assert_eq!(
        claims
            .claims
            .iter()
            .filter(|claim| claim.phase == MachineClaimPhase::Active)
            .count(),
        1
    );
}

#[test]
fn pre_revoke_registration_reappearance_cancels_mutation_and_keeps_retry() {
    let fixture = Fixture::new();
    fixture.phase(MachineClaimPhase::CleanupPending {
        since_unix_millis: 1,
    });
    let desktop = Desktop::for_fixture(&fixture);
    let request = grant_request();
    let mut observations = [SystemMachineRegistration::Absent, fixture.registered()].into_iter();
    let _lock = fixture.state.lock().unwrap();
    let report = reconcile_with_state(
        &fixture.state,
        request.projection().host_socket(),
        |_| Ok(&desktop),
        |_| observations.next().unwrap(),
        || Ok(1 + CLAIM_RECONCILE_GRACE_MILLIS),
    )
    .unwrap();
    assert_eq!(desktop.removals.get(), 0);
    assert_eq!(report.retry(), X11ReconcileRetry::RegisteredMachine);
    assert_eq!(fixture.claim().phase, MachineClaimPhase::Active);
    // A backwards wall-clock adjustment can also make the last observation
    // pending. It must cancel mutation and retain the grace-period retry.
    fixture.phase(MachineClaimPhase::CleanupPending {
        since_unix_millis: 100,
    });
    let mut times = [100 + CLAIM_RECONCILE_GRACE_MILLIS, 101].into_iter();
    let report = reconcile_with_state(
        &fixture.state,
        request.projection().host_socket(),
        |_| Ok(&desktop),
        |_| SystemMachineRegistration::Absent,
        || Ok(times.next().unwrap()),
    )
    .unwrap();
    assert_eq!(desktop.removals.get(), 0);
    assert_eq!(report.retry(), X11ReconcileRetry::PendingObservation);
    assert_eq!(
        report.pending_record_ids(),
        std::slice::from_ref(&fixture.record.record_id)
    );
}

#[test]
fn cleanup_finishes_retired_record_history_but_preserves_an_unexplained_record() {
    for has_history in [true, false] {
        let fixture = Fixture::new();
        let request = grant_request();
        let mut old = fixture.record.clone();
        old.record_id = "11111111111111111111111111111111".into();
        fixture
            .state
            .write(&format!("grant-{}.json", old.record_id), &old)
            .unwrap();
        if has_history {
            let mut ended = ManagedX11MachineClaim::active_from_record(&old);
            ended.phase = MachineClaimPhase::Ended;
            fixture.state.write_claim(&ended).unwrap();
        }
        let mut pending = ManagedX11MachineClaim::active_from_record(&fixture.record);
        pending.phase = MachineClaimPhase::CleanupPending {
            since_unix_millis: 1,
        };
        fixture.state.write_claim(&pending).unwrap();
        let desktop = Desktop::for_fixture(&fixture);
        let _lock = fixture.state.lock().unwrap();
        let report = reconcile_with_state(
            &fixture.state,
            request.projection().host_socket(),
            |_| Ok(&desktop),
            |_| SystemMachineRegistration::Absent,
            || Ok(1 + CLAIM_RECONCILE_GRACE_MILLIS),
        )
        .unwrap();
        if has_history {
            assert_eq!(desktop.removals.get(), 1);
            assert!(report.is_complete());
            assert_eq!(report.revoked_record_ids().len(), 2);
            assert!(!fixture.watcher_needed());
        } else {
            assert_eq!(desktop.removals.get(), 0);
            assert!(report
                .diagnostics()
                .iter()
                .any(|message| message.contains("has no machine claim")));
            assert!(fixture.watcher_needed());
        }
    }
}

#[test]
fn ended_claims_only_finalize_absent_acl_entries() {
    for entry_present in [false, true] {
        let fixture = Fixture::new();
        fixture.end_claims();
        let request = grant_request();
        let desktop = Desktop::for_fixture(&fixture);
        if !entry_present {
            *desktop.acl.borrow_mut() = X11AclSnapshot::from_wire(1, Vec::new());
        }
        let _lock = fixture.state.lock().unwrap();
        let report = reconcile_with_state(
            &fixture.state,
            request.projection().host_socket(),
            |_| Ok(&desktop),
            |_| panic!("ended claims do not need another machine observation"),
            || Ok(1),
        )
        .unwrap();
        assert_eq!(desktop.removals.get(), 0);
        if entry_present {
            assert!(report
                .diagnostics()
                .iter()
                .any(|message| message.contains("all its machine claims have ended")));
            assert!(fixture.watcher_needed());
        } else {
            assert!(report.is_complete());
            assert_eq!(
                report.revoked_record_ids(),
                std::slice::from_ref(&fixture.record.record_id)
            );
            assert!(!fixture.watcher_needed());
        }
    }
}

#[test]
fn interrupted_record_finalization_can_finish_without_repeating_acl_removal() {
    let fixture = Fixture::new();
    let request = grant_request();
    let records = fixture.state.load_records();
    let record_path = fixture
        ._directory
        .path()
        .join("grants")
        .join(format!("grant-{}.json", fixture.record.record_id));
    let backup = record_path.with_extension("backup");
    let _lock = fixture.state.lock().unwrap();
    std::fs::rename(&record_path, &backup).unwrap();
    std::fs::create_dir(&record_path).unwrap();
    let error = fixture
        .state
        .finish_revocation(&[&records.records[0]])
        .unwrap_err();
    assert!(error.contains("grant record"), "{error}");
    assert_eq!(fixture.claim().phase, MachineClaimPhase::Ended);
    std::fs::remove_dir(&record_path).unwrap();
    std::fs::rename(&backup, &record_path).unwrap();
    assert!(fixture.watcher_needed());
    let desktop = Desktop::for_fixture(&fixture);
    *desktop.acl.borrow_mut() = X11AclSnapshot::from_wire(1, Vec::new());
    let report = reconcile_with_state(
        &fixture.state,
        request.projection().host_socket(),
        |_| Ok(&desktop),
        |_| panic!("the consumer was already retired"),
        || Ok(1),
    )
    .unwrap();
    assert!(report.is_complete());
    assert_eq!(desktop.removals.get(), 0);
    assert_eq!(
        report.revoked_record_ids(),
        std::slice::from_ref(&fixture.record.record_id)
    );
    assert!(!fixture.watcher_needed());
}

#[test]
fn manual_revoke_does_not_mutate_a_key_with_an_uncertain_operation() {
    for phase in [
        GrantRecordPhase::Pending,
        GrantRecordPhase::OutcomeUnknown {
            reason: "interrupted change".into(),
        },
    ] {
        let fixture = Fixture::new();
        let request = grant_request();
        let mut uncertain = fixture.record.clone();
        uncertain.record_id = "11111111111111111111111111111111".into();
        uncertain.phase = phase;
        fixture
            .state
            .write(&format!("grant-{}.json", uncertain.record_id), &uncertain)
            .unwrap();
        let desktop = Desktop::for_fixture(&fixture);
        let revoke = X11RevokeRequest::new(
            request.target().clone(),
            request.projection().clone(),
            fixture.record.record_id.clone(),
        );
        let _lock = fixture.state.lock().unwrap();
        assert!(fixture
            .revocation_check(&request)
            .unwrap_err()
            .contains("unresolved ACL operation for the same key"));
        let error =
            revoke_access_with_state(&fixture.state, &revoke, |_| Ok(&desktop)).unwrap_err();
        assert!(
            error.contains("unresolved ACL operation for the same key"),
            "{error}"
        );
        assert_eq!(desktop.removals.get(), 0);
    }
}

#[test]
fn process_only_claims_survive_reconcile_then_stop_without_namespace_access() {
    let instance =
        ObservedMachineInstance::from_process(42, std::num::NonZeroU64::new(77).unwrap(), None);
    let fixture = Fixture::with_request(grant_request_for_instance(instance));
    assert_eq!(
        fixture.step(fixture.registered(), 1),
        ReconcileClaimStatus::Active,
    );
    assert_eq!(
        fixture.step(SystemMachineRegistration::Absent, 100),
        ReconcileClaimStatus::Pending
    );
    assert_eq!(
        fixture.step(
            SystemMachineRegistration::Absent,
            100 + CLAIM_RECONCILE_GRACE_MILLIS
        ),
        ReconcileClaimStatus::Ready,
    );
}

#[test]
fn replaced_instance_reaches_cleanup_without_authorizing_its_replacement() {
    let first =
        ObservedMachineInstance::from_process(42, std::num::NonZeroU64::new(77).unwrap(), None);
    let replacement =
        ObservedMachineInstance::from_process(42, std::num::NonZeroU64::new(88).unwrap(), None);
    for phase in [
        MachineClaimPhase::Active,
        MachineClaimPhase::CleanupPending {
            since_unix_millis: 1,
        },
    ] {
        let fixture = Fixture::with_request(grant_request_for_instance(first));
        fixture.phase(phase);
        let first_pass = fixture.step(registered(replacement), 100);
        assert_eq!(first_pass, ReconcileClaimStatus::Pending);
        assert_eq!(
            fixture.step(registered(replacement), 100 + CLAIM_RECONCILE_GRACE_MILLIS),
            ReconcileClaimStatus::Ready,
        );
        let records = fixture.state.load_records();
        let old_claim = fixture.claim();
        assert!(!old_claim
            .active_evidence(&records.records[0])
            .unwrap()
            .matches(
                &records.records[0],
                grant_request().target(),
                grant_request_for_instance(replacement)
                    .projection()
                    .identity(),
            ));
    }
}

#[test]
fn registered_machine_cancels_pending_cleanup_and_requires_a_new_grace_period() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.step(SystemMachineRegistration::Absent, 100),
        ReconcileClaimStatus::Pending,
    );
    assert_eq!(
        fixture.step(fixture.registered(), 101),
        ReconcileClaimStatus::Active,
    );
    assert_eq!(fixture.claim().phase, MachineClaimPhase::Active);
    let stopped_again = 100 + CLAIM_RECONCILE_GRACE_MILLIS;
    assert_eq!(
        fixture.step(SystemMachineRegistration::Absent, stopped_again),
        ReconcileClaimStatus::Pending,
    );
    assert_eq!(
        fixture.step(
            SystemMachineRegistration::Absent,
            stopped_again + CLAIM_RECONCILE_GRACE_MILLIS
        ),
        ReconcileClaimStatus::Ready,
    );
    assert!(fixture.watcher_needed());
}

#[test]
fn unprivileged_observation_then_stop_reaches_cleanup_across_worker_restarts() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.step(
            SystemMachineRegistration::Present {
                leader_pid: fixture.record.machine_leader_pid,
                instance: None,
            },
            1
        ),
        ReconcileClaimStatus::Active
    );
    assert!(matches!(fixture.claim().phase, MachineClaimPhase::Active));
    assert!(fixture.watcher_needed());

    assert_eq!(
        fixture.step(SystemMachineRegistration::Absent, 100),
        ReconcileClaimStatus::Pending
    );
    assert_eq!(
        fixture.claim().phase,
        MachineClaimPhase::CleanupPending {
            since_unix_millis: 100
        }
    );
    assert!(fixture.watcher_needed());
    assert_eq!(
        fixture.step(
            SystemMachineRegistration::Absent,
            100 + CLAIM_RECONCILE_GRACE_MILLIS - 1
        ),
        ReconcileClaimStatus::Pending
    );
    assert_eq!(
        fixture.step(
            SystemMachineRegistration::Absent,
            100 + CLAIM_RECONCILE_GRACE_MILLIS
        ),
        ReconcileClaimStatus::Ready
    );
    assert!(fixture.watcher_needed()); // Readiness alone never means the ACL was removed.

    fixture
        .state
        .mark_record_phase(&fixture.record.record_id, GrantRecordPhase::Revoked)
        .unwrap();
    assert!(fixture.watcher_needed()); // Claim finalization must also finish.
    fixture.end_claims();
    assert!(!fixture.watcher_needed());
}

#[test]
fn legacy_namespace_failure_is_reobserved_without_manual_record_editing() {
    let fixture = Fixture::new();
    fixture.phase(MachineClaimPhase::Unknown {
        reason: "machine namespace identity is unavailable; cleanup is blocked".into(),
    });
    assert!(fixture.watcher_needed());
    assert_eq!(
        fixture.step(fixture.registered(), 1),
        ReconcileClaimStatus::Active
    );
    assert!(matches!(fixture.claim().phase, MachineClaimPhase::Active));

    // A worker may first see the old persisted failure after the machine stopped.
    fixture.phase(MachineClaimPhase::Unknown {
        reason: "cannot inspect system machine instance /run/systemd/machines/archlinux: Permission denied (os error 13)".into(),
    });
    assert_eq!(
        fixture.step(SystemMachineRegistration::Absent, 100),
        ReconcileClaimStatus::Pending
    );
    assert_eq!(
        fixture.step(
            SystemMachineRegistration::Absent,
            100 + CLAIM_RECONCILE_GRACE_MILLIS
        ),
        ReconcileClaimStatus::Ready
    );
}

#[test]
fn failed_observation_resets_grace_period_and_remains_retryable() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.step(SystemMachineRegistration::Absent, 100),
        ReconcileClaimStatus::Pending
    );
    assert!(matches!(
        fixture.step(
            SystemMachineRegistration::Unknown("registration read failed".into()),
            101
        ),
        ReconcileClaimStatus::Unavailable(_)
    ));
    assert!(matches!(fixture.claim().phase, MachineClaimPhase::Active));
    assert!(fixture.watcher_needed());

    let restart = 100 + CLAIM_RECONCILE_GRACE_MILLIS;
    assert_eq!(
        fixture.step(SystemMachineRegistration::Absent, restart),
        ReconcileClaimStatus::Pending
    );
    assert_eq!(
        fixture.step(
            SystemMachineRegistration::Absent,
            restart + CLAIM_RECONCILE_GRACE_MILLIS - 1
        ),
        ReconcileClaimStatus::Pending
    );
    assert_eq!(
        fixture.step(
            SystemMachineRegistration::Absent,
            restart + CLAIM_RECONCILE_GRACE_MILLIS
        ),
        ReconcileClaimStatus::Ready
    );
}

#[test]
fn old_registration_review_states_resume_lifecycle_observation() {
    let fixture = Fixture::new();
    for reason in [
        "machine registration belongs to a different machine instance",
        "machine leader changed but namespace identity is unavailable",
        "machine registration reappeared before cleanup; explicit preparation is required",
        "machine registration reappeared but namespace identity is unavailable; explicit preparation is required",
        "a different machine instance reappeared before cleanup; explicit preparation is required",
    ] {
        fixture.phase(MachineClaimPhase::NeedsReview {
            reason: reason.into(),
        });
        assert_eq!(
            fixture.step(fixture.registered(), 1),
            ReconcileClaimStatus::Active
        );
        assert_eq!(fixture.claim().phase, MachineClaimPhase::Active);
        fixture.phase(MachineClaimPhase::NeedsReview {
            reason: reason.into(),
        });
        assert_eq!(
            fixture.step(SystemMachineRegistration::Absent, 100),
            ReconcileClaimStatus::Pending
        );
        assert_eq!(
            fixture.step(
                SystemMachineRegistration::Absent,
                100 + CLAIM_RECONCILE_GRACE_MILLIS
            ),
            ReconcileClaimStatus::Ready
        );
        assert!(fixture.watcher_needed());
    }
}

#[test]
fn uncertain_acl_outcome_is_not_recovered_as_an_observation_failure() {
    let fixture = Fixture::new();
    for reason in [
        "X11 connection closed",
        "ACL confirmation: connection lost",
        "unrecognized legacy failure",
    ] {
        let phase = MachineClaimPhase::Unknown {
            reason: reason.into(),
        };
        fixture.phase(phase.clone());
        assert!(matches!(
            fixture.step(SystemMachineRegistration::Absent, 100),
            ReconcileClaimStatus::Blocked(_)
        ));
        assert_eq!(fixture.claim().phase, phase);
        assert!(fixture.watcher_needed());
    }
}

#[test]
fn shared_grant_waits_for_every_machine_and_all_observation_failures() {
    let fixture = Fixture::new();
    let claim = fixture.claim();
    for status in [
        ReconcileClaimStatus::Active,
        ReconcileClaimStatus::Pending,
        ReconcileClaimStatus::Unavailable("read failed".into()),
        ReconcileClaimStatus::Blocked("read failed".into()),
    ] {
        let mut group = ReconcileGroup::new(ReconcileClaim {
            claim_id: claim.claim_id.clone(),
            record_id: claim.grant_record_id.clone(),
            key: claim.key.clone(),
            status: ReconcileClaimStatus::Ready,
        });
        group.add(ReconcileClaim {
            claim_id: "another-claim".into(),
            record_id: "another-grant".into(),
            key: claim.key.clone(),
            status,
        });
        assert!(!group.ready());
    }
}

#[test]
fn reusing_a_managed_acl_persists_each_machine_without_duplicate_mutation_records() {
    let first =
        ObservedMachineInstance::from_process(42, std::num::NonZeroU64::new(77).unwrap(), None);
    let second =
        ObservedMachineInstance::from_process(43, std::num::NonZeroU64::new(88).unwrap(), None);
    let fixture = Fixture::with_request(grant_request_for_instance(first));
    let request = grant_request_for_instance(second);
    let request = crate::application::x11::X11AuthorizationRequest::for_explicit_session(
        crate::application::sessions::GuestTarget::new(
            crate::domain::machine::MachineName::new("fedora").unwrap(),
            request.target().user().clone(),
        ),
        request.projection().clone(),
    );
    for _ in 0..2 {
        assert_eq!(
            reuse_managed_grant_sync(
                &fixture.state,
                &fixture.state.load_records(),
                &fixture.state.load_claims(),
                &request,
                77,
                &fixture.record.boot_id,
            )
            .unwrap()
            .map(|reused| reused.record_id),
            Some(fixture.record.record_id.clone()),
        );
        assert_eq!(fixture.state.load_claims().claims.len(), 2);
        assert_eq!(fixture.state.load_records().records.len(), 1);
    }
    let claims = fixture.state.load_claims();
    assert!(claims.complete);
    let original = claims
        .claims
        .iter()
        .find(|claim| claim.machine == "archlinux")
        .unwrap();
    let reused = claims
        .claims
        .iter()
        .find(|claim| claim.machine == "fedora")
        .unwrap();
    assert_ne!(original.claim_id, reused.claim_id);
    assert_eq!(original.grant_record_id, reused.grant_record_id);
    assert_eq!(original.key, reused.key);
    assert_eq!(reused.compare_machine_instance(second), Some(true));
    assert_eq!(reused.compare_machine_instance(first), Some(false));

    let now = 100;
    let mut group = ReconcileGroup::new(ReconcileClaim {
        claim_id: original.claim_id.clone(),
        record_id: original.grant_record_id.clone(),
        key: original.key.clone(),
        status: ReconcileClaimStatus::Ready,
    });
    group.add(ReconcileClaim {
        claim_id: reused.claim_id.clone(),
        record_id: reused.grant_record_id.clone(),
        key: reused.key.clone(),
        status: reconcile_claim(
            &fixture.state,
            reused,
            &fixture.record.boot_id,
            registered(second),
            now,
        )
        .unwrap(),
    });
    assert!(
        !group.ready(),
        "the second machine must keep the shared ACL alive"
    );
    assert_eq!(
        reconcile_claim(
            &fixture.state,
            reused,
            &fixture.record.boot_id,
            SystemMachineRegistration::Absent,
            now
        ),
        Some(ReconcileClaimStatus::Pending),
    );
    let pending = fixture
        .state
        .load_claims()
        .claims
        .into_iter()
        .find(|claim| claim.claim_id == reused.claim_id)
        .unwrap();
    group.claims[1].status = reconcile_claim(
        &fixture.state,
        &pending,
        &fixture.record.boot_id,
        SystemMachineRegistration::Absent,
        now + CLAIM_RECONCILE_GRACE_MILLIS,
    )
    .unwrap();
    assert!(group.ready());

    fixture.end_claims();
    assert!(fixture
        .state
        .load_claims()
        .claims
        .iter()
        .all(|claim| matches!(claim.phase, MachineClaimPhase::Ended)));
}

#[test]
fn reuse_after_restart_replaces_the_consumer_without_rewriting_grant_history() {
    let first =
        ObservedMachineInstance::from_process(42, std::num::NonZeroU64::new(77).unwrap(), None);
    let second =
        ObservedMachineInstance::from_process(43, std::num::NonZeroU64::new(88).unwrap(), None);
    let request = grant_request_for_instance(second);
    for phase in [
        MachineClaimPhase::Active,
        MachineClaimPhase::CleanupPending {
            since_unix_millis: 100,
        },
        MachineClaimPhase::NeedsReview {
            reason: "a different machine instance reappeared before cleanup; explicit preparation is required".into(),
        },
    ] {
        let fixture = Fixture::with_request(grant_request_for_instance(first));
        fixture.phase(phase);
        for _ in 0..2 {
            assert_eq!(
                reuse_managed_grant_sync(
                    &fixture.state,
                    &fixture.state.load_records(),
                    &fixture.state.load_claims(),
                    &request,
                    77,
                    &fixture.record.boot_id,
                )
                .unwrap().map(|reused| reused.record_id),
                Some(fixture.record.record_id.clone()),
            );
        }
        let claims = fixture.state.load_claims();
        assert_eq!(claims.claims.len(), 2);
        let previous = claims
            .claims
            .iter()
            .find(|claim| claim.compare_machine_instance(first) == Some(true))
            .unwrap();
        assert_eq!(previous.phase, MachineClaimPhase::Ended);
        let current = claims
            .claims
            .iter()
            .find(|claim| claim.phase == MachineClaimPhase::Active)
            .unwrap();
        assert_eq!(current.compare_machine_instance(second), Some(true));
        let records = fixture.state.load_records();
        assert_eq!(records.records.len(), 1);
        assert_eq!(records.records[0].identity.instance(), first);
        assert!(current
            .active_evidence(&records.records[0])
            .unwrap()
            .matches(
                &records.records[0],
                request.target(),
                request.projection().identity(),
            ));
        assert_eq!(
            reconcile_claim(
                &fixture.state,
                current,
                &fixture.record.boot_id,
                SystemMachineRegistration::Absent,
                100,
            ),
            Some(ReconcileClaimStatus::Pending),
        );
        let pending = fixture
            .state
            .load_claims()
            .claims
            .into_iter()
            .find(|claim| claim.claim_id == current.claim_id)
            .unwrap();
        assert_eq!(
            reconcile_claim(
                &fixture.state,
                &pending,
                &fixture.record.boot_id,
                SystemMachineRegistration::Absent,
                100 + CLAIM_RECONCILE_GRACE_MILLIS,
            ),
            Some(ReconcileClaimStatus::Ready),
        );
    }
}

#[test]
fn explicit_reuse_renews_pending_claim_without_clearing_uncertain_acl_outcomes() {
    let fixture = Fixture::new();
    fixture.phase(MachineClaimPhase::CleanupPending {
        since_unix_millis: 100,
    });
    let request = grant_request();
    reuse_managed_grant_sync(
        &fixture.state,
        &fixture.state.load_records(),
        &fixture.state.load_claims(),
        &request,
        77,
        &fixture.record.boot_id,
    )
    .unwrap();
    assert_eq!(fixture.claim().phase, MachineClaimPhase::Active);

    let unknown = MachineClaimPhase::Unknown {
        reason: "ACL confirmation: connection lost".into(),
    };
    fixture.phase(unknown.clone());
    reuse_managed_grant_sync(
        &fixture.state,
        &fixture.state.load_records(),
        &fixture.state.load_claims(),
        &request,
        77,
        &fixture.record.boot_id,
    )
    .unwrap();
    let claims = fixture.state.load_claims();
    assert_eq!(claims.claims.len(), 2);
    assert!(claims.claims.iter().any(|claim| claim.phase == unknown));
    assert!(fixture.watcher_needed());
}

#[test]
fn external_historical_or_other_generation_acls_are_not_claimed_on_reuse() {
    let fixture = Fixture::new();
    let request = grant_request();
    let original = fixture.state.load_records();
    let mut catalogs = vec![crate::application::x11::X11GrantRecordCatalog::empty()];
    for phase in [
        crate::application::x11::X11GrantRecordPhase::Pending,
        crate::application::x11::X11GrantRecordPhase::Revoked,
        crate::application::x11::X11GrantRecordPhase::OutcomeUnknown {
            reason: "uncertain".into(),
        },
    ] {
        let mut catalog = original.clone();
        catalog.records[0].phase = phase;
        catalogs.push(catalog);
    }
    let mut replaced = original.clone();
    replaced.records[0].socket_revision.inode += 1;
    catalogs.push(replaced);
    let mut other_boot = original.clone();
    other_boot.records[0].boot_id = "22222222-2222-4222-8222-222222222222".into();
    catalogs.push(other_boot);
    for records in catalogs {
        assert_eq!(
            reuse_managed_grant_sync(
                &fixture.state,
                &records,
                &fixture.state.load_claims(),
                &request,
                77,
                &fixture.record.boot_id,
            )
            .unwrap()
            .map(|reused| reused.record_id),
            None
        );
    }
    assert_eq!(
        reuse_managed_grant_sync(
            &fixture.state,
            &original,
            &fixture.state.load_claims(),
            &request,
            78,
            &fixture.record.boot_id,
        )
        .unwrap()
        .map(|reused| reused.record_id),
        None
    );
    assert_eq!(fixture.state.load_claims().claims.len(), 1);
}

#[test]
fn a_managed_acl_is_not_reused_when_the_new_machine_claim_cannot_be_saved() {
    let mut fixture = Fixture::new();
    let request = grant_request_for_instance(ObservedMachineInstance::from_process(
        43,
        std::num::NonZeroU64::new(88).unwrap(),
        None,
    ));
    fixture.state.claims = None;
    let error = reuse_managed_grant_sync(
        &fixture.state,
        &fixture.state.load_records(),
        &fixture.state.load_claims(),
        &request,
        77,
        &fixture.record.boot_id,
    )
    .unwrap_err();
    assert!(error.contains("could not confirm persistence of current machine claim"));
    assert_eq!(fixture.state.load_records().records.len(), 1);
}

#[test]
fn watcher_survives_unresolved_claims_and_missing_or_incomplete_records() {
    let fixture = Fixture::new();
    fixture
        .state
        .mark_record_phase(&fixture.record.record_id, GrantRecordPhase::Revoked)
        .unwrap();
    for phase in [
        MachineClaimPhase::Active,
        MachineClaimPhase::CleanupPending {
            since_unix_millis: 100,
        },
        MachineClaimPhase::Unknown {
            reason: "uncertain".into(),
        },
        MachineClaimPhase::NeedsReview {
            reason: "replacement".into(),
        },
    ] {
        fixture.phase(phase);
        assert!(fixture.watcher_needed());
    }
    fixture.end_claims();
    assert!(!fixture.watcher_needed());

    fixture
        .state
        .mark_record_phase(&fixture.record.record_id, GrantRecordPhase::ConfirmedAdded)
        .unwrap();
    assert!(fixture.watcher_needed()); // A confirmed orphan must not lose its wake-up.
    let mut claims = fixture.state.load_claims();
    claims.complete = false;
    assert!(claims_need_activation(&claims, &fixture.state.load_records()).is_err());
    let mut records = fixture.state.load_records();
    records.complete = false;
    assert!(claims_need_activation(&fixture.state.load_claims(), &records).is_err());
}

#[test]
fn failed_claim_persistence_prevents_cleanup() {
    let mut fixture = Fixture::new();
    let claim = fixture.claim();
    fixture.state.claims = None;
    assert!(matches!(
        reconcile_claim(&fixture.state, &claim, &fixture.record.boot_id, SystemMachineRegistration::Absent, 100),
        Some(ReconcileClaimStatus::Blocked(reason)) if reason.contains("could not persist")
    ));
}
