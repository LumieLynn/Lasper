extern crate std;

use super::*;
use crate::*;

fn request() -> PrepareRequestFrame<'static> {
    PrepareRequestFrame::new(
        2,
        BrokerId::new([1; 16]),
        PrepareRequest::new(
            "root",
            ExecutionKind::Terminal,
            GuestProcess::LoginShell,
            ProcessEnvironment::default(),
            Some(TerminalSize::new(80, 24).unwrap()),
        )
        .unwrap(),
    )
}

#[test]
fn identity_codecs_enforce_the_selected_user_capacity() {
    let request = request();
    let ready = request
        .ready_frame(
            PrepareReady::new(PreparationId::new([2; 16]), request.broker_id(), 45000).unwrap(),
        )
        .unwrap();
    let challenge = ready.identity_challenge([3; 32]);
    let reply = GuestIdentityReply::new(challenge, request.request(), "root", 0, 0).unwrap();
    let profile = &protocol_profiles()[0];
    let mut packet = [0; 1024];
    let length = profile.encode_identity_reply(&reply, &mut packet).unwrap();
    assert_eq!(
        profile.decode_identity_reply(&packet[..length], challenge, request.request()),
        Ok(reply.clone())
    );

    let limited = ProtocolProfile {
        capacities: ProfileCapacities {
            user_name_bytes: 3,
            ..*profile.capacities()
        },
        ..*profile
    };
    let error = IdentityChannelError::Payload(PayloadCodecError::InvalidRequest);
    assert_eq!(
        limited.decode_identity_reply(&packet[..length], challenge, request.request()),
        Err(error)
    );
    assert_eq!(
        limited.encode_identity_reply(&reply, &mut packet),
        Err(error)
    );
}

fn check_session_envelope(
    encode: impl Fn(
        &ProtocolProfile,
        &mut [u8],
    ) -> Result<(ControlFrameHeader, usize), SessionFrameError>,
    decode: impl Fn(&ProtocolProfile, ControlFrameHeader, &[u8]) -> Result<(), SessionFrameError>,
) {
    let profile = ProtocolProfile {
        version: ProtocolVersion { major: 1, minor: 1 },
        ..protocol_profiles()[0]
    };
    let mut bytes = [0; 4096];
    let (header, length) = encode(&profile, &mut bytes).unwrap();
    assert_eq!(header.protocol_version(), profile.version());
    let payload = &bytes[..length];
    decode(&profile, header, payload).unwrap();
    let wrong_version = ControlFrameHeader::new_with_version(
        ProtocolVersion { major: 1, minor: 0 },
        header.kind(),
        header.request_id(),
        header.payload_bytes(),
        0,
    )
    .unwrap();
    assert_eq!(
        decode(&profile, wrong_version, payload),
        Err(SessionFrameError::ProtocolVersion(
            ProtocolVersionMismatch {
                expected: profile.version(),
                actual: wrong_version.protocol_version()
            }
        ))
    );

    let maximum = CONTROL_FRAME_HEADER_BYTES as u32 + length as u32 - 1;
    let limited = ProtocolProfile {
        capacities: ProfileCapacities {
            min_control_frame_bytes: CONTROL_FRAME_HEADER_BYTES as u32,
            max_control_frame_bytes: maximum,
            ..*profile.capacities()
        },
        ..profile
    };
    let error = SessionFrameError::Limit(ControlLimitError::FrameTooLarge {
        declared: maximum + 1,
        maximum,
    });
    assert_eq!(decode(&limited, header, payload), Err(error));
    assert_eq!(encode(&limited, &mut bytes), Err(error));
}

