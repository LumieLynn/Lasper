use super::*;
use crate::{
    BrokerId, ControlLimits, FeatureSet, HelloRequest, HelloResponse, HELLO_PAYLOAD_BUFFER_BYTES,
};

#[test]
fn control_session_requires_exactly_one_bounded_hello() {
    let mut host = ControlConnectionState::host();
    let mut guest = ControlConnectionState::guest();
    let prepare = crate::ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::PrepareRequest,
        1,
        0,
        0,
    )
    .unwrap();
    assert_eq!(
        host.next_outbound(prepare),
        Err(ControlSessionError::UnexpectedMessage {
            expected: MessageKind::HelloRequest,
            actual: MessageKind::PrepareRequest,
        })
    );
    assert_eq!(
        host.next_inbound(prepare),
        Err(ControlSessionError::ExpectedOutbound(
            MessageKind::HelloRequest
        ))
    );
    assert_eq!(
        guest.next_outbound(prepare),
        Err(ControlSessionError::ExpectedInbound(
            MessageKind::HelloRequest
        ))
    );

    let oversized_request = crate::ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloRequest,
        17,
        HELLO_PAYLOAD_BUFFER_BYTES as u32 + 1,
        0,
    )
    .unwrap();
    assert!(matches!(
        host.next_outbound(oversized_request),
        Err(ControlSessionError::Limit(
            ControlLimitError::FrameTooLarge { .. }
        ))
    ));

    let request_header = crate::ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloRequest,
        17,
        0,
        0,
    )
    .unwrap();
    host = host.next_outbound(request_header).unwrap();
    guest = guest.next_inbound(request_header).unwrap();
    assert_eq!(
        host.next_outbound(request_header),
        Err(ControlSessionError::ExpectedInbound(
            MessageKind::HelloResponse
        ))
    );
    assert_eq!(
        guest.next_inbound(request_header),
        Err(ControlSessionError::ExpectedOutbound(
            MessageKind::HelloResponse
        ))
    );

    let response_header = crate::ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloResponse,
        17,
        0,
        0,
    )
    .unwrap();
    guest = guest.next_outbound(response_header).unwrap();
    host = host.next_inbound(response_header).unwrap();
    assert_eq!(
        host.next_inbound(response_header),
        Err(ControlSessionError::NegotiationPending)
    );
    assert_eq!(
        guest.next_outbound(response_header),
        Err(ControlSessionError::NegotiationPending)
    );

    let request = HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap();
    let (response, _) = HelloResponse::accept(
        request,
        crate::HelloOffer::local(ControlLimits::CURRENT),
        BrokerId::new([3; 16]),
    )
    .unwrap();
    let established = request
        .negotiate_protocol(ProtocolVersion { major: 1, minor: 0 }, response)
        .unwrap();
    assert_eq!(
        ControlConnectionState::host().complete_protocol(established),
        Err(ControlSessionError::NegotiationPending)
    );
    assert_eq!(
        ControlConnectionState::guest().complete_protocol(established),
        Err(ControlSessionError::NegotiationPending)
    );
    host.complete_protocol(established).unwrap();
    guest.complete_protocol(established).unwrap();

    let repeated_hello = crate::ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloResponse,
        1,
        0,
        0,
    )
    .unwrap();
    assert_eq!(
        host.next_inbound(repeated_hello),
        Err(ControlSessionError::UnexpectedHello)
    );
    assert_eq!(
        host.complete_protocol(established),
        Err(ControlSessionError::HandshakeAlreadyComplete)
    );
    assert_eq!(host.next_outbound(prepare), Ok(host));
    assert_eq!(guest.next_inbound(prepare), Ok(guest));
    let wrong_direction = Err(ControlSessionError::UnsupportedMessage {
        kind: MessageKind::PrepareRequest,
        direction: crate::Direction::GuestToHost,
    });
    assert_eq!(host.next_inbound(prepare), wrong_direction);
    assert_eq!(guest.next_outbound(prepare), wrong_direction);
}

#[test]
fn hello_rejection_cannot_be_completed_or_reused() {
    let request = crate::ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloRequest,
        17,
        0,
        0,
    )
    .unwrap();
    let rejected = crate::ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloRejected,
        17,
        0,
        0,
    )
    .unwrap();
    let host = ControlConnectionState::host()
        .next_outbound(request)
        .unwrap();
    let guest = ControlConnectionState::guest()
        .next_inbound(request)
        .unwrap();
    let mut host = host.next_inbound(rejected).unwrap();
    let mut guest = guest.next_outbound(rejected).unwrap();
    let (_, selection) = HelloResponse::accept(
        HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap(),
        crate::HelloOffer::local(ControlLimits::CURRENT),
        BrokerId::new([3; 16]),
    )
    .unwrap();
    let prepare = crate::ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::PrepareRequest,
        18,
        0,
        0,
    )
    .unwrap();
    for state in [&mut host, &mut guest] {
        assert_eq!(
            state.complete_protocol(selection),
            Err(ControlSessionError::HandshakeRejected)
        );
        for header in [request, rejected, prepare] {
            assert_eq!(
                state.next_outbound(header),
                Err(ControlSessionError::HandshakeRejected)
            );
            assert_eq!(
                state.next_inbound(header),
                Err(ControlSessionError::HandshakeRejected)
            );
        }
    }
}

#[test]
fn hello_response_must_match_the_request_id() {
    let request = crate::ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloRequest,
        41,
        0,
        0,
    )
    .unwrap();
    let response = crate::ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloResponse,
        42,
        0,
        0,
    )
    .unwrap();
    let host = ControlConnectionState::host()
        .next_outbound(request)
        .unwrap();
    let guest = ControlConnectionState::guest()
        .next_inbound(request)
        .unwrap();

    let expected = Err(ControlSessionError::MismatchedHelloRequestId {
        expected: 41,
        actual: 42,
    });
    assert_eq!(host.next_inbound(response), expected);
    assert_eq!(guest.next_outbound(response), expected);
}
