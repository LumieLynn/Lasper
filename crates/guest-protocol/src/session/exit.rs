use serde::{Deserialize, Serialize};

use super::frame::{
    check_reply, codec_error, encode_frame, validate_token, SessionFrameError, SessionPayloadError,
};
use super::{GuestFailure, GuestFailureStage};
use crate::json::decode_json_payload;
use crate::{BrokerId, ControlFrameHeader, MessageKind, PreparationId, PrepareRequestFrame};

#[cfg(test)]
#[path = "../../tests/profile/exit_token.rs"]
mod fixture;

/// The guardian's observation of exec, independent of the worker's wait status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionStatus {
    NotStarted,
    Starting,
    Started,
    Failed { errno: i32 },
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitStatus {
    Exited(u8),
    Signaled(u8),
}

/// Cleanup observed by the guardian. Losing the connection does not establish
/// either completion or failure.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CleanupStatus {
    InProgress,
    Pending { errno: Option<i32> },
    Complete,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    PreparationExpired,
    WorkerPreparationFailed,
    WorkerObservationFailed,
    ClockFailed,
    ReadyDeliveryFailed,
    Cancelled,
    LeaseExpired,
    ClientClosed,
    BrokerExited,
    WorkerExitedBeforeStart,
    WorkerExited,
    ExecFailed,
    StartOutcomeUnknown,
    SupervisorStopped,
    ExecutionTimedOut,
}

/// Exec, wait and cleanup are separate observations. A command may have exited
/// while descendants still need reaping; its wait status may be known even when
/// exec was not confirmed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExitReport {
    execution: ExecutionStatus,
    exit: Option<ExitStatus>,
    cleanup: CleanupStatus,
    reason: ExitReason,
    failure: Option<GuestFailure>,
}

impl ExitReport {
    pub fn new(
        execution: ExecutionStatus,
        exit: Option<ExitStatus>,
        cleanup: CleanupStatus,
        reason: ExitReason,
    ) -> Result<Self, SessionPayloadError> {
        if matches!(execution, ExecutionStatus::Failed { errno } if !(1..=4095).contains(&errno))
            || matches!(exit, Some(ExitStatus::Signaled(signal)) if !(1..=64).contains(&signal))
            || matches!(cleanup, CleanupStatus::Pending { errno: Some(errno) } if !(1..=4095).contains(&errno))
            || matches!(execution, ExecutionStatus::Starting)
                && (exit.is_some() || cleanup == CleanupStatus::Complete)
            || cleanup == CleanupStatus::Complete && exit.is_none()
            || reason == ExitReason::ExecFailed
                && !matches!(execution, ExecutionStatus::Failed { .. })
            || reason == ExitReason::StartOutcomeUnknown && execution != ExecutionStatus::Unknown
        {
            return Err(SessionPayloadError::InvalidResponse);
        }
        Ok(Self {
            execution,
            exit,
            cleanup,
            reason,
            failure: None,
        })
    }

    /// Attaches the worker failure that caused cleanup, never a later cleanup error.
    pub fn with_failure(
        mut self,
        failure: Option<GuestFailure>,
    ) -> Result<Self, SessionPayloadError> {
        if let Some(failure) = failure {
            failure.validate()?;
            let valid = match self.reason {
                ExitReason::WorkerPreparationFailed => failure.stage() != GuestFailureStage::Exec,
                ExitReason::ExecFailed => {
                    failure.stage() == GuestFailureStage::Exec
                        && self.execution
                            == ExecutionStatus::Failed {
                                errno: failure.errno(),
                            }
                }
                _ => false,
            };
            if !valid {
                return Err(SessionPayloadError::InvalidResponse);
            }
        }
        self.failure = failure;
        Ok(self)
    }

    pub const fn failure(&self) -> Option<GuestFailure> {
        self.failure
    }

