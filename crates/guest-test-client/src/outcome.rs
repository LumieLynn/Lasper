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
#[path = "../tests/outcome.rs"]
mod tests;
