use super::support::protocol;
use lasper_guest_protocol::{
    BrokerId, CommandArgv, ControlFrameHeader, ExecutionKind, GuestProcess, MessageKind,
    PreparationControl, PreparationControlFrame, PreparationControlKind, PreparationId,
    PrepareRequest, PrepareRequestFrame, SessionFrameError, SessionPayloadError,
    StartAcceptedFrame, StartFrame, StartRequest, TerminalSize, MAX_EXECUTION_TIMEOUT_MS,
};

fn interactive_request() -> PrepareRequestFrame<'static> {
    PrepareRequestFrame::new(
        7,
        BrokerId::new([1; 16]),
        PrepareRequest::new(
            "alice",
            ExecutionKind::Terminal,
            GuestProcess::Command(CommandArgv::new(&["/usr/bin/editor"]).unwrap()),
            lasper_guest_protocol::ProcessEnvironment::default(),
            Some(TerminalSize::new(80, 24).unwrap()),
        )
        .unwrap(),
    )
}

fn preparation_id() -> PreparationId {
    PreparationId::new([9; 16])
}

fn mutate_first_array_value(bytes: &mut [u8], field: &[u8], value: u8) {
    let mut marker = std::vec::Vec::from(field);
    marker.extend_from_slice(b":[");
    let start = bytes
        .windows(marker.len())
        .position(|window| window == marker)
        .expect("field is serialized");
    bytes[start + marker.len()] = b'0' + value;
}

#[test]
fn start_contains_only_final_dimensions_and_timeout() {
    let request = interactive_request();
    let start = StartRequest::new(
        &request,
        preparation_id(),
        Some(TerminalSize::new(120, 40).unwrap()),
        Some(120000),
    )
    .unwrap();
    let mut bytes = [0; 512];
    let n = protocol().encode_start_payload(&start, &mut bytes).unwrap();
    let decoded = protocol()
        .decode_start(&request, preparation_id(), &bytes[..n])
        .unwrap();
    assert_eq!(decoded, start);
    assert_eq!(decoded.execution_timeout_ms(), Some(120000));
    let text = std::str::from_utf8(&bytes[..n]).unwrap();
    for extra in [",\"wayland_display\":\"wayland-0\"}", ",\"x11_display\":2}"] {
        let changed = text.trim_end_matches('}').to_owned() + extra;
        assert!(protocol()
            .decode_start(&request, preparation_id(), changed.as_bytes())
            .is_err());
    }
}

#[test]
fn start_decode_rejects_wrong_broker_and_preparation_tokens() {
    let request = interactive_request();
    let valid = StartRequest::new(
        &request,
        preparation_id(),
        Some(TerminalSize::new(80, 24).unwrap()),
        None,
    )
    .unwrap();
    let mut payload = [0; 512];
    let length = protocol()
        .encode_start_payload(&valid, &mut payload)
        .unwrap();
    let mut bytes = std::vec::Vec::from(&payload[..length]);
    mutate_first_array_value(&mut bytes, b"\"broker_id\"", 2);
    assert_eq!(
        protocol().decode_start(&request, preparation_id(), &bytes),
        Err(SessionPayloadError::BrokerMismatch),
    );

    let length = protocol()
        .encode_start_payload(&valid, &mut payload)
        .unwrap();
    let mut bytes = std::vec::Vec::from(&payload[..length]);
    mutate_first_array_value(&mut bytes, b"\"preparation_id\"", 8);
    assert_eq!(
        protocol().decode_start(&request, preparation_id(), &bytes),
        Err(SessionPayloadError::PreparationMismatch),
    );
}

#[test]
fn start_terminal_shape_matches_execution_kind() {
    let request = interactive_request();
    assert!(StartRequest::new(&request, preparation_id(), None, None).is_ok());

    let bounded = PrepareRequestFrame::new(
        8,
        request.broker_id(),
        PrepareRequest::new(
            "alice",
            ExecutionKind::Piped,
            GuestProcess::Command(CommandArgv::new(&["/usr/bin/true"]).unwrap()),
            lasper_guest_protocol::ProcessEnvironment::default(),
            None,
        )
        .unwrap(),
    );
    assert_eq!(
        StartRequest::new(
            &bounded,
            preparation_id(),
            Some(TerminalSize::new(80, 24).unwrap()),
            None
        ),
        Err(SessionPayloadError::InvalidTerminalSize),
    );
    assert!(StartRequest::new(&bounded, preparation_id(), None, None).is_ok());
}

