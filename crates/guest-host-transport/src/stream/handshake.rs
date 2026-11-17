//! Performs Hello once and retains the selection on the stream that exchanged it.

use super::{invalid_data, invalid_outbound_session, shutdown_stream, UnixControlStream};
use lasper_guest_protocol::{
    CompatibilityError, ControlFrameHeader, EnforcedControlLimits, HelloRequest, HelloResponse,
    MessageKind, HELLO_PAYLOAD_BUFFER_BYTES,
};
use std::{fmt, io};

impl UnixControlStream {
    /// Exchanges Hello, retains its result on this stream and returns a snapshot.
    /// The caller owns the deadline; cancellation during I/O closes the stream.
    /// This does not verify the peer's machine membership or helper readiness.
    pub async fn handshake(
        &mut self,
        request_id: u64,
        request: HelloRequest,
    ) -> Result<NegotiatedHello, HandshakeError> {
        let mut payload = [0; HELLO_PAYLOAD_BUFFER_BYTES];
        let length = request.encode_payload(&mut payload).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid Hello request: {error:?}"),
            )
        })?;
        let header = ControlFrameHeader::new_with_version(
            lasper_guest_protocol::ProtocolVersion {
                major: request.offer().protocols.major,
                minor: lasper_guest_protocol::BOOTSTRAP_MANIFEST.minor,
            },
            MessageKind::HelloRequest,
            request_id,
            length as u32,
            0,
        )
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid Hello header: {error:?}"),
            )
        })?;
        self.send_frame(header, &payload[..length], &[]).await?;
        let frame = self.receive_frame().await?;
        let result = (|| {
            if frame.header().kind() == MessageKind::HelloRejected {
                let rejected =
                    lasper_guest_protocol::HelloRejected::decode_payload(frame.payload())
                        .map_err(|e| invalid_data(format!("invalid Hello rejection: {e:?}")))?;
                rejected
                    .validate_for(request, frame.header().protocol_version())
                    .map_err(HandshakeError::Negotiation)?;
                let reason = match rejected.reason {
                    CompatibilityError::MajorMismatch { local, peer } => {
                        CompatibilityError::MajorMismatch {
                            local: peer,
                            peer: local,
                        }
                    }
                    CompatibilityError::NoCommonMinor { local, peer } => {
                        CompatibilityError::NoCommonMinor {
                            local: peer,
                            peer: local,
                        }
                    }
                    reason => reason,
                };
                return Err(HandshakeError::Negotiation(reason));
            }
            let response = HelloResponse::decode_payload(frame.payload())
                .map_err(|error| invalid_data(format!("invalid Hello response: {error:?}")))?;
            let negotiated = request
                .negotiate_protocol(frame.header().protocol_version(), response)
                .map_err(HandshakeError::Negotiation)?;
            self.session
                .complete_protocol(negotiated)
                .map_err(invalid_outbound_session)?;
            let negotiated = NegotiatedHello::new(request, response, negotiated);
            self.negotiated = Some(negotiated);
            Ok(negotiated)
        })();
        if result.is_err() {
            self.poisoned = true;
            shutdown_stream(&self.stream);
        }
        result
    }
}

/// A snapshot of a completed Hello exchange and its installed limits.
/// [`Self::response`] retains the peer's offer, which may exceed those limits.
/// Worker readiness reads the stream's retained record rather than this value.
#[derive(Clone, Copy, Debug)]
pub struct NegotiatedHello {
    request: HelloRequest,
    response: HelloResponse,
    negotiated: lasper_guest_protocol::NegotiatedProtocol,
}

impl NegotiatedHello {
    pub(super) const fn new(
        request: HelloRequest,
        response: HelloResponse,
        negotiated: lasper_guest_protocol::NegotiatedProtocol,
    ) -> Self {
        Self {
            request,
            response,
            negotiated,
        }
    }

    pub const fn profile(&self) -> &'static lasper_guest_protocol::ProtocolProfile {
        self.negotiated.profile()
    }
    pub const fn protocol(&self) -> lasper_guest_protocol::NegotiatedProtocol {
        self.negotiated
    }
    pub const fn version(&self) -> lasper_guest_protocol::ProtocolVersion {
        self.negotiated.profile().version()
    }
    pub const fn features(&self) -> lasper_guest_protocol::FeatureSet {
        self.negotiated.features()
    }
    pub const fn broker_id(&self) -> lasper_guest_protocol::BrokerId {
        self.response.broker_id()
    }

    pub const fn request(&self) -> HelloRequest {
        self.request
    }

    pub const fn response(&self) -> HelloResponse {
        self.response
    }

    pub const fn limits(&self) -> EnforcedControlLimits {
        self.negotiated.limits()
    }
}

/// A failed Hello exchange or a peer whose capabilities do not meet the request.
#[derive(Debug)]
pub enum HandshakeError {
    Transport(io::Error),
    Negotiation(CompatibilityError),
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
