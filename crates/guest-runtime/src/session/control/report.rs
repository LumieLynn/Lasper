use super::super::{
    CleanupProgress, CleanupReason, ExecOutcome, GuardianError, GuardianObservation, SessionPhase,
    WorkerExit, WorkerObservation,
};
use lasper_guest_protocol::{
    CleanupStatus, ExecutionStatus, ExitReason, ExitReport, ExitStatus, GuestErrorCode,
    GuestFailure, GuestFailureStage,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PrepareError {
    pub code: GuestErrorCode,
    pub errno: Option<i32>,
    pub stage: Option<GuestFailureStage>,
}

pub(super) fn prepare_error(reason: CleanupReason, worker: WorkerObservation) -> PrepareError {
    // Cleanup may subsequently record a worker failure. Keep the reason that
    // ended preparation instead of reporting that secondary failure as its cause.
    let (code, errno, stage) = match reason {
        CleanupReason::PreparationExpired | CleanupReason::LeaseExpired => {
            (GuestErrorCode::Expired, None, None)
        }
        CleanupReason::WorkerExitedBeforeStart => {
            (GuestErrorCode::WorkerExitedBeforeStart, None, None)
        }
        _ => match (worker.failure, worker.error) {
            (Some(failure), _) if reason == CleanupReason::WorkerPreparationFailed => (
                GuestErrorCode::System,
                Some(failure.errno),
                Some(failure.stage),
            ),
            (_, Some(GuardianError::System { errno, .. }))
                if matches!(
                    reason,
                    CleanupReason::WorkerPreparationFailed | CleanupReason::WorkerObservationFailed
                ) =>
            {
                (GuestErrorCode::System, Some(errno), None)
            }
            _ => (GuestErrorCode::Unavailable, None, None),
        },
    };
    PrepareError { code, errno, stage }
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
    let report = ExitReport::new(execution, exit, cleanup, reason)
        .map_err(|_| GuardianError::ReplyEncoding)?;
    // Optional failure details must not suppress the primary exit facts.
    let failure = if matches!(
        reason,
        ExitReason::WorkerPreparationFailed | ExitReason::ExecFailed
    ) {
        observation
            .worker
            .failure
            .and_then(|failure| GuestFailure::new(failure.stage, failure.errno).ok())
    } else {
        None
    };
    Ok(Some(report.with_failure(failure).unwrap_or(report)))
}

#[cfg(test)]
#[path = "../../../tests/control/report.rs"]
mod tests;
