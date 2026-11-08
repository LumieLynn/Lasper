use lasper_guest_protocol::{
    BrokerId, CommandArgv, ControlFrameHeader, ExecutionKind, GuestProcess, MessageKind,
    PreparationControl, PreparationControlFrame, PreparationControlKind, PreparationId,
    PrepareRequest, PrepareRequestFrame, ProjectionRequests, SessionFrameError,
    SessionPayloadError, StartAcceptedFrame, StartFrame, StartRequest, TerminalEnvironment,
    TerminalSize, WaylandProjectionRequest, X11ProjectionRequest,
};

fn interactive_request() -> PrepareRequestFrame<'static> {
    interactive_request_with_wayland("/run/user/1000/wayland-0")
}

fn interactive_request_with_wayland(path: &'static str) -> PrepareRequestFrame<'static> {
    PrepareRequestFrame::new(
        7,
        BrokerId::new([1; 16]),
        PrepareRequest::new(
            "alice",
            ExecutionKind::Interactive,
            GuestProcess::Command(CommandArgv::new(&["/usr/bin/editor"]).unwrap()),
            Some(TerminalEnvironment::new("xterm-256color", None, None).unwrap()),
            Some(TerminalSize::new(80, 24).unwrap()),
            ProjectionRequests::new(
                Some(WaylandProjectionRequest::new(path).unwrap()),
                Some(X11ProjectionRequest::new(2, "/tmp/.X11-unix", "/tmp/.X11-unix/X2").unwrap()),
            ),
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
fn start_round_trips_only_the_requested_projection_environment() {
    let request = interactive_request();
    let start = StartRequest::new(
        &request,
        preparation_id(),
        Some("/run/user/1000/wayland-0"),
        Some(2),
        Some(TerminalSize::new(120, 40).unwrap()),
    )
    .unwrap();
    let mut payload = [0; 512];
    let length = start.encode_payload(&mut payload).unwrap();

    let decoded = StartRequest::decode_for(&request, preparation_id(), &payload[..length]).unwrap();
    assert_eq!(decoded, start);
    assert_eq!(decoded.wayland_display(), Some("/run/user/1000/wayland-0"));
    assert_eq!(decoded.x11_display(), Some(2));
    assert_eq!(
        decoded.terminal_size(),
        Some(TerminalSize::new(120, 40).unwrap())
    );
}

#[test]
fn start_decode_unescapes_a_wayland_path_before_matching_the_prepare() {
    let path = r##"/run/user/1000/way\"land-0"##;
    let request = interactive_request_with_wayland(path);
    let start = StartRequest::new(
        &request,
        preparation_id(),
        Some(path),
        None,
        Some(TerminalSize::new(80, 24).unwrap()),
    )
    .unwrap();
    let mut payload = [0; 512];
    let length = start.encode_payload(&mut payload).unwrap();
    assert!(std::str::from_utf8(&payload[..length])
        .unwrap()
        .contains("\\\\"));

    let decoded = StartRequest::decode_for(&request, preparation_id(), &payload[..length]).unwrap();
    assert_eq!(decoded.wayland_display(), Some(path));
}

#[test]
fn start_rejects_a_projection_not_bound_by_prepare() {
    let request = interactive_request();
    assert_eq!(
        StartRequest::new(
            &request,
            preparation_id(),
            Some("/run/user/1000/wayland-1"),
            None,
            Some(TerminalSize::new(80, 24).unwrap()),
        ),
        Err(SessionPayloadError::UnrequestedProjection),
    );
    assert_eq!(
        StartRequest::new(
            &request,
            preparation_id(),
            None,
            Some(3),
            Some(TerminalSize::new(80, 24).unwrap()),
        ),
        Err(SessionPayloadError::UnrequestedProjection),
    );
}

#[test]
fn start_decode_rejects_wrong_broker_and_preparation_tokens() {
    let request = interactive_request();
    let valid = StartRequest::new(
        &request,
        preparation_id(),
        None,
        None,
        Some(TerminalSize::new(80, 24).unwrap()),
    )
    .unwrap();
    let mut payload = [0; 512];
    let length = valid.encode_payload(&mut payload).unwrap();
    let mut bytes = std::vec::Vec::from(&payload[..length]);
    mutate_first_array_value(&mut bytes, b"\"broker_id\"", 2);
    assert_eq!(
        StartRequest::decode_for(&request, preparation_id(), &bytes),
        Err(SessionPayloadError::BrokerMismatch),
    );

    let length = valid.encode_payload(&mut payload).unwrap();
    let mut bytes = std::vec::Vec::from(&payload[..length]);
    mutate_first_array_value(&mut bytes, b"\"preparation_id\"", 8);
    assert_eq!(
        StartRequest::decode_for(&request, preparation_id(), &bytes),
        Err(SessionPayloadError::PreparationMismatch),
    );
}

#[test]
fn start_terminal_shape_matches_execution_kind() {
    let request = interactive_request();
    assert_eq!(
        StartRequest::new(&request, preparation_id(), None, None, None),
        Err(SessionPayloadError::InvalidTerminalSize),
    );

    let bounded = PrepareRequestFrame::new(
        8,
        request.broker_id(),
        PrepareRequest::new(
            "alice",
            ExecutionKind::Bounded,
            GuestProcess::Command(CommandArgv::new(&["/usr/bin/true"]).unwrap()),
            None,
            None,
            ProjectionRequests::default(),
        )
        .unwrap(),
    );
    assert_eq!(
        StartRequest::new(
            &bounded,
            preparation_id(),
            None,
            None,
            Some(TerminalSize::new(80, 24).unwrap()),
        ),
        Err(SessionPayloadError::InvalidTerminalSize),
    );
    assert!(StartRequest::new(&bounded, preparation_id(), None, None, None).is_ok());
}

#[test]
fn preparation_control_requires_the_current_broker_and_worker_token() {
    let request = interactive_request();
    let control = PreparationControl::new(request.broker_id(), preparation_id());
    let mut payload = [0; 128];
    let length = control.encode_payload(&mut payload).unwrap();
    assert_eq!(
        PreparationControl::decode_for(&request, preparation_id(), &payload[..length]),
        Ok(control),
    );
    assert_eq!(
        PreparationControl::decode_for(&request, PreparationId::new([8; 16]), &payload[..length],),
        Err(SessionPayloadError::PreparationMismatch),
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
        let (header, length) = frame.encode_parts(&mut payload).unwrap();
        let decoded = PreparationControlFrame::decode_for(
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
    let (header, length) = frame.encode_parts(&mut payload).unwrap();
    let payload = &payload[..length];

    assert_eq!(
        PreparationControlFrame::decode_for(23, &request, preparation_id(), header, payload, 0,),
        Err(SessionFrameError::MismatchedRequestId {
            expected: 23,
            actual: 22,
        }),
    );
    assert_eq!(
        PreparationControlFrame::decode_for(
            22,
            &request,
            preparation_id(),
            ControlFrameHeader::new(MessageKind::PrepareRequest, 22, payload.len() as u32, 0)
                .unwrap(),
            payload,
            0,
        ),
        Err(SessionFrameError::UnexpectedMessageKind {
            actual: MessageKind::PrepareRequest,
        }),
    );
    assert_eq!(
        PreparationControlFrame::decode_for(22, &request, preparation_id(), header, payload, 1,),
        Err(SessionFrameError::UnexpectedDescriptors {
            declared: 0,
            received: 1,
        }),
    );
    assert_eq!(
        PreparationControlFrame::decode_for(
            22,
            &request,
            preparation_id(),
            ControlFrameHeader::new(MessageKind::Cancel, 22, payload.len() as u32, 1).unwrap(),
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
        Some("/run/user/1000/wayland-0"),
        Some(2),
        Some(TerminalSize::new(100, 30).unwrap()),
    )
    .unwrap();
    let frame = StartFrame::new(31, start);
    let mut payload = [0; 1024];
    let (header, length) = frame.encode_parts(&mut payload).unwrap();
    let payload = &payload[..length];

    assert_eq!(
        StartFrame::decode_for(31, &request, preparation_id(), header, payload, 0).unwrap(),
        frame,
    );
    assert_eq!(
        StartFrame::decode_for(
            31,
            &request,
            preparation_id(),
            ControlFrameHeader::new(MessageKind::Cancel, 31, payload.len() as u32, 0).unwrap(),
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
    let (header, length) = frame.encode_parts(&mut payload).unwrap();
    assert_eq!(
        StartAcceptedFrame::decode_for(
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
        StartAcceptedFrame::decode_for(
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
