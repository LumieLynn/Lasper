use super::{BrokerSessionError, SessionExit};
use lasper_guest_protocol::ErrorFrame;
use std::fmt;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use tokio::sync::oneshot;

/// The observed result of one Start attempt. Acceptance does not confirm exec;
/// an unknown result must not be retried as a new command.
#[derive(Debug)]
#[must_use]
pub enum StartOutcome {
    /// The guardian accepted Start. A PTY session also returns its master.
    Accepted { terminal: Option<OwnedFd> },
    /// Exit arrived before the acknowledgement. Cleanup may still be in progress.
    Reported(Arc<SessionExit>),
    /// Start may have reached the guest. The cause cannot establish non-execution.
    Unknown { cause: BrokerSessionError },
}

pub(super) async fn receive(
    received: oneshot::Receiver<Result<StartOutcome, StartError>>,
    delivered: oneshot::Sender<()>,
) -> Result<StartOutcome, StartError> {
    // The command is already queued. Losing its reply cannot prove non-execution.
    let outcome = received.await.unwrap_or_else(|_| {
        Ok(StartOutcome::Unknown {
            cause: BrokerSessionError::DriverStopped,
        })
    })?;
    // Do not yield between confirming delivery and returning the owned PTY.
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
mod tests {
    use super::*;

    #[tokio::test]
    async fn losing_the_driver_reply_does_not_claim_start_was_not_submitted() {
        let (reply, received) = oneshot::channel();
        let (delivered, _delivery) = oneshot::channel();
        drop(reply);
        assert!(matches!(
            receive(received, delivered).await.unwrap(),
            StartOutcome::Unknown {
                cause: BrokerSessionError::DriverStopped
            }
        ));
    }
}