#[test]
fn every_session_codec_checks_its_profile_version_and_frame_capacity() {
    let request = request();
    let preparation = PreparationId::new([2; 16]);
    let control = PreparationControl::new(request.broker_id(), preparation);
    for kind in [
        PreparationControlKind::KeepPrepared,
        PreparationControlKind::Recheck,
        PreparationControlKind::Cancel,
        PreparationControlKind::Close,
    ] {
        let command = PreparationControlFrame::new(3, kind, control);
        check_session_envelope(
            |profile, output| profile.encode_control_request(&command, output),
            |profile, header, payload| {
                profile
                    .decode_control_request(3, &request, preparation, header, payload, 0)
                    .map(|_| ())
            },
        );
        let status = match kind {
            PreparationControlKind::KeepPrepared | PreparationControlKind::Recheck => {
                ControlReplyStatus::Prepared {
                    remaining_lease_ms: 45_000,
                }
            }
            PreparationControlKind::Cancel | PreparationControlKind::Close => {
                ControlReplyStatus::CleanupRequested
            }
        };
        let reply = ControlReplyFrame::new(command, status).unwrap();
        check_session_envelope(
            |profile, output| profile.encode_control_reply(&reply, output),
            |profile, header, payload| {
                profile
                    .decode_control_reply(&command, header, payload, 0)
                    .map(|_| ())
            },
        );
    }
    let start = StartFrame::new(
        4,
        StartRequest::new(&request, preparation, None, Some(100)).unwrap(),
    );
    check_session_envelope(
        |profile, output| profile.encode_start(&start, output),
        |profile, header, payload| {
            profile
                .decode_start_frame(4, &request, preparation, header, payload, 0)
                .map(|_| ())
        },
    );
    let accepted = StartAcceptedFrame::new(4, control);
    check_session_envelope(
        |profile, output| profile.encode_start_accepted(&accepted, output),
        |profile, header, payload| {
            profile
                .decode_start_accepted(4, &request, preparation, header, payload, 0)
                .map(|_| ())
        },
    );
    let exit = ExitFrame::new(
        &request,
        preparation,
        ExitReport::new(
            ExecutionStatus::Started,
            Some(ExitStatus::Exited(7)),
            CleanupStatus::Complete,
            ExitReason::WorkerExited,
        )
        .unwrap(),
    )
    .unwrap();
    check_session_envelope(
        |profile, output| profile.encode_exit(&exit, output),
        |profile, header, payload| {
            profile
                .decode_exit(&request, preparation, header, payload, 0)
                .map(|_| ())
        },
    );
    let error = ErrorFrame::new(
        4,
        request.broker_id(),
        Some(preparation),
        GuestOperation::Start,
        GuestErrorCode::InvalidState,
        None,
    )
    .unwrap();
    let start_header = ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::Start,
        4,
        0,
        0,
    )
    .unwrap();
    check_session_envelope(
        |profile, output| profile.encode_error(&error, output),
        |profile, header, payload| {
            profile
                .decode_error(
                    start_header,
                    &request,
                    Some(preparation),
                    header,
                    payload,
                    0,
                )
                .map(|_| ())
        },
    );
}

