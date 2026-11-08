use lasper_guest_protocol::{
    ControlLimitError, ControlLimits, ControlSessionError, MessageKind, CONTROL_FRAME_HEADER_BYTES,
};
use lasper_guest_runtime::transport::TransportError;
use std::io::{Read, Write};

use super::support::{established, header, receive, send, stream_pair};

#[test]
fn response_requires_a_received_hello_request() {
    let (guest, _peer) = stream_pair();
    assert!(matches!(
        guest.begin_send(header(MessageKind::HelloResponse, 0, 0), &[], &[]),
        Err(TransportError::Session(
            ControlSessionError::ExpectedInbound(MessageKind::HelloRequest)
        ))
    ));
    assert!(guest.can_continue());
}

#[test]
fn guest_rejects_a_hello_response() {
    let (guest, mut peer) = stream_pair();
    peer.write_all(&header(MessageKind::HelloResponse, 0, 0).encode())
        .unwrap();
    assert!(matches!(
        receive(&guest, &mut []),
        Err(TransportError::Session(
            ControlSessionError::UnexpectedMessage { .. }
        ))
    ));
    assert!(!guest.can_continue());
}

#[test]
fn second_request_is_rejected_before_hello_completion() {
    let (guest, mut peer) = stream_pair();
    let request = header(MessageKind::HelloRequest, 0, 0).encode();
    peer.write_all(&request).unwrap();
    receive(&guest, &mut []).unwrap();
    peer.write_all(&request).unwrap();
    assert!(matches!(
        receive(&guest, &mut []),
        Err(TransportError::Session(
            ControlSessionError::ExpectedOutbound(MessageKind::HelloResponse)
        ))
    ));
    assert!(!guest.can_continue());
}

#[test]
fn hello_length_is_checked_before_payload_storage_is_requested() {
    let (guest, mut peer) = stream_pair();
    peer.write_all(
        &header(
            MessageKind::HelloRequest,
            lasper_guest_protocol::HELLO_PAYLOAD_BUFFER_BYTES + 1,
            0,
        )
        .encode(),
    )
    .unwrap();
    let mut reader = guest.begin_receive().unwrap();
    assert!(matches!(
        reader.advance(),
        Err(TransportError::Session(ControlSessionError::Limit(
            ControlLimitError::FrameTooLarge { .. }
        )))
    ));
}

#[test]
fn negotiated_limits_reject_headers_before_payload_or_fd_duplication() {
    let limits = ControlLimits::new(CONTROL_FRAME_HEADER_BYTES as u32 + 8, 0).unwrap();
    let (guest, mut peer) = established(limits);
    assert!(matches!(
        guest.begin_send(header(MessageKind::Exit, 9, 0), &[0; 9], &[]),
        Err(TransportError::Session(ControlSessionError::Limit(
            ControlLimitError::FrameTooLarge { .. }
        )))
    ));
    assert!(matches!(
        guest.begin_send(header(MessageKind::PrepareReady, 0, 1), &[], &[-1]),
        Err(TransportError::Session(ControlSessionError::Limit(
            ControlLimitError::TooManyDescriptors { .. }
        )))
    ));
    assert!(guest.can_continue());
    send(&guest, header(MessageKind::Exit, 2, 0), b"ok", &[]).unwrap();
    let mut bytes = [0; CONTROL_FRAME_HEADER_BYTES + 2];
    peer.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes[CONTROL_FRAME_HEADER_BYTES..], b"ok");
    peer.write_all(&header(MessageKind::PrepareRequest, 9, 0).encode())
        .unwrap();
    assert!(matches!(
        receive(&guest, &mut []),
        Err(TransportError::Session(ControlSessionError::Limit(
            ControlLimitError::FrameTooLarge { .. }
        )))
    ));
}
