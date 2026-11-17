//! Hello uses a fixed envelope because its payload negotiates the later limits.

use crate::{ControlLimits, Direction, MessageKind, CONTROL_FRAME_HEADER_BYTES};
use serde::{de::DeserializeOwned, Serialize};

pub(super) trait BootstrapPayload: Serialize + DeserializeOwned {
    const KIND: MessageKind;
    const CODEC: BootstrapSchemaId;
}

pub(super) fn encode<T: BootstrapPayload>(
    value: &T,
    output: &mut [u8],
) -> Result<usize, crate::PayloadCodecError> {
    validate_codec::<T>()?;
    crate::json::encode_bounded_json(value, output, BOOTSTRAP_MANIFEST.payload_bytes())
}

pub(super) fn decode<T: BootstrapPayload>(bytes: &[u8]) -> Result<T, crate::PayloadCodecError> {
    validate_codec::<T>()?;
    crate::json::decode_bounded_json(bytes, BOOTSTRAP_MANIFEST.payload_bytes())
}

fn validate_codec<T: BootstrapPayload>() -> Result<(), crate::PayloadCodecError> {
    if BOOTSTRAP_MANIFEST
        .schema(T::KIND)
        .map(|schema| schema.codec)
        != Some(T::CODEC)
    {
        return Err(crate::PayloadCodecError::InvalidRequest);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootstrapSchemaId {
    HelloRequestV1,
    HelloResponseV1,
    HelloRejectedV1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootstrapMessageSchema {
    pub kind: MessageKind,
    pub direction: Direction,
    pub codec: BootstrapSchemaId,
}

/// Shared by every session profile. The major comes from the sender's offer;
/// the bootstrap minor, codecs and envelope do not depend on the selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootstrapManifest {
    pub minor: u16,
    pub limits: ControlLimits,
    pub messages: &'static [BootstrapMessageSchema],
}

impl BootstrapManifest {
    pub const fn payload_bytes(self) -> usize {
        self.limits.max_control_frame_bytes() as usize - CONTROL_FRAME_HEADER_BYTES
    }

    pub const fn schema(self, kind: MessageKind) -> Option<BootstrapMessageSchema> {
        let mut index = 0;
        while index < self.messages.len() {
            let schema = self.messages[index];
            if schema.kind as u16 == kind as u16 {
                return Some(schema);
            }
            index += 1;
        }
        None
    }
}

pub const BOOTSTRAP_MANIFEST: BootstrapManifest = BootstrapManifest {
    minor: 0,
    limits: match ControlLimits::new(1024, 0) {
        Ok(limits) => limits,
        Err(_) => panic!("bootstrap capacity must hold a control header"),
    },
    messages: &[
        BootstrapMessageSchema {
            kind: MessageKind::HelloRequest,
            direction: Direction::HostToGuest,
            codec: BootstrapSchemaId::HelloRequestV1,
        },
        BootstrapMessageSchema {
            kind: MessageKind::HelloResponse,
            direction: Direction::GuestToHost,
            codec: BootstrapSchemaId::HelloResponseV1,
        },
        BootstrapMessageSchema {
            kind: MessageKind::HelloRejected,
            direction: Direction::GuestToHost,
            codec: BootstrapSchemaId::HelloRejectedV1,
        },
    ],
};

#[cfg(test)]
#[path = "../../tests/messages/bootstrap.rs"]
mod tests;
