use super::tests::{established_stream, established_stream_with_current_limits};
use super::*;
use lasper_guest_protocol::MessageKind;
use std::future::{poll_fn, Future};
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::task::Poll;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn header(descriptors: u16) -> ControlFrameHeader {
    ControlFrameHeader::new(MessageKind::PrepareRequest, 17, 1, descriptors).unwrap()
}

async fn receive(stream: &mut UnixControlStream) -> ReceivedControlFrame {
    loop {
        if let ControlIoEvent::Input(Some(frame)) = stream.advance_io().await.unwrap() {
            return frame;
        }
    }
}

#[tokio::test]
async fn partial_input_keeps_its_descriptor_while_output_is_sent() {
    let (sender, receiver) = UnixStream::pair().unwrap();
    let mut sender = established_stream(sender);
    let mut receiver = established_stream(receiver);
    let (endpoint, mut peer) = StdUnixStream::pair().unwrap();
    sender
        .send_frame(header(1), b"x", &[endpoint.as_fd()])
        .await
        .unwrap();
    drop(endpoint);
    assert!(matches!(
        receiver.advance_io().await.unwrap(),
        ControlIoEvent::Input(None)
    ));

    receiver
        .queue_frame(header(0), Box::from(&b"y"[..]))
        .unwrap();
    while !matches!(receiver.advance_io().await.unwrap(), ControlIoEvent::Sent) {}
    assert_eq!(sender.receive_frame().await.unwrap().payload(), b"y");
    let frame = receive(&mut receiver).await;
    assert_eq!(frame.payload(), b"x");
    let (_, _, mut descriptors) = frame.into_parts();
    assert_eq!(descriptors.len(), 1);
    let mut endpoint = StdUnixStream::from(descriptors.pop().unwrap());
    endpoint.write_all(b"received").unwrap();
    let mut bytes = [0; 8];
    peer.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"received");
}

#[tokio::test]
async fn available_input_resumes_a_buffered_descriptor_without_readiness_notification() {
    let (host, peer) = UnixStream::pair().unwrap();
    let mut host = established_stream(host);
    let (endpoint, mut other) = StdUnixStream::pair().unwrap();
    sendmsg_raw(
        &peer,
        &header(1).encode()[..1],
        &[],
        &[endpoint.as_raw_fd()],
    )
    .unwrap();
    let mut peer = peer.into_std().unwrap();
    drop(endpoint);
    let mut budget = 1;
    assert!(host.try_receive_available(&mut budget).unwrap().is_none());
    assert_eq!(budget, 0);
    let mut budget = CONTROL_FRAME_HEADER_BYTES;
    assert!(host.try_receive_available(&mut budget).unwrap().is_none());
    assert_eq!(budget, CONTROL_FRAME_HEADER_BYTES);
    peer.write_all(&header(1).encode()[1..]).unwrap();
    peer.write_all(b"x").unwrap();
    let frame = host.try_receive_available(&mut budget).unwrap().unwrap();
    assert_eq!(budget, 0);
    assert_eq!(frame.payload(), b"x");
    let (_, _, mut descriptors) = frame.into_parts();
    let mut endpoint = StdUnixStream::from(descriptors.pop().unwrap());
    endpoint.write_all(b"kept").unwrap();
    let mut bytes = [0; 4];
    other.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"kept");
}

#[tokio::test]
async fn available_input_cannot_read_past_its_byte_budget() {
    let (host, peer) = UnixStream::pair().unwrap();
    let mut host = established_stream_with_current_limits(host, 0);
    let mut peer = peer.into_std().unwrap();
    let payload = vec![0x5a; 64 * 1024];
    let header =
        ControlFrameHeader::new(MessageKind::PrepareRequest, 17, payload.len() as u32, 0).unwrap();
    peer.write_all(&header.encode()).unwrap();
    peer.write_all(&payload).unwrap();
    let mut budget = CONTROL_FRAME_HEADER_BYTES + 7;
    assert!(host.try_receive_available(&mut budget).unwrap().is_none());
    assert_eq!(budget, 0);
    let mut budget = payload.len() - 7;
    let frame = host.try_receive_available(&mut budget).unwrap().unwrap();
    assert_eq!(budget, 0);
    assert_eq!(frame.payload(), payload);
    assert!(host.try_receive_available(&mut budget).unwrap().is_none());
}

