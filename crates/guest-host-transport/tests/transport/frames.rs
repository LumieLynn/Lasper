use super::*;
use lasper_guest_protocol::{
    BrokerId, ControlLimits, EnforcedControlLimits, FeatureSet, HelloRequest, HelloResponse,
    MessageKind,
};
use std::fs::File;
use std::io::Read;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream as StdUnixStream;
use tokio::time::{timeout, Duration};

const TEST_CONTROL_FRAME_BYTES: u32 = 4096;

#[tokio::test]
async fn every_frame_split_preserves_the_anchor_descriptor_exactly_once() {
    use std::future::Future;
    let payload = b"split payload";
    let header = test_header(payload.len(), 1);
    let mut bytes = header.encode().to_vec();
    bytes.extend_from_slice(payload);
    for split in 1..bytes.len() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let mut receiver = established_stream(receiver);
        let (descriptor, mut witness) = StdUnixStream::pair().unwrap();
        witness.set_nonblocking(true).unwrap();
        assert_eq!(
            sendmsg_once(&sender, &bytes[..split], &[], &[descriptor.as_fd()])
                .await
                .unwrap(),
            split
        );
        drop(descriptor);
        let mut receive = Box::pin(receiver.receive_frame());
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(receive.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        assert_eq!(
            sendmsg_once(&sender, &bytes[split..], &[], &[])
                .await
                .unwrap(),
            bytes.len() - split
        );
        let frame = receive.await.unwrap();
        assert_eq!(frame.header(), header);
        assert_eq!(frame.payload(), payload);
        assert_eq!(frame.descriptors().len(), 1);
        assert_eq!(
            witness.read(&mut [0; 1]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(frame);
        assert_descriptor_released(witness).await;
    }
}

#[tokio::test]
async fn disconnect_at_every_frame_offset_releases_the_anchor_descriptor() {
    let payload = b"truncated payload";
    let mut bytes = test_header(payload.len(), 1).encode().to_vec();
    bytes.extend_from_slice(payload);
    for end in 1..bytes.len() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let mut receiver = established_stream(receiver);
        let (descriptor, witness) = StdUnixStream::pair().unwrap();
        witness.set_nonblocking(true).unwrap();
        assert_eq!(
            sendmsg_once(&sender, &bytes[..end], &[], &[descriptor.as_fd()])
                .await
                .unwrap(),
            end
        );
        drop((descriptor, sender));
        assert_eq!(
            receiver.receive_frame().await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert!(!receiver.can_continue());
        assert_descriptor_released(witness).await;
    }
}

#[tokio::test]
async fn rights_at_every_non_anchor_offset_are_rejected_and_released() {
    let mut bytes = test_header(3, 0).encode().to_vec();
    bytes.extend_from_slice(b"end");
    for offset in 1..bytes.len() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let mut receiver = established_stream(receiver);
        let (descriptor, witness) = StdUnixStream::pair().unwrap();
        witness.set_nonblocking(true).unwrap();
        assert_eq!(
            sendmsg_once(&sender, &bytes[..offset], &[], &[])
                .await
                .unwrap(),
            offset
        );
        assert_eq!(
            sendmsg_once(&sender, &bytes[offset..], &[], &[descriptor.as_fd()])
                .await
                .unwrap(),
            bytes.len() - offset
        );
        drop(descriptor);
        assert_eq!(
            receiver.receive_frame().await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(!receiver.can_continue());
        assert_descriptor_released(witness).await;
    }
}

async fn assert_descriptor_released(witness: StdUnixStream) {
    let mut witness = UnixStream::from_std(witness).unwrap();
    // Kernel final file release can finish after close returns. Wait for EOF
    // rather than requiring that work to have completed in the same timeslice.
    assert_eq!(
        timeout(
            Duration::from_secs(3),
            tokio::io::AsyncReadExt::read(&mut witness, &mut [0; 1])
        )
        .await
        .expect("received descriptor was not released")
        .unwrap(),
        0
    );
}

#[tokio::test]
async fn hello_rejections_validate_the_offer_reason_and_header_before_reporting_incompatibility() {
    use lasper_guest_protocol::{CompatibilityError, ProtocolVersion};
    for (major, range, capacity, reason, expected) in [
        (
            7,
            (0, 0),
            4096,
            r#"{"MajorMismatch":{"local":7,"peer":1}}"#,
            Some(CompatibilityError::MajorMismatch { local: 1, peer: 7 }),
        ),
        (
            7,
            (0, 0),
            4096,
            r#""InvalidOffer""#,
            Some(CompatibilityError::InvalidSelection),
        ),
        (
            1,
            (0, 0),
            4096,
            r#"{"MajorMismatch":{"local":7,"peer":1}}"#,
            Some(CompatibilityError::InvalidSelection),
        ),
        (7, (9, 0), 4096, r#""InvalidOffer""#, None),
        (7, (0, 0), 31, r#""InvalidOffer""#, None),
    ] {
        let (host, peer) = UnixStream::pair().unwrap();
        let mut host = UnixControlStream::new(host);
        let mut guest = UnixControlStream::new(peer);
        guest.session = lasper_guest_protocol::ControlConnectionState::guest();
        let payload = format!(
            r#"{{"offer":{{"protocols":{{"major":7,"min_minor":{},"max_minor":{}}},"supported_features":3,"limits":{{"max_control_frame_bytes":{},"max_descriptors":4}}}},"reason":{}}}"#,
            range.0, range.1, capacity, reason
        );
        let request = HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap();
        let (result, ()) = tokio::join!(host.handshake(1, request), async {
            let request = guest.receive_frame().await.unwrap();
            let id = request.header().request_id();
            drop(request);
            let header = ControlFrameHeader::new_with_version(
                ProtocolVersion { major, minor: 0 },
                MessageKind::HelloRejected,
                id,
                payload.len() as u32,
                0,
            )
            .unwrap();
            guest
                .send_frame(header, payload.as_bytes(), &[])
                .await
                .unwrap();
        });
        match (result, expected) {
            (Err(HandshakeError::Negotiation(actual)), Some(expected)) => {
                assert_eq!(actual, expected)
            }
            (Err(HandshakeError::Transport(error)), None) => {
                assert_eq!(error.kind(), io::ErrorKind::InvalidData)
            }
            other => panic!("unexpected rejection result: {other:?}"),
        }
        assert!(!host.can_continue());
    }
}

fn test_header(payload_bytes: usize, descriptor_count: u16) -> ControlFrameHeader {
    ControlFrameHeader::new_with_version(
        lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::Recheck,
        17,
        payload_bytes as u32,
        descriptor_count,
    )
    .unwrap()
}

fn negotiated_limits(max_descriptors: u16) -> EnforcedControlLimits {
    negotiated_limits_for(TEST_CONTROL_FRAME_BYTES, max_descriptors)
}

fn negotiated_current_limits(max_descriptors: u16) -> EnforcedControlLimits {
    negotiated_limits_for(
        lasper_guest_protocol::MAX_CONTROL_FRAME_BYTES,
        max_descriptors,
    )
}

fn negotiated_limits_for(max_frame_bytes: u32, max_descriptors: u16) -> EnforcedControlLimits {
    let limits = ControlLimits::new(max_frame_bytes, max_descriptors).unwrap();
    HelloResponse::accept(
        HelloRequest::new(FeatureSet::EMPTY, limits).unwrap(),
        lasper_guest_protocol::HelloOffer::local(limits),
        BrokerId::new([1; 16]),
    )
    .unwrap()
    .1
    .limits()
}

pub(super) fn established_stream(stream: UnixStream) -> UnixControlStream {
    established_stream_with_descriptor_limit(stream, 2)
}

fn established_stream_with_descriptor_limit(
    stream: UnixStream,
    max_descriptors: u16,
) -> UnixControlStream {
    established_stream_with_negotiated_limits(stream, negotiated_limits(max_descriptors))
}

pub(super) fn established_stream_with_current_limits(
    stream: UnixStream,
    max_descriptors: u16,
) -> UnixControlStream {
    established_stream_with_negotiated_limits(stream, negotiated_current_limits(max_descriptors))
}

fn established_stream_with_negotiated_limits(
    stream: UnixStream,
    limits: EnforcedControlLimits,
) -> UnixControlStream {
    let mut stream = UnixControlStream::new(stream);
    let request = ControlFrameHeader::new_with_version(
        lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloRequest,
        1,
        0,
        0,
    )
    .unwrap();
    let response = ControlFrameHeader::new_with_version(
        lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloResponse,
        1,
        0,
        0,
    )
    .unwrap();
    stream.session = stream
        .session
        .next_outbound(request)
        .unwrap()
        .next_inbound(response)
        .unwrap();
    let (_, protocol) = HelloResponse::accept(
        HelloRequest::new(FeatureSet::EMPTY, limits.get()).unwrap(),
        lasper_guest_protocol::HelloOffer::local(limits.get()),
        BrokerId::new([1; 16]),
    )
    .unwrap();
    stream.session.complete_protocol(protocol).unwrap();
    stream
}

#[tokio::test]
async fn host_transport_allows_only_one_outbound_hello_request() {
    let (stream, _peer) = UnixStream::pair().unwrap();
    let mut host = UnixControlStream::new(stream);
    let request = ControlFrameHeader::new_with_version(
        lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloRequest,
        7,
        0,
        0,
    )
    .unwrap();
    let response = ControlFrameHeader::new_with_version(
        lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloResponse,
        7,
        0,
        0,
    )
    .unwrap();

    let error = host.send_frame(response, &[], &[]).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(host.can_continue());

    host.send_frame(request, &[], &[]).await.unwrap();
    let error = host.send_frame(request, &[], &[]).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(host.can_continue());
}

#[tokio::test]
async fn host_transport_rejects_inbound_hello_request() {
    let (stream, peer) = UnixStream::pair().unwrap();
    let mut host = UnixControlStream::new(stream);
    let request = ControlFrameHeader::new_with_version(
        lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloRequest,
        7,
        0,
        0,
    )
    .unwrap();
    sendmsg_raw(&peer, &request.encode(), &[], &[]).unwrap();

    let error = host.receive_frame().await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(!host.can_continue());
}

fn assert_peer_observes_close(mut peer: StdUnixStream) {
    let mut byte = [0];
    assert_eq!(peer.read(&mut byte).unwrap(), 0);
}

#[tokio::test]
async fn frames_round_trip_and_pass_owned_close_on_exec_descriptors() {
    let (sender, receiver) = UnixStream::pair().unwrap();
    let mut sender = established_stream(sender);
    let mut receiver = established_stream(receiver);
    let file = File::open("/dev/null").unwrap();
    let payload = b"prepare";
    let header = test_header(payload.len(), 1);
    let descriptors = [file.as_fd()];

    let (sent, frame) = tokio::join!(
        sender.send_frame(header, payload, &descriptors),
        receiver.receive_frame()
    );
    sent.unwrap();
    let frame = frame.unwrap();

    assert_eq!(frame.header(), header);
    assert_eq!(frame.payload(), payload);
    assert_eq!(frame.descriptors().len(), 1);
    assert_ne!(
        unsafe { libc::fcntl(frame.descriptors()[0].as_raw_fd(), libc::F_GETFD) }
            & libc::FD_CLOEXEC,
        0
    );
}

#[tokio::test]
async fn large_frame_short_writes_keep_rights_attached_once() {
    let (sender, receiver) = UnixStream::pair().unwrap();
    let small_send_buffer: libc::c_int = 4096;
    let result = unsafe {
        libc::setsockopt(
            sender.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&small_send_buffer as *const libc::c_int).cast(),
            size_of::<libc::c_int>() as _,
        )
    };
    assert_eq!(result, 0);

    let mut sender = established_stream_with_current_limits(sender, 2);
    let mut receiver = established_stream_with_current_limits(receiver, 2);
    let file = File::open("/dev/null").unwrap();
    let payload = vec![b'x'; 512 * 1024];
    let header = test_header(payload.len(), 1);
    let descriptors = [file.as_fd()];
    let (sent, frame) = tokio::join!(
        sender.send_frame(header, &payload, &descriptors),
        receiver.receive_frame()
    );

    sent.unwrap();
    let frame = frame.unwrap();
    assert_eq!(frame.payload(), payload);
    assert_eq!(frame.descriptors().len(), 1);
    assert!(sender.can_continue());
    assert!(receiver.can_continue());
}

#[tokio::test]
async fn coalesced_frames_are_received_individually() {
    let (sender, receiver) = UnixStream::pair().unwrap();
    let mut sender = established_stream(sender);
    let mut receiver = established_stream(receiver);
    let first = test_header(5, 0);
    let second = test_header(6, 0);

    sender.send_frame(first, b"first", &[]).await.unwrap();
    sender.send_frame(second, b"second", &[]).await.unwrap();

    assert_eq!(receiver.receive_frame().await.unwrap().payload(), b"first");
    assert_eq!(receiver.receive_frame().await.unwrap().payload(), b"second");
}

#[tokio::test]
async fn negotiated_limits_reject_outbound_frames_before_io() {
    let (sender, receiver) = UnixStream::pair().unwrap();
    let mut sender = established_stream_with_descriptor_limit(sender, 0);
    let mut receiver = established_stream_with_descriptor_limit(receiver, 0);
    let oversized = vec![0; TEST_CONTROL_FRAME_BYTES as usize - CONTROL_FRAME_HEADER_BYTES + 1];

    let error = sender
        .send_frame(test_header(oversized.len(), 0), &oversized, &[])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(sender.can_continue());

    sender
        .send_frame(test_header(2, 0), b"ok", &[])
        .await
        .unwrap();
    assert_eq!(receiver.receive_frame().await.unwrap().payload(), b"ok");
}

#[tokio::test]
async fn negotiated_limits_reject_outbound_descriptors_before_io() {
    let (sender, receiver) = UnixStream::pair().unwrap();
    let mut sender = established_stream_with_descriptor_limit(sender, 0);
    let mut receiver = established_stream_with_descriptor_limit(receiver, 0);
    let file = File::open("/dev/null").unwrap();

    let error = sender
        .send_frame(test_header(0, 1), &[], &[file.as_fd()])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(sender.can_continue());

    sender
        .send_frame(test_header(2, 0), b"ok", &[])
        .await
        .unwrap();
    assert_eq!(receiver.receive_frame().await.unwrap().payload(), b"ok");
}

#[tokio::test]
async fn negotiated_limits_reject_inbound_header_before_payload() {
    let (sender, receiver) = UnixStream::pair().unwrap();
    let mut receiver = established_stream_with_descriptor_limit(receiver, 0);
    let oversized_payload = TEST_CONTROL_FRAME_BYTES as usize - CONTROL_FRAME_HEADER_BYTES + 1;
    sendmsg_raw(
        &sender,
        &test_header(oversized_payload, 0).encode(),
        &[],
        &[],
    )
    .unwrap();

    let result = timeout(Duration::from_millis(20), receiver.receive_frame()).await;
    let error = result.expect("receiver must reject the header without waiting for payload");
    assert_eq!(error.unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert!(!receiver.can_continue());
}

#[tokio::test]
async fn negotiated_descriptor_limit_closes_received_rights() {
    let (sender, receiver) = UnixStream::pair().unwrap();
    let mut receiver = established_stream_with_descriptor_limit(receiver, 0);
    let (transferred, peer) = StdUnixStream::pair().unwrap();
    let header = test_header(0, 1).encode();

    sendmsg_raw(&sender, &header, &[], &[transferred.as_raw_fd()]).unwrap();
    drop(transferred);
    let error = receiver.receive_frame().await.unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(!receiver.can_continue());
    assert_peer_observes_close(peer);
}

#[tokio::test]
async fn descriptor_count_mismatch_rejects_and_poison_closes_the_stream() {
    let (sender, receiver) = UnixStream::pair().unwrap();
    let mut receiver = established_stream(receiver);
    let file = File::open("/dev/null").unwrap();
    let header = test_header(0, 0).encode();

    let written = sendmsg_once(&sender, &header, &[], &[file.as_fd()])
        .await
        .unwrap();
    assert_eq!(written, header.len());
    let error = receiver.receive_frame().await.unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(!receiver.can_continue());
    assert!(receiver.receive_frame().await.is_err());
}

#[tokio::test]
async fn descriptors_attached_after_the_first_frame_byte_are_rejected() {
    let (sender, receiver) = UnixStream::pair().unwrap();
    let mut receiver = established_stream(receiver);
    let file = File::open("/dev/null").unwrap();
    let header = test_header(1, 0).encode();
    sendmsg_raw(&sender, &header, &[], &[]).unwrap();
    sendmsg_once(&sender, &[], b"x", &[file.as_fd()])
        .await
        .unwrap();

    let error = receiver.receive_frame().await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(!receiver.can_continue());
}

#[tokio::test]
async fn cancelling_a_partial_frame_receive_invalidates_the_stream() {
    let (sender, receiver) = UnixStream::pair().unwrap();
    let mut receiver = UnixControlStream::new(receiver);
    sendmsg_raw(&sender, &[0x4c], &[], &[]).unwrap();

    let result = timeout(Duration::from_millis(20), receiver.receive_frame()).await;

    assert!(result.is_err());
    assert!(!receiver.can_continue());
    assert!(receiver.into_inner().is_err());
}

#[tokio::test]
async fn healthy_stream_can_return_its_inner_socket() {
    let (stream, _peer) = UnixStream::pair().unwrap();

    assert!(UnixControlStream::new(stream).into_inner().is_ok());
}

#[tokio::test]
async fn outbound_shape_errors_do_not_touch_the_stream() {
    let (sender, _receiver) = UnixStream::pair().unwrap();
    let mut sender = UnixControlStream::new(sender);
    let file = File::open("/dev/null").unwrap();

    let error = sender
        .send_frame(test_header(0, 0), b"not-empty", &[])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(sender.can_continue());

    let error = sender
        .send_frame(test_header(0, 0), &[], &[file.as_fd()])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(sender.can_continue());
}