#[test]
fn start_optional_fields_must_be_omitted_instead_of_null() {
    let hello = lasper_guest_protocol::HelloRequest::new(
        lasper_guest_protocol::FeatureSet::EMPTY,
        lasper_guest_protocol::ControlLimits::CURRENT,
    )
    .unwrap();
    let selected = lasper_guest_protocol::negotiate_hello(
        hello,
        lasper_guest_protocol::HelloOffer {
            supported_features: lasper_guest_protocol::FeatureSet::WORKER_PIDFD,
            ..lasper_guest_protocol::HelloOffer::local(
                lasper_guest_protocol::ControlLimits::CURRENT,
            )
        },
    )
    .unwrap();
    for execution in [ExecutionKind::Piped, ExecutionKind::Terminal] {
        let request = super::support::request(execution);
        let start = StartRequest::new(&request, preparation_id(), None, None).unwrap();
        let mut bytes = [0; 512];
        let length = protocol().encode_start_payload(&start, &mut bytes).unwrap();
        let payload = std::str::from_utf8(&bytes[..length]).unwrap();
        for field in ["terminal_size", "execution_timeout_ms"] {
            let invalid = format!("{},\"{field}\":null}}", &payload[..payload.len() - 1]);
            let header = ControlFrameHeader::new_with_version(
                lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
                MessageKind::Start,
                10,
                invalid.len() as u32,
                0,
            )
            .unwrap();
            assert!(
                selected
                    .decode_start_frame(
                        10,
                        &request,
                        preparation_id(),
                        header,
                        invalid.as_bytes(),
                        0
                    )
                    .is_err(),
                "{execution:?}: {field}"
            );
        }
        assert!(protocol()
            .decode_start(&request, preparation_id(), payload.as_bytes())
            .is_ok());
    }
}

#[test]
fn execution_timeout_round_trips_independently_of_terminal_mode() {
    for execution in [ExecutionKind::Piped, ExecutionKind::Terminal] {
        let request = super::support::request(execution);
        for timeout in [None, Some(1), Some(120_000), Some(MAX_EXECUTION_TIMEOUT_MS)] {
            let start = StartRequest::new(
                &request,
                preparation_id(),
                request.request().initial_size(),
                timeout,
            )
            .unwrap();
            let mut bytes = [0; 512];
            let length = protocol().encode_start_payload(&start, &mut bytes).unwrap();
            let decoded = protocol()
                .decode_start(&request, preparation_id(), &bytes[..length])
                .unwrap();
            assert_eq!(decoded.execution_timeout_ms(), timeout);
            assert_eq!(decoded, start);
            assert_eq!(
                std::str::from_utf8(&bytes[..length])
                    .unwrap()
                    .contains("execution_timeout_ms"),
                timeout.is_some()
            );
        }
    }
}

#[test]
fn execution_timeout_must_be_within_the_protocol_range() {
    for execution in [ExecutionKind::Piped, ExecutionKind::Terminal] {
        let request = super::support::request(execution);
        let start = StartRequest::new(
            &request,
            preparation_id(),
            request.request().initial_size(),
            None,
        )
        .unwrap();
        let mut bytes = [0; 512];
        let length = protocol().encode_start_payload(&start, &mut bytes).unwrap();
        let prefix = std::str::from_utf8(&bytes[..length - 1]).unwrap();
        for timeout in [0, MAX_EXECUTION_TIMEOUT_MS + 1, u64::MAX] {
            assert_eq!(
                StartRequest::new(
                    &request,
                    preparation_id(),
                    request.request().initial_size(),
                    Some(timeout)
                ),
                Err(SessionPayloadError::InvalidExecutionTimeout)
            );
            let invalid = format!("{prefix},\"execution_timeout_ms\":{timeout}}}");
            assert_eq!(
                protocol().decode_start(&request, preparation_id(), invalid.as_bytes()),
                Err(SessionPayloadError::InvalidExecutionTimeout)
            );
        }
        for value in ["-1", "1.5", "18446744073709551616"] {
            let invalid = format!("{prefix},\"execution_timeout_ms\":{value}}}");
            assert!(protocol()
                .decode_start(&request, preparation_id(), invalid.as_bytes())
                .is_err());
        }
    }
}

#[test]
fn preparation_control_requires_the_current_broker_and_worker_token() {
    let request = interactive_request();
    let control = PreparationControl::new(request.broker_id(), preparation_id());
    let frame = PreparationControlFrame::new(8, PreparationControlKind::Recheck, control);
    let mut payload = [0; 128];
    let (header, length) = protocol()
        .encode_control_request(&frame, &mut payload)
        .unwrap();
    assert_eq!(
        protocol().decode_control_request(
            8,
            &request,
            preparation_id(),
            header,
            &payload[..length],
            0
        ),
        Ok(frame),
    );
    assert_eq!(
        protocol().decode_control_request(
            8,
            &request,
            PreparationId::new([8; 16]),
            header,
            &payload[..length],
            0
        ),
        Err(SessionFrameError::Payload(
            SessionPayloadError::PreparationMismatch
        )),
    );
}

