use heapless::String;
use serde::{Deserialize, Serialize};

use crate::messages::{decode_json_payload, decode_json_payload_escaped, encode_json_payload};
use crate::prepare::{validate_absolute_path, MAX_PROJECTION_PATH_BYTES};
use crate::{
    BrokerId, ControlFrameError, ControlFrameHeader, ControlFrameHeaderError, MessageKind,
    PayloadCodecError, PreparationId, PrepareRequestFrame, TerminalSize,
};

/// Correlates commands sent after `PrepareReady` with one broker generation
/// and one prepared worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct PreparationControl {
    broker_id: BrokerId,
    preparation_id: PreparationId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePreparationControl {
    broker_id: BrokerId,
    preparation_id: PreparationId,
}

impl PreparationControl {
    pub const fn new(broker_id: BrokerId, preparation_id: PreparationId) -> Self {
        Self {
            broker_id,
            preparation_id,
        }
    }

    pub const fn broker_id(self) -> BrokerId {
        self.broker_id
    }

    pub const fn preparation_id(self) -> PreparationId {
        self.preparation_id
    }

    pub fn encode_payload(self, output: &mut [u8]) -> Result<usize, PayloadCodecError> {
        encode_json_payload(&self, output)
    }

    /// Decodes a control payload only when it belongs to the prepared request
    /// on this connection.
    pub fn decode_for(
        request: &PrepareRequestFrame<'_>,
        preparation_id: PreparationId,
        bytes: &[u8],
    ) -> Result<Self, SessionPayloadError> {
        let wire: WirePreparationControl =
            decode_json_payload(bytes).map_err(SessionPayloadError::Payload)?;
        validate_token(request, preparation_id, wire.broker_id, wire.preparation_id)?;
        Ok(Self::new(wire.broker_id, wire.preparation_id))
    }
}

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreparationControlKind {
    KeepPrepared,
    Recheck,
    Cancel,
    Close,
}

impl PreparationControlKind {
    const fn message_kind(self) -> MessageKind {
        match self {
            Self::KeepPrepared => MessageKind::KeepPrepared,
            Self::Recheck => MessageKind::Recheck,
            Self::Cancel => MessageKind::Cancel,
            Self::Close => MessageKind::Close,
        }
    }

    fn from_message_kind(kind: MessageKind) -> Option<Self> {
        match kind {
            MessageKind::KeepPrepared => Some(Self::KeepPrepared),
            MessageKind::Recheck => Some(Self::Recheck),
            MessageKind::Cancel => Some(Self::Cancel),
            MessageKind::Close => Some(Self::Close),
            _ => None,
        }
    }
}

/// A control operation other than Start. Its small fixed layout does not
/// reserve the path buffer carried only by Start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreparationControlFrame {
    request_id: u64,
    kind: PreparationControlKind,
    control: PreparationControl,
}

impl PreparationControlFrame {
    pub const fn new(
        request_id: u64,
        kind: PreparationControlKind,
        control: PreparationControl,
    ) -> Self {
        Self {
            request_id,
            kind,
            control,
        }
    }

    pub const fn request_id(&self) -> u64 {
        self.request_id
    }

    pub const fn kind(&self) -> PreparationControlKind {
        self.kind
    }

    pub const fn control(&self) -> PreparationControl {
        self.control
    }

    pub fn encode_parts(
        &self,
        payload: &mut [u8],
    ) -> Result<(ControlFrameHeader, usize), SessionFrameError> {
        let payload_length = self
            .control
            .encode_payload(payload)
            .map_err(SessionPayloadError::Payload)
            .map_err(SessionFrameError::Payload)?;
        let header = ControlFrameHeader::new(
            self.kind.message_kind(),
            self.request_id,
            payload_length as u32,
            0,
        )
        .map_err(SessionFrameError::Header)?;
        Ok((header, payload_length))
    }

