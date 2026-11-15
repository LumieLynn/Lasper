mod control;
mod error;
mod exit;
mod failure;
mod frame;
mod output;
mod start;

pub use control::{
    ControlReplyFrame, ControlReplyStatus, PreparationControl, PreparationControlFrame,
    PreparationControlKind,
};
pub use error::{ErrorFrame, GuestErrorCode, GuestOperation};
pub use exit::{CleanupStatus, ExecutionStatus, ExitFrame, ExitReason, ExitReport, ExitStatus};
pub use failure::{GuestFailure, GuestFailureStage};
pub use frame::{SessionFrameError, SessionPayloadError};
pub use output::{CapturedOutput, OutputStream, MAX_CAPTURED_OUTPUT_BYTES};
pub use start::{StartAcceptedFrame, StartFrame, StartRequest, MAX_EXECUTION_TIMEOUT_MS};

/// Helper sessions need room for status even when no output can be retained.
pub const MIN_HELPER_CONTROL_FRAME_BYTES: u32 = 4096;
