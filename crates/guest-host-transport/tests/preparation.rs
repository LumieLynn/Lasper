use support::control::{receive_guest_frame, send_guest_frame};
mod support;
#[path = "support/target.rs"]
mod target;

#[path = "support/invalid_hello.rs"]
mod invalid_hello;

use lasper_guest_host_transport::raw::{
    HandshakeError, PreparedWorker, WorkerHello, WorkerReadiness, WorkerReadinessError,
};
// The shared fixture's gated rechecks are used by tests/session/lease.rs.
#[allow(dead_code)]
#[path = "support/worker.rs"]
mod worker;
use lasper_guest_protocol::{
    BrokerId, CommandArgv, CompatibilityError, ControlFrameHeader, ControlLimits, ExecutionKind,
    FeatureSet, GuestProcess, HelloRequest, HelloResponse, MessageKind, PreparationId,
    PrepareReady, PrepareRequest, PrepareRequestFrame, TerminalSize, CONTROL_FRAME_HEADER_BYTES,
};
use lasper_guest_runtime::readiness::{GuestHelloError, GuestWorkerReadiness};
use lasper_guest_runtime::transport::GuestControlStream;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};

#[test]
fn unsupported_local_features_are_request_errors_before_host_readiness_checks() {
    let supported =
        lasper_guest_protocol::HelloOffer::local(ControlLimits::CURRENT).supported_features;
    let unsupported = FeatureSet::from_bits(!supported.bits());
    assert_ne!(unsupported, FeatureSet::EMPTY);
    assert!(matches!(
        WorkerHello::with_features(ControlLimits::CURRENT, unsupported),
        Err(WorkerReadinessError::InvalidRequest(
            lasper_guest_protocol::PayloadCodecError::InvalidRequest
        ))
    ));
}

#[test]
fn missing_optional_features_keep_the_negotiation_diagnostic() {
    let error = lasper_guest_host_transport::BrokerConnectError::Incompatible(
        CompatibilityError::MissingFeatures {
            required: FeatureSet::EXECUTION_TIMEOUT,
            available: FeatureSet::WORKER_PIDFD,
        },
    );
    assert!(error.to_string().contains("MissingFeatures"));
}
use support::handshake::{establish_guest, establish_host, raw_stream_pair};
use worker::TestWorker;

fn preparation_request(execution: ExecutionKind) -> PrepareRequestFrame<'static> {
    let size = match execution {
        ExecutionKind::Terminal => Some(TerminalSize::new(80, 24).unwrap()),
        ExecutionKind::Piped => None,
    };
    PrepareRequestFrame::new(
        17,
        BrokerId::new([0x5a; 16]),
        PrepareRequest::new(
            "alice",
            execution,
            GuestProcess::Command(CommandArgv::new(&["/usr/bin/id"]).unwrap()),
            lasper_guest_protocol::ProcessEnvironment::default(),
            size,
        )
        .unwrap(),
    )
}

fn assigned_pidfd() -> OwnedFd {
    // SAFETY: The test process remains live while its pidfd is acquired.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, std::process::id(), 0) };
    assert!(fd >= 0, "{}", io::Error::last_os_error());
    // SAFETY: pidfd_open returned a new descriptor with no other owner.
    unsafe { OwnedFd::from_raw_fd(fd as i32) }
}

fn identity_pair() -> (OwnedFd, OwnedFd) {
    let mut fds = [-1; 2];
    // SAFETY: socketpair writes two descriptors into this array on success.
    assert_eq!(
        unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        },
        0
    );
    // SAFETY: Each newly created endpoint moves into exactly one owner.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

fn run_isolated(test: &str) -> bool {
    const CHILD: &str = "LASPER_PREPARATION_TEST_CHILD";
    if std::env::var_os(CHILD).as_deref() == Some(std::ffi::OsStr::new(test)) {
        return false;
    }
    // Create the tested endpoints only after exec, with this one test selected.
    // Other tests can still fork in parallel, but cannot inherit those endpoints.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            test,
            "--nocapture",
            "--test-threads=1",
            "--format=pretty",
            "--color=never",
        ])
        .env(CHILD, test)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // libtest also exits successfully when the filter matches nothing or the
    // selected test is ignored. Require one executed test, not just exit zero.
    let passed = stdout
        .lines()
        .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;"));
    assert!(
        output.status.success() && passed,
        "isolated preparation test {test}: expected exactly one passed test, got {}\n{stdout}\n{stderr}",
        output.status
    );
    true
}

