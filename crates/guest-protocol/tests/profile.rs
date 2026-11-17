use lasper_guest_protocol::*;
fn offer() -> HelloOffer {
    HelloOffer::local(ControlLimits::CURRENT)
}

#[test]
fn larger_peer_capacity_is_intersected_before_enforcing_local_storage() {
    let local = offer();
    let larger_limits = ControlLimits::new(u32::MAX, u16::MAX).unwrap();
    let larger = HelloOffer {
        limits: larger_limits,
        protocols: ProtocolRange {
            max_minor: 1,
            ..local.protocols
        },
        ..local
    };
    for (host, guest) in [(local, larger), (larger, local)] {
        let request = HelloRequest::from_offer(host, FeatureSet::EMPTY).unwrap();
        let mut bytes = [0; HELLO_PAYLOAD_BUFFER_BYTES];
        let length = request.encode_payload(&mut bytes).unwrap();
        let request = HelloRequest::decode_payload(&bytes[..length]).unwrap();
        let (response, selected) =
            HelloResponse::accept(request, guest, BrokerId::new([1; 16])).unwrap();
        assert_eq!(
            selected.profile().version(),
            ProtocolVersion { major: 1, minor: 0 }
        );
        assert_eq!(selected.limits().get(), local.limits);
        let length = response.encode_payload(&mut bytes).unwrap();
        let response = HelloResponse::decode_payload(&bytes[..length]).unwrap();
        assert_eq!(
            request
                .negotiate_protocol(ProtocolVersion::BOOTSTRAP, response)
                .unwrap(),
            selected
        );
    }
    assert_eq!(HelloOffer::local(larger_limits).limits, local.limits);
    let unconstrained = HelloOffer {
        protocols: local.protocols,
        ..larger
    };
    let request = HelloRequest::from_offer(unconstrained, FeatureSet::EMPTY).unwrap();
    assert_eq!(
        negotiate_hello(request, unconstrained)
            .unwrap()
            .limits()
            .get(),
        local.limits
    );
}

#[test]
fn hello_and_artifact_identity_advertise_the_manifest_features() {
    let compiled = protocol_profiles()
        .iter()
        .fold(FeatureSet::EMPTY, |features, profile| {
            features.union(profile.features())
        });
    let request = HelloRequest::new(compiled, ControlLimits::CURRENT).unwrap();
    assert_eq!(request.offer().supported_features, compiled);
    assert_eq!(
        HelloOffer::local(ControlLimits::CURRENT).supported_features,
        compiled
    );
    for architecture in [Architecture::X86_64, Architecture::Aarch64] {
        assert_eq!(
            RuntimeIdentity::current(architecture).compiled_features,
            compiled
        );
    }
    for bit in 0..u64::BITS {
        let feature = FeatureSet::from_bits(1 << bit);
        assert_eq!(
            HelloRequest::new(feature, ControlLimits::CURRENT).is_ok(),
            compiled.contains(feature)
        );
    }
}
#[test]
fn selected_profile_owns_capacities_and_feature_requirements() {
    let request = HelloRequest::new(
        FeatureSet::EXECUTION_TIMEOUT,
        ControlLimits::new(8192, 3).unwrap(),
    )
    .unwrap();
    let selected = negotiate_hello(request, offer()).unwrap();
    assert_eq!(
        selected.profile().version(),
        ProtocolVersion { major: 1, minor: 0 }
    );
    assert_eq!(selected.profile().capacities(), &V1_0_CAPACITIES);
    assert_eq!(selected.limits().get(), request.limits());
    assert!(selected.features().contains(FeatureSet::EXECUTION_TIMEOUT));
    let missing = HelloOffer {
        supported_features: FeatureSet::WORKER_PIDFD,
        ..offer()
    };
    assert!(matches!(
        negotiate_hello(request, missing),
        Err(CompatibilityError::MissingFeatures { .. })
    ));
    let small = HelloOffer {
        limits: ControlLimits::new(4095, 4).unwrap(),
        ..offer()
    };
    assert!(matches!(
        negotiate_hello(request, small),
        Err(CompatibilityError::InsufficientFrameCapacity { required: 4096, .. })
    ));
}
#[test]
fn incompatibility_is_decided_by_ranges_not_artifact_identity() {
    let request = HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap();
    let wrong = HelloOffer {
        protocols: ProtocolRange {
            major: 2,
            min_minor: 0,
            max_minor: 0,
        },
        ..offer()
    };
    assert!(matches!(
        negotiate_hello(request, wrong),
        Err(CompatibilityError::MajorMismatch { .. })
    ));
    let wrong = HelloOffer {
        protocols: ProtocolRange {
            major: 1,
            min_minor: 1,
            max_minor: 1,
        },
        ..offer()
    };
    assert!(matches!(
        negotiate_hello(request, wrong),
        Err(CompatibilityError::NoCommonMinor { .. })
    ));
}

