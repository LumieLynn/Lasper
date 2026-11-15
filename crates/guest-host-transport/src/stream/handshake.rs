use lasper_guest_protocol::{
    EnforcedControlLimits, HelloNegotiationError, HelloRequest, HelloResponse,
};
use std::{fmt, io};

/// A snapshot of a completed Hello exchange and its installed limits.
/// [`Self::response`] retains the peer's offer, which may exceed those limits.
/// Worker readiness reads the stream's retained record rather than this value.
#[derive(Clone, Copy, Debug)]
pub struct NegotiatedHello {
    request: HelloRequest,
    response: HelloResponse,
    limits: EnforcedControlLimits,
}

impl NegotiatedHello {
    pub(super) const fn new(
        request: HelloRequest,
        response: HelloResponse,
        limits: EnforcedControlLimits,
    ) -> Self {
        Self {
            request,
            response,
            limits,
        }
    }

    pub const fn request(&self) -> HelloRequest {
        self.request
    }

    pub const fn response(&self) -> HelloResponse {
        self.response
    }

    pub const fn limits(&self) -> EnforcedControlLimits {
        self.limits
    }
}

/// A failed Hello exchange or a peer whose capabilities do not meet the request.
#[derive(Debug)]
pub enum HandshakeError {
    Transport(io::Error),
    Negotiation(HelloNegotiationError),
}

impl From<io::Error> for HandshakeError {
    fn from(error: io::Error) -> Self {
        Self::Transport(error)
    }
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => error.fmt(formatter),
            Self::Negotiation(error) => write!(formatter, "Hello negotiation failed: {error:?}"),
        }
    }
}

impl std::error::Error for HandshakeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::Negotiation(_) => None,
        }
    }
}