#[tokio::test]
async fn queued_input_budget_does_not_include_later_writes() {
    let (host, peer) = UnixStream::pair().unwrap();
    let mut host = established_stream(host);
    let mut peer = peer.into_std().unwrap();
    peer.write_all(&header(0).encode()).unwrap();
    peer.write_all(b"x").unwrap();
    let mut budget = host.queued_input_bytes().unwrap();
    assert_eq!(budget, CONTROL_FRAME_HEADER_BYTES + 1);
    peer.write_all(&header(0).encode()).unwrap();
    peer.write_all(b"y").unwrap();

    let frame = host.try_receive_available(&mut budget).unwrap().unwrap();
    assert_eq!(frame.payload(), b"x");
    assert_eq!(budget, 0);
    assert!(host.try_receive_available(&mut budget).unwrap().is_none());
    assert_eq!(
        host.queued_input_bytes().unwrap(),
        CONTROL_FRAME_HEADER_BYTES + 1
    );
    let mut budget = host.queued_input_bytes().unwrap();
    assert_eq!(
        host.try_receive_available(&mut budget)
            .unwrap()
            .unwrap()
            .payload(),
        b"y"
    );
}

#[tokio::test]
async fn queued_input_budget_cannot_complete_a_frame_with_a_later_tail() {
    let (host, peer) = UnixStream::pair().unwrap();
    let mut host = established_stream(host);
    let mut peer = peer.into_std().unwrap();
    peer.write_all(&header(0).encode()[..1]).unwrap();
    let mut budget = host.queued_input_bytes().unwrap();
    assert_eq!(budget, 1);
    peer.write_all(&header(0).encode()[1..]).unwrap();
    peer.write_all(b"x").unwrap();

    assert!(host.try_receive_available(&mut budget).unwrap().is_none());
    assert_eq!(budget, 0);
    assert!(host.try_receive_available(&mut budget).unwrap().is_none());
    let mut budget = host.queued_input_bytes().unwrap();
    assert_eq!(
        host.try_receive_available(&mut budget)
            .unwrap()
            .unwrap()
            .payload(),
        b"x"
    );
}

