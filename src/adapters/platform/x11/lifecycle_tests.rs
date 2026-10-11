//! Exercise persisted claims across multiple worker invocations. These tests
//! never modify the desktop ACL or install units in the real user manager.

use super::access::reuse_managed_grant_sync;
use super::common::CLAIM_RECONCILE_GRACE_MILLIS;
use super::lifecycle::*;
use super::state::*;
use super::tests::{grant_request, grant_request_for_instance};
use crate::adapters::trusted_state::TrustedDirectory;
use crate::application::sessions::{ObservedMachineInstance, ObservedNamespaceIdentity};
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

    fn registered_without_namespaces(&self) -> SystemMachineRegistration {
        SystemMachineRegistration::Present {
            leader_pid: self.record.machine_leader_pid,
            instance: None,
        }
    }

    fn watcher_needed(&self) -> bool {
        claims_need_activation(&self.state.load_claims(), &self.state.load_records()).unwrap()
    }
}

#[test]
fn process_only_claims_survive_reconcile_then_stop_without_namespace_access() {
    let instance =
        ObservedMachineInstance::from_process(42, std::num::NonZeroU64::new(77).unwrap(), None);
    let fixture = Fixture::with_request(grant_request_for_instance(instance));
    assert_eq!(
        fixture.step(
            SystemMachineRegistration::Present {
                leader_pid: 42,
                instance: Some(instance)
            },
            1,
        ),
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
fn same_pid_with_a_new_start_time_requires_review_even_without_namespaces() {
    let instance =
        ObservedMachineInstance::from_process(42, std::num::NonZeroU64::new(77).unwrap(), None);
    let fixture = Fixture::with_request(grant_request_for_instance(instance));
    let replacement =
        ObservedMachineInstance::from_process(42, std::num::NonZeroU64::new(78).unwrap(), None);
    assert!(matches!(
        fixture.step(
            SystemMachineRegistration::Present {
                leader_pid: 42,
                instance: Some(replacement)
            },
            1
        ),
        ReconcileClaimStatus::Blocked(_),
    ));
    assert!(matches!(
        fixture.claim().phase,
        MachineClaimPhase::NeedsReview { .. }
    ));
}

#[test]
fn legacy_namespace_only_claim_is_retained_when_current_evidence_is_process_only() {
    let fixture = Fixture::new();
    let current = ObservedMachineInstance::from_process(
        fixture.record.machine_leader_pid,
        std::num::NonZeroU64::new(77).unwrap(),
        None,
    );
    assert_eq!(
        fixture.step(
            SystemMachineRegistration::Present {
                leader_pid: current.leader_pid(),
                instance: Some(current)
            },
            1
        ),
        ReconcileClaimStatus::Active,
    );
    assert!(fixture.watcher_needed());
}
#[test]
fn unprivileged_observation_then_stop_reaches_cleanup_across_worker_restarts() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.step(fixture.registered_without_namespaces(), 1),
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

    mark_record_phase_sync(
        &fixture.state,
        &fixture.record.record_id,
        GrantRecordPhase::Revoked,
    )
    .unwrap();
    assert!(fixture.watcher_needed()); // Claim finalization must also finish.
    fixture.state.end_claims(&fixture.record.record_id).unwrap();
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
        fixture.step(fixture.registered_without_namespaces(), 1),
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
        ReconcileClaimStatus::Blocked(_)
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
fn replacement_or_reappeared_machine_still_requires_review() {
    let fixture = Fixture::new();
    let replacement = ObservedMachineInstance::new(
        fixture.record.machine_leader_pid,
        ObservedNamespaceIdentity::new(1, 99),
        ObservedNamespaceIdentity::new(1, 100),
    );
    let cases = [
        (
            MachineClaimPhase::Active,
            SystemMachineRegistration::Present {
                leader_pid: replacement.leader_pid(),
                instance: Some(replacement),
            },
        ),
        (
            MachineClaimPhase::Active,
            SystemMachineRegistration::Present {
                leader_pid: replacement.leader_pid() + 1,
                instance: None,
            },
        ),
        (
            MachineClaimPhase::CleanupPending {
                since_unix_millis: 100,
            },
            fixture.registered_without_namespaces(),
        ),
    ];
    for (phase, registration) in cases {
        fixture.phase(phase);
        assert!(matches!(
            fixture.step(registration, 100 + CLAIM_RECONCILE_GRACE_MILLIS),
            ReconcileClaimStatus::Blocked(_)
        ));
        assert!(matches!(
            fixture.claim().phase,
            MachineClaimPhase::NeedsReview { .. }
        ));
        assert!(matches!(
            fixture.step(SystemMachineRegistration::Absent, 10_000),
            ReconcileClaimStatus::Blocked(_)
        ));
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
        crate::application::sessions::ShellTarget::new(
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
            .unwrap(),
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
            SystemMachineRegistration::Present {
                leader_pid: 43,
                instance: Some(second),
            },
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

    fixture.state.end_claims(&fixture.record.record_id).unwrap();
    assert!(fixture
        .state
        .load_claims()
        .claims
        .iter()
        .all(|claim| matches!(claim.phase, MachineClaimPhase::Ended)));
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
            .unwrap(),
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
        .unwrap(),
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
    assert!(error.contains("machine claim could not be saved"));
    assert_eq!(fixture.state.load_records().records.len(), 1);
}

#[test]
fn watcher_survives_unresolved_claims_and_missing_or_incomplete_records() {
    let fixture = Fixture::new();
    mark_record_phase_sync(
        &fixture.state,
        &fixture.record.record_id,
        GrantRecordPhase::Revoked,
    )
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
    fixture.state.end_claims(&fixture.record.record_id).unwrap();
    assert!(!fixture.watcher_needed());

    mark_record_phase_sync(
        &fixture.state,
        &fixture.record.record_id,
        GrantRecordPhase::ConfirmedAdded,
    )
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
