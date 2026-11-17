use serde::{Deserialize, Serialize};

use super::control::PreparationControl;
use super::frame::{
    encode_frame, validate_session_frame, validate_token, SessionFrameError, SessionPayloadError,
};
use crate::json::{decode_json_payload, encode_json_payload};
use crate::{
    BrokerId, ControlFrameHeader, MessageKind, PayloadCodecError, PreparationId,
    PrepareRequestFrame, TerminalSize,
};

/// Build ceiling for explicit execution budgets. The selected profile supplies
/// the connection's timeout limit, which may be lower.
pub const MAX_EXECUTION_TIMEOUT_MS: u64 = crate::BUILD_CAPACITIES.max_execution_timeout_ms;

/// Final terminal dimensions and the execution budget for one prepared worker.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StartRequest {
    broker_id: BrokerId,
    preparation_id: PreparationId,
    #[serde(skip_serializing_if = "Option::is_none")]
    terminal_size: Option<TerminalSize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    execution_timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireStartRequest {
    broker_id: BrokerId,
    preparation_id: PreparationId,
    #[serde(default, deserialize_with = "present")]
    terminal_size: Option<TerminalSize>,
    #[serde(default, deserialize_with = "present")]
    execution_timeout_ms: Option<u64>,
}

// Missing fields use None; a present field must contain a value, never null.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

impl StartRequest {
    pub fn new(
        request: &PrepareRequestFrame<'_>,
        preparation_id: PreparationId,
        terminal_size: Option<TerminalSize>,
        execution_timeout_ms: Option<u64>,
    ) -> Result<Self, SessionPayloadError> {
        let start = Self {
            broker_id: request.broker_id(),
            preparation_id,
            terminal_size,
            execution_timeout_ms,
        };
        start.validate_for(request)?;
        Ok(start)
    }

    pub const fn broker_id(&self) -> BrokerId {
        self.broker_id
    }

    pub const fn preparation_id(&self) -> PreparationId {
        self.preparation_id
    }

    pub const fn terminal_size(&self) -> Option<TerminalSize> {
        self.terminal_size
    }

    /// Milliseconds from acceptance of Start, measured by the guest's monotonic
    /// clock. `None` leaves execution unlimited. A supplied budget requires the
    /// negotiated timeout feature and must fit the selected profile's limit.
    pub const fn execution_timeout_ms(&self) -> Option<u64> {
        self.execution_timeout_ms
    }

    pub(crate) fn encode_payload(&self, output: &mut [u8]) -> Result<usize, PayloadCodecError> {
        encode_json_payload(self, output)
    }

    /// Decodes a Start and checks its token and I/O options against the Prepare
    /// request that created the worker.
    pub(crate) fn decode_for(
        request: &PrepareRequestFrame<'_>,
        preparation_id: PreparationId,
        bytes: &[u8],
    ) -> Result<Self, SessionPayloadError> {
        let wire: WireStartRequest =
            decode_json_payload(bytes).map_err(SessionPayloadError::Payload)?;
        validate_token(
            request.broker_id(),
            preparation_id,
            wire.broker_id,
            wire.preparation_id,
        )?;
        let start = Self {
            broker_id: wire.broker_id,
            preparation_id: wire.preparation_id,
            terminal_size: wire.terminal_size,
            execution_timeout_ms: wire.execution_timeout_ms,
        };
        start.validate_for(request)?;
        Ok(start)
    }

    fn validate_for(&self, request: &PrepareRequestFrame<'_>) -> Result<(), SessionPayloadError> {
        if self
            .execution_timeout_ms
            .is_some_and(|timeout| !(1..=MAX_EXECUTION_TIMEOUT_MS).contains(&timeout))
        {
            return Err(SessionPayloadError::InvalidExecutionTimeout);
        }
        match (request.request().execution(), self.terminal_size) {
            (crate::ExecutionKind::Terminal, _) | (crate::ExecutionKind::Piped, None) => Ok(()),
            _ => Err(SessionPayloadError::InvalidTerminalSize),
        }
    }
}

/// Correlates a Start request with its connection-local request ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartFrame {
    request_id: u64,
    request: StartRequest,
}

impl StartFrame {
    pub const fn new(request_id: u64, request: StartRequest) -> Self {
        Self {
            request_id,
            request,
        }
    }

    pub const fn request_id(&self) -> u64 {
        self.request_id
    }

    pub const fn request(&self) -> &StartRequest {
        &self.request
    }

    pub fn into_request(self) -> StartRequest {
        self.request
    }

    pub(crate) fn encode_parts_with_version(
        &self,
        version: crate::ProtocolVersion,
        payload: &mut [u8],
    ) -> Result<(ControlFrameHeader, usize), SessionFrameError> {
        encode_frame(
            version,
            MessageKind::Start,
            self.request_id,
            &self.request,
            payload,
        )
    }

    pub(crate) fn decode_for(
        expected_request_id: u64,
        request: &PrepareRequestFrame<'_>,
        preparation_id: PreparationId,
        header: ControlFrameHeader,
        payload: &[u8],
        received_descriptors: usize,
    ) -> Result<Self, SessionFrameError> {
        validate_session_frame(expected_request_id, header, payload, received_descriptors)?;
        if header.kind() != MessageKind::Start {
            return Err(SessionFrameError::UnexpectedMessageKind {
                actual: header.kind(),
            });
        }
        let request = StartRequest::decode_for(request, preparation_id, payload)
            .map_err(SessionFrameError::Payload)?;
        Ok(Self::new(header.request_id(), request))
    }
}

/// Acknowledges the guardian's acceptance of Start. The exec result is reported
/// in Exit; this acknowledgement does not confirm that the program is running.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StartAcceptedFrame {
    request_id: u64,
    control: PreparationControl,
}

impl StartAcceptedFrame {
    pub const fn new(request_id: u64, control: PreparationControl) -> Self {
        Self {
            request_id,
            control,
        }
    }

    pub const fn request_id(&self) -> u64 {
        self.request_id
    }

    pub const fn control(&self) -> PreparationControl {
        self.control
    }

    pub(crate) fn encode_parts_with_version(
        &self,
        version: crate::ProtocolVersion,
        payload: &mut [u8],
    ) -> Result<(ControlFrameHeader, usize), SessionFrameError> {
        encode_frame(
            version,
            MessageKind::Start,
            self.request_id,
            &self.control,
            payload,
        )
    }

    pub(crate) fn decode_for(
        expected_request_id: u64,
        request: &PrepareRequestFrame<'_>,
        preparation_id: PreparationId,
        header: ControlFrameHeader,
        payload: &[u8],
        received_descriptors: usize,
    ) -> Result<Self, SessionFrameError> {
        validate_session_frame(expected_request_id, header, payload, received_descriptors)?;
        if header.kind() != MessageKind::Start {
            return Err(SessionFrameError::UnexpectedMessageKind {
                actual: header.kind(),
            });
        }
        let control = PreparationControl::decode_for(request, preparation_id, payload)
            .map_err(SessionFrameError::Payload)?;
        Ok(Self::new(header.request_id(), control))
    }
}