fn assert_identity_peer_closed(peer: &OwnedFd) {
    loop {
        let mut byte = 0u8;
        // SAFETY: The peer remains owned, and recv writes only to this byte.
        let received = unsafe {
            libc::recv(
                peer.as_raw_fd(),
                (&mut byte as *mut u8).cast(),
                1,
                libc::MSG_DONTWAIT,
            )
        };
        if received < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            panic!("identity peer receive: {error}");
        }
        assert_eq!(received, 0, "received identity endpoint was not closed");
        break;
    }
}

async fn readiness() -> WorkerReadiness {
    let hello = WorkerHello::new(ControlLimits::CURRENT).unwrap();
    let accepted = GuestWorkerReadiness::check()
        .unwrap()
        .accept_hello(
            lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
            hello.request(),
            BrokerId::new([0x5a; 16]),
            ControlLimits::CURRENT,
        )
        .unwrap();
    readiness_from_response(hello, accepted.response())
        .await
        .unwrap()
}

async fn readiness_from_response(
    hello: WorkerHello,
    response: HelloResponse,
) -> Result<WorkerReadiness, HandshakeError> {
    let (mut host, mut peer) = raw_stream_pair();
    let mut payload = [0; lasper_guest_protocol::HELLO_PAYLOAD_BUFFER_BYTES];
    let length = response.encode_payload(&mut payload).unwrap();
    let header = ControlFrameHeader::new_with_version(
        lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloResponse,
        1,
        length as u32,
        0,
    )
    .unwrap();
    peer.write_all(&header.encode()).unwrap();
    peer.write_all(&payload[..length]).unwrap();
    host.handshake(1, hello.request()).await?;
    Ok(hello
        .complete(&host)
        .expect("fixture negotiated worker readiness"))
}

#[tokio::test]
async fn worker_readiness_requires_its_own_hello_request() {
    for other in [
        HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap(),
        HelloRequest::new(
            FeatureSet::WORKER_PIDFD,
            ControlLimits::new(
                lasper_guest_protocol::V1_0_CAPACITIES.min_control_frame_bytes,
                2,
            )
            .unwrap(),
        )
        .unwrap(),
    ] {
        let hello = WorkerHello::new(ControlLimits::CURRENT).unwrap();
        let (mut host, mut peer) = raw_stream_pair();
        let (response, _) = HelloResponse::accept(
            other,
            lasper_guest_protocol::HelloOffer {
                supported_features: FeatureSet::WORKER_PIDFD,
                ..lasper_guest_protocol::HelloOffer::local(ControlLimits::CURRENT)
            },
            BrokerId::new([0x5a; 16]),
        )
        .unwrap();
        let mut payload = [0; lasper_guest_protocol::HELLO_PAYLOAD_BUFFER_BYTES];
        let length = response.encode_payload(&mut payload).unwrap();
        let header = ControlFrameHeader::new_with_version(
            lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
            MessageKind::HelloResponse,
            1,
            length as u32,
            0,
        )
        .unwrap();
        peer.write_all(&header.encode()).unwrap();
        peer.write_all(&payload[..length]).unwrap();
        host.handshake(1, other).await.unwrap();
        assert!(matches!(
            hello.complete(&host),
            Err(WorkerReadinessError::MismatchedHello)
        ));
    }
}

#[tokio::test]
async fn helper_hello_rejects_limits_that_cannot_hold_session_results() {
    let minimum = lasper_guest_protocol::V1_0_CAPACITIES.min_control_frame_bytes;
    assert!(matches!(
        WorkerHello::new(ControlLimits::new(minimum - 1, 3).unwrap()),
        Err(WorkerReadinessError::FrameLimit { required, available }) if required == minimum && available == minimum - 1
    ));
    let guest = GuestWorkerReadiness::check().unwrap();
    for (host_limit, guest_limit) in [(minimum - 1, minimum), (minimum, minimum - 1)] {
        let request = HelloRequest::new(
            FeatureSet::WORKER_PIDFD,
            ControlLimits::new(host_limit, 3).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            guest.accept_hello(lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 }, request, BrokerId::new([1; 16]), ControlLimits::new(guest_limit, 3).unwrap()),
            Err(GuestHelloError::Negotiation(CompatibilityError::InsufficientFrameCapacity { available, .. })) if available == minimum - 1
        ));
    }
    let host = WorkerHello::new(ControlLimits::CURRENT).unwrap();
    let response = invalid_hello::response(
        FeatureSet::WORKER_PIDFD,
        BrokerId::new([1; 16]),
        ControlLimits::new(minimum - 1, 3).unwrap(),
    );
    assert!(matches!(
        readiness_from_response(host, response).await,
        Err(HandshakeError::Negotiation(
            CompatibilityError::InsufficientFrameCapacity { .. }
        ))
    ));
}

