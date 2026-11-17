#![no_std]
#![forbid(unsafe_code)]

//! Wire formats and validation shared by the host and guest.
//!
//! [`ControlFrame`] parses framing within this build's storage bounds.
//! [`ControlConnectionState`] checks Hello order and negotiated header constraints.
//! [`ProtocolProfile`] describes the registered schemas and capacities.
//! [`NegotiatedProtocol`] encodes and decodes session messages with the selected
//! schemas, connection limits and feature gates. [`WireSessionState`] checks
//! session message order.
//!
//! Transports own the descriptors and apply these checks before acting on a
//! message. Process supervision and cleanup belong to the guest guardian.

mod control;
mod features;
mod identity;
mod identity_channel;
mod json;
mod messages;
mod prepare;
pub mod profile;
mod session;
mod version;

pub use control::{
    ControlFrame, ControlFrameError, ControlFrameHeader, ControlFrameHeaderError, MessageKind,
    CONTROL_FRAME_DESCRIPTOR_ANCHOR_BYTES, CONTROL_FRAME_HEADER_BYTES, MAX_CONTROL_DESCRIPTORS,
    MAX_CONTROL_PAYLOAD_BYTES,
};
pub use features::FeatureSet;
pub use identity::{Architecture, IdentityDecodeError, RuntimeIdentity, IDENTITY_FRAME_BYTES};
pub use identity_channel::{
    GuestIdentityReply, IdentityChallenge, IdentityChannelError, IDENTITY_CHALLENGE_BYTES,
    MAX_IDENTITY_MESSAGE_BYTES,
};
pub use json::PayloadCodecError;
pub use messages::{
    BootstrapManifest, BootstrapMessageSchema, BootstrapSchemaId, BrokerId, ControlConnectionState,
    ControlLimitError, ControlLimits, ControlSessionError, EnforcedControlLimits, HelloRejected,
    HelloRequest, HelloResponse, BOOTSTRAP_MANIFEST, HELLO_CONTROL_FRAME_BYTES,
    HELLO_CONTROL_LIMITS, HELLO_PAYLOAD_BUFFER_BYTES, MIN_NEGOTIATED_CONTROL_FRAME_BYTES,
};
pub use prepare::{
    CommandArgv, EnvironmentEntry, ExecutionKind, GuestProcess, PreparationId,
    PrepareDecodeStorage, PrepareFrameError, PrepareReady, PrepareReadyDescriptorRole,
    PrepareReadyFrame, PrepareRequest, PrepareRequestFrame, ProcessEnvironment, TerminalSize,
    MAX_COMMAND_ARGUMENTS, MAX_COMMAND_ARGUMENT_BYTES, MAX_COMMAND_TOTAL_BYTES,
    MAX_DEFAULT_PROCESS_ENVIRONMENT_BYTES, MAX_GUEST_USER_NAME_BYTES, MAX_PREPARED_LEASE_MS,
    MAX_PREPARE_UNESCAPE_BYTES, MAX_PROCESS_ENVIRONMENT_BYTES, MAX_PROCESS_ENVIRONMENT_ENTRIES,
};
pub use profile::*;
pub use session::{
    CleanupStatus, ControlReplyFrame, ControlReplyStatus, ErrorFrame, ExecutionStatus, ExitFrame,
    ExitReason, ExitReport, ExitStatus, GuestErrorCode, GuestFailure, GuestFailureStage,
    GuestOperation, PreparationControl, PreparationControlFrame, PreparationControlKind,
    SessionFrameError, SessionPayloadError, StartAcceptedFrame, StartFrame, StartRequest,
    WireReply, WireSessionError, WireSessionPhase, WireSessionState, MAX_EXECUTION_TIMEOUT_MS,
};
pub use version::{ProtocolVersion, ProtocolVersionMismatch};

pub const MAX_CONTROL_FRAME_BYTES: u32 = BUILD_CAPACITIES.max_control_frame_bytes;
