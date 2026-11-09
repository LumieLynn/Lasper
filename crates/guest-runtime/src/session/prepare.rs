use lasper_guest_protocol::{
    BrokerId, ControlFrameHeader, ControlLimitError, EnforcedControlLimits, PayloadCodecError,
    PrepareFrameError, PrepareRequestFrame,
};

use crate::memory::Mapping;
use crate::transport::GuestReceivedFrame;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrepareStorageError {
    Frame(PrepareFrameError),
    Limits(ControlLimitError),
    HeaderChanged,
    System(i32),
}

/// Mutable only while the transport receives this particular Prepare payload.
pub struct PrepareBuffer {
    header: ControlFrameHeader,
    broker: BrokerId,
    bytes: Mapping,
}

impl PrepareBuffer {
    /// Use the broker ID and limits from this connection's completed Hello.
    pub fn new(
        header: ControlFrameHeader,
        broker: BrokerId,
        limits: EnforcedControlLimits,
    ) -> Result<Self, PrepareStorageError> {
        PrepareRequestFrame::validate_header(header, 0).map_err(PrepareStorageError::Frame)?;
        limits
            .get()
            .validate_header(header)
            .map_err(PrepareStorageError::Limits)?;
        if header.payload_bytes() == 0 {
            return Err(PrepareStorageError::Frame(PrepareFrameError::Payload(
                PayloadCodecError::InvalidRequest,
            )));
        }
        Ok(Self {
            header,
            broker,
            bytes: Mapping::new(header.payload_bytes() as usize)
                .map_err(PrepareStorageError::System)?,
        })
    }
}

impl AsRef<[u8]> for PrepareBuffer {
    fn as_ref(&self) -> &[u8] {
        self.bytes.as_ref()
    }
}

impl AsMut<[u8]> for PrepareBuffer {
    fn as_mut(&mut self) -> &mut [u8] {
        self.bytes.as_mut()
    }
}

/// The guardian retains this immutable owner through cleanup. The request
/// borrows it; later control frames need their own receive storage.
#[must_use = "retain the Prepare storage until session cleanup is complete"]
pub struct PrepareStorage(PrepareBuffer);

impl PrepareStorage {
    pub(super) const fn header(&self) -> ControlFrameHeader {
        self.0.header
    }
    pub fn from_received(
        frame: GuestReceivedFrame<PrepareBuffer>,
    ) -> Result<Self, PrepareStorageError> {
        let (header, buffer, descriptors) = frame.into_parts();
        if !descriptors.is_empty() {
            return Err(PrepareStorageError::Frame(
                PrepareFrameError::UnexpectedDescriptorCount {
                    expected: 0,
                    actual: descriptors.len() as u16,
                },
            ));
        }
        if header != buffer.header {
            return Err(PrepareStorageError::HeaderChanged);
        }
        // Matching the allocation header retains the negotiated-limit check.
        // Frame completion is guaranteed by the transport's finish operation.
        Ok(Self(buffer))
    }

    pub fn request(&self) -> Result<PrepareRequestFrame<'_>, PrepareStorageError> {
        PrepareRequestFrame::decode_parts(self.0.header, self.0.as_ref(), 0, self.0.broker)
            .map_err(PrepareStorageError::Frame)
    }
}