#[tokio::test]
async fn cancelled_read_progress_resumes_after_sending_with_a_partial_header() {
    let (host, mut peer) = UnixStream::pair().unwrap();
    let mut host = established_stream(host);
    let encoded = header(0).encode();
    peer.write_all(&encoded[..1]).await.unwrap();
    assert!(matches!(
        host.advance_io().await.unwrap(),
        ControlIoEvent::Input(None)
    ));
    {
        let progress = host.advance_io();
        tokio::pin!(progress);
        poll_fn(|context| {
            assert!(progress.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
    }
    assert!(host.can_continue());
    host.queue_frame(header(0), Box::from(&b"y"[..])).unwrap();
    while !matches!(host.advance_io().await.unwrap(), ControlIoEvent::Sent) {}
    let mut sent = [0; CONTROL_FRAME_HEADER_BYTES + 1];
    peer.read_exact(&mut sent).await.unwrap();
    assert_eq!(&sent[..CONTROL_FRAME_HEADER_BYTES], &encoded);
    assert_eq!(sent[CONTROL_FRAME_HEADER_BYTES], b'y');
    peer.write_all(&encoded[1..]).await.unwrap();
    peer.write_all(b"x").await.unwrap();
    assert_eq!(receive(&mut host).await.payload(), b"x");
}

#[tokio::test]
async fn peer_eof_discards_partial_input_and_disables_both_directions() {
    let (host, mut peer) = UnixStream::pair().unwrap();
    let mut host = established_stream(host);
    peer.write_all(&header(0).encode()[..1]).await.unwrap();
    assert!(matches!(
        host.advance_io().await.unwrap(),
        ControlIoEvent::Input(None)
    ));
    drop(peer);
    assert_eq!(
        host.advance_io().await.err().unwrap().kind(),
        io::ErrorKind::UnexpectedEof
    );
    assert!(!host.can_continue());
    assert!(host.queue_frame(header(0), Box::from(&b"x"[..])).is_err());
}

#[tokio::test]
async fn write_failure_preserves_partial_input_and_its_descriptor() {
    let (host, peer) = UnixStream::pair().unwrap();
    let mut host = established_stream(host);
    let mut peer = established_stream(peer);
    let (endpoint, mut other) = StdUnixStream::pair().unwrap();
    peer.send_frame(header(1), b"x", &[endpoint.as_fd()])
        .await
        .unwrap();
    drop(endpoint);
    assert!(matches!(
        host.advance_io().await.unwrap(),
        ControlIoEvent::Input(None)
    ));
    // Refuse host writes without closing the peer's sending half. Its frame
    // and received descriptor must survive the failed outgoing request.
    // SAFETY: peer owns this socket; SHUT_RD does not release its descriptor.
    assert_eq!(
        unsafe { libc::shutdown(peer.stream.as_raw_fd(), libc::SHUT_RD) },
        0
    );
    host.queue_frame(header(0), Box::from(&b"y"[..])).unwrap();
    assert!(matches!(
        host.advance_io().await.unwrap(),
        ControlIoEvent::WriteFailed(_)
    ));
    assert!(!host.can_continue());
    assert!(!host.has_output());
    assert!(host.queue_frame(header(0), Box::from(&b"z"[..])).is_err());
    let frame = receive(&mut host).await;
    assert_eq!(frame.payload(), b"x");
    let (_, _, mut descriptors) = frame.into_parts();
    assert_eq!(descriptors.len(), 1);
    let mut endpoint = StdUnixStream::from(descriptors.pop().unwrap());
    endpoint.write_all(b"kept").unwrap();
    let mut bytes = [0; 4];
    other.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"kept");
    drop(peer);
    assert_eq!(
        host.advance_io().await.err().unwrap().kind(),
        io::ErrorKind::UnexpectedEof
    );
    assert!(host.into_inner().is_err());
}

#[tokio::test]
async fn partial_output_resumes_without_repeating_bytes_after_processing_input() {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let (host, mut peer) = UnixStream::pair().unwrap();
        let mut host = established_stream_with_current_limits(host, 0);
        let size = 4096i32;
        // SAFETY: setsockopt reads this live integer for the owned socket.
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    host.stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const i32).cast(),
                    size_of::<i32>() as _,
                )
            },
            0
        );
        let payload: Vec<u8> = (0..512 * 1024).map(|index| index as u8).collect();
        let outgoing =
            ControlFrameHeader::new(MessageKind::PrepareRequest, 17, payload.len() as u32, 0)
                .unwrap();
        host.queue_frame(outgoing, payload.clone().into()).unwrap();
        peer.write_all(&header(0).encode()).await.unwrap();
        peer.write_all(b"x").await.unwrap();
        assert_eq!(receive(&mut host).await.payload(), b"x");
        assert!(
            host.has_output(),
            "the unread peer must backpressure output"
        );
        peer.write_all(&header(0).encode()).await.unwrap();
        peer.write_all(b"z").await.unwrap();
        let mut budget = CONTROL_FRAME_HEADER_BYTES + 1;
        let incoming = host.try_receive_available(&mut budget).unwrap().unwrap();
        assert_eq!(incoming.payload(), b"z");
        assert_eq!(budget, 0);
        assert!(host.has_output());
        let reading = tokio::spawn(async move {
            let mut received = vec![0; CONTROL_FRAME_HEADER_BYTES + 512 * 1024];
            peer.read_exact(&mut received).await.unwrap();
            received
        });
        while !matches!(host.advance_io().await.unwrap(), ControlIoEvent::Sent) {}
        let received = reading.await.unwrap();
        assert_eq!(&received[..CONTROL_FRAME_HEADER_BYTES], &outgoing.encode());
        assert_eq!(&received[CONTROL_FRAME_HEADER_BYTES..], payload);
        assert!(host.can_continue());
        assert!(!host.has_output());
    })
    .await
    .expect("partial output did not resume");
}