#[tokio::test]
async fn prepare_ready_transfers_the_assigned_worker_for_both_execution_kinds() {
    for execution in [ExecutionKind::Terminal, ExecutionKind::Piped] {
        let (mut host, guest) = raw_stream_pair();
        let hello = WorkerHello::new(ControlLimits::CURRENT).unwrap();
        let request = preparation_request(execution);
        let ready = request
            .ready_frame(
                PrepareReady::new(PreparationId::new([8; 16]), request.broker_id(), 30_000)
                    .unwrap(),
            )
            .unwrap();
        let mut worker =
            TestWorker::spawn(request.request().clone(), ready.response().preparation_id());
        let identity = worker.identity.take().unwrap();
        let terminal = worker.terminal.take();
        let pidfd = worker.pidfd.try_clone().unwrap();
        let guest_task = tokio::task::spawn_blocking(move || {
            // SAFETY: The control endpoint transfers its sole ownership to
            // the blocking guest transport.
            let mut guest = unsafe { GuestControlStream::from_raw_fd(guest.into_raw_fd()) };
            let mut payload = [0; lasper_guest_protocol::HELLO_PAYLOAD_BUFFER_BYTES];
            let frame = receive_guest_frame(&guest, &mut payload).unwrap();
            let hello_request = HelloRequest::decode_payload(frame.payload()).unwrap();
            let version = frame.header().protocol_version();
            let accepted = GuestWorkerReadiness::check()
                .unwrap()
                .accept_hello(
                    version,
                    hello_request,
                    ready.response().broker_id(),
                    ControlLimits::CURRENT,
                )
                .unwrap();
            let hello_response = accepted.response();
            let length = hello_response.encode_payload(&mut payload).unwrap();
            send_guest_frame(
                &guest,
                ControlFrameHeader::new_with_version(
                    lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
                    MessageKind::HelloResponse,
                    1,
                    length as u32,
                    0,
                )
                .unwrap(),
                &payload[..length],
                &[],
            )
            .unwrap();
            guest.complete_protocol(accepted.negotiated()).unwrap();
            let mut payload = [0; 4096];
            let frame = receive_guest_frame(&guest, &mut payload).unwrap();
            assert_eq!(frame.header().kind(), MessageKind::PrepareRequest);
            let mut bytes = [0; 512];
            let length = accepted
                .negotiated()
                .encode_ready(ready, &mut bytes)
                .unwrap();
            let mut descriptors = Vec::new();
            if let Some(terminal) = &terminal {
                descriptors.push(terminal.as_raw_fd());
            }
            descriptors.extend([identity.as_raw_fd(), pidfd.as_raw_fd()]);
            let pipes = (execution == ExecutionKind::Piped).then(pipe_readers);
            if let Some(pipes) = &pipes {
                descriptors.extend(pipes.iter().map(AsRawFd::as_raw_fd));
            }
            send_guest_frame(
                &guest,
                ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES]).unwrap(),
                &bytes[CONTROL_FRAME_HEADER_BYTES..length],
                &descriptors,
            )
            .unwrap();
        });
        host.handshake(1, hello.request()).await.unwrap();
        let readiness = hello.complete(&host).unwrap();
        let mut bytes = [0; 4096];
        let length = readiness
            .protocol()
            .encode_prepare_frame(&request, &mut bytes)
            .unwrap();
        host.send_frame(
            ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES]).unwrap(),
            &bytes[CONTROL_FRAME_HEADER_BYTES..length],
            &[],
        )
        .await
        .unwrap();
        let response = host.receive_frame().await.unwrap();
        guest_task.await.unwrap();
        assert_eq!(
            response.descriptors().len(),
            ready.descriptor_count() as usize
        );
        let mut prepared = PreparedWorker::from_frame(
            target::current_target().await,
            &readiness,
            std::sync::Arc::new(request.clone()),
            response,
        )
        .unwrap();
        assert_eq!(prepared.ready(), ready);
        assert_eq!(
            prepared.terminal().is_some(),
            execution == ExecutionKind::Terminal
        );
        let first = prepared.observe().await.unwrap();
        let second = prepared.observe().await.unwrap();
        assert_ne!(first.reply().challenge(), second.reply().challenge());
        assert_eq!(prepared.process().pid(), worker.pid);
    }
}

