use crate::{BrokerConnectError, BrokerSessionError, SessionSnapshot};
use std::fmt;

#[derive(Clone, Debug)]
pub struct RequestError {
    pub field: &'static str,
    pub message: String,
}
impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}
impl std::error::Error for RequestError {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrepareCapacityError {
    FrameBytes { required: usize, available: usize },
    ReadyDescriptors { required: u16, available: u16 },
}
#[derive(Debug)]
pub enum PrepareError {
    InvalidRequest(RequestError),
    Connect(BrokerConnectError),
    Capacity(PrepareCapacityError),
    Session {
        error: BrokerSessionError,
        last_snapshot: SessionSnapshot,
    },
}
impl fmt::Display for PrepareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(e) => e.fmt(f),
            Self::Connect(e) => e.fmt(f),
            Self::Session { error, .. } => error.fmt(f),
            Self::Capacity(capacity) => write!(f, "Prepare capacity exceeded: {capacity:?}"),
        }
    }
}
impl std::error::Error for PrepareError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidRequest(e) => Some(e),
            Self::Connect(e) => Some(e),
            Self::Session { error, .. } => Some(error),
            Self::Capacity(_) => None,
        }
    }
}
pub(super) fn invalid(field: &'static str, message: impl fmt::Debug) -> RequestError {
    RequestError {
        field,
        message: format!("{message:?}"),
    }
}
