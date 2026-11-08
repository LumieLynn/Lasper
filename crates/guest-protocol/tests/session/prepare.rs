use lasper_guest_protocol::{
    CommandArgv, ControlFrame, ControlFrameError, ControlFrameHeader, ExecutionKind, GuestProcess,
    MessageKind, PayloadCodecError, PrepareFrameError, PrepareRequest, PrepareRequestFrame,
};

use super::support;

#[test]
fn split_prepare_preserves_escaped_argv_and_correlation() {
    let base = support::request(ExecutionKind::Bounded);
    let arguments = ["/usr/bin/tool", "quotes\" backslash\\ newline\n", "中文"];
    let request = PrepareRequestFrame::new(
        19,
        base.broker_id(),
        PrepareRequest::new(
            "alice",
            ExecutionKind::Bounded,
            GuestProcess::Command(CommandArgv::new(&arguments).unwrap()),
            None,
            None,
            Default::default(),
        )
        .unwrap(),
    );
    let mut bytes = [0; 4096];
    let length = request.encode_into(&mut bytes).unwrap();
    let frame = ControlFrame::decode(&bytes[..length]).unwrap();
    let decoded =
        PrepareRequestFrame::decode_parts(frame.header(), frame.payload(), 0, base.broker_id())
            .unwrap();
    assert_eq!(decoded.request_id(), 19);
    assert_eq!(decoded.broker_id(), base.broker_id());
    let GuestProcess::Command(argv) = decoded.request().process() else {
        panic!("expected command");
    };
    let mut output = [0; 512];
    let length = argv.write_nul_terminated_into(&mut output).unwrap();
    assert_eq!(
        &output[..length],
        b"/usr/bin/tool\0quotes\" backslash\\ newline\n\0\xe4\xb8\xad\xe6\x96\x87\0"
    );
}

#[test]
fn split_prepare_enforces_the_same_envelope_and_semantics_as_full_decode() {
    let broker = support::request(ExecutionKind::Bounded).broker_id();
    let payload = br#"{"user":"alice","execution":"bounded","process":{"command":["relative"]}}"#;
    let header =
        ControlFrameHeader::new(MessageKind::PrepareRequest, 9, payload.len() as u32, 0).unwrap();
    assert!(matches!(
        PrepareRequestFrame::decode_parts(header, payload, 0, broker),
        Err(PrepareFrameError::Payload(
            PayloadCodecError::InvalidRequest
        ))
    ));
    assert!(matches!(
        PrepareRequestFrame::decode_parts(header, &payload[..payload.len() - 1], 0, broker),
        Err(PrepareFrameError::Frame(
            ControlFrameError::PayloadLengthMismatch { .. }
        ))
    ));
    assert!(matches!(
        PrepareRequestFrame::decode_parts(header, payload, 1, broker),
        Err(PrepareFrameError::Frame(
            ControlFrameError::DescriptorCountMismatch { .. }
        ))
    ));
    let wrong = ControlFrameHeader::new(MessageKind::Start, 9, payload.len() as u32, 0).unwrap();
    assert!(matches!(
        PrepareRequestFrame::validate_header(wrong, 0),
        Err(PrepareFrameError::UnexpectedMessageKind { .. })
    ));
    let with_fd =
        ControlFrameHeader::new(MessageKind::PrepareRequest, 9, payload.len() as u32, 1).unwrap();
    assert!(matches!(
        PrepareRequestFrame::validate_header(with_fd, 1),
        Err(PrepareFrameError::UnexpectedDescriptorCount { .. })
    ));
}
