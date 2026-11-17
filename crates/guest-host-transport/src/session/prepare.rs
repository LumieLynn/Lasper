//! Retains the exact Prepare bytes checked before the connection reaches the driver.

use super::{wire, BrokerSessionError};
use crate::BrokerConnection;
use lasper_guest_protocol::{
    ControlFrameHeader, MessageKind, PrepareRequest, CONTROL_FRAME_HEADER_BYTES,
};

pub(crate) struct EncodedPrepare {
    pub(super) header: ControlFrameHeader,
    pub(super) payload: Box<[u8]>,
}

impl EncodedPrepare {
    /// Takes a complete frame produced by the negotiated Prepare codec.
    pub(crate) fn from_frame(mut bytes: Vec<u8>, length: usize) -> Self {
        bytes.truncate(length);
        let header = ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES])
            .expect("Prepare codec encoded the header");
        let payload = bytes
            .split_off(CONTROL_FRAME_HEADER_BYTES)
            .into_boxed_slice();
        Self { header, payload }
    }

    pub(super) fn encode(
        connection: &BrokerConnection,
        request: PrepareRequest<'_>,
    ) -> Result<Self, BrokerSessionError> {
        let frame = connection
            .readiness()
            .prepare(2, request)
            .map_err(|error| BrokerSessionError::InvalidRequest(error.to_string()))?;
        let hello = connection.hello();
        let maximum =
            hello.limits().get().max_control_frame_bytes() as usize - CONTROL_FRAME_HEADER_BYTES;
        let payload = wire::encode(maximum, |output| {
            hello.protocol().encode_prepare(frame.request(), output)
        })?;
        let header = ControlFrameHeader::new_with_version(
            hello.version(),
            MessageKind::PrepareRequest,
            frame.request_id(),
            payload.len() as u32,
            0,
        )
        .map_err(wire::encoding)?;
        Ok(Self { header, payload })
    }
}