#[test]
fn negotiated_session_decoders_enforce_the_connection_limit_for_every_message() {
    let limits = ControlLimits::new(4096, 4).unwrap();
    let protocol = negotiate_hello(
        HelloRequest::new(FeatureSet::EMPTY, limits).unwrap(),
        HelloOffer::local(ControlLimits::CURRENT),
    )
    .unwrap();
    let request = request();
    let preparation = PreparationId::new([2; 16]);
    let control = PreparationControl::new(request.broker_id(), preparation);
    let mut bytes = [b' '; 4096];
    macro_rules! check {
        ($encoded:expr, $decode:expr) => {{
            let (header, length) = $encoded.unwrap();
            bytes[length..].fill(b' ');
            let at_limit = ControlFrameHeader::new_with_version(
                protocol.profile().version(),
                header.kind(),
                header.request_id(),
                4064,
                0,
            )
            .unwrap();
            ($decode)(at_limit, &bytes[..4064]).unwrap();
            let oversized = ControlFrameHeader::new_with_version(
                protocol.profile().version(),
                header.kind(),
                header.request_id(),
                4065,
                0,
            )
            .unwrap();
            assert_eq!(
                ($decode)(oversized, &bytes[..4065]).unwrap_err(),
                SessionFrameError::Limit(ControlLimitError::FrameTooLarge {
                    declared: 4097,
                    maximum: 4096
                })
            );
        }};
    }
    for kind in [
        PreparationControlKind::KeepPrepared,
        PreparationControlKind::Recheck,
        PreparationControlKind::Cancel,
        PreparationControlKind::Close,
    ] {
        let command = PreparationControlFrame::new(3, kind, control);
        check!(
            protocol.encode_control_request(&command, &mut bytes),
            |header, payload| protocol.decode_control_request(
                3,
                &request,
                preparation,
                header,
                payload,
                0
            )
        );
        let status = match kind {
            PreparationControlKind::KeepPrepared | PreparationControlKind::Recheck => {
                ControlReplyStatus::Prepared {
                    remaining_lease_ms: 45_000,
                }
            }
            _ => ControlReplyStatus::CleanupRequested,
        };
        let reply = ControlReplyFrame::new(command, status).unwrap();
        check!(
            protocol.encode_control_reply(&reply, &mut bytes),
            |header, payload| protocol.decode_control_reply(&command, header, payload, 0)
        );
    }
    let start = StartFrame::new(
        4,
        StartRequest::new(&request, preparation, None, Some(100)).unwrap(),
    );
    check!(
        protocol.encode_start_frame(&start, &mut bytes),
        |header, payload| protocol.decode_start_frame(4, &request, preparation, header, payload, 0)
    );
    let accepted = StartAcceptedFrame::new(4, control);
    check!(
        protocol.encode_start_accepted(&accepted, &mut bytes),
        |header, payload| protocol.decode_start_accepted(
            4,
            &request,
            preparation,
            header,
            payload,
            0
        )
    );
    let exit = ExitFrame::new(
        &request,
        preparation,
        ExitReport::new(
            ExecutionStatus::Started,
            Some(ExitStatus::Exited(7)),
            CleanupStatus::Complete,
            ExitReason::WorkerExited,
        )
        .unwrap(),
    )
    .unwrap();
    check!(
        protocol.encode_exit(&exit, &mut bytes),
        |header, payload| protocol.decode_exit(&request, preparation, header, payload, 0)
    );
    let error = ErrorFrame::new(
        4,
        request.broker_id(),
        Some(preparation),
        GuestOperation::Start,
        GuestErrorCode::InvalidState,
        None,
    )
    .unwrap();
    let start_header = ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::Start,
        4,
        0,
        0,
    )
    .unwrap();
    check!(
        protocol.encode_error(&error, &mut bytes),
        |header, payload| protocol.decode_error(
            start_header,
            &request,
            Some(preparation),
            header,
            payload,
            0
        )
    );
}

#[test]
fn prepare_and_ready_check_the_profile_envelope_before_decoding() {
    let profile = protocol_profiles()[0];
    let request = request();
    let mut bytes = [0; 4096];
    let length = profile
        .encode_prepare(request.request(), &mut bytes)
        .unwrap();
    let wrong = ProtocolVersion { major: 1, minor: 1 };
    let header = ControlFrameHeader::new_with_version(
        wrong,
        MessageKind::PrepareRequest,
        request.request_id(),
        length as u32,
        0,
    )
    .unwrap();
    let mut storage = PrepareDecodeStorage::new();
    let mut scratch = [0; MAX_PREPARE_UNESCAPE_BYTES];
    assert_eq!(
        profile
            .decode_prepare_into(
                &mut storage,
                header,
                &bytes[..length],
                0,
                request.broker_id(),
                &mut scratch
            )
            .unwrap_err(),
        PrepareFrameError::ProtocolVersion(ProtocolVersionMismatch {
            expected: profile.version(),
            actual: wrong
        })
    );
    assert!(storage.request().is_none());

    let preparation = PreparationId::new([2; 16]);
    let ready = request
        .ready_frame(PrepareReady::new(preparation, request.broker_id(), 45_000).unwrap())
        .unwrap();
    let mut bytes = [0; 4096];
    let length = profile.encode_ready(ready, &mut bytes).unwrap();
    let frame = ControlFrame::decode(&bytes[..length]).unwrap();
    let wrong_header = ControlFrameHeader::new_with_version(
        wrong,
        MessageKind::PrepareReady,
        frame.header().request_id(),
        frame.header().payload_bytes(),
        3,
    )
    .unwrap();
    assert!(matches!(
        profile.decode_ready(&request, wrong_header, frame.payload(), 3),
        Err(PrepareFrameError::ProtocolVersion(_))
    ));
    let limited = ProtocolProfile {
        capacities: ProfileCapacities {
            max_control_descriptors: 2,
            ..*profile.capacities()
        },
        ..profile
    };
    let error = PrepareFrameError::Limit(ControlLimitError::TooManyDescriptors {
        declared: 3,
        maximum: 2,
    });
    assert_eq!(
        limited.decode_ready(&request, frame.header(), frame.payload(), 3),
        Err(error)
    );
    assert_eq!(limited.encode_ready(ready, &mut [0; 4096]), Err(error));
}

