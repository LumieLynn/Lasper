extern crate std;

use super::*;
use crate::*;
use std::boxed::Box;

fn protocols(
    kind: MessageKind,
    direction: Direction,
    path: &'static str,
) -> [NegotiatedProtocol; 2] {
    let baseline = protocol_profiles()[0];
    let schema = baseline.schema(kind, direction).unwrap();
    assert!(schema.required_fields.contains(&path) || schema.optional_fields.contains(&path));
    let fields = Box::leak(Box::new([FeatureField {
        path,
        requires: FeatureSet::EXECUTION_TIMEOUT,
    }]));
    let messages = Box::leak(Box::new([MessageSchema {
        gated_fields: fields,
        ..*schema
    }]));
    let profile = Box::leak(Box::new(ProtocolProfile {
        messages,
        ..baseline
    }));
    let disabled = NegotiatedProtocol {
        profile,
        features: FeatureSet::WORKER_PIDFD,
        limits: EnforcedControlLimits::new(ControlLimits::CURRENT),
    };
    [
        disabled,
        NegotiatedProtocol {
            features: disabled.features.union(FeatureSet::EXECUTION_TIMEOUT),
            ..disabled
        },
    ]
}

fn request(terminal: bool) -> PrepareRequestFrame<'static> {
    PrepareRequestFrame::new(
        2,
        BrokerId::new([1; 16]),
        PrepareRequest::new(
            "root",
            if terminal {
                ExecutionKind::Terminal
            } else {
                ExecutionKind::Piped
            },
            GuestProcess::LoginShell,
            ProcessEnvironment::default(),
            terminal.then(|| TerminalSize::new(80, 24).unwrap()),
        )
        .unwrap(),
    )
}

fn check_session_gate(
    protocols: [NegotiatedProtocol; 2],
    present: bool,
    encode: impl Fn(
        &NegotiatedProtocol,
        &mut [u8],
    ) -> Result<(ControlFrameHeader, usize), SessionFrameError>,
    decode: impl Fn(&NegotiatedProtocol, ControlFrameHeader, &[u8]) -> Result<(), SessionFrameError>,
) {
    let [disabled, enabled] = protocols;
    let mut bytes = [0; 4096];
    let (header, length) = encode(&enabled, &mut bytes).unwrap();
    decode(&enabled, header, &bytes[..length]).unwrap();
    let expected = if present {
        Err(SessionFrameError::Payload(
            SessionPayloadError::FeatureNotNegotiated,
        ))
    } else {
        Ok(())
    };
    assert_eq!(decode(&disabled, header, &bytes[..length]), expected);
    assert_eq!(encode(&disabled, &mut bytes).map(|_| ()), expected);
}