#[tokio::test]
async fn invalid_preparation_resources_close_the_received_identity_endpoint() {
    if run_isolated("invalid_preparation_resources_close_the_received_identity_endpoint") {
        return;
    }
    for failure in [
        "request",
        "broker",
        "missing-pidfd",
        "not-pidfd",
        "swapped",
        "not-pty",
    ] {
        let (mut host, guest) = raw_stream_pair();
        let execution = if failure == "not-pty" {
            ExecutionKind::Terminal
        } else {
            ExecutionKind::Piped
        };
        let request = preparation_request(execution);
        let other = PrepareRequestFrame::new(
            if failure == "request" {
                18
            } else {
                request.request_id()
            },
            if failure == "broker" {
                BrokerId::new([9; 16])
            } else {
                request.broker_id()
            },
            request.request().clone(),
        );
        let ready = other
            .ready_frame(
                PrepareReady::new(PreparationId::new([8; 16]), other.broker_id(), 30_000).unwrap(),
            )
            .unwrap();
        let (identity_host, identity_guest) = identity_pair();
        let guest_task = tokio::task::spawn_blocking(move || {
            // SAFETY: The connected control endpoint moves into this transport.
            let mut guest = unsafe { GuestControlStream::from_raw_fd(guest.into_raw_fd()) };
            let protocol = establish_guest(&mut guest);
            let pidfd = if failure == "not-pidfd" {
                File::open("/dev/null").unwrap().into()
            } else {
                assigned_pidfd()
            };
            let terminal = File::open("/dev/null").unwrap();
            let mut descriptors = Vec::new();
            if execution == ExecutionKind::Terminal {
                descriptors.push(terminal.as_raw_fd());
            }
            descriptors.push(identity_host.as_raw_fd());
            if failure != "missing-pidfd" {
                descriptors.push(pidfd.as_raw_fd());
            }
            if failure == "swapped" {
                descriptors.swap(0, 1);
            }
            let mut bytes = [0; 512];
            let length = protocol.encode_ready(ready, &mut bytes).unwrap();
            let header = ControlFrameHeader::new_with_version(
                lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
                MessageKind::PrepareReady,
                ready.request_id(),
                (length - CONTROL_FRAME_HEADER_BYTES) as u32,
                descriptors.len() as u16,
            )
            .unwrap();
            send_guest_frame(
                &guest,
                header,
                &bytes[CONTROL_FRAME_HEADER_BYTES..length],
                &descriptors,
            )
            .unwrap();
        });
        establish_host(&mut host).await;
        let response = host.receive_frame().await.unwrap();
        guest_task.await.unwrap();
        let error = match PreparedWorker::from_frame(
            target::current_target().await,
            &readiness().await,
            std::sync::Arc::new(request.clone()),
            response,
        ) {
            Ok(_) => panic!("accepted invalid preparation: {failure}"),
            Err(error) => error,
        };
        assert!(!error.to_string().is_empty(), "{failure}");
        assert_identity_peer_closed(&identity_guest);
    }
}

async fn deliver_ready(
    ready: lasper_guest_protocol::PrepareReadyFrame,
    mut descriptors: Vec<OwnedFd>,
) -> lasper_guest_host_transport::raw::ReceivedControlFrame {
    if ready.descriptor_count() == 4 && descriptors.len() == 2 {
        descriptors.extend(pipe_readers());
    }
    let (mut host, guest) = raw_stream_pair();
    let sender = tokio::task::spawn_blocking(move || {
        // SAFETY: The connected control endpoint transfers its ownership.
        let mut guest = unsafe { GuestControlStream::from_raw_fd(guest.into_raw_fd()) };
        let protocol = establish_guest(&mut guest);
        let mut bytes = [0; 512];
        let length = protocol.encode_ready(ready, &mut bytes).unwrap();
        let descriptors = descriptors
            .iter()
            .map(AsRawFd::as_raw_fd)
            .collect::<Vec<_>>();
        send_guest_frame(
            &guest,
            ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES]).unwrap(),
            &bytes[CONTROL_FRAME_HEADER_BYTES..length],
            &descriptors,
        )
        .unwrap();
    });
    establish_host(&mut host).await;
    let frame = host.receive_frame().await.unwrap();
    sender.await.unwrap();
    frame
}

