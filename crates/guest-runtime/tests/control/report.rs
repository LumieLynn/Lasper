use super::*;
use crate::session::{WorkerFailure, WorkerStage};

fn worker() -> WorkerObservation {
    WorkerObservation {
        prepared: false,
        start_requested: false,
        exec: None,
        exit: None,
        failure: None,
        error: None,
        cleanup: CleanupProgress::Terminating,
    }
}

#[test]
fn preparation_expiry_is_not_replaced_by_a_later_worker_error() {
    for reason in [
        CleanupReason::PreparationExpired,
        CleanupReason::LeaseExpired,
    ] {
        for failure in [None, Some(WorkerFailure::new(WorkerStage::Account, 5))] {
            assert_eq!(
                prepare_error(
                    reason,
                    WorkerObservation {
                        failure,
                        ..worker()
                    }
                ),
                PrepareError {
                    code: GuestErrorCode::Expired,
                    errno: None,
                    stage: None
                }
            );
        }
    }
}

#[test]
fn premature_worker_exit_is_distinct_from_unavailability_and_system_failure() {
    assert_eq!(
        prepare_error(
            CleanupReason::WorkerExitedBeforeStart,
            WorkerObservation {
                exit: Some(WorkerExit::Exited(23)),
                ..worker()
            }
        ),
        PrepareError {
            code: GuestErrorCode::WorkerExitedBeforeStart,
            errno: None,
            stage: None
        }
    );
    assert_eq!(
        prepare_error(CleanupReason::WorkerPreparationFailed, worker()),
        PrepareError {
            code: GuestErrorCode::Unavailable,
            errno: None,
            stage: None
        }
    );
    for worker in [
        WorkerObservation {
            failure: Some(WorkerFailure::new(WorkerStage::Account, 13)),
            ..worker()
        },
        WorkerObservation {
            error: Some(GuardianError::System {
                operation: "observe assigned worker exit",
                errno: 13,
            }),
            ..worker()
        },
    ] {
        assert_eq!(
            prepare_error(CleanupReason::WorkerPreparationFailed, worker),
            PrepareError {
                code: GuestErrorCode::System,
                errno: Some(13),
                stage: worker.failure.map(|failure| failure.stage),
            }
        );
    }
}

#[test]
fn cleanup_reports_distinguish_progress_pending_and_completion() {
    for (progress, exit, expected) in [
        (
            CleanupProgress::Terminating,
            None,
            CleanupStatus::InProgress,
        ),
        (
            CleanupProgress::Pending { errno: Some(13) },
            None,
            CleanupStatus::Pending { errno: Some(13) },
        ),
        (
            CleanupProgress::Complete,
            Some(WorkerExit::Signaled(9)),
            CleanupStatus::Complete,
        ),
    ] {
        let report = report(GuardianObservation {
            phase: if progress == CleanupProgress::Complete {
                SessionPhase::Closed
            } else {
                SessionPhase::Closing
            },
            cleanup_reason: Some(CleanupReason::Cancelled),
            worker: WorkerObservation {
                cleanup: progress,
                exit,
                ..worker()
            },
        })
        .unwrap()
        .unwrap();
        assert_eq!(report.cleanup(), expected);
        assert_eq!(report.reason(), ExitReason::Cancelled);
        assert_eq!(report.is_final(), progress == CleanupProgress::Complete);
    }
}

#[test]
fn clock_failure_is_reported_separately_from_worker_observation_failure() {
    let report = report(GuardianObservation {
        phase: SessionPhase::Closing,
        cleanup_reason: Some(CleanupReason::ClockFailed),
        worker: worker(),
    })
    .unwrap()
    .unwrap();
    assert_eq!(report.reason(), ExitReason::ClockFailed);
    assert_eq!(report.cleanup(), CleanupStatus::InProgress);
}

