extern crate std;

use super::*;
use crate::ControlFrame;

fn decode_payload(bytes: &[u8]) -> Result<PrepareRequest<'_>, PayloadCodecError> {
    crate::protocol_profiles()[0].decode_prepare(bytes, &mut [0; MAX_PREPARE_UNESCAPE_BYTES])
}

fn decode_frame(
    bytes: &[u8],
    descriptors: usize,
    broker: crate::BrokerId,
) -> Result<PrepareRequestFrame<'_>, PrepareFrameError> {
    let frame = ControlFrame::decode(bytes).map_err(PrepareFrameError::Frame)?;
    let mut storage = PrepareDecodeStorage::new();
    crate::protocol_profiles()[0].decode_prepare_into(
        &mut storage,
        frame.header(),
        frame.payload(),
        descriptors,
        broker,
        &mut [0; MAX_PREPARE_UNESCAPE_BYTES],
    )?;
    Ok(storage.into_frame())
}

fn decode_ready(
    request: &PrepareRequestFrame<'_>,
    bytes: &[u8],
    descriptors: usize,
) -> Result<PrepareReadyFrame, PrepareFrameError> {
    let frame = ControlFrame::decode(bytes).map_err(PrepareFrameError::Frame)?;
    crate::protocol_profiles()[0].decode_ready(
        request,
        frame.header(),
        frame.payload(),
        descriptors,
    )
}

fn terminal_request() -> PrepareRequest<'static> {
    PrepareRequest::new(
        "alice",
        ExecutionKind::Terminal,
        GuestProcess::Command(
            CommandArgv::new(&["/usr/bin/editor", "file with spaces", "", "line\nbreak"]).unwrap(),
        ),
        crate::ProcessEnvironment::default(),
        Some(TerminalSize::new(120, 40).unwrap()),
    )
    .unwrap()
}

#[test]
fn prepare_payload_preserves_command_argument_boundaries() {
    let request = terminal_request();
    let mut payload = std::vec![0; crate::MAX_CONTROL_PAYLOAD_BYTES as usize];

    let length = request.encode_payload(&mut payload).unwrap();
    let decoded = decode_payload(&payload[..length]).unwrap();

    assert_eq!(decoded.user(), request.user());
    assert_eq!(decoded.execution(), request.execution());
    let GuestProcess::Command(argv) = decoded.process() else {
        panic!("expected an explicit command");
    };
    let mut argv_bytes = std::vec![0; MAX_COMMAND_TOTAL_BYTES + 1];
    let argv_length = argv.write_nul_terminated_into(&mut argv_bytes).unwrap();
    assert_eq!(
        &argv_bytes[..argv_length],
        b"/usr/bin/editor\0file with spaces\0\0line\nbreak\0"
    );
}

#[test]
fn piped_request_has_no_terminal_size() {
    let request = PrepareRequest::new(
        "root",
        ExecutionKind::Piped,
        GuestProcess::Command(CommandArgv::new(&["/sbin/poweroff"]).unwrap()),
        crate::ProcessEnvironment::default(),
        None,
    )
    .unwrap();
    let mut payload = [0; 512];

    let length = request.encode_payload(&mut payload).unwrap();
    let decoded = decode_payload(&payload[..length]).unwrap();
    assert_eq!(decoded.user(), "root");
    assert_eq!(decoded.execution(), ExecutionKind::Piped);
    assert_eq!(decoded.initial_size(), None);
    assert!(matches!(decoded.process(), GuestProcess::Command(_)));
}

