use crate::{ProtocolVersion, MAX_CONTROL_FRAME_BYTES};

const CONTROL_FRAME_MAGIC: [u8; 8] = *b"LSPGCTL1";
const CONTROL_FRAME_FORMAT: u16 = 1;
pub const CONTROL_FRAME_HEADER_BYTES: usize = 32;
/// The byte that anchors descriptor rights to a frame.
pub const CONTROL_FRAME_DESCRIPTOR_ANCHOR_BYTES: usize = 1;
pub const MAX_CONTROL_DESCRIPTORS: u16 = crate::BUILD_CAPACITIES.max_control_descriptors;
pub const MAX_CONTROL_PAYLOAD_BYTES: u32 =
    MAX_CONTROL_FRAME_BYTES - CONTROL_FRAME_HEADER_BYTES as u32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum MessageKind {
    HelloRequest = 1,
    HelloResponse = 2,
    PrepareRequest = 3,
    PrepareReady = 4,
    KeepPrepared = 5,
    Recheck = 6,
    Start = 7,
    Cancel = 8,
    Close = 9,
    Exit = 10,
    Error = 11,
    HelloRejected = 12,
}

impl MessageKind {
    fn from_wire(value: u16) -> Result<Self, ControlFrameHeaderError> {
        match value {
            1 => Ok(Self::HelloRequest),
            2 => Ok(Self::HelloResponse),
            3 => Ok(Self::PrepareRequest),
            4 => Ok(Self::PrepareReady),
            5 => Ok(Self::KeepPrepared),
            6 => Ok(Self::Recheck),
            7 => Ok(Self::Start),
            8 => Ok(Self::Cancel),
            9 => Ok(Self::Close),
            10 => Ok(Self::Exit),
            11 => Ok(Self::Error),
            12 => Ok(Self::HelloRejected),
            _ => Err(ControlFrameHeaderError::UnknownMessageKind),
        }
    }
}

/// Header for one frame on the guest control stream.
///
/// The payload immediately follows the header. Descriptors keep their semantic
/// order and are attached to the first header byte so both transports agree on
/// which frame owns them. A framing error after I/O begins makes the remaining
/// stream ambiguous; callers must discard that connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlFrameHeader {
    kind: MessageKind,
    protocol_major: u16,
    protocol_minor: u16,
    request_id: u64,
    payload_bytes: u32,
    descriptor_count: u16,
}

impl ControlFrameHeader {
    /// Stores the supplied version without negotiating it. Callers supply the
    /// bootstrap version for Hello and the selected version thereafter.
    pub const fn new_with_version(
        version: ProtocolVersion,
        kind: MessageKind,
        request_id: u64,
        payload_bytes: u32,
        descriptor_count: u16,
    ) -> Result<Self, ControlFrameHeaderError> {
        if payload_bytes > MAX_CONTROL_PAYLOAD_BYTES {
            return Err(ControlFrameHeaderError::PayloadTooLarge);
        }
        if descriptor_count > MAX_CONTROL_DESCRIPTORS {
            return Err(ControlFrameHeaderError::TooManyDescriptors);
        }
        Ok(Self {
            kind,
            protocol_major: version.major,
            protocol_minor: version.minor,
            request_id,
            payload_bytes,
            descriptor_count,
        })
    }

    pub const fn kind(self) -> MessageKind {
        self.kind
    }

    pub const fn protocol_major(self) -> u16 {
        self.protocol_major
    }

    pub const fn protocol_minor(self) -> u16 {
        self.protocol_minor
    }

    pub const fn protocol_version(self) -> ProtocolVersion {
        ProtocolVersion {
            major: self.protocol_major,
            minor: self.protocol_minor,
        }
    }

    pub const fn request_id(self) -> u64 {
        self.request_id
    }

    pub const fn payload_bytes(self) -> u32 {
        self.payload_bytes
    }

    pub const fn descriptor_count(self) -> u16 {
        self.descriptor_count
    }

    pub const fn encode(self) -> [u8; CONTROL_FRAME_HEADER_BYTES] {
        let mut frame = [0; CONTROL_FRAME_HEADER_BYTES];
        let format = CONTROL_FRAME_FORMAT.to_le_bytes();
        let kind = (self.kind as u16).to_le_bytes();
        let major = self.protocol_major.to_le_bytes();
        let minor = self.protocol_minor.to_le_bytes();
        let request_id = self.request_id.to_le_bytes();
        let payload_bytes = self.payload_bytes.to_le_bytes();
        let descriptor_count = self.descriptor_count.to_le_bytes();

        let mut index = 0;
        while index < CONTROL_FRAME_MAGIC.len() {
            frame[index] = CONTROL_FRAME_MAGIC[index];
            index += 1;
        }
        frame[8] = format[0];
        frame[9] = format[1];
        frame[10] = kind[0];
        frame[11] = kind[1];
        frame[12] = major[0];
        frame[13] = major[1];
        frame[14] = minor[0];
        frame[15] = minor[1];

        index = 0;
        while index < request_id.len() {
            frame[16 + index] = request_id[index];
            index += 1;
        }
        index = 0;
        while index < payload_bytes.len() {
            frame[24 + index] = payload_bytes[index];
            index += 1;
        }
        frame[28] = descriptor_count[0];
        frame[29] = descriptor_count[1];
        frame
    }