#[test]
fn profile_lease_and_timeout_limits_apply_to_both_codec_directions() {
    let high = protocol_profiles()[0];
    let low = ProtocolProfile {
        capacities: ProfileCapacities {
            max_prepared_lease_ms: 100,
            max_execution_timeout_ms: 100,
            ..*high.capacities()
        },
        ..high
    };
    let request = request();
    let preparation = PreparationId::new([2; 16]);
    let token = PreparationControl::new(request.broker_id(), preparation);
    let command = PreparationControlFrame::new(4, PreparationControlKind::Recheck, token);
    for value in [1, 100, 101] {
        let accepted = value <= 100;
        let ready = request
            .ready_frame(PrepareReady::new(preparation, request.broker_id(), value).unwrap())
            .unwrap();
        let mut bytes = [0; 4096];
        let length = high.encode_ready(ready, &mut bytes).unwrap();
        let frame = ControlFrame::decode(&bytes[..length]).unwrap();
        assert!(high
            .decode_ready(&request, frame.header(), frame.payload(), 3)
            .is_ok());
        assert_eq!(
            low.decode_ready(&request, frame.header(), frame.payload(), 3)
                .is_ok(),
            accepted
        );
        assert_eq!(low.encode_ready(ready, &mut [0; 4096]).is_ok(), accepted);

        let reply = ControlReplyFrame::new(
            command,
            ControlReplyStatus::Prepared {
                remaining_lease_ms: value,
            },
        )
        .unwrap();
        let (header, length) = high.encode_control_reply(&reply, &mut bytes).unwrap();
        assert!(high
            .decode_control_reply(&command, header, &bytes[..length], 0)
            .is_ok());
        assert_eq!(
            low.decode_control_reply(&command, header, &bytes[..length], 0)
                .is_ok(),
            accepted
        );
        assert_eq!(
            low.encode_control_reply(&reply, &mut bytes).is_ok(),
            accepted
        );

        let start = StartFrame::new(
            5,
            StartRequest::new(&request, preparation, None, Some(u64::from(value))).unwrap(),
        );
        let (header, length) = high.encode_start(&start, &mut bytes).unwrap();
        assert!(high
            .decode_start_frame(5, &request, preparation, header, &bytes[..length], 0)
            .is_ok());
        assert_eq!(
            low.decode_start_frame(5, &request, preparation, header, &bytes[..length], 0)
                .is_ok(),
            accepted
        );
        assert_eq!(low.encode_start(&start, &mut bytes).is_ok(), accepted);
        assert_eq!(
            low.encode_start_payload(start.request(), &mut bytes)
                .is_ok(),
            accepted
        );
    }
}