#[test]
fn terminal_prepare_frame_round_trips_request_response_and_descriptor_order() {
    let broker_id = crate::BrokerId::new([0xa5; 16]);
    let request = PrepareRequestFrame::new(41, broker_id, terminal_request());
    let mut request_bytes = [0; 4096];

    let request_length = crate::protocol_profile(crate::ProtocolVersion { major: 1, minor: 0 })
        .unwrap()
        .encode_prepare_frame(&request, &mut request_bytes)
        .unwrap();
    let decoded_request = decode_frame(&request_bytes[..request_length], 0, broker_id).unwrap();
    let request_header = ControlFrame::decode(&request_bytes[..request_length])
        .unwrap()
        .header();
    assert_eq!(request_header.kind(), MessageKind::PrepareRequest);
    assert_eq!(request_header.request_id(), 41);
    assert_eq!(request_header.descriptor_count(), 0);
    assert_eq!(decoded_request.request().user(), "alice");

    let ready = PrepareReady::new(
        PreparationId::new([0x5a; PREPARATION_ID_BYTES]),
        broker_id,
        30_000,
    )
    .unwrap();
    let ready_frame = decoded_request.ready_frame(ready).unwrap();
    let challenge = ready_frame.identity_challenge([0x7c; crate::IDENTITY_CHALLENGE_BYTES]);
    assert_eq!(challenge.preparation_id(), ready.preparation_id());
    assert_eq!(challenge.nonce(), &[0x7c; crate::IDENTITY_CHALLENGE_BYTES]);
    assert_eq!(ready_frame.descriptor_count(), 3);
    assert_eq!(
        ready_frame.descriptor_role(0),
        Some(PrepareReadyDescriptorRole::PtyMaster)
    );
    assert_eq!(
        ready_frame.descriptor_role(1),
        Some(PrepareReadyDescriptorRole::IdentityChannel)
    );
    assert_eq!(
        ready_frame.descriptor_role(2),
        Some(PrepareReadyDescriptorRole::WorkerPidfd)
    );
    assert_eq!(ready_frame.descriptor_role(3), None);

    let mut ready_bytes = [0; 256];
    let ready_length = crate::protocol_profile(crate::ProtocolVersion { major: 1, minor: 0 })
        .unwrap()
        .encode_ready(ready_frame, &mut ready_bytes)
        .unwrap();
    let ready_header = ControlFrame::decode(&ready_bytes[..ready_length])
        .unwrap()
        .header();
    assert_eq!(ready_header.kind(), MessageKind::PrepareReady);
    assert_eq!(ready_header.request_id(), 41);
    assert_eq!(ready_header.descriptor_count(), 3);

    let decoded_ready = decode_ready(&decoded_request, &ready_bytes[..ready_length], 3).unwrap();
    assert_eq!(decoded_ready.request_id(), 41);
    assert_eq!(decoded_ready.response(), ready);
    assert_eq!(decoded_ready.response().broker_id(), broker_id);
}

#[test]
fn piped_prepare_frame_carries_identity_channel_and_assigned_worker() {
    let request = PrepareRequest::new(
        "root",
        ExecutionKind::Piped,
        GuestProcess::Command(CommandArgv::new(&["/sbin/poweroff"]).unwrap()),
        crate::ProcessEnvironment::default(),
        None,
    )
    .unwrap();
    let broker_id = crate::BrokerId::new([9; 16]);
    let request = PrepareRequestFrame::new(92, broker_id, request);
    let mut request_bytes = [0; 1024];
    let request_length = crate::protocol_profile(crate::ProtocolVersion { major: 1, minor: 0 })
        .unwrap()
        .encode_prepare_frame(&request, &mut request_bytes)
        .unwrap();
    let decoded_request = decode_frame(&request_bytes[..request_length], 0, broker_id).unwrap();

    let ready = PrepareReady::new(
        PreparationId::new([7; PREPARATION_ID_BYTES]),
        broker_id,
        MAX_PREPARED_LEASE_MS,
    )
    .unwrap();
    let ready_frame = decoded_request.ready_frame(ready).unwrap();
    assert_eq!(ready_frame.descriptor_count(), 4);
    assert_eq!(
        ready_frame.descriptor_role(0),
        Some(PrepareReadyDescriptorRole::IdentityChannel)
    );
    assert_eq!(
        ready_frame.descriptor_role(1),
        Some(PrepareReadyDescriptorRole::WorkerPidfd)
    );
    assert_eq!(
        ready_frame.descriptor_role(2),
        Some(PrepareReadyDescriptorRole::Stdout)
    );
    assert_eq!(
        ready_frame.descriptor_role(3),
        Some(PrepareReadyDescriptorRole::Stderr)
    );
    assert_eq!(ready_frame.descriptor_role(4), None);

    let mut ready_bytes = [0; 256];
    let ready_length = crate::protocol_profile(crate::ProtocolVersion { major: 1, minor: 0 })
        .unwrap()
        .encode_ready(ready_frame, &mut ready_bytes)
        .unwrap();
    let decoded_ready = decode_ready(&decoded_request, &ready_bytes[..ready_length], 4).unwrap();
    assert_eq!(decoded_ready.response(), ready);
}

