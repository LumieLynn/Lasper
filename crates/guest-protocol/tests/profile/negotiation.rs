extern crate std;
use super::*;

#[test]
fn artifact_metadata_aggregates_the_same_profiles_for_every_field() {
    let base = protocol_profiles()[0];
    let profiles = [
        ProtocolProfile {
            version: ProtocolVersion { major: 1, minor: 1 },
            features: FeatureSet::EXECUTION_TIMEOUT,
            capacities: ProfileCapacities {
                max_control_frame_bytes: 64 * 1024,
                ..*base.capacities()
            },
            ..base
        },
        ProtocolProfile {
            features: FeatureSet::WORKER_PIDFD,
            capacities: ProfileCapacities {
                max_control_frame_bytes: 2 * 1024 * 1024,
                ..*base.capacities()
            },
            ..base
        },
    ];
    let identity = crate::RuntimeIdentity::from_profiles(crate::Architecture::Aarch64, &profiles);
    assert_eq!(identity.build_protocol_version, profiles[0].version());
    assert_eq!(identity.compiled_max_control_frame_bytes, 2 * 1024 * 1024);
    assert_eq!(
        identity.compiled_features,
        FeatureSet::WORKER_PIDFD.union(FeatureSet::EXECUTION_TIMEOUT)
    );
    assert_eq!(
        crate::RuntimeIdentity::decode(&identity.encode()),
        Ok(identity)
    );
}

#[test]
fn manifest_growth_updates_identity_and_negotiates_each_request_independently() {
    use crate::{Architecture, BrokerId, HelloResponse, RuntimeIdentity};
    static PROFILES: [ProtocolProfile; 2] = [
        protocol_profiles()[0],
        ProtocolProfile {
            version: ProtocolVersion { major: 1, minor: 1 },
            ..protocol_profiles()[0]
        },
    ];
    let offer = offer_from_profiles(&PROFILES, ControlLimits::CURRENT);
    let identity = RuntimeIdentity::from_profiles(Architecture::X86_64, &PROFILES);
    assert_eq!(
        offer.protocols,
        ProtocolRange {
            major: 1,
            min_minor: 0,
            max_minor: 1
        }
    );
    assert_eq!(
        identity.build_protocol_version,
        ProtocolVersion { major: 1, minor: 1 }
    );
    for max_minor in [0, 1] {
        let request = HelloRequest::from_offer(
            HelloOffer {
                protocols: ProtocolRange {
                    max_minor,
                    ..offer.protocols
                },
                ..offer
            },
            FeatureSet::EMPTY,
        )
        .unwrap();
        let (response, selected) =
            HelloResponse::accept_with_profiles(request, offer, BrokerId::new([1; 16]), &PROFILES)
                .unwrap();
        assert_eq!(
            response.selected().version,
            ProtocolVersion {
                major: 1,
                minor: max_minor
            }
        );
        assert_eq!(
            request
                .negotiate_protocol_with_profiles(ProtocolVersion::BOOTSTRAP, response, &PROFILES)
                .unwrap(),
            selected
        );
        let mut bytes = [0; crate::HELLO_PAYLOAD_BUFFER_BYTES];
        let length = response.encode_payload(&mut bytes).unwrap();
        let decoded = HelloResponse::decode_payload(&bytes[..length]).unwrap();
        assert_eq!(
            request
                .negotiate_protocol_with_profiles(ProtocolVersion::BOOTSTRAP, decoded, &PROFILES)
                .unwrap(),
            selected
        );
        if max_minor == 1 {
            let encoded = std::str::from_utf8(&bytes[..length]).unwrap();
            let downgraded = encoded.replace("\"minor\":1", "\"minor\":0");
            assert_ne!(encoded, downgraded);
            let response = HelloResponse::decode_payload(downgraded.as_bytes()).unwrap();
            assert_eq!(
                request.negotiate_protocol_with_profiles(
                    ProtocolVersion::BOOTSTRAP,
                    response,
                    &PROFILES,
                ),
                Err(CompatibilityError::InvalidSelection),
            );
        }
        assert_eq!(identity.build_protocol_version.minor, 1);
    }
}

