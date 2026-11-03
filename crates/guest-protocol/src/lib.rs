#![no_std]
#![forbid(unsafe_code)]

mod control;
mod identity;

pub use control::{
    ControlFrame, ControlFrameError, ControlFrameHeader, ControlFrameHeaderError, MessageKind,
    CONTROL_FRAME_DESCRIPTOR_ANCHOR_BYTES, CONTROL_FRAME_HEADER_BYTES, MAX_CONTROL_DESCRIPTORS,
    MAX_CONTROL_PAYLOAD_BYTES,
};
pub use identity::{
    Architecture, FeatureSet, IdentityDecodeError, RuntimeCompatibilityError, RuntimeIdentity,
    RuntimeRequirements, IDENTITY_FRAME_BYTES,
};

pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_MINOR: u16 = 0;
pub const MAX_CONTROL_FRAME_BYTES: u32 = 1024 * 1024;