#[test]
fn prepare_ready_frame_rejects_wrong_request_id_and_descriptor_contract() {
    let broker_id = crate::BrokerId::new([4; 16]);
    let request = PrepareRequestFrame::new(101, broker_id, terminal_request());
    let response = PrepareReady::new(
        PreparationId::new([3; PREPARATION_ID_BYTES]),
        broker_id,
        1_000,
    )
    .unwrap();
    let mut bytes = [0; 256];

    let other_request = PrepareRequestFrame::new(102, broker_id, terminal_request());
    let other_length = crate::protocol_profiles()[0]
        .encode_ready(other_request.ready_frame(response).unwrap(), &mut bytes)
        .unwrap();
    assert_eq!(
        decode_ready(&request, &bytes[..other_length], 3),
        Err(PrepareFrameError::MismatchedRequestId {
            expected: 101,
            actual: 102,
        })
    );

    let valid_length = crate::protocol_profiles()[0]
        .encode_ready(request.ready_frame(response).unwrap(), &mut bytes)
        .unwrap();
    assert_eq!(
        decode_ready(&request, &bytes[..valid_length], 1),
        Err(PrepareFrameError::Frame(
            ControlFrameError::DescriptorCountMismatch {
                declared: 3,
                actual: 1,
            }
        ))
    );

    let frame = ControlFrame::decode(&bytes[..valid_length]).unwrap();
    let wrong_header = ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::PrepareReady,
        request.request_id(),
        frame.payload().len() as u32,
        1,
    )
    .unwrap();
    let mut wrong_contract = [0; 256];
    let wrong_length = wrong_header
        .encode_frame(frame.payload(), &mut wrong_contract)
        .unwrap();
    assert_eq!(
        decode_ready(&request, &wrong_contract[..wrong_length], 1),
        Err(PrepareFrameError::UnexpectedDescriptorCount {
            expected: 3,
            actual: 1,
        })
    );
}

#[test]
fn prepare_ready_frame_rejects_a_different_broker_generation() {
    let expected_broker = crate::BrokerId::new([1; 16]);
    let other_broker = crate::BrokerId::new([2; 16]);
    let request = PrepareRequestFrame::new(27, expected_broker, terminal_request());
    let response = PrepareReady::new(
        PreparationId::new([3; PREPARATION_ID_BYTES]),
        other_broker,
        1_000,
    )
    .unwrap();

    assert_eq!(
        request.ready_frame(response),
        Err(PrepareFrameError::MismatchedBrokerId {
            expected: expected_broker,
            actual: other_broker,
        })
    );

    let mut bytes = [0; 256];
    let length = encode_prepare_frame(
        crate::ProtocolVersion { major: 1, minor: 0 },
        PrepareReady::KIND,
        request.request_id(),
        PrepareReady::descriptor_count_for(ExecutionKind::Terminal),
        &response,
        &mut bytes,
    )
    .unwrap();
    assert_eq!(
        decode_ready(&request, &bytes[..length], 3),
        Err(PrepareFrameError::MismatchedBrokerId {
            expected: expected_broker,
            actual: other_broker,
        })
    );
}

#[test]
fn prepare_request_frame_rejects_wrong_kind_and_any_descriptor() {
    let broker_id = crate::BrokerId::new([8; 16]);
    let request = PrepareRequestFrame::new(15, broker_id, terminal_request());
    let mut bytes = [0; 4096];
    let length = crate::protocol_profile(crate::ProtocolVersion { major: 1, minor: 0 })
        .unwrap()
        .encode_prepare_frame(&request, &mut bytes)
        .unwrap();
    let frame = ControlFrame::decode(&bytes[..length]).unwrap();

    let wrong_kind = ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::PrepareReady,
        request.request_id(),
        frame.payload().len() as u32,
        0,
    )
    .unwrap();
    let mut wrong_kind_bytes = [0; 4096];
    let wrong_kind_length = wrong_kind
        .encode_frame(frame.payload(), &mut wrong_kind_bytes)
        .unwrap();
    assert!(matches!(
        decode_frame(&wrong_kind_bytes[..wrong_kind_length], 0, broker_id),
        Err(PrepareFrameError::UnexpectedMessageKind {
            expected: MessageKind::PrepareRequest,
            actual: MessageKind::PrepareReady,
        })
    ));

    let declared_descriptor = ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::PrepareRequest,
        request.request_id(),
        frame.payload().len() as u32,
        1,
    )
    .unwrap();
    let mut declared_descriptor_bytes = [0; 4096];
    let declared_length = declared_descriptor
        .encode_frame(frame.payload(), &mut declared_descriptor_bytes)
        .unwrap();
    assert!(matches!(
        decode_frame(&declared_descriptor_bytes[..declared_length], 1, broker_id),
        Err(PrepareFrameError::UnexpectedDescriptorCount {
            expected: 0,
            actual: 1,
        })
    ));

    assert!(matches!(
        decode_frame(&bytes[..length], 1, broker_id),
        Err(PrepareFrameError::Frame(
            ControlFrameError::DescriptorCountMismatch {
                declared: 0,
                actual: 1,
            }
        ))
    ));
}