    /// Returns an ordinary worker exit after cleanup. Fast exits can precede
    /// exec confirmation, so this does not require observing Started.
    pub const fn command_exit(&self) -> Option<ExitStatus> {
        match (self.reason, self.execution, self.cleanup) {
            (
                ExitReason::WorkerExited,
                ExecutionStatus::Started | ExecutionStatus::Unknown,
                CleanupStatus::Complete,
            ) => self.exit,
            _ => None,
        }
    }

    pub const fn execution(&self) -> ExecutionStatus {
        self.execution
    }
    pub const fn exit(&self) -> Option<ExitStatus> {
        self.exit
    }
    pub const fn cleanup(&self) -> CleanupStatus {
        self.cleanup
    }
    pub const fn reason(&self) -> ExitReason {
        self.reason
    }
    pub const fn is_final(&self) -> bool {
        matches!(self.cleanup, CleanupStatus::Complete)
    }
}

/// A status report correlated with the Prepare request, not the latest command.
/// Bulk output is carried only by the Ready data descriptors.
#[derive(Clone, Debug)]
pub struct ExitFrame {
    request_id: u64,
    broker_id: BrokerId,
    preparation_id: PreparationId,
    report: ExitReport,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireExit {
    broker_id: BrokerId,
    preparation_id: PreparationId,
    execution: ExecutionStatus,
    exit: Option<ExitStatus>,
    cleanup: CleanupStatus,
    #[serde(deserialize_with = "crate::json::deserialize_string_enum")]
    reason: ExitReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure: Option<GuestFailure>,
}

impl ExitFrame {
    pub fn new(
        request: &PrepareRequestFrame<'_>,
        preparation_id: PreparationId,
        report: ExitReport,
    ) -> Result<Self, SessionPayloadError> {
        Ok(Self {
            request_id: request.request_id(),
            broker_id: request.broker_id(),
            preparation_id,
            report,
        })
    }

    pub const fn request_id(&self) -> u64 {
        self.request_id
    }
    pub const fn report(&self) -> ExitReport {
        self.report
    }

    pub(crate) fn encode_parts_with_version(
        &self,
        version: crate::ProtocolVersion,
        payload: &mut [u8],
    ) -> Result<(ControlFrameHeader, usize), SessionFrameError> {
        encode_frame(
            version,
            MessageKind::Exit,
            self.request_id,
            &WireExit {
                broker_id: self.broker_id,
                preparation_id: self.preparation_id,
                execution: self.report.execution,
                exit: self.report.exit,
                cleanup: self.report.cleanup,
                reason: self.report.reason,
                failure: self.report.failure,
            },
            payload,
        )
    }

    pub(crate) fn decode_for(
        request: &PrepareRequestFrame<'_>,
        preparation_id: PreparationId,
        header: ControlFrameHeader,
        payload: &[u8],
        received_descriptors: usize,
    ) -> Result<Self, SessionFrameError> {
        check_reply(
            request.request_id(),
            MessageKind::Exit,
            header,
            payload,
            received_descriptors,
        )?;
        let wire: WireExit = decode_json_payload(payload).map_err(codec_error)?;
        if !matches!(wire.execution, ExecutionStatus::Failed { .. }) {
            crate::json::require_top_level_strings(payload, &["execution"]).map_err(codec_error)?;
        }
        if !matches!(wire.cleanup, CleanupStatus::Pending { .. }) {
            crate::json::require_top_level_strings(payload, &["cleanup"]).map_err(codec_error)?;
        }
        validate_token(
            request.broker_id(),
            preparation_id,
            wire.broker_id,
            wire.preparation_id,
        )
        .map_err(SessionFrameError::Payload)?;
        let report = ExitReport::new(wire.execution, wire.exit, wire.cleanup, wire.reason)
            .and_then(|report| report.with_failure(wire.failure))
            .map_err(SessionFrameError::Payload)?;
        Self::new(request, preparation_id, report).map_err(SessionFrameError::Payload)
    }
}
