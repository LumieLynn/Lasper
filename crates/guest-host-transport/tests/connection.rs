use lasper_guest_host_transport::{BrokerConnectError, BrokerConnection, MachineTarget};
use lasper_guest_protocol::{
    BrokerId, ControlFrameHeader, ControlLimits, FeatureSet, HelloRequest, HelloResponse,
    MessageKind, CONTROL_FRAME_HEADER_BYTES, HELLO_PAYLOAD_BUFFER_BYTES,
};
use std::future::Future;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

#[path = "support/ancillary.rs"]
mod ancillary;
#[path = "support/listener.rs"]
mod listener_fixture;
#[path = "support/target.rs"]
mod target;

const BROKER: BrokerId = BrokerId::new([0x45; 16]);

fn listener() -> (tempfile::TempDir, UnixListener) {
    let directory = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(directory.path().join("broker.sock")).unwrap();
    (directory, listener)
}

async fn connect(
    target: Arc<MachineTarget>,
    path: &Path,
) -> Result<BrokerConnection, BrokerConnectError> {
    BrokerConnection::connect(
        target,
        path.to_str().unwrap(),
        ControlLimits::CURRENT,
        Instant::now() + Duration::from_secs(2),
    )
    .await
}

fn receive_hello(stream: &mut UnixStream) -> ControlFrameHeader {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut header = [0; CONTROL_FRAME_HEADER_BYTES];
    stream.read_exact(&mut header).unwrap();
    let header = ControlFrameHeader::decode(&header).unwrap();
    assert_eq!(header.kind(), MessageKind::HelloRequest);
    let mut payload = vec![0; header.payload_bytes() as usize];
    stream.read_exact(&mut payload).unwrap();
    let request = HelloRequest::decode_payload(&payload).unwrap();
    assert!(request
        .required_features()
        .contains(FeatureSet::WORKER_PIDFD));
    header
}

fn hello_response(request_id: u64) -> Vec<u8> {
    let response =
        HelloResponse::new(FeatureSet::WORKER_PIDFD, BROKER, ControlLimits::CURRENT).unwrap();
    let mut bytes = [0; HELLO_PAYLOAD_BUFFER_BYTES];
    let length = response.encode_payload(&mut bytes).unwrap();
    let header =
        ControlFrameHeader::new(MessageKind::HelloResponse, request_id, length as u32, 0).unwrap();
    let mut frame = header.encode().to_vec();
    frame.extend_from_slice(&bytes[..length]);
    frame
}

fn respond(stream: &mut UnixStream, request_id: u64) {
    stream.write_all(&hello_response(request_id)).unwrap();
}

#[tokio::test]
async fn connection_retains_the_machine_broker_and_installed_limits() {
    let target = target::current_target().await;
    let (directory, listener) = listener();
    let (release, released) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = receive_hello(&mut stream);
        respond(&mut stream, request.request_id());
        released.recv_timeout(Duration::from_secs(3)).unwrap();
    });
    let connection = connect(Arc::clone(&target), &directory.path().join("broker.sock"))
        .await
        .unwrap();
    assert!(Arc::ptr_eq(connection.target(), &target));
    assert_eq!(connection.readiness().broker_id(), BROKER);
    assert_eq!(
        connection.readiness().limits().get(),
        ControlLimits::CURRENT
    );
    connection.recheck().unwrap();
    drop(connection);
    release.send(()).unwrap();
    server.join().unwrap();
}

#[tokio::test]
async fn invalid_endpoint_is_rejected_without_waiting_for_startup() {
    let target = target::current_target().await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("regular");
    std::fs::write(&path, "not a socket").unwrap();
    assert!(
        matches!(connect(target, &path).await, Err(BrokerConnectError::Endpoint(error)) if error.kind() == std::io::ErrorKind::InvalidData)
    );
}

#[tokio::test]
async fn absent_or_permanently_refused_endpoint_expires_at_the_original_deadline() {
    for refused in [false, true] {
        let target = target::current_target().await;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("broker.sock");
        if refused {
            drop(UnixListener::bind(&path).unwrap());
        }
        let result = BrokerConnection::connect(
            target,
            path.to_str().unwrap(),
            ControlLimits::CURRENT,
            Instant::now() + Duration::from_millis(70),
        )
        .await;
        assert!(matches!(result, Err(BrokerConnectError::Deadline)));
    }
}

