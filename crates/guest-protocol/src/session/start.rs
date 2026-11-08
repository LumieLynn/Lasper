use heapless::String;
use serde::{Deserialize, Serialize};

use super::control::PreparationControl;
use super::frame::{
    encode_frame, validate_session_frame, validate_token, SessionFrameError, SessionPayloadError,
};
use crate::messages::{decode_json_payload_escaped, encode_json_payload};
use crate::prepare::{validate_absolute_path, MAX_PROJECTION_PATH_BYTES};
use crate::{
    BrokerId, ControlFrameHeader, MessageKind, PayloadCodecError, PreparationId,
    PrepareRequestFrame, TerminalSize,
};

/// Environment changes accepted when a prepared worker is released.
/// Values can only repeat projections selected by the original Prepare.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StartRequest {
    broker_id: BrokerId,
    preparation_id: PreparationId,
    #[serde(skip_serializing_if = "Option::is_none")]
    wayland_display: Option<String<MAX_PROJECTION_PATH_BYTES>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    x11_display: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    terminal_size: Option<TerminalSize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireStartRequest {
    broker_id: BrokerId,
    preparation_id: PreparationId,
    wayland_display: Option<String<MAX_PROJECTION_PATH_BYTES>>,
    x11_display: Option<u16>,
    terminal_size: Option<TerminalSize>,
}

impl StartRequest {
    pub fn new(
        request: &PrepareRequestFrame<'_>,
        preparation_id: PreparationId,
        wayland_display: Option<&str>,
        x11_display: Option<u16>,
        terminal_size: Option<TerminalSize>,
    ) -> Result<Self, SessionPayloadError> {
        let wayland_display = wayland_display
            .map(|value| bounded_path(value).map_err(SessionPayloadError::Payload))
            .transpose()?;
        let start = Self {
            broker_id: request.broker_id(),
            preparation_id,
            wayland_display,
            x11_display,
            terminal_size,
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

    pub fn wayland_display(&self) -> Option<&str> {
        self.wayland_display.as_ref().map(String::as_str)
    }

    pub const fn x11_display(&self) -> Option<u16> {
        self.x11_display
    }

    pub const fn terminal_size(&self) -> Option<TerminalSize> {
        self.terminal_size
    }

    pub fn encode_payload(&self, output: &mut [u8]) -> Result<usize, PayloadCodecError> {
        encode_json_payload(self, output)
    }

    /// Decodes a Start only after checking its token and environment against
    /// the exact Prepare request that created the worker.
    pub fn decode_for(
        request: &PrepareRequestFrame<'_>,
        preparation_id: PreparationId,
        bytes: &[u8],
    ) -> Result<Self, SessionPayloadError> {
        let mut unescape_buffer = [0; MAX_PROJECTION_PATH_BYTES];
        let wire: WireStartRequest = decode_json_payload_escaped(bytes, &mut unescape_buffer)
            .map_err(SessionPayloadError::Payload)?;
        validate_token(request, preparation_id, wire.broker_id, wire.preparation_id)?;
        let start = Self {
            broker_id: wire.broker_id,
            preparation_id: wire.preparation_id,
            wayland_display: wire.wayland_display,
            x11_display: wire.x11_display,
            terminal_size: wire.terminal_size,
        };
        start.validate_for(request)?;
        Ok(start)
    }

    fn validate_for(&self, request: &PrepareRequestFrame<'_>) -> Result<(), SessionPayloadError> {
        let projections = request.request().projections();
        if let Some(display) = self.wayland_display() {
            validate_absolute_path(display).map_err(SessionPayloadError::Payload)?;
            if !matches!(
                projections.wayland(),
                Some(projection) if projection.target() == display
            ) {
                return Err(SessionPayloadError::UnrequestedProjection);
            }
        }
        if let Some(display) = self.x11_display {
            if !matches!(
                projections.x11(),
                Some(projection) if projection.display() == display
            ) {
                return Err(SessionPayloadError::UnrequestedProjection);
            }
        }
        match (request.request().execution(), self.terminal_size) {
            (crate::ExecutionKind::Interactive, Some(_))
            | (crate::ExecutionKind::Bounded, None) => Ok(()),
            _ => Err(SessionPayloadError::InvalidTerminalSize),
        }
    }
}

/// A Start frame is separate from small preparation-control frames so Keep,
/// Recheck, Cancel, and Close do not carry a 4 KiB inline path buffer.
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

    pub fn encode_parts(
        &self,
        payload: &mut [u8],
    ) -> Result<(ControlFrameHeader, usize), SessionFrameError> {
        encode_frame(MessageKind::Start, self.request_id, &self.request, payload)
    }

    pub fn decode_for(
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

/// Guest acknowledgement emitted after the guardian commits Prepared -> Starting.
/// It does not mean that `execve` has succeeded; that result is reported separately.
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

    pub fn encode_parts(
        &self,
        payload: &mut [u8],
    ) -> Result<(ControlFrameHeader, usize), SessionFrameError> {
        encode_frame(MessageKind::Start, self.request_id, &self.control, payload)
    }

    pub fn decode_for(
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

fn bounded_path(value: &str) -> Result<String<MAX_PROJECTION_PATH_BYTES>, PayloadCodecError> {
    let mut path = String::new();
    path.push_str(value)
        .map_err(|_| PayloadCodecError::InvalidRequest)?;
    Ok(path)
}
