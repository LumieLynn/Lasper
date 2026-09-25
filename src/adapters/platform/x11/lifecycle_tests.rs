//! Exercise persisted claims across multiple worker invocations. These tests
//! never modify the desktop ACL or install units in the real user manager.

use super::common::CLAIM_RECONCILE_GRACE_MILLIS;
use super::lifecycle::*;
use super::state::*;
use super::tests::grant_request;
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
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let access =
            TrustedDirectory::open_existing(directory.path(), uzers::get_effective_uid()).unwrap();
        let state = X11RuntimeState {
            grants: access.open_or_create_child("grants", 0o700).unwrap(),
            claims: Some(access.open_or_create_child("claims", 0o700).unwrap()),
            access,
        };
        let mut record = ManagedX11GrantRecord::pending(
            &grant_request(),
            "0123456789abcdef0123456789abcdef".into(),
            77,
        )
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
    fixture.state.end_claim(&fixture.record.record_id).unwrap();
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
    fixture.state.end_claim(&fixture.record.record_id).unwrap();
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