#[test]
fn selected_minor_survives_every_established_frame_encoder() {
    for minor in [0, 1] {
        check_established_frame_versions(minor);
    }
}

fn check_established_frame_versions(minor: u16) {
    use crate::*;
    static FIXTURES: [ProtocolProfile; 2] = [
        protocol_profiles()[0],
        ProtocolProfile {
            version: ProtocolVersion { major: 1, minor: 1 },
            ..protocol_profiles()[0]
        },
    ];
    let offer = offer_from_profiles(&FIXTURES, ControlLimits::CURRENT);
    let request = HelloRequest::from_offer(
        HelloOffer {
            protocols: ProtocolRange {
                max_minor: minor,
                ..offer.protocols
            },
            ..offer
        },
        FeatureSet::EMPTY,
    )
    .unwrap();
    let selected = negotiate_with(request, offer, &FIXTURES).unwrap();
    let version = selected.profile().version();
    assert_eq!(version.minor, minor);
    let hello = ControlFrameHeader::new_with_version(
        ProtocolVersion::BOOTSTRAP,
        MessageKind::HelloRequest,
        1,
        0,
        0,
    )
    .unwrap();
    let response = ControlFrameHeader::new_with_version(
        ProtocolVersion::BOOTSTRAP,
        MessageKind::HelloResponse,
        1,
        0,
        0,
    )
    .unwrap();
    assert_eq!(hello.protocol_minor(), 0);
    let mut host = ControlConnectionState::host()
        .next_outbound(hello)
        .unwrap()
        .next_inbound(response)
        .unwrap();
    let mut guest = ControlConnectionState::guest()
        .next_inbound(hello)
        .unwrap()
        .next_outbound(response)
        .unwrap();
    host.complete_protocol(selected).unwrap();
    guest.complete_protocol(selected).unwrap();
    let frame = PrepareRequestFrame::new(
        2,
        BrokerId::new([1; 16]),
        PrepareRequest::new(
            "root",
            ExecutionKind::Piped,
            GuestProcess::LoginShell,
            ProcessEnvironment::default(),
            None,
        )
        .unwrap(),
    );
    let preparation = PreparationId::new([2; 16]);
    let token = PreparationControl::new(frame.broker_id(), preparation);
    let mut bytes = [0; 4096];
    let length = selected.encode_prepare_frame(&frame, &mut bytes).unwrap();
    let prepare = ControlFrame::decode(&bytes[..length]).unwrap();
    let decoded = selected
        .decode_prepare_frame(
            prepare.header(),
            prepare.payload(),
            0,
            frame.broker_id(),
            &mut [0; MAX_PREPARE_UNESCAPE_BYTES],
        )
        .unwrap();
    assert_eq!(decoded.request().user(), "root");
    drop(decoded);
    let mut headers = std::vec![(Direction::HostToGuest, prepare.header())];
    let ready = frame
        .ready_frame(PrepareReady::new(preparation, frame.broker_id(), 45000).unwrap())
        .unwrap();
    let length = selected.encode_ready(ready, &mut bytes).unwrap();
    let received = ControlFrame::decode(&bytes[..length]).unwrap();
    selected
        .decode_ready(&frame, received.header(), received.payload(), 4)
        .unwrap();
    headers.push((Direction::GuestToHost, received.header()));
    let start = StartFrame::new(
        3,
        StartRequest::new(&frame, preparation, None, None).unwrap(),
    );
    headers.push((
        Direction::HostToGuest,
        selected.encode_start_frame(&start, &mut bytes).unwrap().0,
    ));
    let accepted = StartAcceptedFrame::new(3, token);
    headers.push((
        Direction::GuestToHost,
        selected
            .encode_start_accepted(&accepted, &mut bytes)
            .unwrap()
            .0,
    ));
    for (index, kind) in [
        PreparationControlKind::KeepPrepared,
        PreparationControlKind::Recheck,
        PreparationControlKind::Cancel,
        PreparationControlKind::Close,
    ]
    .into_iter()
    .enumerate()
    {
        let control = PreparationControlFrame::new(4 + index as u64, kind, token);
        headers.push((
            Direction::HostToGuest,
            selected
                .encode_control_request(&control, &mut bytes)
                .unwrap()
                .0,
        ));
        let status = if matches!(
            kind,
            PreparationControlKind::Cancel | PreparationControlKind::Close
        ) {
            ControlReplyStatus::CleanupRequested
        } else {
            ControlReplyStatus::Prepared {
                remaining_lease_ms: 45000,
            }
        };
        let reply = ControlReplyFrame::new(control, status).unwrap();
        headers.push((
            Direction::GuestToHost,
            selected.encode_control_reply(&reply, &mut bytes).unwrap().0,
        ));
    }
    let error = ErrorFrame::new(
        8,
        frame.broker_id(),
        Some(preparation),
        GuestOperation::Start,
        GuestErrorCode::InvalidState,
        None,
    )
    .unwrap();
    headers.push((
        Direction::GuestToHost,
        selected.encode_error(&error, &mut bytes).unwrap().0,
    ));
    let exit = ExitFrame::new(
        &frame,
        preparation,
        ExitReport::new(
            ExecutionStatus::Started,
            Some(ExitStatus::Exited(0)),
            CleanupStatus::Complete,
            ExitReason::WorkerExited,
        )
        .unwrap(),
    )
    .unwrap();
    headers.push((
        Direction::GuestToHost,
        selected.encode_exit(&exit, &mut bytes).unwrap().0,
    ));
    for (direction, header) in headers {
        let decoded = ControlFrameHeader::decode(&header.encode()).unwrap();
        assert_eq!(decoded.protocol_version(), version, "{:?}", header.kind());
        match direction {
            Direction::HostToGuest => {
                host = host.next_outbound(decoded).unwrap();
                guest = guest.next_inbound(decoded).unwrap();
            }
            Direction::GuestToHost => {
                guest = guest.next_outbound(decoded).unwrap();
                host = host.next_inbound(decoded).unwrap();
            }
        }
    }
    let wrong = ControlFrameHeader::new_with_version(
        ProtocolVersion {
            major: version.major,
            minor: 1 - minor,
        },
        MessageKind::Close,
        9,
        0,
        0,
    )
    .unwrap();
    assert!(matches!(
        host.next_outbound(wrong),
        Err(ControlSessionError::ProtocolVersion(_))
    ));
    assert!(matches!(
        guest.next_inbound(wrong),
        Err(ControlSessionError::ProtocolVersion(_))
    ));
}

