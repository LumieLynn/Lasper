use lasper_guest_host_transport::UnixControlStream;
use lasper_guest_protocol::{
    BrokerId, ControlFrameHeader, ControlLimits, EnforcedControlLimits, FeatureSet, HelloRequest,
    HelloResponse, MessageKind, CONTROL_FRAME_HEADER_BYTES,
};
use lasper_guest_runtime::transport::GuestControlStream;
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::{size_of, zeroed};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use tokio::net::UnixStream;

#[repr(C, align(16))]
struct AncillaryBuffer([usize; 16]);

fn header(kind: MessageKind, payload: &[u8], descriptor_count: u16) -> ControlFrameHeader {
    ControlFrameHeader::new(kind, 17, payload.len() as u32, descriptor_count).unwrap()
}

fn raw_stream_pair() -> (UnixControlStream, StdUnixStream) {
    let (host, guest) = StdUnixStream::pair().unwrap();
    host.set_nonblocking(true).unwrap();
    (
        UnixControlStream::new(UnixStream::from_std(host).unwrap()),
        guest,
    )
}

fn current_limits() -> EnforcedControlLimits {
    HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT)
        .unwrap()
        .negotiate(
            lasper_guest_protocol::PROTOCOL_MAJOR,
            HelloResponse::new(
                FeatureSet::EMPTY,
                BrokerId::new([0x5a; 16]),
                ControlLimits::CURRENT,
            )
            .unwrap(),
        )
        .unwrap()
}

async fn establish_host(host: &mut UnixControlStream) {
    host.send_frame(header(MessageKind::HelloRequest, &[], 0), &[], &[])
        .await
        .unwrap();
    let response = host.receive_frame().await.unwrap();
    assert_eq!(response.header().kind(), MessageKind::HelloResponse);
    host.complete_hello(current_limits()).unwrap();
}

fn establish_guest(guest: &mut GuestControlStream) {
    let mut payload = [];
    let request = guest.receive_frame(&mut payload).unwrap();
    assert_eq!(request.header().kind(), MessageKind::HelloRequest);
    guest
        .send_frame(
            ControlFrameHeader::new(
                MessageKind::HelloResponse,
                request.header().request_id(),
                0,
                0,
            )
            .unwrap(),
            &[],
            &[],
        )
        .unwrap();
    guest.complete_hello(current_limits()).unwrap();
}

fn establish_guest_with_peer(peer: &mut StdUnixStream, guest: &mut GuestControlStream) {
    let request = header(MessageKind::HelloRequest, &[], 0);
    peer.write_all(&request.encode()).unwrap();

    let mut payload = [];
    let received = guest.receive_frame(&mut payload).unwrap();
    assert_eq!(received.header().kind(), MessageKind::HelloRequest);
    guest
        .send_frame(
            ControlFrameHeader::new(
                MessageKind::HelloResponse,
                received.header().request_id(),
                0,
                0,
            )
            .unwrap(),
            &[],
            &[],
        )
        .unwrap();

    let mut response = [0; CONTROL_FRAME_HEADER_BYTES];
    peer.read_exact(&mut response).unwrap();
    let response = ControlFrameHeader::decode(&response).unwrap();
    assert_eq!(response.kind(), MessageKind::HelloResponse);
    guest.complete_hello(current_limits()).unwrap();
}

fn set_small_send_buffer(stream: &StdUnixStream) {
    let bytes: libc::c_int = 4096;
    // `setsockopt` only reads the live integer passed for this call.
    let result = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&bytes as *const libc::c_int).cast(),
            size_of::<libc::c_int>() as _,
        )
    };
    assert_eq!(result, 0);
}