#[test]
fn worker_failures_keep_their_stage_without_overwriting_cleanup_causes() {
    for (stage, execution, reason) in [
        (
            WorkerStage::UserIdentity,
            None,
            CleanupReason::WorkerPreparationFailed,
        ),
        (
            WorkerStage::Exec,
            Some(ExecOutcome::Failed { errno: 13 }),
            CleanupReason::ExecFailed { errno: 13 },
        ),
    ] {
        let observation = GuardianObservation {
            phase: SessionPhase::Closing,
            cleanup_reason: Some(reason),
            worker: WorkerObservation {
                failure: Some(WorkerFailure::new(stage, 13)),
                exec: execution,
                ..worker()
            },
        };
        let failure = report(observation).unwrap().unwrap().failure().unwrap();
        assert_eq!(failure.stage(), stage);
        assert_eq!(failure.errno(), 13);
        for cause in [
            CleanupReason::Cancelled,
            CleanupReason::ClockFailed,
            CleanupReason::PreparationExpired,
        ] {
            let report = report(GuardianObservation {
                cleanup_reason: Some(cause),
                ..observation
            })
            .unwrap()
            .unwrap();
            assert!(report.failure().is_none());
        }
    }
}

#[test]
fn preparation_rejection_does_not_promote_a_secondary_observation_error() {
    let error = prepare_error(
        CleanupReason::SupervisorStopped,
        WorkerObservation {
            error: Some(GuardianError::System {
                operation: "reap worker",
                errno: 5,
            }),
            failure: Some(WorkerFailure::new(WorkerStage::Account, 2)),
            ..worker()
        },
    );
    assert_eq!(
        error,
        PrepareError {
            code: GuestErrorCode::Unavailable,
            errno: None,
            stage: None
        }
    );
}

#[test]
fn inconsistent_failure_details_preserve_the_primary_exit_report() {
    for (reason, exec, failure) in [
        (
            CleanupReason::WorkerPreparationFailed,
            None,
            WorkerFailure::new(WorkerStage::Exec, 2),
        ),
        (
            CleanupReason::WorkerPreparationFailed,
            None,
            WorkerFailure::new(WorkerStage::Account, 0),
        ),
        (
            CleanupReason::WorkerPreparationFailed,
            None,
            WorkerFailure::new(WorkerStage::Account, 4096),
        ),
        (
            CleanupReason::ExecFailed { errno: 2 },
            Some(ExecOutcome::Failed { errno: 2 }),
            WorkerFailure::new(WorkerStage::Account, 2),
        ),
        (
            CleanupReason::ExecFailed { errno: 2 },
            Some(ExecOutcome::Failed { errno: 2 }),
            WorkerFailure::new(WorkerStage::Exec, 13),
        ),
    ] {
        for (phase, cleanup, exit) in [
            (SessionPhase::Closing, CleanupProgress::Terminating, None),
            (
                SessionPhase::Closing,
                CleanupProgress::Pending { errno: Some(13) },
                None,
            ),
            (
                SessionPhase::Closed,
                CleanupProgress::Complete,
                Some(WorkerExit::Exited(126)),
            ),
        ] {
            let observation = GuardianObservation {
                phase,
                cleanup_reason: Some(reason),
                worker: WorkerObservation {
                    exec,
                    exit,
                    cleanup,
                    ..worker()
                },
            };
            let expected = report(observation).unwrap().unwrap();
            let actual = report(GuardianObservation {
                worker: WorkerObservation {
                    failure: Some(failure),
                    ..observation.worker
                },
                ..observation
            })
            .unwrap()
            .unwrap();
            assert_eq!(actual, expected, "{reason:?}, {cleanup:?}, {failure:?}");
            assert!(actual.failure().is_none());
        }
    }
}

#[test]
fn invalid_primary_exit_facts_still_fail_validation() {
    assert_eq!(
        report(GuardianObservation {
            phase: SessionPhase::Closed,
            cleanup_reason: Some(CleanupReason::ExecFailed { errno: 0 }),
            worker: WorkerObservation {
                exec: Some(ExecOutcome::Failed { errno: 0 }),
                exit: Some(WorkerExit::Exited(126)),
                cleanup: CleanupProgress::Complete,
                failure: Some(WorkerFailure::new(WorkerStage::Exec, 2)),
                ..worker()
            },
        }),
        Err(GuardianError::ReplyEncoding)
    );
}