fn pipe_readers() -> [OwnedFd; 2] {
    std::array::from_fn(|_| {
        let mut descriptors = [-1; 2];
        // SAFETY: pipe2 initializes two result slots on success; each endpoint
        // immediately acquires exactly one owner.
        assert_eq!(
            unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let (read, write) = unsafe {
            (
                OwnedFd::from_raw_fd(descriptors[0]),
                OwnedFd::from_raw_fd(descriptors[1]),
            )
        };
        drop(write);
        read
    })
}

#[tokio::test]
async fn another_workers_pty_cannot_be_combined_with_valid_identity_resources() {
    let readiness = readiness().await;
    let request = readiness
        .prepare(
            17,
            preparation_request(ExecutionKind::Terminal)
                .request()
                .clone(),
        )
        .unwrap();
    let ready = request
        .ready_frame(
            PrepareReady::new(PreparationId::new([8; 16]), request.broker_id(), 30_000).unwrap(),
        )
        .unwrap();
    let mut assigned =
        TestWorker::spawn(request.request().clone(), ready.response().preparation_id());
    let mut other = TestWorker::spawn(request.request().clone(), PreparationId::new([9; 16]));
    let descriptors = vec![
        other.terminal.take().unwrap(),
        assigned.identity.take().unwrap(),
        assigned.pidfd.try_clone().unwrap(),
    ];
    let frame = deliver_ready(ready, descriptors).await;
    let error = match PreparedWorker::from_frame(
        target::current_target().await,
        &readiness,
        std::sync::Arc::new(request.clone()),
        frame,
    ) {
        Ok(_) => panic!("accepted another worker's PTY"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("does not belong"));
}

#[tokio::test]
async fn terminal_assignment_is_rechecked_before_accepting_another_observation() {
    let readiness = readiness().await;
    let request = readiness
        .prepare(
            17,
            preparation_request(ExecutionKind::Terminal)
                .request()
                .clone(),
        )
        .unwrap();
    let ready = request
        .ready_frame(
            PrepareReady::new(PreparationId::new([8; 16]), request.broker_id(), 30_000).unwrap(),
        )
        .unwrap();
    let mut worker = TestWorker::releasing_terminal_on_recheck(
        request.request().clone(),
        ready.response().preparation_id(),
    );
    let terminal = worker.terminal.take().unwrap();
    let descriptors = vec![
        terminal,
        worker.identity.take().unwrap(),
        worker.pidfd.try_clone().unwrap(),
    ];
    let frame = deliver_ready(ready, descriptors).await;
    let mut prepared = PreparedWorker::from_frame(
        target::current_target().await,
        &readiness,
        std::sync::Arc::new(request.clone()),
        frame,
    )
    .unwrap();
    prepared.observe().await.unwrap();
    assert_eq!(prepared.process().pid(), worker.pid);
    let error = prepared.observe().await.unwrap_err();
    assert!(error.to_string().contains("PTY"), "{error}");
    prepared.process().recheck().unwrap();
    assert!(!prepared.can_continue());
}

#[tokio::test]
async fn hello_rejects_missing_worker_pidfd_before_any_prepare() {
    let hello = WorkerHello::new(ControlLimits::CURRENT).unwrap();
    assert!(hello
        .request()
        .required_features()
        .contains(FeatureSet::WORKER_PIDFD));
    let result = readiness_from_response(
        hello,
        invalid_hello::response(
            FeatureSet::EMPTY,
            BrokerId::new([0x5a; 16]),
            ControlLimits::CURRENT,
        ),
    )
    .await;
    let error = match result {
        Ok(_) => panic!("accepted a broker without worker pidfd support"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        HandshakeError::Negotiation(CompatibilityError::MissingFeatures { .. })
    ));
}

#[tokio::test]
async fn guest_rejects_legacy_host_during_hello_without_a_prepare_ready() {
    for descriptors in [2, 3] {
        let (mut host, guest) = raw_stream_pair();
        let guest_task = tokio::task::spawn_blocking(move || {
            // SAFETY: This connected endpoint transfers its ownership.
            let guest = unsafe { GuestControlStream::from_raw_fd(guest.into_raw_fd()) };
            let mut payload = [0; 256];
            let frame = receive_guest_frame(&guest, &mut payload).unwrap();
            let request = HelloRequest::decode_payload(frame.payload()).unwrap();
            let result = GuestWorkerReadiness::check().unwrap().accept_hello(
                frame.header().protocol_version(),
                request,
                BrokerId::new([0x5a; 16]),
                ControlLimits::CURRENT,
            );
            assert!(matches!(
                result,
                Err(GuestHelloError::Negotiation(
                    CompatibilityError::MissingFeatures { .. }
                ))
            ));
            // Reject before responding or creating any worker resources.
        });
        let legacy_request = HelloRequest::from_offer(
            lasper_guest_protocol::HelloOffer {
                supported_features: FeatureSet::EMPTY,
                ..lasper_guest_protocol::HelloOffer::local(
                    ControlLimits::new(lasper_guest_protocol::MAX_CONTROL_FRAME_BYTES, descriptors)
                        .unwrap(),
                )
            },
            FeatureSet::EMPTY,
        )
        .unwrap();
        let mut payload = [0; 256];
        let length = legacy_request.encode_payload(&mut payload).unwrap();
        host.send_frame(
            ControlFrameHeader::new_with_version(
                lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
                MessageKind::HelloRequest,
                1,
                length as u32,
                0,
            )
            .unwrap(),
            &payload[..length],
            &[],
        )
        .await
        .unwrap();
        let error = host.receive_frame().await.unwrap_err();
        guest_task.await.unwrap();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert!(!host.can_continue());
    }
}

#[tokio::test]
async fn insufficient_descriptor_limit_is_rejected_before_sending_prepare() {
    let readiness = readiness_from_response(
        WorkerHello::new(ControlLimits::CURRENT).unwrap(),
        support::handshake::response_for(
            HelloRequest::new(FeatureSet::WORKER_PIDFD, ControlLimits::CURRENT).unwrap(),
            FeatureSet::WORKER_PIDFD,
            BrokerId::new([0x5a; 16]),
            ControlLimits::new(lasper_guest_protocol::MAX_CONTROL_FRAME_BYTES, 2).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let request = preparation_request(ExecutionKind::Terminal);
    assert!(matches!(
        readiness.prepare(17, request.request().clone()),
        Err(WorkerReadinessError::DescriptorLimit {
            required: 3,
            available: 2
        })
    ));
    let request = preparation_request(ExecutionKind::Piped);
    assert!(matches!(
        readiness.prepare(17, request.request().clone()),
        Err(WorkerReadinessError::DescriptorLimit {
            required: 4,
            available: 2
        })
    ));
}

#[tokio::test]
async fn target_leader_exit_invalidates_preparation_without_rebinding_to_a_replacement() {
    let readiness = readiness().await;
    let request = preparation_request(ExecutionKind::Piped);
    let ready = request
        .ready_frame(
            PrepareReady::new(PreparationId::new([8; 16]), request.broker_id(), 30_000).unwrap(),
        )
        .unwrap();
    let leader = TestWorker::spawn(request.request().clone(), PreparationId::new([9; 16]));
    let target = target::target_for(leader.pid as u32).await;
    let mut worker =
        TestWorker::spawn(request.request().clone(), ready.response().preparation_id());
    let descriptors = vec![
        worker.identity.take().unwrap(),
        worker.pidfd.try_clone().unwrap(),
    ];
    let frame = deliver_ready(ready, descriptors).await;
    let mut prepared = PreparedWorker::from_frame(
        target.clone(),
        &readiness,
        std::sync::Arc::new(request.clone()),
        frame,
    )
    .unwrap();
    prepared.observe().await.unwrap();

    // SAFETY: TestWorker exclusively owns waitpid for this unreaped child.
    assert_eq!(unsafe { libc::kill(leader.pid, libc::SIGKILL) }, 0);
    let mut descriptor = libc::pollfd {
        fd: leader.pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: The pidfd remains owned by the fixture throughout this wait.
    assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 2000) }, 1);
    let _replacement = TestWorker::spawn(request.request().clone(), PreparationId::new([10; 16]));
    assert_eq!(
        target.recheck().unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    let error = prepared.observe().await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert!(!prepared.can_continue());
    // The identity worker itself is still alive; the target's exit invalidated
    // the preparation rather than the sender or terminal disappearing.
    prepared.process().recheck().unwrap();
}

#[tokio::test]
async fn target_rejects_a_unit_cgroup_that_does_not_contain_its_leader() {
    let error = match lasper_guest_host_transport::MachineTarget::resolve(
        &target::FixtureSource {
            leader: std::process::id(),
            root: "/not-this-machines-unit".into(),
        },
        "fixture",
    )
    .await
    {
        Ok(_) => panic!("accepted an unrelated unit cgroup"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("target machine cgroup subtree"),
        "{error}"
    );
}

#[tokio::test]
async fn target_exit_while_waiting_for_identity_rejects_the_otherwise_valid_reply() {
    use lasper_guest_protocol::{GuestIdentityReply, MAX_IDENTITY_MESSAGE_BYTES};
    use lasper_guest_runtime::identity::GuestIdentityChannel;
    use std::sync::mpsc;
    use std::time::Duration;

    let readiness = readiness().await;
    let request = preparation_request(ExecutionKind::Piped);
    let ready = request
        .ready_frame(
            PrepareReady::new(PreparationId::new([8; 16]), request.broker_id(), 30_000).unwrap(),
        )
        .unwrap();
    let leader = TestWorker::spawn(request.request().clone(), PreparationId::new([9; 16]));
    let target = target::target_for(leader.pid as u32).await;
    let (host, guest) = identity_pair();
    let frame = deliver_ready(ready, vec![host, assigned_pidfd()]).await;
    let mut prepared = PreparedWorker::from_frame(
        target,
        &readiness,
        std::sync::Arc::new(request.clone()),
        frame,
    )
    .unwrap();
    let (waiting, challenge_received) = tokio::sync::oneshot::channel();
    let (release, released) = mpsc::channel();
    let worker_request = request.request().clone();
    let profile = readiness.profile();
    let worker = tokio::task::spawn_blocking(move || {
        // SAFETY: Transfer the sole ownership of a connected blocking endpoint.
        let mut guest = unsafe {
            GuestIdentityChannel::from_raw_fd(
                guest.into_raw_fd(),
                ready.response().preparation_id(),
                profile,
            )
        };
        let mut bytes = [0; MAX_IDENTITY_MESSAGE_BYTES];
        let challenge = guest.receive_challenge(&mut bytes).unwrap();
        waiting.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(5)).unwrap();
        let reply = GuestIdentityReply::new(
            challenge,
            &worker_request,
            "alice",
            unsafe { libc::getuid() },
            unsafe { libc::getgid() },
        )
        .unwrap();
        guest.send_reply(&reply, &mut bytes).unwrap();
    });
    let mut observation = Box::pin(prepared.observe());
    tokio::select! {
        result = &mut observation => panic!("observation completed before the reply: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(5), challenge_received) => result.unwrap().unwrap(),
    }
    // SAFETY: The unreaped child and its pidfd remain owned by the fixture.
    assert_eq!(unsafe { libc::kill(leader.pid, libc::SIGKILL) }, 0);
    let mut descriptor = libc::pollfd {
        fd: leader.pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 2000) }, 1);
    release.send(()).unwrap();
    let error = tokio::time::timeout(Duration::from_secs(5), observation)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert!(!prepared.can_continue());
    worker.await.unwrap();
}

#[tokio::test]
async fn exited_target_rejects_ready_and_closes_received_resources() {
    if run_isolated("exited_target_rejects_ready_and_closes_received_resources") {
        return;
    }
    let readiness = readiness().await;
    let request = preparation_request(ExecutionKind::Piped);
    let ready = request
        .ready_frame(
            PrepareReady::new(PreparationId::new([8; 16]), request.broker_id(), 30_000).unwrap(),
        )
        .unwrap();
    let leader = TestWorker::spawn(request.request().clone(), PreparationId::new([9; 16]));
    let target = target::target_for(leader.pid as u32).await;
    // SAFETY: The fixture keeps this child unreaped and its pidfd open.
    assert_eq!(unsafe { libc::kill(leader.pid, libc::SIGKILL) }, 0);
    let mut descriptor = libc::pollfd {
        fd: leader.pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 2000) }, 1);
    let (identity, peer) = identity_pair();
    let frame = deliver_ready(ready, vec![identity, assigned_pidfd()]).await;
    let error = match PreparedWorker::from_frame(
        target,
        &readiness,
        std::sync::Arc::new(request.clone()),
        frame,
    ) {
        Ok(_) => panic!("accepted resources for an exited target"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert_identity_peer_closed(&peer);
}
