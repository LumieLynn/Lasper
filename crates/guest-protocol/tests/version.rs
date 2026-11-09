use lasper_guest_protocol::{
    Architecture, BrokerId, ControlFrameHeader, ControlLimits, ControlSessionError,
    ControlSessionState, FeatureSet, HelloNegotiationError, HelloRequest, HelloResponse,
    MessageKind, ProtocolVersion, ProtocolVersionMismatch, RuntimeCompatibilityError,
    RuntimeIdentity, RuntimeRequirements,
};

fn other_versions() -> [ProtocolVersion; 2] {
    [
        ProtocolVersion {
            major: ProtocolVersion::CURRENT.major + 1,
            ..ProtocolVersion::CURRENT
        },
        ProtocolVersion {
            minor: ProtocolVersion::CURRENT.minor + 1,
            ..ProtocolVersion::CURRENT
        },
    ]
}

fn header(kind: MessageKind, version: ProtocolVersion) -> ControlFrameHeader {
    let mut bytes = ControlFrameHeader::new(kind, 1, 0, 0).unwrap().encode();
    bytes[12..14].copy_from_slice(&version.major.to_le_bytes());
    bytes[14..16].copy_from_slice(&version.minor.to_le_bytes());
    ControlFrameHeader::decode(&bytes).unwrap()
}

fn hello() -> (HelloRequest, HelloResponse) {
    (
        HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap(),
        HelloResponse::new(
            FeatureSet::EMPTY,
            BrokerId::new([1; 16]),
            ControlLimits::CURRENT,
        )
        .unwrap(),
    )
}

#[test]
fn identity_and_hello_use_the_same_exact_version_requirement() {
    let requirements = RuntimeRequirements {
        architecture: Architecture::X86_64,
        required_features: FeatureSet::EMPTY,
        minimum_control_frame_bytes: 4096,
    };
    let current = RuntimeIdentity::current(Architecture::X86_64);
    let (request, response) = hello();
    assert_eq!(requirements.check(current), Ok(()));
    assert!(request
        .negotiate(ProtocolVersion::CURRENT, response)
        .is_ok());
    for version in other_versions() {
        let identity = RuntimeIdentity {
            protocol_major: version.major,
            protocol_minor: version.minor,
            ..current
        };
        let identity = RuntimeIdentity::decode(&identity.encode()).unwrap();
        assert_eq!(identity.protocol_version(), version);
        let error = ProtocolVersionMismatch {
            expected: ProtocolVersion::CURRENT,
            actual: version,
        };
        assert_eq!(
            requirements.check(identity),
            Err(RuntimeCompatibilityError::ProtocolVersion(error))
        );
        assert_eq!(
            request.negotiate(version, response),
            Err(HelloNegotiationError::ProtocolVersion(error))
        );
    }
}

#[test]
fn both_hello_directions_reject_different_major_or_minor_versions() {
    let request = header(MessageKind::HelloRequest, ProtocolVersion::CURRENT);
    let host = ControlSessionState::host().next_outbound(request).unwrap();
    let guest = ControlSessionState::guest().next_inbound(request).unwrap();
    for version in other_versions() {
        let error = Err(ControlSessionError::ProtocolVersion(
            ProtocolVersionMismatch {
                expected: ProtocolVersion::CURRENT,
                actual: version,
            },
        ));
        let foreign_request = header(MessageKind::HelloRequest, version);
        let foreign_response = header(MessageKind::HelloResponse, version);
        assert_eq!(
            ControlSessionState::host().next_outbound(foreign_request),
            error
        );
        assert_eq!(
            ControlSessionState::guest().next_inbound(foreign_request),
            error
        );
        assert_eq!(host.next_inbound(foreign_response), error);
        assert_eq!(guest.next_outbound(foreign_response), error);
    }
}

#[test]
fn established_sessions_cannot_change_versions_in_either_direction() {
    let request = header(MessageKind::HelloRequest, ProtocolVersion::CURRENT);
    let response = header(MessageKind::HelloResponse, ProtocolVersion::CURRENT);
    let mut host = ControlSessionState::host()
        .next_outbound(request)
        .unwrap()
        .next_inbound(response)
        .unwrap();
    let mut guest = ControlSessionState::guest()
        .next_inbound(request)
        .unwrap()
        .next_outbound(response)
        .unwrap();
    let (request, response) = hello();
    let limits = request
        .negotiate(ProtocolVersion::CURRENT, response)
        .unwrap();
    host.complete_hello(limits).unwrap();
    guest.complete_hello(limits).unwrap();
    for version in other_versions() {
        let error = Err(ControlSessionError::ProtocolVersion(
            ProtocolVersionMismatch {
                expected: ProtocolVersion::CURRENT,
                actual: version,
            },
        ));
        let request = header(MessageKind::PrepareRequest, version);
        let response = header(MessageKind::Exit, version);
        assert_eq!(host.next_outbound(request), error);
        assert_eq!(guest.next_inbound(request), error);
        assert_eq!(guest.next_outbound(response), error);
        assert_eq!(host.next_inbound(response), error);
    }
}
