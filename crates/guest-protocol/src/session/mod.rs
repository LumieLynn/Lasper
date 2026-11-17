//! Session messages bind operations to one Prepare on one connection.
//! Payload codecs validate that binding; the shared wire state checks ordering.
//! Neither owns the worker or performs cleanup.

mod control;
mod error;
mod exit;
mod failure;
mod frame;
mod start;
mod state;

pub use control::{
    ControlReplyFrame, ControlReplyStatus, PreparationControl, PreparationControlFrame,
    PreparationControlKind,
};
pub use error::{ErrorFrame, GuestErrorCode, GuestOperation};
pub use exit::{CleanupStatus, ExecutionStatus, ExitFrame, ExitReason, ExitReport, ExitStatus};
pub use failure::{GuestFailure, GuestFailureStage};
pub use frame::{SessionFrameError, SessionPayloadError};
pub use start::{StartAcceptedFrame, StartFrame, StartRequest, MAX_EXECUTION_TIMEOUT_MS};
pub use state::{WireReply, WireSessionError, WireSessionPhase, WireSessionState};