#[test]
fn prepare_uses_the_selected_environment_capacity_and_scratch_requirement() {
    let high = protocol_profiles()[0];
    let low = ProtocolProfile {
        capacities: ProfileCapacities {
            environment_bytes: 64,
            command_argument_bytes: 32,
            ..*high.capacities()
        },
        ..high
    };
    let value = "x".repeat(64);
    let request = PrepareRequest::new(
        "root",
        ExecutionKind::Piped,
        GuestProcess::LoginShell,
        ProcessEnvironment::new([("EXTRA", value.as_str())]).unwrap(),
        None,
    )
    .unwrap();
    let mut bytes = [0; 4096];
    let length = high.encode_prepare(&request, &mut bytes).unwrap();
    assert!(low.encode_prepare(&request, &mut [0; 4096]).is_err());
    assert!(high
        .decode_prepare(&bytes[..length], &mut [0; MAX_PREPARE_UNESCAPE_BYTES])
        .is_ok());
    assert!(low.decode_prepare(&bytes[..length], &mut [0; 64]).is_err());
    assert_eq!(low.capacities().prepare_unescape_bytes(), 64);
    assert_eq!(
        high.capacities().prepare_unescape_bytes(),
        MAX_PREPARE_UNESCAPE_BYTES
    );

    let empty = PrepareRequest::new(
        "root",
        ExecutionKind::Piped,
        GuestProcess::LoginShell,
        ProcessEnvironment::default(),
        None,
    )
    .unwrap();
    let length = high.encode_prepare(&empty, &mut bytes).unwrap();
    assert!(low.decode_prepare(&bytes[..length], &mut [0; 64]).is_ok());
    assert_eq!(
        low.decode_prepare(&bytes[..length], &mut [0; 63])
            .unwrap_err(),
        PayloadCodecError::OutputTooSmall
    );
}