#[test]
fn manifest_drives_the_serialized_offers_and_selected_profile() {
    static FIXTURES: [ProtocolProfile; 2] = [
        ProtocolProfile {
            version: ProtocolVersion { major: 7, minor: 5 },
            capacities: V1_0_CAPACITIES,
            messages: &[],
            features: FeatureSet::WORKER_PIDFD.union(FeatureSet::EXECUTION_TIMEOUT),
            required_features: FeatureSet::WORKER_PIDFD,
            identity_schema: IdentitySchemaId::IdentityV1,
            required_behaviors: &[],
        },
        ProtocolProfile {
            version: ProtocolVersion { major: 7, minor: 4 },
            capacities: V1_0_CAPACITIES,
            messages: &[],
            features: FeatureSet::WORKER_PIDFD,
            required_features: FeatureSet::WORKER_PIDFD,
            identity_schema: IdentitySchemaId::IdentityV1,
            required_behaviors: &[],
        },
    ];
    let offer = offer_from_profiles(&FIXTURES, ControlLimits::new(8192, 3).unwrap());
    assert_eq!(
        offer.protocols,
        ProtocolRange {
            major: 7,
            min_minor: 4,
            max_minor: 5
        }
    );
    assert_eq!(offer.supported_features, FIXTURES[0].features());
    assert_eq!(offer.limits, ControlLimits::new(8192, 3).unwrap());
    let request = HelloRequest::from_offer(offer, FeatureSet::EXECUTION_TIMEOUT).unwrap();
    let mut bytes = [0; crate::HELLO_PAYLOAD_BUFFER_BYTES];
    let length = request.encode_payload(&mut bytes).unwrap();
    let received = HelloRequest::decode_payload(&bytes[..length]).unwrap();
    assert_eq!(received.offer(), offer);
    let selected = negotiate_with(received, offer, &FIXTURES).unwrap();
    assert_eq!(selected.profile().version(), FIXTURES[0].version());
    assert_eq!(selected.features(), offer.supported_features);
    assert_eq!(selected.limits().get(), offer.limits);
    let old_peer = HelloOffer {
        protocols: ProtocolRange {
            max_minor: 4,
            ..offer.protocols
        },
        supported_features: FeatureSet::WORKER_PIDFD,
        ..offer
    };
    let request = HelloRequest::from_offer(offer, FeatureSet::EMPTY).unwrap();
    let selected = negotiate_with(request, old_peer, &FIXTURES).unwrap();
    assert_eq!(selected.profile().version(), FIXTURES[1].version());
    assert_eq!(selected.features(), FeatureSet::WORKER_PIDFD);
}