fn send_with_rights(
    stream: &StdUnixStream,
    bytes: &[u8],
    descriptors: &[RawFd],
) -> io::Result<usize> {
    let mut vector = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let mut control = AncillaryBuffer([0; 16]);
    // The helper fills every field that `sendmsg` reads before the syscall.
    let mut message: libc::msghdr = unsafe { zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;

    if !descriptors.is_empty() {
        let data_bytes = std::mem::size_of_val(descriptors);
        let control_bytes = unsafe { libc::CMSG_SPACE(data_bytes as _) } as usize;
        assert!(control_bytes <= size_of::<AncillaryBuffer>());
        message.msg_control = control.0.as_mut_ptr().cast();
        message.msg_controllen = control_bytes;
        // The local buffer is aligned and sized for this descriptor list.
        let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        assert!(!header.is_null());
        // `header` points into the checked control allocation above.
        unsafe {
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(data_bytes as _) as _;
            std::ptr::copy_nonoverlapping(
                descriptors.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(header),
                data_bytes,
            );
        }
    }

    // The kernel only borrows the live byte and control buffers for this call.
    let written = unsafe { libc::sendmsg(stream.as_raw_fd(), &message, libc::MSG_NOSIGNAL) };
    if written < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(written as usize)
    }
}

fn assert_peer_observes_close(mut peer: StdUnixStream) {
    let mut byte = [0];
    assert_eq!(peer.read(&mut byte).unwrap(), 0);
}

#[tokio::test]
async fn host_and_guest_round_trip_frames_without_descriptors() {
    let (mut host, guest) = raw_stream_pair();
    let request_payload = b"prepare";
    let response_payload = b"ready";

    let guest_task = tokio::task::spawn_blocking(move || {
        // Ownership moves into the guest transport; the original socket is not
        // used again in this task.
        let mut guest = unsafe { GuestControlStream::from_raw_fd(guest.into_raw_fd()) };
        establish_guest(&mut guest);
        let mut payload = [0; 32];
        let frame = guest.receive_frame(&mut payload).unwrap();
        assert_eq!(frame.payload(), request_payload);
        assert!(frame.descriptors().is_empty());
        guest
            .send_frame(
                header(MessageKind::PrepareReady, response_payload, 0),
                response_payload,
                &[],
            )
            .unwrap();
    });

    establish_host(&mut host).await;
    host.send_frame(
        header(MessageKind::PrepareRequest, request_payload, 0),
        request_payload,
        &[],
    )
    .await
    .unwrap();
    let response = host.receive_frame().await.unwrap();
    guest_task.await.unwrap();

    assert_eq!(response.payload(), response_payload);
    assert!(response.descriptors().is_empty());
}

#[tokio::test]
async fn host_and_guest_transports_exchange_typed_hello_payloads() {
    let (mut host, guest) = raw_stream_pair();
    let request = HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap();
    let mut request_payload = [0; 256];
    let request_len = request.encode_payload(&mut request_payload).unwrap();

    let guest_task = tokio::task::spawn_blocking(move || {
        // Ownership moves into the guest transport; the original socket is not
        // used again in this task.
        let mut guest = unsafe { GuestControlStream::from_raw_fd(guest.into_raw_fd()) };
        let mut payload = [0; 256];
        let frame = guest.receive_frame(&mut payload).unwrap();
        assert_eq!(frame.header().kind(), HelloRequest::KIND);
        assert!(frame.descriptors().is_empty());
        let request = HelloRequest::decode_payload(frame.payload()).unwrap();

        let response = HelloResponse::new(
            FeatureSet::EMPTY,
            BrokerId::new([0x5a; 16]),
            ControlLimits::CURRENT,
        )
        .unwrap();
        let mut response_payload = [0; 256];
        let response_len = response.encode_payload(&mut response_payload).unwrap();
        let negotiated = request
            .negotiate(lasper_guest_protocol::PROTOCOL_MAJOR, response)
            .unwrap();
        guest
            .send_frame(
                header(HelloResponse::KIND, &response_payload[..response_len], 0),
                &response_payload[..response_len],
                &[],
            )
            .unwrap();
        guest.complete_hello(negotiated).unwrap();

        let mut next_payload = [0; 32];
        let next = guest.receive_frame(&mut next_payload).unwrap();
        assert_eq!(next.header().kind(), MessageKind::PrepareRequest);
        assert_eq!(next.payload(), b"prepare");
        negotiated
    });

    host.send_frame(
        header(HelloRequest::KIND, &request_payload[..request_len], 0),
        &request_payload[..request_len],
        &[],
    )
    .await
    .unwrap();
    let response = host.receive_frame().await.unwrap();
    assert_eq!(response.header().kind(), HelloResponse::KIND);
    assert!(response.descriptors().is_empty());
    let peer_protocol_major = response.header().protocol_major();
    let response = HelloResponse::decode_payload(response.payload()).unwrap();
    let negotiated = request.negotiate(peer_protocol_major, response).unwrap();
    host.complete_hello(negotiated).unwrap();
    host.send_frame(
        header(MessageKind::PrepareRequest, b"prepare", 0),
        b"prepare",
        &[],
    )
    .await
    .unwrap();

    assert_eq!(guest_task.await.unwrap().get(), ControlLimits::CURRENT);
}