#[test]
fn prepare_ready_round_trips_ids_lease_and_fd_contract() {
    let ready = PrepareReady::new(
        PreparationId::new([0x5a; PREPARATION_ID_BYTES]),
        crate::BrokerId::new([0xa5; 16]),
        MAX_PREPARED_LEASE_MS,
    )
    .unwrap();
    let mut payload = [0; 256];

    let length = encode_json_payload(&ready, &mut payload).unwrap();
    let decoded = PrepareReady::decode_payload(&payload[..length]).unwrap();

    assert_eq!(decoded, ready);
    assert_eq!(decoded.preparation_id().as_bytes(), &[0x5a; 16]);
    assert_eq!(decoded.broker_id().as_bytes(), &[0xa5; 16]);
    assert_eq!(decoded.remaining_lease_ms(), MAX_PREPARED_LEASE_MS);
    assert_eq!(
        PrepareReady::descriptor_count_for(ExecutionKind::Terminal),
        3
    );
    assert_eq!(PrepareReady::descriptor_count_for(ExecutionKind::Piped), 4);
}

#[test]
fn prepare_ready_rejects_zero_and_overlong_leases() {
    assert!(matches!(
        PrepareReady::new(
            PreparationId::new([1; PREPARATION_ID_BYTES]),
            crate::BrokerId::new([2; 16]),
            0,
        ),
        Err(PayloadCodecError::InvalidRequest)
    ));
    assert!(matches!(
        PrepareReady::new(
            PreparationId::new([1; PREPARATION_ID_BYTES]),
            crate::BrokerId::new([2; 16]),
            MAX_PREPARED_LEASE_MS + 1,
        ),
        Err(PayloadCodecError::InvalidRequest)
    ));

    let zeros = std::iter::repeat_n("0", PREPARATION_ID_BYTES)
        .collect::<std::vec::Vec<_>>()
        .join(",");
    let overlong_lease = std::format!(
        r#"{{"preparation_id":[{zeros}],"broker_id":[{zeros}],"remaining_lease_ms":{}}}"#,
        MAX_PREPARED_LEASE_MS + 1
    );
    assert!(matches!(
        PrepareReady::decode_payload(overlong_lease.as_bytes()),
        Err(PayloadCodecError::InvalidRequest)
    ));
}

