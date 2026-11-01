//! Bounded observation of a guest command's output. A complete protocol reply
//! is not a process exit status, and PTY EOF cannot supply that status.

use super::{
    GuestExecutionError, GuestExecutionFailure, GuestExecutionPort, GuestExecutionRequest,
    PreparedGuestExecution,
};
use crate::application::sessions::{SessionError, TerminalSessionHandle};
use crate::domain::session::{SessionId, SessionSize};
use std::time::Duration;

pub(crate) struct GuestOutputLimits {
    /// Shared across candidates; an unsuccessful probe cannot reset it.
    pub(crate) deadline: tokio::time::Instant,
    pub(crate) opening: Duration,
    pub(crate) capture: Duration,
    pub(crate) bytes: usize,
}

/// The consumer owns its protocol. The execution layer owns output limits and
/// session closure, without knowing X11, Wayland, or their frame fields.
pub(crate) trait GuestOutputBoundary: Sync {
    fn complete(&self, output: &[u8]) -> Result<Option<usize>, SessionError>;
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GuestOutputEnd {
    Complete,
    Closed,
    TimedOut,
    LimitReached,
}

#[derive(Debug)]
pub(crate) struct GuestCommandOutput {
    pub(crate) bytes: Vec<u8>,
    pub(crate) end: GuestOutputEnd,
}

pub(crate) async fn run_bounded(
    execution: &impl GuestExecutionPort,
    request: GuestExecutionRequest,
    id: SessionId,
    limits: &GuestOutputLimits,
    boundary: &impl GuestOutputBoundary,
) -> Result<GuestCommandOutput, GuestExecutionError> {
    let now = tokio::time::Instant::now();
    let opening_deadline =
        (now + limits.opening).min(limits.deadline.checked_sub(limits.capture).unwrap_or(now));
    if tokio::time::Instant::now() >= opening_deadline {
        return Err(GuestExecutionError::new(
            GuestExecutionFailure::NotAttempted,
            SessionError::new("guest command deadline expired before preparation"),
        ));
    }
    // Providers enforce this deadline and finish cancellation/reaping before
    // returning. An outer timeout would interrupt that cleanup at the same
    // instant it is needed.
    let prepared = execution.prepare(request, opening_deadline).await?;
    // Do not submit a new process when preparation used up the opening budget.
    if tokio::time::Instant::now() >= opening_deadline {
        return Err(GuestExecutionError::new(
            GuestExecutionFailure::NotAttempted,
            SessionError::new("guest command deadline expired before submission"),
        ));
    }
    let handle = prepared
        .open_local(
            id,
            SessionSize::new(80, 24).expect("fixed probe size is valid"),
        )
        .await?;
    collect_output(handle, limits, boundary)
        .await
        .map_err(|error| GuestExecutionError::new(GuestExecutionFailure::OutcomeUnknown, error))
}

pub(crate) async fn collect_output(
    mut handle: TerminalSessionHandle,
    limits: &GuestOutputLimits,
    boundary: &impl GuestOutputBoundary,
) -> Result<GuestCommandOutput, SessionError> {
    // Owning the handle across all awaits also closes it on cancellation.
    let result = capture(&mut handle, limits, boundary).await;
    handle.close();
    result
}

async fn capture(
    handle: &mut TerminalSessionHandle,
    limits: &GuestOutputLimits,
    boundary: &impl GuestOutputBoundary,
) -> Result<GuestCommandOutput, SessionError> {
    let mut output = handle
        .take_output()
        .ok_or_else(|| SessionError::new("guest command output is unavailable"))?;
    let deadline = (tokio::time::Instant::now() + limits.capture).min(limits.deadline);
    let mut bytes = Vec::new();
    let end = loop {
        match tokio::time::timeout_at(deadline, output.recv()).await {
            Ok(Some(chunk)) => {
                let remaining = limits.bytes.saturating_sub(bytes.len());
                bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                if let Some(end) = boundary.complete(&bytes)? {
                    if end > bytes.len() {
                        return Err(SessionError::new(
                            "guest output boundary exceeds captured data",
                        ));
                    }
                    bytes.truncate(end);
                    break GuestOutputEnd::Complete;
                }
                if bytes.len() == limits.bytes {
                    break GuestOutputEnd::LimitReached;
                }
            }
            Ok(None) => break GuestOutputEnd::Closed,
            Err(_) => break GuestOutputEnd::TimedOut,
        }
    };
    Ok(GuestCommandOutput { bytes, end })
}

#[cfg(test)]
mod tests;
