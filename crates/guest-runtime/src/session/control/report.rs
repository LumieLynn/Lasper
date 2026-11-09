use super::super::{
    CleanupProgress, CleanupReason, ExecOutcome, GuardianError, GuardianObservation, SessionPhase,
    WorkerExit, WorkerObservation,
};
use lasper_guest_protocol::{
    CleanupStatus, ExecutionStatus, ExitReason, ExitReport, ExitStatus, GuestErrorCode,
};

pub(super) fn prepare_error(
    reason: CleanupReason,
    worker: WorkerObservation,
) -> (GuestErrorCode, Option<i32>) {
    // Cleanup may subsequently record a worker failure. Keep the reason that
    // ended preparation instead of reporting that secondary failure as its cause.
    match reason {
        CleanupReason::PreparationExpired | CleanupReason::LeaseExpired => {
            return (GuestErrorCode::Expired, None);
        }
        CleanupReason::WorkerExitedBeforeStart => {
            return (GuestErrorCode::WorkerExitedBeforeStart, None);
        }
        _ => {}
    }
    let errno = match (worker.failure, worker.error) {
        (Some(failure), _) => Some(failure.errno),
        (_, Some(GuardianError::System { errno, .. })) => Some(errno),
        _ => None,
    };
    match errno {
        Some(errno) => (GuestErrorCode::System, Some(errno)),
        None => (GuestErrorCode::Unavailable, None),
    }
}

pub(super) fn report(
    observation: GuardianObservation,
) -> Result<Option<ExitReport>, GuardianError> {
    let Some(reason) = observation.cleanup_reason else {
        return Ok(None);
    };
    let execution = match observation.worker.exec {
        Some(ExecOutcome::Started) => ExecutionStatus::Started,
        Some(ExecOutcome::Failed { errno }) => ExecutionStatus::Failed { errno },
        Some(ExecOutcome::Unknown) => ExecutionStatus::Unknown,
        None if matches!(observation.phase, SessionPhase::Starting) => ExecutionStatus::Starting,
        None if observation.worker.start_requested => ExecutionStatus::Unknown,
        None => ExecutionStatus::NotStarted,
    };
    let exit = observation.worker.exit.map(|exit| match exit {
        WorkerExit::Exited(code) => ExitStatus::Exited(code as u8),
        WorkerExit::Signaled(signal) => ExitStatus::Signaled(signal as u8),
    });
    let cleanup = match observation.worker.cleanup {
        CleanupProgress::NotStarted | CleanupProgress::Terminating => CleanupStatus::InProgress,
        CleanupProgress::Pending { errno } => CleanupStatus::Pending { errno },
        CleanupProgress::Complete => CleanupStatus::Complete,
    };
    let reason = match reason {
        CleanupReason::PreparationExpired => ExitReason::PreparationExpired,
        CleanupReason::WorkerPreparationFailed => ExitReason::WorkerPreparationFailed,
        CleanupReason::WorkerObservationFailed => ExitReason::WorkerObservationFailed,
        CleanupReason::ClockFailed => ExitReason::ClockFailed,
        CleanupReason::ReadyDeliveryFailed => ExitReason::ReadyDeliveryFailed,
        CleanupReason::Cancelled => ExitReason::Cancelled,
        CleanupReason::LeaseExpired => ExitReason::LeaseExpired,
        CleanupReason::ClientClosed => ExitReason::ClientClosed,
        CleanupReason::BrokerExited => ExitReason::BrokerExited,
        CleanupReason::WorkerExitedBeforeStart => ExitReason::WorkerExitedBeforeStart,
        CleanupReason::WorkerExited => ExitReason::WorkerExited,
        CleanupReason::ExecFailed { .. } => ExitReason::ExecFailed,
        CleanupReason::StartOutcomeUnknown => ExitReason::StartOutcomeUnknown,
        CleanupReason::SupervisorStopped => ExitReason::SupervisorStopped,
        CleanupReason::ExecutionTimedOut => ExitReason::ExecutionTimedOut,
    };
    ExitReport::new(execution, exit, cleanup, reason)
        .map(Some)
        .map_err(|_| GuardianError::ReplyEncoding)
}

#[cfg(test)]
#[path = "../../../tests/control/report.rs"]
mod tests;