#[test]
fn preparation_control_frames_round_trip_their_operation_and_token() {
    let request = interactive_request();
    let token = PreparationControl::new(request.broker_id(), preparation_id());
    let kinds = [
        PreparationControlKind::KeepPrepared,
        PreparationControlKind::Recheck,
        PreparationControlKind::Cancel,
        PreparationControlKind::Close,
    ];

    for (index, kind) in kinds.into_iter().enumerate() {
        let frame = PreparationControlFrame::new(100 + index as u64, kind, token);
        let mut payload = [0; 256];
        let (header, length) = protocol()
            .encode_control_request(&frame, &mut payload)
            .unwrap();
        let decoded = protocol()
            .decode_control_request(
                frame.request_id(),
                &request,
                preparation_id(),
                header,
                &payload[..length],
                0,
            )
            .unwrap();
        assert_eq!(decoded, frame);
    }
}

#[test]
fn preparation_control_frame_rejects_wrong_kind_id_and_descriptor_contract() {
    let request = interactive_request();
    let token = PreparationControl::new(request.broker_id(), preparation_id());
    let frame = PreparationControlFrame::new(22, PreparationControlKind::Cancel, token);
    let mut payload = [0; 256];
    let (header, length) = protocol()
        .encode_control_request(&frame, &mut payload)
        .unwrap();
    let payload = &payload[..length];

    assert_eq!(
        protocol().decode_control_request(23, &request, preparation_id(), header, payload, 0,),
        Err(SessionFrameError::MismatchedRequestId {
            expected: 23,
            actual: 22,
        }),
    );
    assert_eq!(
        protocol().decode_control_request(
            22,
            &request,
            preparation_id(),
            ControlFrameHeader::new_with_version(
                lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
                MessageKind::PrepareRequest,
                22,
                payload.len() as u32,
                0
            )
            .unwrap(),
            payload,
            0,
        ),
        Err(SessionFrameError::UnexpectedMessageKind {
            actual: MessageKind::PrepareRequest,
        }),
    );
    assert_eq!(
        protocol().decode_control_request(22, &request, preparation_id(), header, payload, 1,),
        Err(SessionFrameError::UnexpectedDescriptors {
            declared: 0,
            received: 1,
        }),
    );
    assert_eq!(
        protocol().decode_control_request(
            22,
            &request,
            preparation_id(),
            ControlFrameHeader::new_with_version(
                lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
                MessageKind::Cancel,
                22,
                payload.len() as u32,
                1
            )
            .unwrap(),
            payload,
            0,
        ),
        Err(SessionFrameError::UnexpectedDescriptors {
            declared: 1,
            received: 0,
        }),
    );
}

#[test]
fn start_frame_round_trips_and_rejects_other_message_kinds() {
    let request = interactive_request();
    let start = StartRequest::new(
        &request,
        preparation_id(),
        Some(TerminalSize::new(100, 30).unwrap()),
        None,
    )
    .unwrap();
    let frame = StartFrame::new(31, start);
    let mut payload = [0; 1024];
    let (header, length) = protocol().encode_start_frame(&frame, &mut payload).unwrap();
    let payload = &payload[..length];

    assert_eq!(
        protocol()
            .decode_start_frame(31, &request, preparation_id(), header, payload, 0)
            .unwrap(),
        frame,
    );
    assert_eq!(
        protocol().decode_start_frame(
            31,
            &request,
            preparation_id(),
            ControlFrameHeader::new_with_version(
                lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
                MessageKind::Cancel,
                31,
                payload.len() as u32,
                0
            )
            .unwrap(),
            payload,
            0,
        ),
        Err(SessionFrameError::UnexpectedMessageKind {
            actual: MessageKind::Cancel,
        }),
    );
}

#[test]
fn start_accepted_frame_round_trips_and_rejects_a_different_preparation() {
    let request = interactive_request();
    let frame = StartAcceptedFrame::new(
        31,
        PreparationControl::new(request.broker_id(), preparation_id()),
    );
    let mut payload = [0; 256];
    let (header, length) = protocol()
        .encode_start_accepted(&frame, &mut payload)
        .unwrap();
    assert_eq!(
        protocol()
            .decode_start_accepted(
                31,
                &request,
                preparation_id(),
                header,
                &payload[..length],
                0,
            )
            .unwrap(),
        frame,
    );
    assert_eq!(
        protocol().decode_start_accepted(
            31,
            &request,
            PreparationId::new([8; 16]),
            header,
            &payload[..length],
            0,
        ),
        Err(SessionFrameError::Payload(
            SessionPayloadError::PreparationMismatch,
        )),
    );
}
