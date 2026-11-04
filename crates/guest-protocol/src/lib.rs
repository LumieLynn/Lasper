#![no_std]
#![forbid(unsafe_code)]

mod control;
mod identity;
mod messages;
mod prepare;

pub use control::{
    ControlFrame, ControlFrameError, ControlFrameHeader, ControlFrameHeaderError, MessageKind,
    CONTROL_FRAME_DESCRIPTOR_ANCHOR_BYTES, CONTROL_FRAME_HEADER_BYTES, MAX_CONTROL_DESCRIPTORS,
    MAX_CONTROL_PAYLOAD_BYTES,
};
pub use identity::{
    Architecture, FeatureSet, IdentityDecodeError, RuntimeCompatibilityError, RuntimeIdentity,
    RuntimeRequirements, IDENTITY_FRAME_BYTES,
};
pub use messages::{
    BrokerId, ControlLimitError, ControlLimits, ControlSessionError, ControlSessionState,
    EnforcedControlLimits, HelloNegotiationError, HelloRequest, HelloResponse, PayloadCodecError,
    HELLO_CONTROL_FRAME_BYTES, HELLO_CONTROL_LIMITS, HELLO_PAYLOAD_BUFFER_BYTES,
    MIN_NEGOTIATED_CONTROL_FRAME_BYTES,
};
pub use prepare::{
    CommandArgv, ExecutionKind, GuestProcess, PreparationId, PrepareFrameError, PrepareReady,
    PrepareReadyDescriptorRole, PrepareReadyFrame, PrepareRequest, PrepareRequestFrame,
    ProjectionRequests, TerminalEnvironment, TerminalSize, WaylandProjectionRequest,
    X11ProjectionRequest, MAX_COMMAND_ARGUMENTS, MAX_COMMAND_ARGUMENT_BYTES,
    MAX_COMMAND_TOTAL_BYTES, MAX_GUEST_USER_NAME_BYTES, MAX_PREPARED_LEASE_MS,
    MAX_PROJECTION_PATH_BYTES,
};

pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_MINOR: u16 = 0;
pub const MAX_CONTROL_FRAME_BYTES: u32 = 1024 * 1024;