#[tokio::test]
async fn endpoint_created_after_the_first_connect_poll_is_discovered() {
    let target = target::current_target().await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("not-created-yet/broker.sock");
    let attempt = connect(target, &path);
    tokio::pin!(attempt);
    // No server or directory exists until this poll has entered startup backoff.
    assert!(
        std::future::poll_fn(|context| std::task::Poll::Ready(attempt.as_mut().poll(context)))
            .await
            .is_pending()
    );
    std::fs::create_dir(path.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&path).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = receive_hello(&mut stream);
        respond(&mut stream, request.request_id());
    });
    assert_eq!(attempt.await.unwrap().readiness().broker_id(), BROKER);
    server.join().unwrap();
}

#[tokio::test]
async fn bound_endpoint_waits_for_listen_before_exchanging_hello() {
    let target = target::current_target().await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("broker.sock");
    let bound = listener_fixture::BoundSocket::bind(&path);
    assert!(path.exists());
    let attempt = connect(target, &path);
    tokio::pin!(attempt);
    // bind has finished, but listen cannot run before this refused attempt.
    assert!(
        std::future::poll_fn(|context| std::task::Poll::Ready(attempt.as_mut().poll(context)))
            .await
            .is_pending()
    );
    let listener = bound.listen();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = receive_hello(&mut stream);
        respond(&mut stream, request.request_id());
    });
    assert_eq!(attempt.await.unwrap().readiness().broker_id(), BROKER);
    server.join().unwrap();
}

#[tokio::test]
async fn refused_stale_inode_is_resolved_again_after_broker_replacement() {
    let target = target::current_target().await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("broker.sock");
    drop(UnixListener::bind(&path).unwrap());
    let attempt = connect(target, &path);
    tokio::pin!(attempt);
    assert!(
        std::future::poll_fn(|context| std::task::Poll::Ready(attempt.as_mut().poll(context)))
            .await
            .is_pending()
    );
    std::fs::remove_file(&path).unwrap();
    let listener = UnixListener::bind(&path).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = receive_hello(&mut stream);
        respond(&mut stream, request.request_id());
    });
    assert_eq!(attempt.await.unwrap().readiness().broker_id(), BROKER);
    server.join().unwrap();
}

#[tokio::test]
async fn retry_rejects_a_socket_path_replaced_with_a_regular_file() {
    let target = target::current_target().await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("broker.sock");
    drop(UnixListener::bind(&path).unwrap());
    let attempt = connect(target, &path);
    tokio::pin!(attempt);
    assert!(
        std::future::poll_fn(|context| std::task::Poll::Ready(attempt.as_mut().poll(context)))
            .await
            .is_pending()
    );
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, "replacement is not a socket").unwrap();
    assert!(
        matches!(attempt.await, Err(BrokerConnectError::Endpoint(error)) if error.kind() == std::io::ErrorKind::InvalidData)
    );
}

#[tokio::test]
async fn machine_exit_ends_endpoint_waiting_instead_of_rebinding_the_target() {
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let target = target::target_for(child.id()).await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("broker.sock");
    let attempt = connect(target, &path);
    tokio::pin!(attempt);
    assert!(
        std::future::poll_fn(|context| std::task::Poll::Ready(attempt.as_mut().poll(context)))
            .await
            .is_pending()
    );
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(matches!(attempt.await, Err(BrokerConnectError::Target(_))));
}

#[tokio::test]
async fn hello_response_must_match_this_request() {
    let (directory, listener) = listener();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = receive_hello(&mut stream);
        respond(&mut stream, request.request_id() + 1);
    });
    let result = connect(
        target::current_target().await,
        &directory.path().join("broker.sock"),
    )
    .await;
    assert!(
        matches!(result, Err(BrokerConnectError::Hello(error)) if error.kind() == std::io::ErrorKind::InvalidData)
    );
    server.join().unwrap();
}

#[tokio::test]
async fn malformed_hello_drops_the_control_connection() {
    let (directory, listener) = listener();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = receive_hello(&mut stream);
        let header =
            ControlFrameHeader::new(MessageKind::HelloResponse, request.request_id(), 1, 0)
                .unwrap();
        stream.write_all(&header.encode()).unwrap();
        stream.write_all(b"{").unwrap();
        let mut byte = [0];
        assert_eq!(stream.read(&mut byte).unwrap(), 0);
    });
    assert!(matches!(
        connect(
            target::current_target().await,
            &directory.path().join("broker.sock")
        )
        .await,
        Err(BrokerConnectError::Hello(_))
    ));
    server.join().unwrap();
}