#[tokio::test]
async fn hello_phase_rejects_oversized_response_before_negotiation_finishes() {
    let (mut host, mut guest) = raw_stream_pair();
    let limits =
        ControlLimits::new(lasper_guest_protocol::MIN_NEGOTIATED_CONTROL_FRAME_BYTES, 0).unwrap();
    let request = HelloRequest::new(FeatureSet::EMPTY, limits).unwrap();
    let mut request_payload = [0; lasper_guest_protocol::HELLO_PAYLOAD_BUFFER_BYTES];
    let request_len = request.encode_payload(&mut request_payload).unwrap();
    host.send_frame(
        header(HelloRequest::KIND, &request_payload[..request_len], 0),
        &request_payload[..request_len],
        &[],
    )
    .await
    .unwrap();

    let malicious_payload = lasper_guest_protocol::HELLO_PAYLOAD_BUFFER_BYTES + 1;
    let malicious_header =
        ControlFrameHeader::new(HelloResponse::KIND, 17, malicious_payload as u32, 0).unwrap();
    guest.write_all(&malicious_header.encode()).unwrap();

    let result = tokio::time::timeout(std::time::Duration::from_millis(20), host.receive_frame())
        .await
        .expect("host must reject the response header without waiting for its payload");
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert!(!host.can_continue());
}

#[tokio::test]
async fn hello_frames_use_the_fixed_bootstrap_envelope() {
    let (mut host, mut peer) = raw_stream_pair();
    let declared_limits =
        ControlLimits::new(lasper_guest_protocol::MIN_NEGOTIATED_CONTROL_FRAME_BYTES, 0).unwrap();
    let request = HelloRequest::new(FeatureSet::EMPTY, declared_limits).unwrap();
    let mut request_payload = [0; lasper_guest_protocol::HELLO_PAYLOAD_BUFFER_BYTES];
    let request_len = request.encode_payload(&mut request_payload).unwrap();
    let request_header = header(HelloRequest::KIND, &request_payload[..request_len], 0);
    assert!(
        CONTROL_FRAME_HEADER_BYTES as u32 + request_header.payload_bytes()
            > declared_limits.max_control_frame_bytes()
    );
    host.send_frame(request_header, &request_payload[..request_len], &[])
        .await
        .unwrap();

    let response = HelloResponse::new(
        FeatureSet::EMPTY,
        BrokerId::new([0x5a; 16]),
        ControlLimits::CURRENT,
    )
    .unwrap();
    let mut response_payload = [0; lasper_guest_protocol::HELLO_PAYLOAD_BUFFER_BYTES];
    let response_len = response.encode_payload(&mut response_payload).unwrap();
    assert!(
        CONTROL_FRAME_HEADER_BYTES + response_len
            > declared_limits.max_control_frame_bytes() as usize
    );
    let response_header = header(HelloResponse::KIND, &response_payload[..response_len], 0);
    peer.write_all(&response_header.encode()).unwrap();
    peer.write_all(&response_payload[..response_len]).unwrap();

    let received = host.receive_frame().await.unwrap();
    assert_eq!(received.header().kind(), HelloResponse::KIND);
    assert_eq!(received.payload(), &response_payload[..response_len]);
    let response = HelloResponse::decode_payload(received.payload()).unwrap();
    let negotiated = request
        .negotiate(received.header().protocol_major(), response)
        .unwrap();
    host.complete_hello(negotiated).unwrap();

    let error = host
        .send_frame(header(MessageKind::PrepareRequest, b"x", 0), b"x", &[])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(host.can_continue());
}

