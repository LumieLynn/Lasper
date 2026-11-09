use lasper_guest_protocol::{
    BrokerId, ControlFrameHeader, ControlReplyFrame, ControlReplyStatus, ErrorFrame, ExecutionKind,
    GuestErrorCode, GuestOperation, MessageKind, PreparationControl, PreparationControlFrame,
    PreparationControlKind, PreparationId, SessionFrameError, SessionPayloadError,
    CONTROL_FRAME_HEADER_BYTES, MAX_PREPARED_LEASE_MS,
};

use super::support::{preparation, request, resized};

fn command(kind: PreparationControlKind) -> PreparationControlFrame {
    PreparationControlFrame::new(
        9,
        kind,
        PreparationControl::new(
            request(ExecutionKind::Interactive).broker_id(),
            preparation(),
        ),
    )
}

#[test]
fn control_replies_distinguish_live_preparations_from_requested_cleanup() {
    for kind in [
        PreparationControlKind::KeepPrepared,
        PreparationControlKind::Recheck,
    ] {
        for remaining_lease_ms in [1, MAX_PREPARED_LEASE_MS] {
            round_trip(
                command(kind),
                ControlReplyStatus::Prepared { remaining_lease_ms },
            );
        }
        for remaining_lease_ms in [0, MAX_PREPARED_LEASE_MS + 1] {
            assert!(ControlReplyFrame::new(
                command(kind),
                ControlReplyStatus::Prepared { remaining_lease_ms }
            )
            .is_err());
        }
        assert!(
            ControlReplyFrame::new(command(kind), ControlReplyStatus::CleanupRequested).is_err()
        );
    }
    for kind in [
        PreparationControlKind::Cancel,
        PreparationControlKind::Close,
    ] {
        round_trip(command(kind), ControlReplyStatus::CleanupRequested);
        assert!(ControlReplyFrame::new(
            command(kind),
            ControlReplyStatus::Prepared {
                remaining_lease_ms: 1
            }
        )
        .is_err());
    }
}

fn round_trip(command: PreparationControlFrame, status: ControlReplyStatus) {
    let reply = ControlReplyFrame::new(command, status).unwrap();
    let mut bytes = [0; 512];
    let (header, length) = reply.encode_parts(&mut bytes).unwrap();
    assert_eq!(
        ControlReplyFrame::decode_for(&command, header, &bytes[..length], 0).unwrap(),
        reply
    );
}

#[test]
fn control_reply_is_bound_to_the_command_and_both_session_tokens() {
    let expected = command(PreparationControlKind::KeepPrepared);
    let reply = ControlReplyFrame::new(
        expected,
        ControlReplyStatus::Prepared {
            remaining_lease_ms: 100,
        },
    )
    .unwrap();
    let mut bytes = [0; 512];
    let (header, length) = reply.encode_parts(&mut bytes).unwrap();
    let payload = &bytes[..length];
    let wrong_id = ControlFrameHeader::new(header.kind(), 10, length as u32, 0).unwrap();
    assert!(matches!(
        ControlReplyFrame::decode_for(&expected, wrong_id, payload, 0),
        Err(SessionFrameError::MismatchedRequestId { .. })
    ));
    let other = command(PreparationControlKind::Recheck);
    assert!(matches!(
        ControlReplyFrame::decode_for(&other, header, payload, 0),
        Err(SessionFrameError::UnexpectedMessageKind { .. })
    ));
    assert!(matches!(
        ControlReplyFrame::decode_for(&expected, header, payload, 1),
        Err(SessionFrameError::UnexpectedDescriptors { .. })
    ));
    for (broker, preparation, error) in [
        (
            BrokerId::new([1; 16]),
            preparation(),
            SessionPayloadError::BrokerMismatch,
        ),
        (
            request(ExecutionKind::Interactive).broker_id(),
            PreparationId::new([1; 16]),
            SessionPayloadError::PreparationMismatch,
        ),
    ] {
        let other = PreparationControlFrame::new(
            expected.request_id(),
            expected.kind(),
            PreparationControl::new(broker, preparation),
        );
        assert_eq!(
            ControlReplyFrame::decode_for(&other, header, payload, 0),
            Err(SessionFrameError::Payload(error))
        );
    }
    let text = std::str::from_utf8(payload).unwrap().replace("100", "0");
    assert_eq!(
        ControlReplyFrame::decode_for(&expected, resized(header, text.len()), text.as_bytes(), 0),
        Err(SessionFrameError::Payload(
            SessionPayloadError::InvalidResponse
        ))
    );
}

