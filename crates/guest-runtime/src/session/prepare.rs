use lasper_guest_protocol::{
    BrokerId, ControlFrameHeader, ControlLimitError, EnforcedControlLimits, PayloadCodecError,
    PrepareDecodeStorage, PrepareFrameError, PrepareRequestFrame,
};

use crate::memory::Mapping;
use crate::transport::GuestReceivedFrame;
use core::ops::Deref;

const METADATA_OFFSET: usize = lasper_guest_protocol::MAX_PREPARE_UNESCAPE_BYTES
    .next_multiple_of(core::mem::align_of::<PrepareDecodeStorage<'static>>());
const DECODE_STORAGE_BYTES: usize = (METADATA_OFFSET
    + core::mem::size_of::<PrepareDecodeStorage<'static>>())
.next_multiple_of(64 * 1024);

const _: () = {
    assert!(METADATA_OFFSET.is_multiple_of(core::mem::align_of::<PrepareDecodeStorage<'static>>()));
    assert!(
        METADATA_OFFSET + core::mem::size_of::<PrepareDecodeStorage<'static>>()
            <= DECODE_STORAGE_BYTES
    );
};

/// Owns decoded request metadata while its string views borrow the wire storage.
pub struct DecodedPrepare<'a>(PrepareRequestFrame<'a>);

impl<'a> Deref for DecodedPrepare<'a> {
    type Target = PrepareRequestFrame<'a>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

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

    pub fn request_with(
        &self,
        protocol: lasper_guest_protocol::NegotiatedProtocol,
    ) -> Result<DecodedPrepare<'_>, PrepareStorageError> {
        decode_request(protocol, self.0.header, self.0.as_ref(), self.0.broker)
    }
}

pub(super) fn decode_request<'a>(
    protocol: lasper_guest_protocol::NegotiatedProtocol,
    header: ControlFrameHeader,
    payload: &'a [u8],
    broker: BrokerId,
) -> Result<DecodedPrepare<'a>, PrepareStorageError> {
    let mut decode = Mapping::new(DECODE_STORAGE_BYTES).map_err(PrepareStorageError::System)?;
    let (scratch, metadata) = decode.as_mut().split_at_mut(METADATA_OFFSET);
    // SAFETY: mmap is page aligned, the offset preserves metadata alignment,
    // and the const assertions reserve the whole initialized object.
    let storage = unsafe {
        let slot = metadata.as_mut_ptr().cast::<PrepareDecodeStorage<'_>>();
        slot.write(PrepareDecodeStorage::new());
        &mut *slot
    };
    let result = protocol
        .decode_prepare_into(storage, header, payload, 0, broker, scratch)
        .cloned();
    // Only the validated view metadata escapes. Its strings borrow the
    // wire payload, so the decoder mapping can be released before fork.
    // SAFETY: storage was initialized above and no reference to it escapes.
    unsafe { core::ptr::drop_in_place(storage) };
    result
        .map(DecodedPrepare)
        .map_err(PrepareStorageError::Frame)
}