#[tokio::test]
async fn host_and_guest_round_trip_two_ordered_descriptors() {
    let (mut host, guest) = raw_stream_pair();
    let null = File::open("/dev/null").unwrap();
    let zero = File::open("/dev/zero").unwrap();
    let request_payload = b"two-fds";
    let descriptors = [null.as_fd(), zero.as_fd()];

    let guest_task = tokio::task::spawn_blocking(move || {
        // Ownership moves into the guest transport; the original socket is not
        // used again in this task.
        let mut guest = unsafe { GuestControlStream::from_raw_fd(guest.into_raw_fd()) };
        establish_guest(&mut guest);
        let mut payload = [0; 32];
        let frame = guest.receive_frame(&mut payload).unwrap();
        assert_eq!(frame.payload(), request_payload);
        assert_eq!(frame.descriptors().len(), 2);
        for descriptor in frame.descriptors().as_raw_fds() {
            // `F_GETFD` only reads flags from this live received descriptor.
            assert_ne!(
                unsafe { libc::fcntl(*descriptor, libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
        }

        let (_, _, descriptors) = frame.into_parts();
        let (raw, count) = descriptors.into_raw_fds();
        let response_payload = b"returned";
        guest
            .send_frame(
                header(MessageKind::PrepareReady, response_payload, count as u16),
                response_payload,
                &raw[..count],
            )
            .unwrap();
        for descriptor in raw.into_iter().take(count) {
            // `into_raw_fds` transferred ownership of these entries.
            drop(unsafe { OwnedFd::from_raw_fd(descriptor) });
        }
    });

    establish_host(&mut host).await;
    host.send_frame(
        header(MessageKind::PrepareRequest, request_payload, 2),
        request_payload,
        &descriptors,
    )
    .await
    .unwrap();
    let response = host.receive_frame().await.unwrap();
    guest_task.await.unwrap();

    assert_eq!(response.payload(), b"returned");
    let (_, _, descriptors) = response.into_parts();
    let mut descriptors = descriptors.into_iter();
    let mut returned_null = File::from(descriptors.next().unwrap());
    let mut returned_zero = File::from(descriptors.next().unwrap());
    let mut byte = [1];
    assert_eq!(returned_null.read(&mut byte).unwrap(), 0);
    assert_eq!(returned_zero.read(&mut byte).unwrap(), 1);
    assert_eq!(byte, [0]);
    assert!(descriptors.next().is_none());
}

#[tokio::test]
async fn host_and_guest_round_trip_large_payloads_with_a_descriptor() {
    let (host, guest) = StdUnixStream::pair().unwrap();
    set_small_send_buffer(&host);
    set_small_send_buffer(&guest);
    host.set_nonblocking(true).unwrap();
    let mut host = UnixControlStream::new(UnixStream::from_std(host).unwrap());
    let file = File::open("/dev/null").unwrap();
    let request = vec![0x5a; 512 * 1024];
    let expected_request = request.clone();
    let response = vec![0xa5; 384 * 1024];
    let expected_response = response.clone();

    let guest_task = tokio::task::spawn_blocking(move || {
        // Ownership moves into the guest transport; the original socket is not
        // used again in this task.
        let mut guest = unsafe { GuestControlStream::from_raw_fd(guest.into_raw_fd()) };
        establish_guest(&mut guest);
        let mut payload = vec![0; expected_request.len()];
        let frame = guest.receive_frame(&mut payload).unwrap();
        assert_eq!(frame.payload(), expected_request);
        assert_eq!(frame.descriptors().len(), 1);

        let (_, _, descriptors) = frame.into_parts();
        let (raw, count) = descriptors.into_raw_fds();
        guest
            .send_frame(
                header(MessageKind::PrepareReady, &response, count as u16),
                &response,
                &raw[..count],
            )
            .unwrap();
        for descriptor in raw.into_iter().take(count) {
            // `into_raw_fds` transferred ownership of these entries.
            drop(unsafe { OwnedFd::from_raw_fd(descriptor) });
        }
    });

    establish_host(&mut host).await;
    host.send_frame(
        header(MessageKind::PrepareRequest, &request, 1),
        &request,
        &[file.as_fd()],
    )
    .await
    .unwrap();
    let returned = host.receive_frame().await.unwrap();
    guest_task.await.unwrap();

    assert_eq!(returned.payload(), expected_response);
    assert_eq!(returned.descriptors().len(), 1);
    let (_, _, mut descriptors) = returned.into_parts();
    let mut returned_file = File::from(descriptors.pop().unwrap());
    let mut byte = [0xff];
    assert_eq!(returned_file.read(&mut byte).unwrap(), 0);
}

#[test]
fn guest_shape_errors_leave_the_stream_usable() {
    let (mut host, guest) = StdUnixStream::pair().unwrap();
    // Ownership moves into the guest transport; the original socket is not
    // used again below.
    let mut guest = unsafe { GuestControlStream::from_raw_fd(guest.into_raw_fd()) };
    establish_guest_with_peer(&mut host, &mut guest);

    assert!(guest
        .send_frame(
            header(MessageKind::PrepareRequest, b"", 0),
            b"mismatch",
            &[],
        )
        .is_err());
    assert!(guest.can_continue());
}

#[test]
fn guest_rejects_descriptors_attached_after_the_frame_anchor() {
    let (mut sender, guest) = StdUnixStream::pair().unwrap();
    let (transferred, peer) = StdUnixStream::pair().unwrap();
    // Ownership moves into the guest transport; the original socket is not
    // used again below.
    let mut guest = unsafe { GuestControlStream::from_raw_fd(guest.into_raw_fd()) };
    establish_guest_with_peer(&mut sender, &mut guest);
    let frame = header(MessageKind::PrepareRequest, b"x", 0).encode();
    sender.write_all(&frame).unwrap();
    assert_eq!(
        send_with_rights(&sender, b"x", &[transferred.as_raw_fd()]).unwrap(),
        1
    );
    drop(transferred);
    let mut payload = [0; 8];
    let error = match guest.receive_frame(&mut payload) {
        Ok(_) => panic!("guest accepted a descriptor after the frame anchor"),
        Err(error) => error,
    };

    assert_eq!(
        error,
        lasper_guest_runtime::transport::TransportError::UnexpectedDescriptors
    );
    assert!(!guest.can_continue());
    assert_peer_observes_close(peer);
}

#[test]
fn guest_rejects_truncated_rights_and_closes_every_received_descriptor() {
    let (mut sender, guest) = StdUnixStream::pair().unwrap();
    let (transferred, peers): (Vec<_>, Vec<_>) =
        (0..6).map(|_| StdUnixStream::pair().unwrap()).unzip();
    let raw = transferred
        .iter()
        .map(AsRawFd::as_raw_fd)
        .collect::<Vec<_>>();
    // Ownership moves into the guest transport; the original socket is not
    // used again below.
    let mut guest = unsafe { GuestControlStream::from_raw_fd(guest.into_raw_fd()) };
    establish_guest_with_peer(&mut sender, &mut guest);
    let frame = header(MessageKind::PrepareRequest, b"", 2).encode();
    assert_eq!(
        send_with_rights(&sender, &frame, &raw).unwrap(),
        frame.len()
    );
    drop(transferred);
    let mut payload = [];
    let error = match guest.receive_frame(&mut payload) {
        Ok(_) => panic!("guest accepted truncated descriptor rights"),
        Err(error) => error,
    };

    assert_eq!(
        error,
        lasper_guest_runtime::transport::TransportError::Truncated
    );
    assert!(!guest.can_continue());
    for peer in peers {
        assert_peer_observes_close(peer);
    }
}

#[test]
fn guest_count_mismatch_closes_the_received_descriptor() {
    let (mut sender, guest) = StdUnixStream::pair().unwrap();
    let (transferred, peer) = StdUnixStream::pair().unwrap();
    // Ownership moves into the guest transport; the original socket is not
    // used again below.
    let mut guest = unsafe { GuestControlStream::from_raw_fd(guest.into_raw_fd()) };
    establish_guest_with_peer(&mut sender, &mut guest);
    let frame = header(MessageKind::PrepareRequest, b"", 0).encode();
    assert_eq!(
        send_with_rights(&sender, &frame, &[transferred.as_raw_fd()]).unwrap(),
        frame.len()
    );
    drop(transferred);
    let mut payload = [];
    let error = match guest.receive_frame(&mut payload) {
        Ok(_) => panic!("guest accepted a descriptor count mismatch"),
        Err(error) => error,
    };

    assert_eq!(
        error,
        lasper_guest_runtime::transport::TransportError::DescriptorCountMismatch {
            declared: 0,
            actual: 1,
        }
    );
    assert!(!guest.can_continue());
    assert_peer_observes_close(peer);
}

#[test]
fn guest_invalid_header_closes_descriptors_received_with_it() {
    let (mut sender, guest) = StdUnixStream::pair().unwrap();
    let (transferred, peer) = StdUnixStream::pair().unwrap();
    // Ownership moves into the guest transport; the original socket is not
    // used again below.
    let mut guest = unsafe { GuestControlStream::from_raw_fd(guest.into_raw_fd()) };
    establish_guest_with_peer(&mut sender, &mut guest);
    let mut frame = header(MessageKind::PrepareRequest, b"", 1).encode();
    frame[0] ^= 0xff;
    assert_eq!(
        send_with_rights(&sender, &frame, &[transferred.as_raw_fd()]).unwrap(),
        frame.len()
    );
    drop(transferred);
    let mut payload = [];
    let error = match guest.receive_frame(&mut payload) {
        Ok(_) => panic!("guest accepted an invalid control header"),
        Err(error) => error,
    };

    assert_eq!(
        error,
        lasper_guest_runtime::transport::TransportError::InvalidHeader(
            lasper_guest_protocol::ControlFrameHeaderError::WrongMagic,
        )
    );
    assert!(!guest.can_continue());
    assert_peer_observes_close(peer);
}

#[test]
fn guest_payload_buffer_failure_closes_descriptors_received_with_the_header() {
    let (mut sender, guest) = StdUnixStream::pair().unwrap();
    let (transferred, peer) = StdUnixStream::pair().unwrap();
    let payload = b"payload-does-not-fit";
    // Ownership of the connected socket moves into the guest transport.
    let mut guest = unsafe { GuestControlStream::from_raw_fd(guest.into_raw_fd()) };
    establish_guest_with_peer(&mut sender, &mut guest);
    let mut frame = header(MessageKind::PrepareRequest, payload, 1)
        .encode()
        .to_vec();
    frame.extend_from_slice(payload);
    assert_eq!(
        send_with_rights(&sender, &frame, &[transferred.as_raw_fd()]).unwrap(),
        frame.len()
    );
    drop(transferred);
    let mut payload_buffer = [0; 4];
    let error = match guest.receive_frame(&mut payload_buffer) {
        Ok(_) => panic!("guest accepted a payload larger than its receive buffer"),
        Err(error) => error,
    };

    assert_eq!(
        error,
        lasper_guest_runtime::transport::TransportError::PayloadBufferTooSmall {
            required: payload.len(),
            available: payload_buffer.len(),
        }
    );
    assert!(!guest.can_continue());
    assert_peer_observes_close(peer);
}
