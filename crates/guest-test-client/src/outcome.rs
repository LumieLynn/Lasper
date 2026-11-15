use anyhow::{bail, Result};
use lasper_guest_protocol::{ExitReason, ExitReport, ExitStatus};

pub fn exit_status(report: ExitReport) -> Result<u8> {
    if let Some(status) = report.command_exit() {
        return Ok(match status {
            ExitStatus::Exited(code) => code,
            ExitStatus::Signaled(signal) => 128 + signal,
        });
    }
    let failure = match report.reason() {
        ExitReason::WorkerExited => "guest command exit could not be confirmed",
        ExitReason::PreparationExpired => "guest preparation expired",
        ExitReason::WorkerPreparationFailed => "guest process setup failed",
        ExitReason::WorkerObservationFailed => "guest process observation failed",
        ExitReason::ClockFailed => "guest session clock failed",
        ExitReason::ReadyDeliveryFailed => "guest readiness response could not be delivered",
        ExitReason::Cancelled => "guest command was cancelled",
        ExitReason::LeaseExpired => "guest preparation lease expired",
        ExitReason::ClientClosed => "guest control connection closed",
        ExitReason::BrokerExited => "guest broker exited",
        ExitReason::WorkerExitedBeforeStart => "guest worker exited before Start",
        ExitReason::ExecFailed => "guest command could not be executed",
        ExitReason::StartOutcomeUnknown => "guest command start outcome is unknown",
        ExitReason::SupervisorStopped => "guest session supervisor was stopped",
        ExitReason::ExecutionTimedOut => "guest command exceeded its execution deadline",
    };
    bail!("{failure}: {report:?}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use lasper_guest_protocol::{CleanupStatus, ExecutionStatus, GuestFailure, GuestFailureStage};

    fn completed(reason: ExitReason, status: ExitStatus) -> ExitReport {
        ExitReport::new(
            ExecutionStatus::Started,
            Some(status),
            CleanupStatus::Complete,
            reason,
        )
        .unwrap()
    }

    #[test]
    fn command_exit_codes_are_preserved() {
        for code in [0, 1, 7, 125, 126, 127, 255] {
            assert_eq!(
                exit_status(completed(
                    ExitReason::WorkerExited,
                    ExitStatus::Exited(code)
                ))
                .unwrap(),
                code
            );
        }
    }

    #[test]
    fn command_signals_are_preserved() {
        for signal in [2, 9, 15] {
            assert_eq!(
                exit_status(completed(
                    ExitReason::WorkerExited,
                    ExitStatus::Signaled(signal)
                ))
                .unwrap(),
                128 + signal
            );
        }
    }

    #[test]
    fn commands_that_exit_before_exec_can_be_observed_keep_their_status() {
        for status in [
            ExitStatus::Exited(0),
            ExitStatus::Exited(7),
            ExitStatus::Signaled(9),
        ] {
            let report = ExitReport::new(
                ExecutionStatus::Unknown,
                Some(status),
                CleanupStatus::Complete,
                ExitReason::WorkerExited,
            )
            .unwrap();
            let expected = match status {
                ExitStatus::Exited(code) => code,
                ExitStatus::Signaled(signal) => 128 + signal,
            };
            assert_eq!(exit_status(report).unwrap(), expected);
        }
    }

    #[test]
    fn timeout_is_not_mistaken_for_a_command_exit() {
        for status in [
            ExitStatus::Exited(0),
            ExitStatus::Signaled(15),
            ExitStatus::Signaled(9),
        ] {
            let error = exit_status(completed(ExitReason::ExecutionTimedOut, status))
                .unwrap_err()
                .to_string();
            assert!(error.contains("exceeded its execution deadline"));
            assert!(error.contains("ExecutionTimedOut"));
        }
    }

    #[test]
    fn session_failures_are_not_mistaken_for_successful_commands() {
        for reason in [
            ExitReason::PreparationExpired,
            ExitReason::WorkerPreparationFailed,
            ExitReason::WorkerObservationFailed,
            ExitReason::ClockFailed,
            ExitReason::ReadyDeliveryFailed,
            ExitReason::Cancelled,
            ExitReason::LeaseExpired,
            ExitReason::ClientClosed,
            ExitReason::BrokerExited,
            ExitReason::WorkerExitedBeforeStart,
            ExitReason::SupervisorStopped,
        ] {
            assert!(exit_status(completed(reason, ExitStatus::Exited(0))).is_err());
        }
    }

    #[test]
    fn exec_failure_and_unknown_start_remain_errors() {
        for (execution, reason) in [
            (ExecutionStatus::Failed { errno: 2 }, ExitReason::ExecFailed),
            (ExecutionStatus::Unknown, ExitReason::StartOutcomeUnknown),
        ] {
            let report = ExitReport::new(
                execution,
                Some(ExitStatus::Exited(126)),
                CleanupStatus::Complete,
                reason,
            )
            .unwrap();
            assert!(exit_status(report).is_err());
        }
    }

    #[test]
    fn incomplete_cleanup_remains_an_error() {
        let report = ExitReport::new(
            ExecutionStatus::Started,
            Some(ExitStatus::Exited(0)),
            CleanupStatus::Pending { errno: Some(13) },
            ExitReason::WorkerExited,
        )
        .unwrap();
        assert!(exit_status(report).is_err());
    }

    #[test]
    fn setup_failure_diagnostic_keeps_the_stage_and_errno() {
        let report = ExitReport::new(
            ExecutionStatus::NotStarted,
            Some(ExitStatus::Exited(78)),
            CleanupStatus::Complete,
            ExitReason::WorkerPreparationFailed,
        )
        .unwrap()
        .with_failure(Some(
            GuestFailure::new(GuestFailureStage::HomeDirectory, 2).unwrap(),
        ))
        .unwrap();
        let diagnostic = exit_status(report).unwrap_err().to_string();
        assert!(diagnostic.contains("HomeDirectory"));
        assert!(diagnostic.contains("errno: 2"));
    }
}