#[tokio::test]
async fn hello_rejects_smuggled_descriptors_at_the_header_and_payload() {
    for offset in [0, CONTROL_FRAME_HEADER_BYTES] {
        let (directory, listener) = listener();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = receive_hello(&mut stream);
            let frame = hello_response(request.request_id());
            let (transferred, mut peer) = UnixStream::pair().unwrap();
            stream.write_all(&frame[..offset]).unwrap();
            let sent =
                ancillary::send_with_rights(&stream, &frame[offset..], &[transferred.as_raw_fd()])
                    .unwrap();
            assert_eq!(sent, frame.len() - offset);
            drop(transferred);
            peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut byte = [0];
            assert_eq!(peer.read(&mut byte).unwrap(), 0, "received FD leaked");
            assert_eq!(stream.read(&mut byte).unwrap(), 0);
        });
        assert!(
            matches!(
                connect(
                    target::current_target().await,
                    &directory.path().join("broker.sock"),
                )
                .await,
                Err(BrokerConnectError::Hello(error)) if error.kind() == std::io::ErrorKind::InvalidData
            ),
            "accepted a descriptor at byte {offset} of HelloResponse",
        );
        server.join().unwrap();
    }
}

#[tokio::test]
async fn truncated_hello_header_and_payload_close_the_connection() {
    let frame_bytes = hello_response(1).len();
    for offset in [
        0,
        1,
        CONTROL_FRAME_HEADER_BYTES - 1,
        CONTROL_FRAME_HEADER_BYTES,
        CONTROL_FRAME_HEADER_BYTES + 1,
        frame_bytes - 1,
    ] {
        let (directory, listener) = listener();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = receive_hello(&mut stream);
            let frame = hello_response(request.request_id());
            stream.write_all(&frame[..offset]).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).unwrap(), 0);
        });
        assert!(
            matches!(
                connect(
                    target::current_target().await,
                    &directory.path().join("broker.sock"),
                )
                .await,
                Err(BrokerConnectError::Hello(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof
            ),
            "did not report truncated Hello at byte {offset}",
        );
        server.join().unwrap();
    }
}

#[tokio::test]
async fn stalled_hello_uses_the_callers_deadline_and_closes_the_socket() {
    let (directory, listener) = listener();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        receive_hello(&mut stream);
        let mut byte = [0];
        assert_eq!(stream.read(&mut byte).unwrap(), 0);
    });
    let target = target::current_target().await;
    let deadline = Instant::now() + Duration::from_millis(150);
    let result = BrokerConnection::connect(
        target,
        directory.path().join("broker.sock").to_str().unwrap(),
        ControlLimits::CURRENT,
        deadline,
    )
    .await;
    assert!(matches!(result, Err(BrokerConnectError::Deadline)));
    server.join().unwrap();
}

#[tokio::test]
async fn stalled_partial_hello_releases_rights_when_the_deadline_cancels_receiving() {
    let (directory, listener) = listener();
    let (sent, received) = tokio::sync::oneshot::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = receive_hello(&mut stream);
        let frame = hello_response(request.request_id());
        let (transferred, mut peer) = UnixStream::pair().unwrap();
        assert_eq!(
            ancillary::send_with_rights(&stream, &frame[..1], &[transferred.as_raw_fd()]).unwrap(),
            1,
        );
        drop(transferred);
        sent.send(()).unwrap();
        // Keep the control connection open: only the host's cancellation can
        // release the transferred endpoint while the header is incomplete.
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut byte = [0];
        assert_eq!(peer.read(&mut byte).unwrap(), 0, "received FD leaked");
        assert_eq!(stream.read(&mut byte).unwrap(), 0);
    });
    let target = target::current_target().await;
    let path = directory.path().join("broker.sock");
    let attempt = tokio::spawn(async move {
        BrokerConnection::connect(
            target,
            path.to_str().unwrap(),
            ControlLimits::CURRENT,
            Instant::now() + Duration::from_secs(1),
        )
        .await
    });
    received.await.unwrap();
    assert!(matches!(
        attempt.await.unwrap(),
        Err(BrokerConnectError::Deadline)
    ));
    server.join().unwrap();
}