#[test]
fn prepare_errors_can_precede_assignment_but_system_errors_require_errno() {
    let request = request(ExecutionKind::Bounded);
    let mut bytes = [0; 512];
    let mut prepare_payload = [0; 1024];
    request.encode_into(&mut prepare_payload).unwrap();
    let prepare_header =
        ControlFrameHeader::decode(&prepare_payload[..CONTROL_FRAME_HEADER_BYTES]).unwrap();
    for (code, errno) in [
        (GuestErrorCode::Expired, None),
        (GuestErrorCode::WorkerExitedBeforeStart, None),
        (GuestErrorCode::Unavailable, None),
        (GuestErrorCode::System, Some(13)),
    ] {
        let reply = ErrorFrame::new(
            request.request_id(),
            request.broker_id(),
            None,
            GuestOperation::Prepare,
            code,
            errno,
        )
        .unwrap();
        let (header, length) = reply.encode_parts(&mut bytes).unwrap();
        assert_eq!(
            ErrorFrame::decode_for(prepare_header, &request, None, header, &bytes[..length], 0)
                .unwrap(),
            reply
        );
    }
    for errno in [None, Some(0), Some(-1), Some(4096)] {
        assert!(ErrorFrame::new(
            1,
            request.broker_id(),
            None,
            GuestOperation::Prepare,
            GuestErrorCode::System,
            errno
        )
        .is_err());
    }
    assert!(ErrorFrame::new(
        1,
        request.broker_id(),
        None,
        GuestOperation::Start,
        GuestErrorCode::InvalidState,
        None
    )
    .is_err());
    assert!(ErrorFrame::new(
        1,
        request.broker_id(),
        None,
        GuestOperation::Prepare,
        GuestErrorCode::InvalidRequest,
        Some(5)
    )
    .is_err());
}

#[test]
fn error_reply_cannot_change_the_operation_or_preparation_being_rejected() {
    let request = request(ExecutionKind::Bounded);
    let start_header = ControlFrameHeader::new(MessageKind::Start, 9, 0, 0).unwrap();
    let mut bytes = [0; 512];
    let reply = ErrorFrame::new(
        9,
        request.broker_id(),
        Some(preparation()),
        GuestOperation::Start,
        GuestErrorCode::Expired,
        None,
    )
    .unwrap();
    let (header, length) = reply.encode_parts(&mut bytes).unwrap();
    assert_eq!(
        ErrorFrame::decode_for(
            start_header,
            &request,
            Some(preparation()),
            header,
            &bytes[..length],
            0
        )
        .unwrap(),
        reply
    );
    assert!(
        ErrorFrame::decode_for(start_header, &request, None, header, &bytes[..length], 0).is_err()
    );
    let cancel_header = ControlFrameHeader::new(MessageKind::Cancel, 9, 0, 0).unwrap();
    assert_eq!(
        ErrorFrame::decode_for(
            cancel_header,
            &request,
            Some(preparation()),
            header,
            &bytes[..length],
            0
        ),
        Err(SessionFrameError::Payload(
            SessionPayloadError::InvalidResponse
        ))
    );
    let text = std::str::from_utf8(&bytes[..length])
        .unwrap()
        .replace("\"expired\"", "\"system\"");
    assert_eq!(
        ErrorFrame::decode_for(
            start_header,
            &request,
            Some(preparation()),
            resized(header, text.len()),
            text.as_bytes(),
            0
        ),
        Err(SessionFrameError::Payload(
            SessionPayloadError::InvalidResponse
        ))
    );
}
