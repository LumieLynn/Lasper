use serde::Serialize;

use crate::messages::encode_json_payload;
use crate::{
    BrokerId, ControlFrameError, ControlFrameHeader, ControlFrameHeaderError, MessageKind,
    PayloadCodecError, PreparationId, PrepareRequestFrame,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPayloadError {
    Payload(PayloadCodecError),
    BrokerMismatch,
    PreparationMismatch,
    UnrequestedProjection,
    InvalidTerminalSize,
    InvalidResponse,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionFrameError {
    Frame(ControlFrameError),
    Header(ControlFrameHeaderError),
    Payload(SessionPayloadError),
    UnexpectedMessageKind { actual: MessageKind },
    MismatchedRequestId { expected: u64, actual: u64 },
    UnexpectedDescriptors { declared: u16, received: usize },
}

pub(super) fn validate_session_frame(
    expected_request_id: u64,
    header: ControlFrameHeader,
    payload: &[u8],
    received_descriptors: usize,
) -> Result<(), SessionFrameError> {
    if header.request_id() != expected_request_id {
        return Err(SessionFrameError::MismatchedRequestId {
            expected: expected_request_id,
            actual: header.request_id(),
        });
    }
    if received_descriptors != 0 || header.descriptor_count() != 0 {
        return Err(SessionFrameError::UnexpectedDescriptors {
            declared: header.descriptor_count(),
            received: received_descriptors,
        });
    }
    header
        .validate_shape(payload.len(), received_descriptors)
        .map_err(SessionFrameError::Frame)
}

pub(super) fn validate_token(
    request: &PrepareRequestFrame<'_>,
    expected_preparation_id: PreparationId,
    broker_id: BrokerId,
    preparation_id: PreparationId,
) -> Result<(), SessionPayloadError> {
    if broker_id != request.broker_id() {
        return Err(SessionPayloadError::BrokerMismatch);
    }
    if preparation_id != expected_preparation_id {
        return Err(SessionPayloadError::PreparationMismatch);
    }
    Ok(())
}

pub(super) fn check_reply(
    request_id: u64,
    kind: MessageKind,
    header: ControlFrameHeader,
    payload: &[u8],
    received_descriptors: usize,
) -> Result<(), SessionFrameError> {
    validate_session_frame(request_id, header, payload, received_descriptors)?;
    if header.kind() != kind {
        return Err(SessionFrameError::UnexpectedMessageKind {
            actual: header.kind(),
        });
    }
    Ok(())
}

pub(super) fn encode_frame(
    kind: MessageKind,
    request_id: u64,
    value: &impl Serialize,
    payload: &mut [u8],
) -> Result<(ControlFrameHeader, usize), SessionFrameError> {
    let length = encode_json_payload(value, payload).map_err(codec_error)?;
    let header = ControlFrameHeader::new(kind, request_id, length as u32, 0)
        .map_err(SessionFrameError::Header)?;
    Ok((header, length))
}

pub(super) fn codec_error(error: crate::PayloadCodecError) -> SessionFrameError {
    SessionFrameError::Payload(SessionPayloadError::Payload(error))
}
