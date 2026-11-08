use lasper_guest_protocol::{ControlLimits, MessageKind, CONTROL_FRAME_HEADER_BYTES};
use lasper_guest_runtime::transport::TransportError;
use std::io::{Read, Write};
use std::task::Poll;

use super::support::{assert_closed, established, header, raw_fd, small_send_buffer, stream_pair};

#[test]
fn empty_blocking_socket_returns_pending_and_can_be_cancelled() {
    let (guest, _peer) = stream_pair();
    let mut reader = guest.begin_receive().unwrap();
    assert_eq!(reader.advance(), Ok(Poll::Pending));
    assert!(matches!(
        guest.begin_receive(),
        Err(TransportError::ReceiveInProgress)
    ));
    drop(reader);
    assert!(guest.can_continue());
    assert!(guest.begin_receive().is_ok());
}

#[test]
fn header_and_payload_resume_without_reallocating_storage() {
    let (guest, mut peer) = established(ControlLimits::CURRENT);
    let encoded = header(MessageKind::PrepareRequest, 7, 0).encode();
    let mut reader = guest.begin_receive().unwrap();
    peer.write_all(&encoded[..11]).unwrap();
    assert_eq!(reader.advance(), Ok(Poll::Pending));
    peer.write_all(&encoded[11..]).unwrap();
    assert_eq!(
        reader.advance(),
        Ok(Poll::Ready(header(MessageKind::PrepareRequest, 7, 0)))
    );
    let mut payload = reader.into_payload(vec![0; 7]).unwrap();
    peer.write_all(b"pay").unwrap();
    assert_eq!(payload.advance(), Ok(Poll::Pending));
    peer.write_all(b"load").unwrap();
    assert_eq!(payload.advance(), Ok(Poll::Ready(())));
    let frame = payload.finish().unwrap();
    assert_eq!(frame.payload(), b"payload");
    let (_, buffer, _) = frame.into_parts();
    assert_eq!(buffer, b"payload");
    assert!(guest.can_continue());
}

#[test]
fn abandoning_partial_header_or_payload_closes_the_stream() {
    for header_only in [true, false] {
        let (guest, mut peer) = established(ControlLimits::CURRENT);
        let encoded = header(MessageKind::PrepareRequest, 3, 0).encode();
        let mut reader = guest.begin_receive().unwrap();
        if header_only {
            peer.write_all(&encoded[..5]).unwrap();
            assert_eq!(reader.advance(), Ok(Poll::Pending));
            drop(reader);
        } else {
            peer.write_all(&encoded).unwrap();
            assert!(reader.advance().unwrap().is_ready());
            let mut payload = reader.into_payload([0; 3]).unwrap();
            assert_eq!(payload.advance(), Ok(Poll::Pending));
            drop(payload);
        }
        assert!(!guest.can_continue());
        assert_closed(peer);
    }
}

#[test]
fn sender_without_io_can_be_cancelled_and_a_second_writer_is_rejected() {
    let (guest, _peer) = established(ControlLimits::CURRENT);
    let writer = guest
        .begin_send(header(MessageKind::Exit, 0, 0), &[], &[])
        .unwrap();
    assert!(matches!(
        guest.begin_send(header(MessageKind::Exit, 0, 0), &[], &[]),
        Err(TransportError::SendInProgress)
    ));
    drop(writer);
    assert!(guest.can_continue());
}

#[test]
fn pending_input_does_not_prevent_output() {
    let (guest, mut peer) = established(ControlLimits::CURRENT);
    let mut reader = guest.begin_receive().unwrap();
    assert_eq!(reader.advance(), Ok(Poll::Pending));
    let mut writer = guest
        .begin_send(header(MessageKind::Exit, 2, 0), b"ok", &[])
        .unwrap();
    assert_eq!(writer.advance(), Ok(Poll::Ready(())));
    drop(writer);
    let mut bytes = [0; CONTROL_FRAME_HEADER_BYTES + 2];
    peer.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes[CONTROL_FRAME_HEADER_BYTES..], b"ok");
    drop(reader);
}

#[test]
fn stalled_peer_keeps_send_progress_and_partial_cancel_poisoning() {
    let (guest, mut peer) = established(ControlLimits::CURRENT);
    small_send_buffer(guest.as_raw_fd());
    let payload = vec![0x5a; 256 * 1024];
    let mut writer = guest
        .begin_send(header(MessageKind::Exit, payload.len(), 0), &payload, &[])
        .unwrap();
    assert_eq!(writer.advance(), Ok(Poll::Pending));
    assert_eq!(writer.advance(), Ok(Poll::Pending));
    drop(writer);
    assert!(!guest.can_continue());
    let mut received = Vec::new();
    peer.read_to_end(&mut received).unwrap();
    assert!(received.len() >= CONTROL_FRAME_HEADER_BYTES);
    assert!(received.len() < CONTROL_FRAME_HEADER_BYTES + payload.len());
}

#[test]
fn large_receive_yields_within_the_per_turn_budget() {
    let (guest, mut peer) = established(ControlLimits::CURRENT);
    let payload = vec![0x5a; 96 * 1024];
    peer.write_all(&header(MessageKind::PrepareRequest, payload.len(), 0).encode())
        .unwrap();
    let sender = std::thread::spawn(move || {
        peer.write_all(&payload).unwrap();
        payload
    });
    let mut reader = guest.begin_receive().unwrap();
    assert!(reader.advance().unwrap().is_ready());
    let mut body = reader.into_payload(vec![0; 96 * 1024]).unwrap();
    assert_eq!(body.advance(), Ok(Poll::Pending));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while body.advance().unwrap().is_pending() {
        assert!(
            std::time::Instant::now() < deadline,
            "guest receive stalled"
        );
        std::thread::yield_now();
    }
    assert_eq!(body.finish().unwrap().payload(), sender.join().unwrap());
}

#[test]
fn duplicate_failure_releases_send_slot_without_touching_the_stream() {
    let (guest, peer) = established(ControlLimits::CURRENT);
    assert!(matches!(
        guest.begin_send(
            header(MessageKind::PrepareReady, 0, 2),
            &[],
            &[raw_fd(&peer), -1]
        ),
        Err(TransportError::Syscall(9))
    ));
    assert!(guest.can_continue());
    assert!(guest
        .begin_send(header(MessageKind::Exit, 0, 0), &[], &[])
        .is_ok());
}