#[test]
fn an_unregistered_highest_common_minor_is_an_invalid_offer() {
    let unsupported = HelloOffer {
        protocols: ProtocolRange {
            max_minor: 1,
            ..offer().protocols
        },
        ..offer()
    };
    let request = HelloRequest::from_offer(unsupported, FeatureSet::EMPTY).unwrap();
    assert_eq!(
        negotiate_hello(request, unsupported),
        Err(CompatibilityError::InvalidOffer)
    );
    // A newer peer can advertise more codecs than this build. Only the
    // highest common minor must resolve to a local codec.
    let selected = negotiate_hello(request, offer()).unwrap();
    assert_eq!(
        selected.profile().version(),
        ProtocolVersion { major: 1, minor: 0 }
    );
    let request = HelloRequest::from_offer(offer(), FeatureSet::EMPTY).unwrap();
    assert_eq!(negotiate_hello(request, unsupported).unwrap(), selected);
}
#[test]
fn manifest_registers_all_established_directions_and_capacities() {
    let profile = protocol_profile(ProtocolVersion { major: 1, minor: 0 }).unwrap();
    for kind in [
        MessageKind::PrepareRequest,
        MessageKind::KeepPrepared,
        MessageKind::Recheck,
        MessageKind::Start,
        MessageKind::Cancel,
        MessageKind::Close,
    ] {
        assert!(profile.schema(kind, Direction::HostToGuest).is_some());
    }
    for kind in [
        MessageKind::PrepareReady,
        MessageKind::KeepPrepared,
        MessageKind::Recheck,
        MessageKind::Start,
        MessageKind::Cancel,
        MessageKind::Close,
        MessageKind::Exit,
        MessageKind::Error,
    ] {
        assert!(profile.schema(kind, Direction::GuestToHost).is_some());
    }
    assert_eq!(
        V1_0_CAPACITIES,
        ProfileCapacities {
            user_name_bytes: 32,
            command_arguments: 256,
            command_argument_bytes: 4096,
            command_total_bytes: 65536,
            environment_entries: 32,
            environment_bytes: 16384,
            default_environment_bytes: 16384,
            min_control_frame_bytes: 4096,
            max_control_frame_bytes: 1048576,
            max_control_descriptors: 4,
            max_execution_timeout_ms: u64::MAX / 2,
            max_prepared_lease_ms: 300000,
        }
    );
    assert_eq!(V1_0_CAPACITIES.prepare_unescape_bytes(), 16384);
    assert_eq!(V1_0_CAPACITIES.merged_environment_bytes(), 32768);
    const {
        assert!(BUILD_CAPACITIES.environment_bytes >= V1_0_CAPACITIES.environment_bytes);
        assert!(BUILD_CAPACITIES.command_argument_bytes >= V1_0_CAPACITIES.command_argument_bytes);
    }
}

#[test]
fn negotiated_profile_dispatches_prepare_start_and_rejects_wrong_direction() {
    let request = HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap();
    let selected = negotiate_hello(
        request,
        HelloOffer {
            supported_features: FeatureSet::WORKER_PIDFD,
            ..offer()
        },
    )
    .unwrap();
    let prepare = PrepareRequest::new(
        "root",
        ExecutionKind::Piped,
        GuestProcess::LoginShell,
        ProcessEnvironment::default(),
        None,
    )
    .unwrap();
    let mut bytes = [0; 4096];
    let length = selected.encode_prepare(&prepare, &mut bytes).unwrap();
    let decoded = selected
        .decode_prepare(&bytes[..length], &mut [0; MAX_PREPARE_UNESCAPE_BYTES])
        .unwrap();
    let frame = PrepareRequestFrame::new(2, BrokerId::new([1; 16]), decoded);
    let preparation = PreparationId::new([2; 16]);
    let start = StartRequest::new(&frame, preparation, None, Some(10)).unwrap();
    let mut bytes = [0; 4096];
    let timeout = negotiate_hello(request, offer()).unwrap();
    let length = timeout.encode_start_payload(&start, &mut bytes).unwrap();
    assert_eq!(
        selected.decode_start(&frame, preparation, &bytes[..length]),
        Err(SessionPayloadError::FeatureNotNegotiated)
    );
    assert_eq!(
        timeout
            .decode_start(&frame, preparation, &bytes[..length])
            .unwrap(),
        start
    );

    let hello = ControlFrameHeader::new_with_version(
        lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloRequest,
        1,
        0,
        0,
    )
    .unwrap();
    let response = ControlFrameHeader::new_with_version(
        lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloResponse,
        1,
        0,
        0,
    )
    .unwrap();
    let mut host = ControlConnectionState::host()
        .next_outbound(hello)
        .unwrap()
        .next_inbound(response)
        .unwrap();
    host.complete_protocol(selected).unwrap();
    let exit = ControlFrameHeader::new_with_version(
        lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::Exit,
        0,
        0,
        0,
    )
    .unwrap();
    assert!(matches!(
        host.next_outbound(exit),
        Err(ControlSessionError::UnsupportedMessage {
            kind: MessageKind::Exit,
            direction: Direction::HostToGuest
        })
    ));
    assert!(host.next_inbound(exit).is_ok());
}
