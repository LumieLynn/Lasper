//! Header validation for Hello and established control connections. Payload
//! validity and session ordering are checked by their respective codecs and
//! WireSessionState before the transport acts on a message.

use crate::{
    ControlLimitError, ControlLimits, Direction, MessageKind, ProtocolVersion,
    ProtocolVersionMismatch, BOOTSTRAP_MANIFEST,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlSessionError {
    ProtocolVersion(ProtocolVersionMismatch),
    Limit(ControlLimitError),
    ExpectedOutbound(MessageKind),
    ExpectedInbound(MessageKind),
    UnexpectedMessage {
        expected: MessageKind,
        actual: MessageKind,
    },
    MismatchedHelloRequestId {
        expected: u64,
        actual: u64,
    },
    NegotiationPending,
    HandshakeRejected,
    HandshakeAlreadyComplete,
    UnexpectedHello,
    UnsupportedMessage {
        kind: MessageKind,
        direction: crate::Direction,
    },
}

/// Tracks Hello and validates negotiated versions, frame limits and message directions.
/// Payload codecs check fields and FD counts; [`crate::WireSessionState`] checks
/// session message order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlConnectionState {
    phase: ConnectionPhase,
    outbound_direction: Direction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectionPhase {
    HostSendRequest,
    HostReceiveResponse { request_id: u64 },
    GuestReceiveRequest,
    GuestSendResponse { request_id: u64 },
    NegotiationPending,
    Rejected,
    Established(crate::NegotiatedProtocol),
}

impl ControlConnectionState {
    pub const fn host() -> Self {
        Self {
            phase: ConnectionPhase::HostSendRequest,
            outbound_direction: Direction::HostToGuest,
        }
    }

    pub const fn guest() -> Self {
        Self {
            phase: ConnectionPhase::GuestReceiveRequest,
            outbound_direction: Direction::GuestToHost,
        }
    }

    pub fn next_outbound(
        self,
        header: crate::ControlFrameHeader,
    ) -> Result<Self, ControlSessionError> {
        match self.phase {
            ConnectionPhase::HostSendRequest => {
                validate_hello_kind(header, MessageKind::HelloRequest, self.outbound_direction)?;
                Ok(Self {
                    phase: ConnectionPhase::HostReceiveResponse {
                        request_id: header.request_id(),
                    },
                    ..self
                })
            }
            ConnectionPhase::GuestSendResponse { request_id } => {
                self.with_hello_response(header, request_id)
            }
            ConnectionPhase::HostReceiveResponse { .. } => Err(
                ControlSessionError::ExpectedInbound(MessageKind::HelloResponse),
            ),
            ConnectionPhase::GuestReceiveRequest => Err(ControlSessionError::ExpectedInbound(
                MessageKind::HelloRequest,
            )),
            ConnectionPhase::NegotiationPending => Err(ControlSessionError::NegotiationPending),
            ConnectionPhase::Rejected => Err(ControlSessionError::HandshakeRejected),
            ConnectionPhase::Established(protocol) => {
                validate_established_header(
                    protocol.limits().get(),
                    header,
                    protocol.profile().version(),
                )?;
                self.validate_profile(protocol, header, self.outbound_direction)?;
                Ok(self)
            }
        }
    }

    pub fn next_inbound(
        self,
        header: crate::ControlFrameHeader,
    ) -> Result<Self, ControlSessionError> {
        match self.phase {
            ConnectionPhase::GuestReceiveRequest => {
                validate_hello_kind(header, MessageKind::HelloRequest, Direction::HostToGuest)?;
                Ok(Self {
                    phase: ConnectionPhase::GuestSendResponse {
                        request_id: header.request_id(),
                    },
                    ..self
                })
            }
            ConnectionPhase::HostReceiveResponse { request_id } => {
                self.with_hello_response(header, request_id)
            }
            ConnectionPhase::HostSendRequest => Err(ControlSessionError::ExpectedOutbound(
                MessageKind::HelloRequest,
            )),
            ConnectionPhase::GuestSendResponse { .. } => Err(
                ControlSessionError::ExpectedOutbound(MessageKind::HelloResponse),
            ),
            ConnectionPhase::NegotiationPending => Err(ControlSessionError::NegotiationPending),
            ConnectionPhase::Rejected => Err(ControlSessionError::HandshakeRejected),
            ConnectionPhase::Established(protocol) => {
                validate_established_header(
                    protocol.limits().get(),
                    header,
                    protocol.profile().version(),
                )?;
                let direction = match self.outbound_direction {
                    Direction::HostToGuest => Direction::GuestToHost,
                    Direction::GuestToHost => Direction::HostToGuest,
                };
                self.validate_profile(protocol, header, direction)?;
                Ok(self)
            }
        }
    }

    /// Installs a validated Hello selection after the response header has passed.
    /// The caller must validate the response payload against the original offer
    /// before calling this method. Optional payload fields are checked by
    /// [`crate::NegotiatedProtocol`] codecs, not by this header state.
    pub fn complete_protocol(
        &mut self,
        negotiated: crate::NegotiatedProtocol,
    ) -> Result<(), ControlSessionError> {
        match self.phase {
            ConnectionPhase::NegotiationPending => {
                self.phase = ConnectionPhase::Established(negotiated);
                Ok(())
            }
            ConnectionPhase::Established(_) => Err(ControlSessionError::HandshakeAlreadyComplete),
            ConnectionPhase::Rejected => Err(ControlSessionError::HandshakeRejected),
            ConnectionPhase::HostSendRequest
            | ConnectionPhase::HostReceiveResponse { .. }
            | ConnectionPhase::GuestReceiveRequest
            | ConnectionPhase::GuestSendResponse { .. } => {
                Err(ControlSessionError::NegotiationPending)
            }
        }
    }

    fn with_hello_response(
        self,
        header: crate::ControlFrameHeader,
        request_id: u64,
    ) -> Result<Self, ControlSessionError> {
        validate_hello_kind(
            header,
            if header.kind() == MessageKind::HelloRejected {
                MessageKind::HelloRejected
            } else {
                MessageKind::HelloResponse
            },
            Direction::GuestToHost,
        )?;
        validate_request_id(header, request_id)?;
        Ok(Self {
            phase: if header.kind() == MessageKind::HelloRejected {
                ConnectionPhase::Rejected
            } else {
                ConnectionPhase::NegotiationPending
            },
            ..self
        })
    }

    fn validate_profile(
        &self,
        protocol: crate::NegotiatedProtocol,
        header: crate::ControlFrameHeader,
        direction: Direction,
    ) -> Result<(), ControlSessionError> {
        let features = protocol.features();
        let schema = protocol.profile().schema(header.kind(), direction);
        if !schema.is_some_and(|schema| features.contains(schema.required_features)) {
            return Err(ControlSessionError::UnsupportedMessage {
                kind: header.kind(),
                direction,
            });
        }
        Ok(())
    }
}

fn validate_hello_kind(
    header: crate::ControlFrameHeader,
    expected: MessageKind,
    direction: Direction,
) -> Result<(), ControlSessionError> {
    if header.kind() != expected {
        return Err(ControlSessionError::UnexpectedMessage {
            expected,
            actual: header.kind(),
        });
    }
    if BOOTSTRAP_MANIFEST
        .schema(header.kind())
        .map(|schema| schema.direction)
        != Some(direction)
    {
        return Err(ControlSessionError::UnsupportedMessage {
            kind: header.kind(),
            direction,
        });
    }
    if header.protocol_version().minor != BOOTSTRAP_MANIFEST.minor {
        return Err(ControlSessionError::ProtocolVersion(
            ProtocolVersionMismatch {
                expected: ProtocolVersion {
                    major: header.protocol_version().major,
                    minor: BOOTSTRAP_MANIFEST.minor,
                },
                actual: header.protocol_version(),
            },
        ));
    }
    match BOOTSTRAP_MANIFEST.limits.validate_header(header) {
        Ok(()) => Ok(()),
        Err(error) => Err(ControlSessionError::Limit(error)),
    }
}

fn validate_request_id(
    header: crate::ControlFrameHeader,
    expected: u64,
) -> Result<(), ControlSessionError> {
    if header.request_id() == expected {
        Ok(())
    } else {
        Err(ControlSessionError::MismatchedHelloRequestId {
            expected,
            actual: header.request_id(),
        })
    }
}

fn validate_established_header(
    limits: ControlLimits,
    header: crate::ControlFrameHeader,
    selected_version: ProtocolVersion,
) -> Result<(), ControlSessionError> {
    if BOOTSTRAP_MANIFEST.schema(header.kind()).is_some() {
        return Err(ControlSessionError::UnexpectedHello);
    }
    let expected = selected_version;
    if header.protocol_version() != expected {
        return Err(ControlSessionError::ProtocolVersion(
            ProtocolVersionMismatch {
                expected,
                actual: header.protocol_version(),
            },
        ));
    }
    match limits.validate_header(header) {
        Ok(()) => Ok(()),
        Err(error) => Err(ControlSessionError::Limit(error)),
    }
}

#[cfg(test)]
#[path = "../../tests/messages/connection.rs"]
mod tests;