#[test]
fn every_registered_codec_controls_both_encoding_and_decoding() {
    let baseline = &protocol_profiles()[0];
    let request = request();
    let preparation = PreparationId::new([2; 16]);
    let token = PreparationControl::new(request.broker_id(), preparation);
    let control = PreparationControlFrame::new(4, PreparationControlKind::Close, token);
    let reply = ControlReplyFrame::new(control, ControlReplyStatus::CleanupRequested).unwrap();
    let start = StartFrame::new(
        3,
        StartRequest::new(&request, preparation, None, None).unwrap(),
    );
    let accepted = StartAcceptedFrame::new(3, token);
    let error = ErrorFrame::new(
        4,
        request.broker_id(),
        Some(preparation),
        GuestOperation::Close,
        GuestErrorCode::InvalidState,
        None,
    )
    .unwrap();
    let exit = ExitFrame::new(
        &request,
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
    let ready = request
        .ready_frame(PrepareReady::new(preparation, request.broker_id(), 45000).unwrap())
        .unwrap();
    for schema in baseline.messages() {
        let mut wrong = *schema;
        wrong.codec = if schema.codec == SchemaId::ErrorV1 {
            SchemaId::ExitV1
        } else {
            SchemaId::ErrorV1
        };
        let profile = ProtocolProfile {
            messages: std::boxed::Box::leak(std::boxed::Box::new([wrong])),
            ..*baseline
        };
        let mut bytes = [0; 4096];
        let (header, length) = match schema.codec {
            SchemaId::TestPrepare | SchemaId::TestStart | SchemaId::TestExit => {
                unreachable!("real profiles do not register fixture codecs")
            }
            SchemaId::PrepareV1 => {
                assert!(profile
                    .encode_prepare(request.request(), &mut bytes)
                    .is_err());
                let length = baseline
                    .encode_prepare(request.request(), &mut bytes)
                    .unwrap();
                assert!(baseline
                    .decode_prepare(&bytes[..length], &mut [0; MAX_PREPARE_UNESCAPE_BYTES])
                    .is_ok());
                assert!(profile
                    .decode_prepare(&bytes[..length], &mut [0; MAX_PREPARE_UNESCAPE_BYTES])
                    .is_err());
                continue;
            }
            SchemaId::ReadyV1 => {
                assert!(profile.encode_ready(ready, &mut bytes).is_err());
                let length = baseline.encode_ready(ready, &mut bytes).unwrap();
                let frame = ControlFrame::decode(&bytes[..length]).unwrap();
                assert!(baseline
                    .decode_ready(&request, frame.header(), frame.payload(), 3)
                    .is_ok());
                assert!(profile
                    .decode_ready(&request, frame.header(), frame.payload(), 3)
                    .is_err());
                continue;
            }
            SchemaId::ControlRequestV1 | SchemaId::ControlReplyV1 => {
                // The same codec is registered separately for each operation.
                let kind = match schema.kind {
                    MessageKind::KeepPrepared => PreparationControlKind::KeepPrepared,
                    MessageKind::Recheck => PreparationControlKind::Recheck,
                    MessageKind::Cancel => PreparationControlKind::Cancel,
                    MessageKind::Close => PreparationControlKind::Close,
                    _ => unreachable!(),
                };
                let command = PreparationControlFrame::new(4, kind, token);
                if schema.codec == SchemaId::ControlRequestV1 {
                    assert!(profile
                        .encode_control_request(&command, &mut bytes)
                        .is_err());
                    let (header, length) = baseline
                        .encode_control_request(&command, &mut bytes)
                        .unwrap();
                    assert!(baseline
                        .decode_control_request(
                            4,
                            &request,
                            preparation,
                            header,
                            &bytes[..length],
                            0
                        )
                        .is_ok());
                    assert!(profile
                        .decode_control_request(
                            4,
                            &request,
                            preparation,
                            header,
                            &bytes[..length],
                            0
                        )
                        .is_err());
                } else {
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
                    let frame = ControlReplyFrame::new(command, status).unwrap();
                    assert!(profile.encode_control_reply(&frame, &mut bytes).is_err());
                    let (header, length) =
                        baseline.encode_control_reply(&frame, &mut bytes).unwrap();
                    assert!(baseline
                        .decode_control_reply(&command, header, &bytes[..length], 0)
                        .is_ok());
                    assert!(profile
                        .decode_control_reply(&command, header, &bytes[..length], 0)
                        .is_err());
                }
                continue;
            }
            SchemaId::StartV1 => {
                assert!(profile.encode_start(&start, &mut bytes).is_err());
                baseline.encode_start(&start, &mut bytes).unwrap()
            }
            SchemaId::StartAcceptedV1 => {
                assert!(profile
                    .encode_start_accepted(&accepted, &mut bytes)
                    .is_err());
                baseline
                    .encode_start_accepted(&accepted, &mut bytes)
                    .unwrap()
            }
            SchemaId::ExitV1 => {
                assert!(profile.encode_exit(&exit, &mut bytes).is_err());
                baseline.encode_exit(&exit, &mut bytes).unwrap()
            }
            SchemaId::ErrorV1 => {
                assert!(profile.encode_error(&error, &mut bytes).is_err());
                baseline.encode_error(&error, &mut bytes).unwrap()
            }
        };
        let payload = &bytes[..length];
        match schema.codec {
            SchemaId::StartV1 => {
                assert!(baseline
                    .decode_start_frame(3, &request, preparation, header, payload, 0)
                    .is_ok());
                assert!(profile
                    .decode_start_frame(3, &request, preparation, header, payload, 0)
                    .is_err());
            }
            SchemaId::StartAcceptedV1 => {
                assert!(baseline
                    .decode_start_accepted(3, &request, preparation, header, payload, 0)
                    .is_ok());
                assert!(profile
                    .decode_start_accepted(3, &request, preparation, header, payload, 0)
                    .is_err());
            }
            SchemaId::ExitV1 => {
                assert!(baseline
                    .decode_exit(&request, preparation, header, payload, 0)
                    .is_ok());
                assert!(profile
                    .decode_exit(&request, preparation, header, payload, 0)
                    .is_err());
            }
            SchemaId::ErrorV1 => {
                let (sent, _) = baseline
                    .encode_control_request(&control, &mut [0; 4096])
                    .unwrap();
                assert!(baseline
                    .decode_error(sent, &request, Some(preparation), header, payload, 0)
                    .is_ok());
                assert!(profile
                    .decode_error(sent, &request, Some(preparation), header, payload, 0)
                    .is_err());
            }
            _ => unreachable!(),
        }
    }
    // An absent registration rejects the operation as well.
    let empty = ProtocolProfile {
        messages: &[],
        ..*baseline
    };
    assert!(empty.encode_control_reply(&reply, &mut [0; 4096]).is_err());
}

#[test]
fn profile_fields_and_field_gates_are_not_global_defaults() {
    static START: [MessageSchema; 1] = [MessageSchema {
        kind: MessageKind::Start,
        direction: Direction::HostToGuest,
        codec: SchemaId::StartV1,
        required_fields: &["broker_id", "preparation_id"],
        optional_fields: &["execution_timeout_ms", "terminal_size"],
        gated_fields: &[FeatureField {
            path: "terminal_size",
            requires: FeatureSet::EXECUTION_TIMEOUT,
        }],
        required_features: FeatureSet::EMPTY,
    }];
    static PROFILE: ProtocolProfile = ProtocolProfile {
        messages: &START,
        identity_schema: IdentitySchemaId::IdentityV1,
        required_behaviors: &[RequiredBehavior::CleanupOnDisconnect],
        ..protocol_profiles()[0]
    };
    assert_eq!(
        PROFILE.required_behaviors(),
        &[RequiredBehavior::CleanupOnDisconnect]
    );
    assert_eq!(PROFILE.identity_schema(), IdentitySchemaId::IdentityV1);
    let request = request();
    let preparation = PreparationId::new([2; 16]);
    let protocol = NegotiatedProtocol {
        profile: &PROFILE,
        features: FeatureSet::WORKER_PIDFD,
        limits: EnforcedControlLimits::new(ControlLimits::CURRENT),
    };
    let timeout = StartRequest::new(&request, preparation, None, Some(10)).unwrap();
    assert!(protocol.validate_start(&timeout).is_ok());
    let resized = StartRequest::new(
        &request,
        preparation,
        Some(TerminalSize::new(100, 40).unwrap()),
        None,
    )
    .unwrap();
    assert_eq!(
        protocol.validate_start(&resized),
        Err(SessionPayloadError::FeatureNotNegotiated)
    );
    let frame = StartFrame::new(3, resized);
    assert!(protocol.encode_start_frame(&frame, &mut [0; 4096]).is_err());
    let (header, length) = PROFILE.encode_start(&frame, &mut [0; 4096]).unwrap();
    let mut bytes = [0; 4096];
    PROFILE.encode_start(&frame, &mut bytes).unwrap();
    assert!(protocol
        .decode_start_frame(3, &request, preparation, header, &bytes[..length], 0)
        .is_err());
    let enabled = NegotiatedProtocol {
        features: FeatureSet::WORKER_PIDFD.union(FeatureSet::EXECUTION_TIMEOUT),
        ..protocol
    };
    assert!(enabled.encode_start_frame(&frame, &mut bytes).is_ok());
    assert!(enabled
        .decode_start_frame(3, &request, preparation, header, &bytes[..length], 0)
        .is_ok());

    let challenge = IdentityChallenge::new(preparation, [3; IDENTITY_CHALLENGE_BYTES]);
    let length = PROFILE
        .encode_identity_challenge(challenge, &mut bytes)
        .unwrap();
    assert_eq!(
        PROFILE.decode_identity_challenge(&bytes[..length]).unwrap(),
        challenge
    );
    let reply = GuestIdentityReply::new(challenge, request.request(), "root", 0, 0).unwrap();
    let length = PROFILE.encode_identity_reply(&reply, &mut bytes).unwrap();
    assert_eq!(
        PROFILE
            .decode_identity_reply(&bytes[..length], challenge, request.request())
            .unwrap(),
        reply
    );
}
