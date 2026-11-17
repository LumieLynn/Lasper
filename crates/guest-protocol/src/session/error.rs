use serde::{Deserialize, Serialize};

use super::frame::{
    check_reply, codec_error, encode_frame, SessionFrameError, SessionPayloadError,
};
use super::GuestFailureStage;
use crate::json::decode_json_payload;
use crate::{BrokerId, ControlFrameHeader, MessageKind, PreparationId, PrepareRequestFrame};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestOperation {
    Prepare,
    KeepPrepared,
    Recheck,
    Start,
    Cancel,
    Close,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestErrorCode {
    InvalidRequest,
    InvalidState,
    Expired,
    WorkerExitedBeforeStart,
    Unavailable,
    System,
}

/// A request rejection, separate from worker exit and cleanup results. Prepare
/// can fail before a preparation ID exists, but still belongs to this broker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ErrorFrame {
    request_id: u64,
    wire: WireError,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct WireError {
    broker_id: BrokerId,
    preparation_id: Option<PreparationId>,
    #[serde(deserialize_with = "crate::json::deserialize_string_enum")]
    operation: GuestOperation,
    #[serde(deserialize_with = "crate::json::deserialize_string_enum")]
    code: GuestErrorCode,
    errno: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stage: Option<GuestFailureStage>,
}

impl ErrorFrame {
    pub fn new(
        request_id: u64,
        broker_id: BrokerId,
        preparation_id: Option<PreparationId>,
        operation: GuestOperation,
        code: GuestErrorCode,
        errno: Option<i32>,
    ) -> Result<Self, SessionPayloadError> {
        if preparation_id.is_none() && operation != GuestOperation::Prepare {
            return Err(SessionPayloadError::InvalidResponse);
        }
        match (code, errno) {
            (GuestErrorCode::System, Some(1..=4095)) => {}
            (GuestErrorCode::System, _) | (_, Some(_)) => {
                return Err(SessionPayloadError::InvalidResponse)
            }
            _ => {}
        }
        Ok(Self {
            request_id,
            wire: WireError {
                broker_id,
                preparation_id,
                operation,
                code,
                errno,
                stage: None,
            },
        })
    }

    pub fn with_stage(
        mut self,
        stage: Option<GuestFailureStage>,
    ) -> Result<Self, SessionPayloadError> {
        if stage.is_some() && self.wire.code != GuestErrorCode::System {
            return Err(SessionPayloadError::InvalidResponse);
        }
        self.wire.stage = stage;
        Ok(self)
    }

    pub const fn request_id(&self) -> u64 {
        self.request_id
    }
    pub const fn operation(&self) -> GuestOperation {
        self.wire.operation
    }
    pub const fn code(&self) -> GuestErrorCode {
        self.wire.code
    }
    pub const fn errno(&self) -> Option<i32> {
        self.wire.errno
    }
    pub const fn stage(&self) -> Option<GuestFailureStage> {
        self.wire.stage
    }
    pub const fn preparation_id(&self) -> Option<PreparationId> {
        self.wire.preparation_id
    }

    pub(crate) fn encode_parts_with_version(
        &self,
        version: crate::ProtocolVersion,
        payload: &mut [u8],
    ) -> Result<(ControlFrameHeader, usize), SessionFrameError> {
        encode_frame(
            version,
            MessageKind::Error,
            self.request_id,
            &self.wire,
            payload,
        )
    }

    /// Decode against the request envelope that was sent, not a token supplied
    /// by the error itself. Prepare errors may predate assignment of a worker.
    pub(crate) fn decode_for(
        request_header: ControlFrameHeader,
        request: &PrepareRequestFrame<'_>,
        preparation_id: Option<PreparationId>,
        header: ControlFrameHeader,
        payload: &[u8],
        received_descriptors: usize,
    ) -> Result<Self, SessionFrameError> {
        check_reply(
            request_header.request_id(),
            MessageKind::Error,
            header,
            payload,
            received_descriptors,
        )?;
        let operation = GuestOperation::from_kind(request_header.kind()).ok_or(
            SessionFrameError::Payload(SessionPayloadError::InvalidResponse),
        )?;
        let wire: WireError = decode_json_payload(payload).map_err(codec_error)?;
        if wire.stage.is_some() {
            crate::json::require_top_level_strings(payload, &["stage"]).map_err(codec_error)?;
        }
        if wire.broker_id != request.broker_id() {
            return Err(SessionFrameError::Payload(
                SessionPayloadError::BrokerMismatch,
            ));
        }
        if wire.preparation_id != preparation_id {
            return Err(SessionFrameError::Payload(
                SessionPayloadError::PreparationMismatch,
            ));
        }
        if wire.operation != operation {
            return Err(SessionFrameError::Payload(
                SessionPayloadError::InvalidResponse,
            ));
        }
        Self::new(
            request_header.request_id(),
            wire.broker_id,
            wire.preparation_id,
            wire.operation,
            wire.code,
            wire.errno,
        )
        .and_then(|error| error.with_stage(wire.stage))
        .map_err(SessionFrameError::Payload)
    }
}

impl GuestOperation {
    fn from_kind(kind: MessageKind) -> Option<Self> {
        match kind {
            MessageKind::PrepareRequest => Some(Self::Prepare),
            MessageKind::KeepPrepared => Some(Self::KeepPrepared),
            MessageKind::Recheck => Some(Self::Recheck),
            MessageKind::Start => Some(Self::Start),
            MessageKind::Cancel => Some(Self::Cancel),
            MessageKind::Close => Some(Self::Close),
            _ => None,
        }
    }
}