#[test]
fn exit_and_error_gate_only_present_optional_fields() {
    let request = request(true);
    let preparation = PreparationId::new([2; 16]);
    for present in [false, true] {
        let report = ExitReport::new(
            ExecutionStatus::Failed { errno: 2 },
            Some(ExitStatus::Exited(126)),
            CleanupStatus::Complete,
            ExitReason::ExecFailed,
        )
        .unwrap()
        .with_failure(present.then(|| GuestFailure::new(GuestFailureStage::Exec, 2).unwrap()))
        .unwrap();
        let exit = ExitFrame::new(&request, preparation, report).unwrap();
        check_session_gate(
            protocols(MessageKind::Exit, Direction::GuestToHost, "failure"),
            present,
            |protocol, bytes| protocol.encode_exit(&exit, bytes),
            |protocol, header, payload| {
                protocol
                    .decode_exit(&request, preparation, header, payload, 0)
                    .map(|_| ())
            },
        );

        let error = ErrorFrame::new(
            3,
            request.broker_id(),
            Some(preparation),
            GuestOperation::Start,
            GuestErrorCode::System,
            Some(2),
        )
        .unwrap()
        .with_stage(present.then_some(GuestFailureStage::Exec))
        .unwrap();
        let start_header = ControlFrameHeader::new_with_version(
            protocol_profiles()[0].version(),
            MessageKind::Start,
            3,
            0,
            0,
        )
        .unwrap();
        check_session_gate(
            protocols(MessageKind::Error, Direction::GuestToHost, "stage"),
            present,
            |protocol, bytes| protocol.encode_error(&error, bytes),
            |protocol, header, payload| {
                protocol
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
}

#[test]
fn prepare_gate_checks_decoded_fields_and_invalidates_rejected_storage() {
    let [disabled, enabled] = protocols(
        MessageKind::PrepareRequest,
        Direction::HostToGuest,
        "initial_size",
    );
    for present in [false, true] {
        let request = request(present);
        let mut bytes = [0; 4096];
        let length = enabled.encode_prepare_frame(&request, &mut bytes).unwrap();
        let header = ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES]).unwrap();
        let payload = &bytes[CONTROL_FRAME_HEADER_BYTES..length];
        let mut scratch = [0; MAX_PREPARE_UNESCAPE_BYTES];
        assert!(enabled.decode_prepare(payload, &mut scratch).is_ok());
        assert_eq!(
            disabled.decode_prepare(payload, &mut scratch).is_err(),
            present
        );
        assert_eq!(
            disabled
                .encode_prepare(request.request(), &mut [0; 4096])
                .is_err(),
            present
        );
        let mut storage = PrepareDecodeStorage::new();
        enabled
            .decode_prepare_into(
                &mut storage,
                header,
                payload,
                0,
                request.broker_id(),
                &mut scratch,
            )
            .unwrap();
        assert!(storage.request().is_some());
        let expected = if present {
            Err(PrepareFrameError::Payload(
                PayloadCodecError::InvalidRequest,
            ))
        } else {
            Ok(())
        };
        assert_eq!(
            disabled
                .decode_prepare_into(
                    &mut storage,
                    header,
                    payload,
                    0,
                    request.broker_id(),
                    &mut scratch
                )
                .map(|_| ()),
            expected
        );
        assert_eq!(storage.request().is_some(), !present);
        drop(storage);
        assert_eq!(
            disabled
                .encode_prepare_frame(&request, &mut bytes)
                .map(|_| ()),
            expected
        );
    }
}

#[test]
fn payload_codecs_enforce_the_negotiated_frame_budget() {
    let [_, enabled] = protocols(
        MessageKind::PrepareRequest,
        Direction::HostToGuest,
        "initial_size",
    );
    let limited = NegotiatedProtocol {
        limits: EnforcedControlLimits::new(ControlLimits::new(4096, 4).unwrap()),
        ..enabled
    };
    let request = request(false);
    let mut bytes = [0; 8192];
    let length = enabled
        .encode_prepare(request.request(), &mut bytes)
        .unwrap();
    bytes[length..].fill(b' ');
    let boundary = 4096 - CONTROL_FRAME_HEADER_BYTES;
    let mut scratch = [0; MAX_PREPARE_UNESCAPE_BYTES];
    assert!(limited
        .decode_prepare(&bytes[..boundary], &mut scratch)
        .is_ok());
    assert!(matches!(
        limited.decode_prepare(&bytes[..boundary + 1], &mut scratch),
        Err(PayloadCodecError::PayloadTooLarge)
    ));

    let value = "x".repeat(5000);
    let large = PrepareRequest::new(
        "root",
        ExecutionKind::Piped,
        GuestProcess::LoginShell,
        ProcessEnvironment::new([("LARGE", value.as_str())]).unwrap(),
        None,
    )
    .unwrap();
    assert!(enabled.encode_prepare(&large, &mut bytes).is_ok());
    assert_eq!(
        limited.encode_prepare(&large, &mut bytes),
        Err(PayloadCodecError::PayloadTooLarge)
    );

    let [disabled, enabled] = protocols(
        MessageKind::Start,
        Direction::HostToGuest,
        "execution_timeout_ms",
    );
    let start = StartRequest::new(&request, PreparationId::new([2; 16]), None, Some(10)).unwrap();
    let length = enabled.encode_start_payload(&start, &mut bytes).unwrap();
    assert_eq!(
        disabled.encode_start_payload(&start, &mut bytes),
        Err(PayloadCodecError::InvalidRequest)
    );
    let limited = NegotiatedProtocol {
        profile: enabled.profile,
        ..limited
    };
    bytes[length..].fill(b' ');
    assert!(limited
        .decode_start(&request, start.preparation_id(), &bytes[..boundary])
        .is_ok());
    assert_eq!(
        limited.decode_start(&request, start.preparation_id(), &bytes[..boundary + 1]),
        Err(SessionPayloadError::Payload(
            PayloadCodecError::PayloadTooLarge
        ))
    );
}

#[test]
fn ready_checks_field_gates_on_both_codec_paths() {
    let [disabled, enabled] = protocols(
        MessageKind::PrepareReady,
        Direction::GuestToHost,
        "remaining_lease_ms",
    );
    let request = request(true);
    let ready = request
        .ready_frame(
            PrepareReady::new(PreparationId::new([2; 16]), request.broker_id(), 45_000).unwrap(),
        )
        .unwrap();
    let mut bytes = [0; 4096];
    let length = enabled.encode_ready(ready, &mut bytes).unwrap();
    let header = ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES]).unwrap();
    let payload = &bytes[CONTROL_FRAME_HEADER_BYTES..length];
    enabled.decode_ready(&request, header, payload, 3).unwrap();
    assert_eq!(
        disabled.decode_ready(&request, header, payload, 3),
        Err(PrepareFrameError::Payload(
            PayloadCodecError::InvalidRequest
        ))
    );
    assert_eq!(
        disabled.encode_ready(ready, &mut bytes),
        Err(PrepareFrameError::Payload(
            PayloadCodecError::InvalidRequest
        ))
    );
}

#[test]
fn control_and_start_accepted_check_field_gates_on_both_codec_paths() {
    let request = request(true);
    let preparation = PreparationId::new([2; 16]);
    let control = PreparationControl::new(request.broker_id(), preparation);
    for kind in [
        PreparationControlKind::KeepPrepared,
        PreparationControlKind::Recheck,
        PreparationControlKind::Cancel,
        PreparationControlKind::Close,
    ] {
        let command = PreparationControlFrame::new(3, kind, control);
        check_session_gate(
            protocols(kind.message_kind(), Direction::HostToGuest, "broker_id"),
            true,
            |protocol, bytes| protocol.encode_control_request(&command, bytes),
            |protocol, header, payload| {
                protocol
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
            _ => ControlReplyStatus::CleanupRequested,
        };
        let reply = ControlReplyFrame::new(command, status).unwrap();
        check_session_gate(
            protocols(kind.message_kind(), Direction::GuestToHost, "status"),
            true,
            |protocol, bytes| protocol.encode_control_reply(&reply, bytes),
            |protocol, header, payload| {
                protocol
                    .decode_control_reply(&command, header, payload, 0)
                    .map(|_| ())
            },
        );
    }
    let accepted = StartAcceptedFrame::new(3, control);
    check_session_gate(
        protocols(MessageKind::Start, Direction::GuestToHost, "broker_id"),
        true,
        |protocol, bytes| protocol.encode_start_accepted(&accepted, bytes),
        |protocol, header, payload| {
            protocol
                .decode_start_accepted(3, &request, preparation, header, payload, 0)
                .map(|_| ())
        },
    );
}