    pub fn encode_frame(
        self,
        payload: &[u8],
        output: &mut [u8],
    ) -> Result<usize, ControlFrameError> {
        self.validate_payload_length(payload.len())?;

        let frame_bytes = CONTROL_FRAME_HEADER_BYTES + payload.len();
        if output.len() < frame_bytes {
            return Err(ControlFrameError::OutputTooSmall {
                required: frame_bytes,
                available: output.len(),
            });
        }

        output[..CONTROL_FRAME_HEADER_BYTES].copy_from_slice(&self.encode());
        output[CONTROL_FRAME_HEADER_BYTES..frame_bytes].copy_from_slice(payload);
        Ok(frame_bytes)
    }

    pub const fn validate_payload_length(self, actual: usize) -> Result<(), ControlFrameError> {
        if actual > MAX_CONTROL_PAYLOAD_BYTES as usize {
            return Err(ControlFrameError::PayloadTooLarge);
        }
        if actual != self.payload_bytes as usize {
            return Err(ControlFrameError::PayloadLengthMismatch {
                declared: self.payload_bytes,
                actual: actual as u32,
            });
        }
        Ok(())
    }

    /// Confirms that the supplied payload length and FD count match this header.
    pub const fn validate_shape(
        self,
        payload_bytes: usize,
        descriptor_count: usize,
    ) -> Result<(), ControlFrameError> {
        match self.validate_payload_length(payload_bytes) {
            Ok(()) => {}
            Err(error) => return Err(error),
        }
        self.validate_descriptor_count(descriptor_count)
    }

    /// Checks the received count against the header declaration. Message codecs
    /// check the expected count; the transport validates and owns the actual FDs.
    pub const fn validate_descriptor_count(
        self,
        descriptor_count: usize,
    ) -> Result<(), ControlFrameError> {
        if descriptor_count != self.descriptor_count as usize {
            return Err(ControlFrameError::DescriptorCountMismatch {
                declared: self.descriptor_count,
                actual: descriptor_count,
            });
        }
        Ok(())
    }

    pub fn decode(frame: &[u8]) -> Result<Self, ControlFrameHeaderError> {
        if frame.len() != CONTROL_FRAME_HEADER_BYTES {
            return Err(ControlFrameHeaderError::WrongLength);
        }
        if frame[..8] != CONTROL_FRAME_MAGIC {
            return Err(ControlFrameHeaderError::WrongMagic);
        }
        if read_u16(frame, 8) != CONTROL_FRAME_FORMAT {
            return Err(ControlFrameHeaderError::UnsupportedFormat);
        }
        if frame[30..] != [0; 2] {
            return Err(ControlFrameHeaderError::ReservedBytesSet);
        }

        let payload_bytes = read_u32(frame, 24);
        if payload_bytes > MAX_CONTROL_PAYLOAD_BYTES {
            return Err(ControlFrameHeaderError::PayloadTooLarge);
        }
        let descriptor_count = read_u16(frame, 28);
        if descriptor_count > MAX_CONTROL_DESCRIPTORS {
            return Err(ControlFrameHeaderError::TooManyDescriptors);
        }

        Ok(Self {
            kind: MessageKind::from_wire(read_u16(frame, 10))?,
            protocol_major: read_u16(frame, 12),
            protocol_minor: read_u16(frame, 14),
            request_id: read_u64(frame, 16),
            payload_bytes,
            descriptor_count,
        })
    }
}

/// Borrowed header and payload bytes, without ancillary FD ownership or JSON validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlFrame<'a> {
    header: ControlFrameHeader,
    payload: &'a [u8],
}

impl<'a> ControlFrame<'a> {
    pub fn decode(bytes: &'a [u8]) -> Result<Self, ControlFrameError> {
        let (frame, consumed) = Self::decode_prefix(bytes)?;
        if consumed != bytes.len() {
            return Err(ControlFrameError::TrailingBytes {
                expected: consumed,
                actual: bytes.len(),
            });
        }
        Ok(frame)
    }

    /// Parses the first complete frame and returns its byte length. Trailing
    /// bytes may belong to another frame; ancillary FDs are handled separately.
    pub fn decode_prefix(bytes: &'a [u8]) -> Result<(Self, usize), ControlFrameError> {
        if bytes.len() < CONTROL_FRAME_HEADER_BYTES {
            return Err(ControlFrameError::TruncatedHeader {
                actual: bytes.len(),
            });
        }

        let header = ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES])
            .map_err(ControlFrameError::InvalidHeader)?;
        let payload_bytes = header.payload_bytes() as usize;
        let frame_bytes = CONTROL_FRAME_HEADER_BYTES + payload_bytes;
        if bytes.len() < frame_bytes {
            return Err(ControlFrameError::TruncatedPayload {
                declared: payload_bytes,
                available: bytes.len() - CONTROL_FRAME_HEADER_BYTES,
            });
        }
        Ok((
            Self {
                header,
                payload: &bytes[CONTROL_FRAME_HEADER_BYTES..frame_bytes],
            },
            frame_bytes,
        ))
    }

    pub const fn header(self) -> ControlFrameHeader {
        self.header
    }

    pub const fn payload(self) -> &'a [u8] {
        self.payload
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlFrameError {
    InvalidHeader(ControlFrameHeaderError),
    TruncatedHeader { actual: usize },
    TruncatedPayload { declared: usize, available: usize },
    TrailingBytes { expected: usize, actual: usize },
    PayloadLengthMismatch { declared: u32, actual: u32 },
    DescriptorCountMismatch { declared: u16, actual: usize },
    PayloadTooLarge,
    OutputTooSmall { required: usize, available: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlFrameHeaderError {
    WrongLength,
    WrongMagic,
    UnsupportedFormat,
    UnknownMessageKind,
    ReservedBytesSet,
    PayloadTooLarge,
    TooManyDescriptors,
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

#[cfg(test)]
#[path = "../tests/frame/header.rs"]
mod tests;
