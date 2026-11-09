use serde::{Deserialize, Serialize};
use serde_json_core::str::EscapedStr;

use super::frame::{
    check_reply, codec_error, encode_frame, validate_token, SessionFrameError, SessionPayloadError,
};
use super::output::{CapturedOutput, EncodedOutput, OutputStream};
use crate::messages::decode_json_payload_escaped;
use crate::{
    BrokerId, ControlFrameHeader, ExecutionKind, MessageKind, PreparationId, PrepareRequestFrame,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
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

/// Exec, wait and cleanup are separate observations: an exited command can
/// still leave descendants to reap, and an unknown exec can have a known exit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExitReport {
    execution: ExecutionStatus,
    exit: Option<ExitStatus>,
    cleanup: CleanupStatus,
    reason: ExitReason,
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
        })
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

#[derive(Clone, Debug)]
pub struct ExitFrame<'a> {
    request_id: u64,
    broker_id: BrokerId,
    preparation_id: PreparationId,
    report: ExitReport,
    output: Option<CapturedOutput<'a>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireExit<'a> {
    broker_id: BrokerId,
    preparation_id: PreparationId,
    execution: ExecutionStatus,
    exit: Option<ExitStatus>,
    cleanup: CleanupStatus,
    reason: ExitReason,
    #[serde(borrow)]
    output: Option<WireOutput<'a>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireOutput<'a> {
    #[serde(borrow)]
    stdout: WireStream<'a>,
    #[serde(borrow)]
    stderr: WireStream<'a>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireStream<'a> {
    #[serde(borrow)]
    base64: EscapedStr<'a>,
    truncated: bool,
    complete: bool,
}

#[derive(Serialize)]
struct EncodedExit<'a> {
    broker_id: BrokerId,
    preparation_id: PreparationId,
    execution: ExecutionStatus,
    exit: Option<ExitStatus>,
    cleanup: CleanupStatus,
    reason: ExitReason,
    output: Option<EncodedStreams<'a>>,
}

#[derive(Serialize)]
struct EncodedStreams<'a> {
    stdout: EncodedStream<'a>,
    stderr: EncodedStream<'a>,
}

#[derive(Serialize)]
struct EncodedStream<'a> {
    base64: EncodedOutput<'a>,
    truncated: bool,
    complete: bool,
}

impl<'a> ExitFrame<'a> {
    pub fn new(
        request: &PrepareRequestFrame<'_>,
        preparation_id: PreparationId,
        report: ExitReport,
        output: Option<CapturedOutput<'a>>,
    ) -> Result<Self, SessionPayloadError> {
        match (request.request().execution(), output) {
            (ExecutionKind::Interactive, None) | (ExecutionKind::Bounded, Some(_)) => {}
            _ => return Err(SessionPayloadError::InvalidResponse),
        }
        if report.is_final()
            && output.is_some_and(|output| {
                [output.stdout(), output.stderr()]
                    .iter()
                    .any(|stream| !stream.complete() && !stream.truncated())
            })
        {
            return Err(SessionPayloadError::InvalidResponse);
        }
        Ok(Self {
            request_id: request.request_id(),
            broker_id: request.broker_id(),
            preparation_id,
            report,
            output,
        })
    }

    pub const fn request_id(&self) -> u64 {
        self.request_id
    }
    pub const fn report(&self) -> ExitReport {
        self.report
    }
    pub const fn output(&self) -> Option<&CapturedOutput<'a>> {
        self.output.as_ref()
    }

    pub fn encode_parts(
        &self,
        payload: &mut [u8],
    ) -> Result<(ControlFrameHeader, usize), SessionFrameError> {
        let output = self.output.map(|output| EncodedStreams {
            stdout: encode_stream(*output.stdout()),
            stderr: encode_stream(*output.stderr()),
        });
        encode_frame(
            MessageKind::Exit,
            self.request_id,
            &EncodedExit {
                broker_id: self.broker_id,
                preparation_id: self.preparation_id,
                execution: self.report.execution,
                exit: self.report.exit,
                cleanup: self.report.cleanup,
                reason: self.report.reason,
                output,
            },
            payload,
        )
    }

    pub fn decode_for(
        request: &PrepareRequestFrame<'_>,
        preparation_id: PreparationId,
        header: ControlFrameHeader,
        payload: &'a [u8],
        received_descriptors: usize,
    ) -> Result<Self, SessionFrameError> {
        check_reply(
            request.request_id(),
            MessageKind::Exit,
            header,
            payload,
            received_descriptors,
        )?;
        let wire: WireExit<'a> =
            decode_json_payload_escaped(payload, &mut []).map_err(codec_error)?;
        validate_token(request, preparation_id, wire.broker_id, wire.preparation_id)
            .map_err(SessionFrameError::Payload)?;
        let report = ExitReport::new(wire.execution, wire.exit, wire.cleanup, wire.reason)
            .map_err(SessionFrameError::Payload)?;
        let output = wire
            .output
            .map(|output| {
                CapturedOutput::new(
                    OutputStream::decoded(
                        output.stdout.base64,
                        output.stdout.truncated,
                        output.stdout.complete,
                    )?,
                    OutputStream::decoded(
                        output.stderr.base64,
                        output.stderr.truncated,
                        output.stderr.complete,
                    )?,
                )
            })
            .transpose()
            .map_err(codec_error)?;
        Self::new(request, preparation_id, report, output).map_err(SessionFrameError::Payload)
    }
}

fn encode_stream(stream: OutputStream<'_>) -> EncodedStream<'_> {
    EncodedStream {
        base64: EncodedOutput(stream),
        truncated: stream.truncated(),
        complete: stream.complete(),
    }
}