#[test]
fn selection_requires_the_highest_common_minor_to_be_registered() {
    static FIXTURES: [ProtocolProfile; 2] = [
        ProtocolProfile {
            version: ProtocolVersion { major: 1, minor: 0 },
            capacities: V1_0_CAPACITIES,
            messages: &[],
            features: FeatureSet::WORKER_PIDFD,
            required_features: FeatureSet::WORKER_PIDFD,
            identity_schema: IdentitySchemaId::IdentityV1,
            required_behaviors: &[],
        },
        ProtocolProfile {
            version: ProtocolVersion { major: 1, minor: 2 },
            capacities: V1_0_CAPACITIES,
            messages: &[],
            features: FeatureSet::WORKER_PIDFD,
            required_features: FeatureSet::WORKER_PIDFD,
            identity_schema: IdentitySchemaId::IdentityV1,
            required_behaviors: &[],
        },
    ];
    let host = HelloRequest::from_offer(
        HelloOffer {
            protocols: ProtocolRange {
                major: 1,
                min_minor: 0,
                max_minor: 3,
            },
            supported_features: FeatureSet::WORKER_PIDFD,
            limits: ControlLimits::CURRENT,
        },
        FeatureSet::EMPTY,
    )
    .unwrap();
    let guest = HelloOffer {
        protocols: ProtocolRange {
            major: 1,
            min_minor: 0,
            max_minor: 2,
        },
        ..host.offer()
    };
    assert_eq!(
        negotiate_with(host, guest, &FIXTURES)
            .unwrap()
            .profile()
            .version()
            .minor,
        2
    );
    let guest = HelloOffer {
        protocols: ProtocolRange {
            max_minor: 1,
            ..guest.protocols
        },
        ..guest
    };
    assert_eq!(
        negotiate_with(host, guest, &FIXTURES),
        Err(CompatibilityError::InvalidOffer)
    );
    let guest = HelloOffer {
        protocols: host.offer().protocols,
        ..guest
    };
    assert_eq!(
        negotiate_with(host, guest, &FIXTURES),
        Err(CompatibilityError::InvalidOffer)
    );
}

#[test]
#[should_panic(expected = "profile minors must be unique and continuous")]
fn local_offer_rejects_a_gap_in_registered_minors() {
    let baseline = protocol_profiles()[0];
    let profiles = [
        baseline,
        ProtocolProfile {
            version: ProtocolVersion {
                minor: baseline.version().minor + 2,
                ..baseline.version()
            },
            ..baseline
        },
    ];
    offer_from_profiles(&profiles, ControlLimits::CURRENT);
}

#[test]
#[should_panic(expected = "profile minors must be unique and continuous")]
fn local_offer_rejects_duplicate_minors() {
    let baseline = protocol_profiles()[0];
    offer_from_profiles(&[baseline, baseline], ControlLimits::CURRENT);
}
