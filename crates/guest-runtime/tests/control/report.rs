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
                (GuestErrorCode::Expired, None)
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
        (GuestErrorCode::WorkerExitedBeforeStart, None)
    );
    assert_eq!(
        prepare_error(CleanupReason::WorkerPreparationFailed, worker()),
        (GuestErrorCode::Unavailable, None)
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
            (GuestErrorCode::System, Some(13))
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