#[tokio::test]
async fn full_unix_accept_queue_expires_without_leaving_a_pending_connection() {
    let (directory, listener) = listener();
    // SAFETY: listen changes only this fixture's socket backlog.
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
    let _queued = UnixStream::connect(directory.path().join("broker.sock")).unwrap();
    let target = target::current_target().await;
    let result = BrokerConnection::connect(
        target,
        directory.path().join("broker.sock").to_str().unwrap(),
        ControlLimits::CURRENT,
        Instant::now() + Duration::from_millis(70),
    )
    .await;
    assert!(matches!(result, Err(BrokerConnectError::Deadline)));
    listener.set_nonblocking(true).unwrap();
    let _accepted = listener.accept().unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn machine_exit_during_hello_invalidates_the_connection() {
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let target = target::target_for(child.id()).await;
    let (directory, listener) = listener();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = receive_hello(&mut stream);
        child.kill().unwrap();
        child.wait().unwrap();
        respond(&mut stream, request.request_id());
    });
    assert!(matches!(
        connect(target, &directory.path().join("broker.sock")).await,
        Err(BrokerConnectError::InvalidPeer(_))
    ));
    server.join().unwrap();
}

#[tokio::test]
async fn full_queue_retry_keeps_the_pinned_socket_when_its_path_is_replaced() {
    let (directory, listener) = listener();
    // SAFETY: this changes only the fixture's listen backlog.
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
    let _queued = UnixStream::connect(directory.path().join("broker.sock")).unwrap();
    let target = target::current_target().await;
    let path = directory.path().join("broker.sock");
    let attempt = connect(target, &path);
    tokio::pin!(attempt);
    // One explicit poll encounters EAGAIN before the server drains the queue.
    assert!(
        std::future::poll_fn(|context| std::task::Poll::Ready(attempt.as_mut().poll(context)))
            .await
            .is_pending()
    );
    std::fs::remove_file(&path).unwrap();
    let replacement = UnixListener::bind(&path).unwrap();
    replacement.set_nonblocking(true).unwrap();
    // The replacement is already listening when the original queue opens.
    // A retry by pathname would send Hello to the wrong server.
    let _accepted = listener.accept().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = receive_hello(&mut stream);
        respond(&mut stream, request.request_id());
    });
    assert_eq!(attempt.await.unwrap().readiness().broker_id(), BROKER);
    server.join().unwrap();
    assert_eq!(
        replacement.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
    );
}

#[tokio::test]
async fn cancelled_hello_drops_its_connection_without_a_background_task() {
    let (directory, listener) = listener();
    let target = target::current_target().await;
    let path = directory.path().join("broker.sock");
    let (received, ready) = tokio::sync::oneshot::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        receive_hello(&mut stream);
        received.send(()).unwrap();
        let mut byte = [0];
        assert_eq!(stream.read(&mut byte).unwrap(), 0);
    });
    let attempt = tokio::spawn(async move { connect(target, &path).await });
    ready.await.unwrap();
    attempt.abort();
    assert!(matches!(attempt.await, Err(error) if error.is_cancelled()));
    server.join().unwrap();
}

#[tokio::test]
async fn incompatible_or_oversized_hello_is_rejected_before_reading_its_payload() {
    for oversized in [false, true] {
        let (directory, listener) = listener();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = receive_hello(&mut stream);
            let mut header = ControlFrameHeader::new(
                MessageKind::HelloResponse,
                request.request_id(),
                if oversized {
                    HELLO_PAYLOAD_BUFFER_BYTES as u32 + 1
                } else {
                    0
                },
                0,
            )
            .unwrap()
            .encode();
            if !oversized {
                header[14..16].copy_from_slice(
                    &(lasper_guest_protocol::ProtocolVersion::CURRENT.minor + 1).to_le_bytes(),
                );
            }
            stream.write_all(&header).unwrap();
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).unwrap(), 0);
        });
        assert!(matches!(
            connect(
                target::current_target().await,
                &directory.path().join("broker.sock")
            )
            .await,
            Err(BrokerConnectError::Hello(_))
        ));
        server.join().unwrap();
    }
}
