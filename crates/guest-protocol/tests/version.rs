use lasper_guest_protocol::*;

fn other_versions(selected: ProtocolVersion) -> [ProtocolVersion; 2] {
    [
        ProtocolVersion {
            major: selected.major + 1,
            ..selected
        },
        ProtocolVersion {
            minor: selected.minor + 1,
            ..selected
        },
    ]
}

fn header(kind: MessageKind, version: ProtocolVersion) -> ControlFrameHeader {
    ControlFrameHeader::new_with_version(version, kind, 1, 0, 0).unwrap()
}

fn hello() -> (HelloRequest, HelloResponse) {
    let request = HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap();
    let (response, _) = HelloResponse::accept(
        request,
        HelloOffer::local(ControlLimits::CURRENT),
        BrokerId::new([1; 16]),
    )
    .unwrap();
    (request, response)
}

#[test]
fn bootstrap_allows_foreign_major_to_reach_hello_negotiation() {
    let foreign = ProtocolVersion { major: 2, minor: 0 };
    assert!(ControlConnectionState::guest()
        .next_inbound(header(MessageKind::HelloRequest, foreign))
        .is_ok());
    let invalid = ProtocolVersion { major: 1, minor: 1 };
    assert!(ControlConnectionState::guest()
        .next_inbound(header(MessageKind::HelloRequest, invalid))
        .is_err());
}

#[test]
fn established_sessions_cannot_change_versions_in_either_direction() {
    let request = header(MessageKind::HelloRequest, ProtocolVersion::BOOTSTRAP);
    let response = header(MessageKind::HelloResponse, ProtocolVersion::BOOTSTRAP);
    let mut host = ControlConnectionState::host()
        .next_outbound(request)
        .unwrap()
        .next_inbound(response)
        .unwrap();
    let mut guest = ControlConnectionState::guest()
        .next_inbound(request)
        .unwrap()
        .next_outbound(response)
        .unwrap();
    let (request, response) = hello();
    let protocol = request
        .negotiate_protocol(ProtocolVersion::BOOTSTRAP, response)
        .unwrap();
    host.complete_protocol(protocol).unwrap();
    guest.complete_protocol(protocol).unwrap();
    for version in other_versions(protocol.profile().version()) {
        let error = Err(ControlSessionError::ProtocolVersion(
            ProtocolVersionMismatch {
                expected: protocol.profile().version(),
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
