use super::{wire, SessionExit, SessionPhase};
use lasper_guest_protocol::{ErrorFrame, PayloadCodecError, SessionFrameError};
use std::{fmt, io, sync::Arc};

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum BrokerSessionError {
    Io(Arc<io::Error>),
    Protocol(SessionFrameError),
    Rejected(ErrorFrame),
    InvalidRequest(String),
    InvalidState(SessionPhase),
    ProjectionChanged,
    Deadline,
    DriverStopped,
    /// The report arrived before the Start acknowledgement. Use its execution
    /// status; this error alone does not mean the command was never run.
    StartInterrupted(Arc<SessionExit>),
    StartOutcomeUnknown(Arc<BrokerSessionError>),
}

impl fmt::Display for BrokerSessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "guest session: {error}"),
            Self::Protocol(error) => write!(f, "invalid guest session reply: {error:?}"),
            Self::Rejected(error) => write!(
                f,
                "guest {:?} rejected: {:?} (errno {:?})",
                error.operation(),
                error.code(),
                error.errno()
            ),
            Self::InvalidRequest(error) => write!(f, "invalid guest session request: {error}"),
            Self::InvalidState(phase) => {
                write!(f, "guest session cannot accept this operation in {phase:?}")
            }
            Self::ProjectionChanged => f.write_str(
                "guest display evidence changed; recheck and prepare display access again",
            ),
            Self::Deadline => f.write_str("guest session operation deadline expired"),
            Self::DriverStopped => f.write_str("guest session owner has stopped"),
            Self::StartInterrupted(exit) => write!(
                f,
                "guest reported {:?} before confirming Start acceptance (cleanup {:?})",
                exit.report().execution(),
                exit.report().cleanup()
            ),
            Self::StartOutcomeUnknown(error) => {
                write!(f, "guest Start outcome is unknown: {error}")
            }
        }
    }
}

impl std::error::Error for BrokerSessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error.as_ref()),
            Self::StartOutcomeUnknown(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

impl From<io::Error> for BrokerSessionError {
    fn from(error: io::Error) -> Self {
        Self::Io(Arc::new(error))
    }
}

impl From<SessionFrameError> for BrokerSessionError {
    fn from(error: SessionFrameError) -> Self {
        Self::Protocol(error)
    }
}

impl From<PayloadCodecError> for BrokerSessionError {
    fn from(error: PayloadCodecError) -> Self {
        wire::encoding(error)
    }
}
