use super::BrokerSessionError;
use lasper_guest_protocol::ErrorFrame;
use std::fmt;
use std::sync::Arc;
use tokio::sync::oneshot;

/// The observed result of one Start attempt. Acceptance does not confirm exec;
/// an unknown result must not be retried as a new command.
#[derive(Clone, Debug)]
#[must_use]
pub enum StartOutcome {
    /// The guardian accepted Start; data descriptors were delivered by Prepare.
    Accepted,
    /// Exit arrived before the acknowledgement. Cleanup may still be in progress.
    Reported(lasper_guest_protocol::ExitReport),
    /// Start may have reached the guest. The cause cannot establish non-execution.
    Unknown { cause: Arc<BrokerSessionError> },
}

pub(super) async fn receive(
    received: oneshot::Receiver<Result<StartOutcome, StartError>>,
    delivered: oneshot::Sender<()>,
) -> Result<StartOutcome, StartError> {
    // The command is already queued. Losing its reply cannot prove non-execution.
    let outcome = received.await.unwrap_or_else(|_| {
        Ok(StartOutcome::Unknown {
            cause: Arc::new(BrokerSessionError::DriverStopped),
        })
    })?;
    // Confirm delivery without yielding, so cancellation cannot abandon an
    // acceptance after the caller has already received it.
    let _ = delivered.send(());
    Ok(outcome)
}

/// This attempt did not submit Start, or the guest explicitly rejected it.
/// This says nothing about execution by an earlier attempt.
#[derive(Clone, Debug)]
pub enum StartError {
    NotSubmitted(BrokerSessionError),
    Rejected(ErrorFrame),
}

impl fmt::Display for StartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSubmitted(cause) => write!(formatter, "Start was not submitted: {cause}"),
            Self::Rejected(frame) => write!(formatter, "{}", BrokerSessionError::Rejected(*frame)),
        }
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotSubmitted(cause) => Some(cause),
            Self::Rejected(_) => None,
        }
    }
}

#[cfg(test)]
#[path = "../../tests/session/start_reply.rs"]
mod tests;