#[test]
fn prepare_decode_rejects_zero_terminal_dimensions() {
    let valid = serde_json_core::from_slice::<TerminalSize>(br#"{"cols":80,"rows":24}"#)
        .unwrap()
        .0;
    assert_eq!(valid, TerminalSize::new(80, 24).unwrap());
    assert!(serde_json_core::from_slice::<TerminalSize>(br#"{"cols":0,"rows":24}"#).is_err());
    assert!(serde_json_core::from_slice::<TerminalSize>(br#"{"cols":80,"rows":0}"#).is_err());

    let invalid_size = br#"{"user":"alice","execution":"terminal","environment":[],"process":"login_shell","initial_size":{"cols":0,"rows":24}}"#;
    assert!(matches!(
        decode_payload(invalid_size),
        Err(PayloadCodecError::InvalidRequest)
    ));
}

#[test]
fn payload_decoding_distinguishes_semantic_and_json_errors() {
    let invalid_command =
        br#"{"user":"alice","execution":"piped","environment":[],"process":{"command":["relative"]}}"#;
    assert!(matches!(
        decode_payload(invalid_command),
        Err(PayloadCodecError::InvalidRequest)
    ));

    let oversized_argument = std::format!(
        r#"{{"user":"alice","execution":"piped","environment":[],"process":{{"command":["/bin/tool","{}"]}}}}"#,
        "x".repeat(MAX_COMMAND_ARGUMENT_BYTES + 1)
    );
    assert!(matches!(
        decode_payload(oversized_argument.as_bytes()),
        Err(PayloadCodecError::InvalidRequest)
    ));

    let nul_argument = br#"{"user":"alice","execution":"piped","environment":[],"process":{"command":["/bin/tool","bad\u0000arg"]}}"#;
    assert!(matches!(
        decode_payload(nul_argument),
        Err(PayloadCodecError::InvalidRequest)
    ));

    let too_many_arguments = std::format!(
        r#"{{"user":"alice","execution":"piped","environment":[],"process":{{"command":["/bin/tool",{}]}}}}"#,
        std::iter::repeat_n(r#""x""#, MAX_COMMAND_ARGUMENTS + 1)
            .collect::<std::vec::Vec<_>>()
            .join(",")
    );
    assert!(matches!(
        decode_payload(too_many_arguments.as_bytes()),
        Err(PayloadCodecError::InvalidRequest)
    ));

    assert!(matches!(
        decode_payload(br#"{"user":"alice","execution":"piped","environment":[],"process":}"#),
        Err(PayloadCodecError::InvalidJson)
    ));
    assert!(matches!(
        decode_payload(br#"{"user":"alice","execution":"piped","environment":[],"process":{"command":["/bin/tool","bad\q"]}}"#),
        Err(PayloadCodecError::InvalidJson)
    ));

    let invalid_control =
        "{\"user\":\"alice\",\"execution\":\"piped\",\"environment\":[],\"process\":{\"command\":[\"/bin/tool\",\"bad\u{1}\"]}}";
    assert!(matches!(
        decode_payload(invalid_control.as_bytes()),
        Err(PayloadCodecError::InvalidJson)
    ));
}

#[test]
fn command_argv_preserves_the_64k_boundary_without_losing_empty_arguments() {
    let mut arguments = std::vec!["x".repeat(MAX_COMMAND_ARGUMENT_BYTES); 15];
    arguments.push("x".repeat(MAX_COMMAND_ARGUMENT_BYTES - 18));
    let mut borrowed = std::vec!["/x"];
    borrowed.extend(arguments.iter().map(std::string::String::as_str));

    let encoded = CommandArgv::new(&borrowed).unwrap();
    assert_eq!(encoded.values.len(), 17);
    assert_eq!(encoded.argument_count(), 16);
    assert_eq!(encoded.total_bytes, MAX_COMMAND_TOTAL_BYTES);
    let mut output = std::vec![0; MAX_COMMAND_TOTAL_BYTES + 1];
    assert_eq!(
        encoded.write_nul_terminated_into(&mut output).unwrap(),
        MAX_COMMAND_TOTAL_BYTES + 1
    );

    let too_large = std::vec!["x".repeat(MAX_COMMAND_ARGUMENT_BYTES); 16];
    let mut borrowed = std::vec!["/x"];
    borrowed.extend(too_large.iter().map(std::string::String::as_str));
    assert!(matches!(
        CommandArgv::new(&borrowed),
        Err(PayloadCodecError::InvalidRequest)
    ));
}

#[test]
fn request_validation_rejects_invalid_names_and_dimensions() {
    assert!(matches!(
        PrepareRequest::new(
            "../root",
            ExecutionKind::Terminal,
            GuestProcess::LoginShell,
            crate::ProcessEnvironment::default(),
            Some(TerminalSize::new(80, 24).unwrap())
        ),
        Err(PayloadCodecError::InvalidRequest)
    ));
    assert_eq!(
        TerminalSize::new(0, 24),
        Err(PayloadCodecError::InvalidRequest)
    );
    assert!(PrepareRequest::new(
        "root",
        ExecutionKind::Piped,
        GuestProcess::LoginShell,
        crate::ProcessEnvironment::default(),
        None
    )
    .is_ok());
}

#[test]
fn command_rejects_relative_program_nul_and_too_many_arguments() {
    assert!(matches!(
        CommandArgv::new(&["relative"]),
        Err(PayloadCodecError::InvalidRequest)
    ));
    assert!(matches!(
        CommandArgv::new(&["/bin/echo", "bad\0arg"]),
        Err(PayloadCodecError::InvalidRequest)
    ));
    let arguments = std::vec!["arg"; MAX_COMMAND_ARGUMENTS + 1];
    let mut argv = std::vec!["/bin/echo"];
    argv.extend(arguments);
    assert!(matches!(
        CommandArgv::new(&argv),
        Err(PayloadCodecError::InvalidRequest)
    ));
}