    /// Decodes a command against the Prepare that owns this control
    /// connection, including its correlation ID and descriptor-free contract.
    pub fn decode_for(
        expected_request_id: u64,
        request: &PrepareRequestFrame<'_>,
        preparation_id: PreparationId,
        header: ControlFrameHeader,
        payload: &[u8],
        received_descriptors: usize,
    ) -> Result<Self, SessionFrameError> {
        validate_session_frame(expected_request_id, header, payload, received_descriptors)?;
        let kind = PreparationControlKind::from_message_kind(header.kind()).ok_or(
            SessionFrameError::UnexpectedMessageKind {
                actual: header.kind(),
            },
        )?;
        let control = PreparationControl::decode_for(request, preparation_id, payload)
            .map_err(SessionFrameError::Payload)?;
        Ok(Self::new(header.request_id(), kind, control))
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
        let payload_length = self
            .request
            .encode_payload(payload)
            .map_err(SessionPayloadError::Payload)
            .map_err(SessionFrameError::Payload)?;
        let header = ControlFrameHeader::new(
            MessageKind::Start,
            self.request_id,
            payload_length as u32,
            0,
        )
        .map_err(SessionFrameError::Header)?;
        Ok((header, payload_length))
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
        let payload_length = self
            .control
            .encode_payload(payload)
            .map_err(SessionPayloadError::Payload)
            .map_err(SessionFrameError::Payload)?;
        let header = ControlFrameHeader::new(
            MessageKind::Start,
            self.request_id,
            payload_length as u32,
            0,
        )
        .map_err(SessionFrameError::Header)?;
        Ok((header, payload_length))
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

fn validate_session_frame(
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

fn validate_token(
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

fn bounded_path(value: &str) -> Result<String<MAX_PROJECTION_PATH_BYTES>, PayloadCodecError> {
    let mut path = String::new();
    path.push_str(value)
        .map_err(|_| PayloadCodecError::InvalidRequest)?;
    Ok(path)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPayloadError {
    Payload(PayloadCodecError),
    BrokerMismatch,
    PreparationMismatch,
    UnrequestedProjection,
    InvalidTerminalSize,
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

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::{
        CommandArgv, ExecutionKind, GuestProcess, PrepareRequest, ProjectionRequests,
        WaylandProjectionRequest, X11ProjectionRequest,
    };

    fn interactive_request() -> PrepareRequestFrame<'static> {
        interactive_request_with_wayland("/run/user/1000/wayland-0")
    }

    fn interactive_request_with_wayland(path: &'static str) -> PrepareRequestFrame<'static> {
        PrepareRequestFrame::new(
            7,
            BrokerId::new([1; 16]),
            PrepareRequest::new(
                "alice",
                ExecutionKind::Interactive,
                GuestProcess::Command(CommandArgv::new(&["/usr/bin/editor"]).unwrap()),
                Some(crate::TerminalEnvironment::new("xterm-256color", None, None).unwrap()),
                Some(TerminalSize::new(80, 24).unwrap()),
                ProjectionRequests::new(
                    Some(WaylandProjectionRequest::new(path).unwrap()),
                    Some(
                        X11ProjectionRequest::new(2, "/tmp/.X11-unix", "/tmp/.X11-unix/X2")
                            .unwrap(),
                    ),
                ),
            )
            .unwrap(),
        )
    }

    fn preparation_id() -> PreparationId {
        PreparationId::new([9; 16])
    }

    fn mutate_first_array_value(bytes: &mut [u8], field: &[u8], value: u8) {
        let mut marker = std::vec::Vec::from(field);
        marker.extend_from_slice(b":[");
        let start = bytes
            .windows(marker.len())
            .position(|window| window == marker)
            .expect("field is serialized");
        bytes[start + marker.len()] = b'0' + value;
    }

    #[test]
    fn start_round_trips_only_the_requested_projection_environment() {
        let request = interactive_request();
        let start = StartRequest::new(
            &request,
            preparation_id(),
            Some("/run/user/1000/wayland-0"),
            Some(2),
            Some(TerminalSize::new(120, 40).unwrap()),
        )
        .unwrap();
        let mut payload = [0; 512];
        let length = start.encode_payload(&mut payload).unwrap();

        let decoded =
            StartRequest::decode_for(&request, preparation_id(), &payload[..length]).unwrap();
        assert_eq!(decoded, start);
        assert_eq!(decoded.wayland_display(), Some("/run/user/1000/wayland-0"));
        assert_eq!(decoded.x11_display(), Some(2));
        assert_eq!(
            decoded.terminal_size(),
            Some(TerminalSize::new(120, 40).unwrap())
        );
    }

    #[test]
    fn start_decode_unescapes_a_wayland_path_before_matching_the_prepare() {
        let path = r##"/run/user/1000/way\"land-0"##;
        let request = interactive_request_with_wayland(path);
        let start = StartRequest::new(
            &request,
            preparation_id(),
            Some(path),
            None,
            Some(TerminalSize::new(80, 24).unwrap()),
        )
        .unwrap();
        let mut payload = [0; 512];
        let length = start.encode_payload(&mut payload).unwrap();
        assert!(std::str::from_utf8(&payload[..length])
            .unwrap()
            .contains("\\\\"));

        let decoded =
            StartRequest::decode_for(&request, preparation_id(), &payload[..length]).unwrap();
        assert_eq!(decoded.wayland_display(), Some(path));
    }

    #[test]
    fn start_rejects_a_projection_not_bound_by_prepare() {
        let request = interactive_request();
        assert_eq!(
            StartRequest::new(
                &request,
                preparation_id(),
                Some("/run/user/1000/wayland-1"),
                None,
                Some(TerminalSize::new(80, 24).unwrap()),
            ),
            Err(SessionPayloadError::UnrequestedProjection),
        );
        assert_eq!(
            StartRequest::new(
                &request,
                preparation_id(),
                None,
                Some(3),
                Some(TerminalSize::new(80, 24).unwrap()),
            ),
            Err(SessionPayloadError::UnrequestedProjection),
        );
    }

    #[test]
    fn start_decode_rejects_wrong_broker_and_preparation_tokens() {
        let request = interactive_request();
        let valid = StartRequest::new(
            &request,
            preparation_id(),
            None,
            None,
            Some(TerminalSize::new(80, 24).unwrap()),
        )
        .unwrap();
        let mut payload = [0; 512];
        let length = valid.encode_payload(&mut payload).unwrap();
        let mut bytes = std::vec::Vec::from(&payload[..length]);
        mutate_first_array_value(&mut bytes, b"\"broker_id\"", 2);
        assert_eq!(
            StartRequest::decode_for(&request, preparation_id(), &bytes),
            Err(SessionPayloadError::BrokerMismatch),
        );

        let length = valid.encode_payload(&mut payload).unwrap();
        let mut bytes = std::vec::Vec::from(&payload[..length]);
        mutate_first_array_value(&mut bytes, b"\"preparation_id\"", 8);
        assert_eq!(
            StartRequest::decode_for(&request, preparation_id(), &bytes),
            Err(SessionPayloadError::PreparationMismatch),
        );
    }

    #[test]
    fn start_terminal_shape_matches_execution_kind() {
        let request = interactive_request();
        assert_eq!(
            StartRequest::new(&request, preparation_id(), None, None, None),
            Err(SessionPayloadError::InvalidTerminalSize),
        );

        let bounded = PrepareRequestFrame::new(
            8,
            request.broker_id(),
            PrepareRequest::new(
                "alice",
                ExecutionKind::Bounded,
                GuestProcess::Command(CommandArgv::new(&["/usr/bin/true"]).unwrap()),
                None,
                None,
                ProjectionRequests::default(),
            )
            .unwrap(),
        );
        assert_eq!(
            StartRequest::new(
                &bounded,
                preparation_id(),
                None,
                None,
                Some(TerminalSize::new(80, 24).unwrap()),
            ),
            Err(SessionPayloadError::InvalidTerminalSize),
        );
        assert!(StartRequest::new(&bounded, preparation_id(), None, None, None).is_ok());
    }

    #[test]
    fn preparation_control_requires_the_current_broker_and_worker_token() {
        let request = interactive_request();
        let control = PreparationControl::new(request.broker_id(), preparation_id());
        let mut payload = [0; 128];
        let length = control.encode_payload(&mut payload).unwrap();
        assert_eq!(
            PreparationControl::decode_for(&request, preparation_id(), &payload[..length]),
            Ok(control),
        );
        assert_eq!(
            PreparationControl::decode_for(
                &request,
                PreparationId::new([8; 16]),
                &payload[..length],
            ),
            Err(SessionPayloadError::PreparationMismatch),
        );
    }

    #[test]
    fn preparation_control_frames_round_trip_their_operation_and_token() {
        let request = interactive_request();
        let token = PreparationControl::new(request.broker_id(), preparation_id());
        let kinds = [
            PreparationControlKind::KeepPrepared,
            PreparationControlKind::Recheck,
            PreparationControlKind::Cancel,
            PreparationControlKind::Close,
        ];

        for (index, kind) in kinds.into_iter().enumerate() {
            let frame = PreparationControlFrame::new(100 + index as u64, kind, token);
            let mut payload = [0; 256];
            let (header, length) = frame.encode_parts(&mut payload).unwrap();
            let decoded = PreparationControlFrame::decode_for(
                frame.request_id(),
                &request,
                preparation_id(),
                header,
                &payload[..length],
                0,
            )
            .unwrap();
            assert_eq!(decoded, frame);
        }
    }

    #[test]
    fn preparation_control_frame_rejects_wrong_kind_id_and_descriptor_contract() {
        let request = interactive_request();
        let token = PreparationControl::new(request.broker_id(), preparation_id());
        let frame = PreparationControlFrame::new(22, PreparationControlKind::Cancel, token);
        let mut payload = [0; 256];
        let (header, length) = frame.encode_parts(&mut payload).unwrap();
        let payload = &payload[..length];

        assert_eq!(
            PreparationControlFrame::decode_for(23, &request, preparation_id(), header, payload, 0,),
            Err(SessionFrameError::MismatchedRequestId {
                expected: 23,
                actual: 22,
            }),
        );
        assert_eq!(
            PreparationControlFrame::decode_for(
                22,
                &request,
                preparation_id(),
                ControlFrameHeader::new(MessageKind::PrepareRequest, 22, payload.len() as u32, 0)
                    .unwrap(),
                payload,
                0,
            ),
            Err(SessionFrameError::UnexpectedMessageKind {
                actual: MessageKind::PrepareRequest,
            }),
        );
        assert_eq!(
            PreparationControlFrame::decode_for(22, &request, preparation_id(), header, payload, 1,),
            Err(SessionFrameError::UnexpectedDescriptors {
                declared: 0,
                received: 1,
            }),
        );
        assert_eq!(
            PreparationControlFrame::decode_for(
                22,
                &request,
                preparation_id(),
                ControlFrameHeader::new(MessageKind::Cancel, 22, payload.len() as u32, 1).unwrap(),
                payload,
                0,
            ),
            Err(SessionFrameError::UnexpectedDescriptors {
                declared: 1,
                received: 0,
            }),
        );
    }

    #[test]
    fn start_frame_round_trips_and_rejects_other_message_kinds() {
        let request = interactive_request();
        let start = StartRequest::new(
            &request,
            preparation_id(),
            Some("/run/user/1000/wayland-0"),
            Some(2),
            Some(TerminalSize::new(100, 30).unwrap()),
        )
        .unwrap();
        let frame = StartFrame::new(31, start);
        let mut payload = [0; 1024];
        let (header, length) = frame.encode_parts(&mut payload).unwrap();
        let payload = &payload[..length];

        assert_eq!(
            StartFrame::decode_for(31, &request, preparation_id(), header, payload, 0).unwrap(),
            frame,
        );
        assert_eq!(
            StartFrame::decode_for(
                31,
                &request,
                preparation_id(),
                ControlFrameHeader::new(MessageKind::Cancel, 31, payload.len() as u32, 0).unwrap(),
                payload,
                0,
            ),
            Err(SessionFrameError::UnexpectedMessageKind {
                actual: MessageKind::Cancel,
            }),
        );
    }

    #[test]
    fn start_accepted_frame_round_trips_and_rejects_a_different_preparation() {
        let request = interactive_request();
        let frame = StartAcceptedFrame::new(
            31,
            PreparationControl::new(request.broker_id(), preparation_id()),
        );
        let mut payload = [0; 256];
        let (header, length) = frame.encode_parts(&mut payload).unwrap();
        assert_eq!(
            StartAcceptedFrame::decode_for(
                31,
                &request,
                preparation_id(),
                header,
                &payload[..length],
                0,
            )
            .unwrap(),
            frame,
        );
        assert_eq!(
            StartAcceptedFrame::decode_for(
                31,
                &request,
                PreparationId::new([8; 16]),
                header,
                &payload[..length],
                0,
            ),
            Err(SessionFrameError::Payload(
                SessionPayloadError::PreparationMismatch,
            )),
        );
    }
}
