use serde::{Deserialize, Serialize};

use super::frame::{
    check_reply, codec_error, encode_frame, validate_session_frame, validate_token,
    SessionFrameError, SessionPayloadError,
};
use crate::messages::{decode_json_payload, encode_json_payload};
use crate::{
    BrokerId, ControlFrameHeader, MessageKind, PayloadCodecError, PreparationId,
    PrepareRequestFrame, MAX_PREPARED_LEASE_MS,
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
        encode_frame(
            self.kind.message_kind(),
            self.request_id,
            &self.control,
            payload,
        )
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlReplyStatus {
    Prepared { remaining_lease_ms: u32 },
    CleanupRequested,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlReplyFrame {
    command: PreparationControlFrame,
    status: ControlReplyStatus,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireControlReply {
    broker_id: BrokerId,
    preparation_id: PreparationId,
    status: ControlReplyStatus,
}

impl ControlReplyFrame {
    pub fn new(
        command: PreparationControlFrame,
        status: ControlReplyStatus,
    ) -> Result<Self, SessionPayloadError> {
        match (command.kind(), status) {
            (
                PreparationControlKind::KeepPrepared | PreparationControlKind::Recheck,
                ControlReplyStatus::Prepared {
                    remaining_lease_ms: 1..=MAX_PREPARED_LEASE_MS,
                },
            ) => {}
            (
                PreparationControlKind::Cancel | PreparationControlKind::Close,
                ControlReplyStatus::CleanupRequested,
            ) => {}
            _ => return Err(SessionPayloadError::InvalidResponse),
        }
        Ok(Self { command, status })
    }

    pub const fn command(&self) -> PreparationControlFrame {
        self.command
    }
    pub const fn status(&self) -> ControlReplyStatus {
        self.status
    }

    pub fn encode_parts(
        &self,
        payload: &mut [u8],
    ) -> Result<(ControlFrameHeader, usize), SessionFrameError> {
        let token = self.command.control();
        encode_frame(
            self.command.kind().message_kind(),
            self.command.request_id(),
            &WireControlReply {
                broker_id: token.broker_id(),
                preparation_id: token.preparation_id(),
                status: self.status,
            },
            payload,
        )
    }

    pub fn decode_for(
        command: &PreparationControlFrame,
        header: ControlFrameHeader,
        payload: &[u8],
        received_descriptors: usize,
    ) -> Result<Self, SessionFrameError> {
        check_reply(
            command.request_id(),
            command.kind().message_kind(),
            header,
            payload,
            received_descriptors,
        )?;
        let wire: WireControlReply = decode_json_payload(payload).map_err(codec_error)?;
        check_token(command.control(), wire.broker_id, Some(wire.preparation_id))?;
        Self::new(*command, wire.status).map_err(SessionFrameError::Payload)
    }
}

fn check_token(
    expected: PreparationControl,
    broker: BrokerId,
    preparation: Option<PreparationId>,
) -> Result<(), SessionFrameError> {
    if broker != expected.broker_id() {
        return Err(SessionFrameError::Payload(
            SessionPayloadError::BrokerMismatch,
        ));
    }
    if preparation != Some(expected.preparation_id()) {
        return Err(SessionFrameError::Payload(
            SessionPayloadError::PreparationMismatch,
        ));
    }
    Ok(())
}
