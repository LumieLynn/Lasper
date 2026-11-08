use support::control::{receive_guest_frame, send_guest_frame};
mod support;
#[path = "support/target.rs"]
mod target;

use lasper_guest_host_transport::{
    PreparedWorker, WorkerHello, WorkerReadiness, WorkerReadinessError,
};
#[path = "support/worker.rs"]
mod worker;
use lasper_guest_protocol::{
    BrokerId, CommandArgv, ControlFrameHeader, ControlLimits, ExecutionKind, FeatureSet,
    GuestProcess, HelloNegotiationError, HelloRequest, HelloResponse, MessageKind, PreparationId,
    PrepareReady, PrepareRequest, PrepareRequestFrame, ProjectionRequests, TerminalEnvironment,
    TerminalSize, CONTROL_FRAME_HEADER_BYTES,
};
use lasper_guest_runtime::readiness::{GuestHelloError, GuestWorkerReadiness};
use lasper_guest_runtime::transport::GuestControlStream;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use support::control::{establish_guest, establish_host, raw_stream_pair};
use worker::TestWorker;

fn preparation_request(execution: ExecutionKind) -> PrepareRequestFrame<'static> {
    let (terminal, size) = match execution {
        ExecutionKind::Interactive => (
            Some(TerminalEnvironment::new("xterm-256color", None, None).unwrap()),
            Some(TerminalSize::new(80, 24).unwrap()),
        ),
        ExecutionKind::Bounded => (None, None),
    };
    PrepareRequestFrame::new(
        17,
        BrokerId::new([0x5a; 16]),
        PrepareRequest::new(
            "alice",
            execution,
            GuestProcess::Command(CommandArgv::new(&["/usr/bin/id"]).unwrap()),
            terminal,
            size,
            ProjectionRequests::default(),
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

fn readiness() -> WorkerReadiness {
    let hello = WorkerHello::new(ControlLimits::CURRENT).unwrap();
    let accepted = GuestWorkerReadiness::check()
        .unwrap()
        .accept_hello(
            lasper_guest_protocol::PROTOCOL_MAJOR,
            hello.request(),
            BrokerId::new([0x5a; 16]),
            ControlLimits::CURRENT,
        )
        .unwrap();
    hello
        .negotiate(lasper_guest_protocol::PROTOCOL_MAJOR, accepted.response())
        .unwrap()
}

#[test]
fn helper_hello_rejects_limits_that_cannot_hold_session_results() {
    let minimum = lasper_guest_protocol::MIN_HELPER_CONTROL_FRAME_BYTES;
    assert!(matches!(
        WorkerHello::new(ControlLimits::new(minimum - 1, 3).unwrap()),
        Err(WorkerReadinessError::FrameLimit { available }) if available == minimum - 1
    ));
    let guest = GuestWorkerReadiness::check().unwrap();
    for (host_limit, guest_limit) in [(minimum - 1, minimum), (minimum, minimum - 1)] {
        let request = HelloRequest::new(
            FeatureSet::WORKER_PIDFD,
            ControlLimits::new(host_limit, 3).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            guest.accept_hello(lasper_guest_protocol::PROTOCOL_MAJOR, request, BrokerId::new([1; 16]), ControlLimits::new(guest_limit, 3).unwrap()),
            Err(GuestHelloError::FrameLimit { available }) if available == minimum - 1
        ));
    }
    let host = WorkerHello::new(ControlLimits::CURRENT).unwrap();
    let response = HelloResponse::new(
        FeatureSet::WORKER_PIDFD,
        BrokerId::new([1; 16]),
        ControlLimits::new(minimum - 1, 3).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        host.negotiate(lasper_guest_protocol::PROTOCOL_MAJOR, response),
        Err(WorkerReadinessError::FrameLimit { .. })
    ));
}

#[tokio::test]
async fn prepare_ready_transfers_the_assigned_worker_for_both_execution_kinds() {
    for execution in [ExecutionKind::Interactive, ExecutionKind::Bounded] {
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
            let mut payload = [0; 256];
            let frame = receive_guest_frame(&guest, &mut payload).unwrap();
            let hello_request = HelloRequest::decode_payload(frame.payload()).unwrap();
            let major = frame.header().protocol_major();
            let accepted = GuestWorkerReadiness::check()
                .unwrap()
                .accept_hello(
                    major,
                    hello_request,
                    ready.response().broker_id(),
                    ControlLimits::CURRENT,
                )
                .unwrap();
            let hello_response = accepted.response();
            let length = hello_response.encode_payload(&mut payload).unwrap();
            send_guest_frame(
                &guest,
                ControlFrameHeader::new(MessageKind::HelloResponse, 1, length as u32, 0).unwrap(),
                &payload[..length],
                &[],
            )
            .unwrap();
            guest.complete_hello(accepted.limits()).unwrap();
            let mut payload = [0; 4096];
            let frame = receive_guest_frame(&guest, &mut payload).unwrap();
            assert_eq!(frame.header().kind(), MessageKind::PrepareRequest);
            let mut bytes = [0; 512];
            let length = ready.encode_into(&mut bytes).unwrap();
            let mut descriptors = Vec::new();
            if let Some(terminal) = &terminal {
                descriptors.push(terminal.as_raw_fd());
            }
            descriptors.extend([identity.as_raw_fd(), pidfd.as_raw_fd()]);
            send_guest_frame(
                &guest,
                ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES]).unwrap(),
                &bytes[CONTROL_FRAME_HEADER_BYTES..length],
                &descriptors,
            )
            .unwrap();
        });
        let mut payload = [0; 256];
        let length = hello.request().encode_payload(&mut payload).unwrap();
        host.send_frame(
            ControlFrameHeader::new(MessageKind::HelloRequest, 1, length as u32, 0).unwrap(),
            &payload[..length],
            &[],
        )
        .await
        .unwrap();
        let response = host.receive_frame().await.unwrap();
        let readiness = hello
            .negotiate(
                response.header().protocol_major(),
                HelloResponse::decode_payload(response.payload()).unwrap(),
            )
            .unwrap();
        host.complete_hello(readiness.limits()).unwrap();
        let mut bytes = [0; 4096];
        let length = request.encode_into(&mut bytes).unwrap();
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
            &request,
            response,
        )
        .unwrap();
        assert_eq!(prepared.ready(), ready);
        assert_eq!(
            prepared.terminal().is_some(),
            execution == ExecutionKind::Interactive
        );
        let first = prepared.observe().await.unwrap();
        let second = prepared.observe().await.unwrap();
        assert_ne!(first.reply().challenge(), second.reply().challenge());
        assert_eq!(prepared.process().pid(), worker.pid);
    }
}

#[tokio::test]
async fn invalid_preparation_resources_close_the_received_identity_endpoint() {
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
            ExecutionKind::Interactive
        } else {
            ExecutionKind::Bounded
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
            establish_guest(&mut guest);
            let pidfd = if failure == "not-pidfd" {
                File::open("/dev/null").unwrap().into()
            } else {
                assigned_pidfd()
            };
            let terminal = File::open("/dev/null").unwrap();
            let mut descriptors = Vec::new();
            if execution == ExecutionKind::Interactive {
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
            let length = ready.encode_into(&mut bytes).unwrap();
            let header = ControlFrameHeader::new(
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
            &readiness(),
            &request,
            response,
        ) {
            Ok(_) => panic!("accepted invalid preparation: {failure}"),
            Err(error) => error,
        };
        assert!(!error.to_string().is_empty(), "{failure}");
        let mut poll = libc::pollfd {
            fd: identity_guest.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // Forked fixtures close inherited copies before entering their loop.
        // Wait for that scheduling window, but still fail if a copy leaks.
        assert_eq!(unsafe { libc::poll(&mut poll, 1, 2000) }, 1, "{failure}");
        let mut byte = 0u8;
        // SAFETY: The test owns the peer. MSG_DONTWAIT avoids hanging if an
        // error path accidentally retains a received identity descriptor.
        assert_eq!(
            unsafe {
                libc::recv(
                    identity_guest.as_raw_fd(),
                    (&mut byte as *mut u8).cast(),
                    1,
                    libc::MSG_DONTWAIT,
                )
            },
            0,
            "{failure}"
        );
    }
}

async fn deliver_ready(
    ready: lasper_guest_protocol::PrepareReadyFrame,
    descriptors: Vec<OwnedFd>,
) -> lasper_guest_host_transport::ReceivedControlFrame {
    let (mut host, guest) = raw_stream_pair();
    let sender = tokio::task::spawn_blocking(move || {
        // SAFETY: The connected control endpoint transfers its ownership.
        let mut guest = unsafe { GuestControlStream::from_raw_fd(guest.into_raw_fd()) };
        establish_guest(&mut guest);
        let mut bytes = [0; 512];
        let length = ready.encode_into(&mut bytes).unwrap();
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

#[tokio::test]
async fn another_workers_pty_cannot_be_combined_with_valid_identity_resources() {
    let readiness = readiness();
    let request = readiness
        .prepare(
            17,
            preparation_request(ExecutionKind::Interactive)
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
        &request,
        frame,
    ) {
        Ok(_) => panic!("accepted another worker's PTY"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("does not belong"));
}

#[tokio::test]
async fn terminal_assignment_is_rechecked_before_accepting_another_observation() {
    let readiness = readiness();
    let request = readiness
        .prepare(
            17,
            preparation_request(ExecutionKind::Interactive)
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
    let mut prepared =
        PreparedWorker::from_frame(target::current_target().await, &readiness, &request, frame)
            .unwrap();
    prepared.observe().await.unwrap();
    assert_eq!(prepared.process().pid(), worker.pid);
    let error = prepared.observe().await.unwrap_err();
    assert!(error.to_string().contains("PTY"), "{error}");
    prepared.process().recheck().unwrap();
    assert!(!prepared.can_continue());
}

#[test]
fn hello_rejects_missing_worker_pidfd_before_any_prepare() {
    let hello = WorkerHello::new(ControlLimits::CURRENT).unwrap();
    assert!(hello
        .request()
        .required_features()
        .contains(FeatureSet::WORKER_PIDFD));
    let result = hello.negotiate(
        lasper_guest_protocol::PROTOCOL_MAJOR,
        HelloResponse::new(
            FeatureSet::EMPTY,
            BrokerId::new([0x5a; 16]),
            ControlLimits::CURRENT,
        )
        .unwrap(),
    );
    let error = match result {
        Ok(_) => panic!("accepted a broker without worker pidfd support"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        WorkerReadinessError::Negotiation(HelloNegotiationError::UnsupportedFeatures { .. })
    ));
    assert_eq!(
        error.to_string(),
        "helper does not support worker pidfd assignment"
    );
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
                frame.header().protocol_major(),
                request,
                BrokerId::new([0x5a; 16]),
                ControlLimits::CURRENT,
            );
            assert!(matches!(
                result,
                Err(GuestHelloError::WorkerPidfdNotRequested)
            ));
            // Reject before responding or creating any worker resources.
        });
        let legacy_request = HelloRequest::new(
            FeatureSet::EMPTY,
            ControlLimits::new(lasper_guest_protocol::MAX_CONTROL_FRAME_BYTES, descriptors)
                .unwrap(),
        )
        .unwrap();
        let mut payload = [0; 256];
        let length = legacy_request.encode_payload(&mut payload).unwrap();
        host.send_frame(
            ControlFrameHeader::new(MessageKind::HelloRequest, 1, length as u32, 0).unwrap(),
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

#[test]
fn insufficient_descriptor_limit_is_rejected_before_sending_prepare() {
    let readiness = WorkerHello::new(ControlLimits::CURRENT)
        .unwrap()
        .negotiate(
            lasper_guest_protocol::PROTOCOL_MAJOR,
            HelloResponse::new(
                FeatureSet::WORKER_PIDFD,
                BrokerId::new([0x5a; 16]),
                ControlLimits::new(lasper_guest_protocol::MAX_CONTROL_FRAME_BYTES, 2).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let request = preparation_request(ExecutionKind::Interactive);
    assert!(matches!(
        readiness.prepare(17, request.request().clone()),
        Err(WorkerReadinessError::DescriptorLimit {
            required: 3,
            available: 2
        })
    ));
    let request = preparation_request(ExecutionKind::Bounded);
    assert!(readiness.prepare(17, request.request().clone()).is_ok());
}

#[tokio::test]
async fn target_leader_exit_invalidates_preparation_without_rebinding_to_a_replacement() {
    let readiness = readiness();
    let request = preparation_request(ExecutionKind::Bounded);
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
    let mut prepared =
        PreparedWorker::from_frame(target.clone(), &readiness, &request, frame).unwrap();
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

    let readiness = readiness();
    let request = preparation_request(ExecutionKind::Bounded);
    let ready = request
        .ready_frame(
            PrepareReady::new(PreparationId::new([8; 16]), request.broker_id(), 30_000).unwrap(),
        )
        .unwrap();
    let leader = TestWorker::spawn(request.request().clone(), PreparationId::new([9; 16]));
    let target = target::target_for(leader.pid as u32).await;
    let (host, guest) = identity_pair();
    let frame = deliver_ready(ready, vec![host, assigned_pidfd()]).await;
    let mut prepared = PreparedWorker::from_frame(target, &readiness, &request, frame).unwrap();
    let (waiting, challenge_received) = tokio::sync::oneshot::channel();
    let (release, released) = mpsc::channel();
    let worker_request = request.request().clone();
    let worker = tokio::task::spawn_blocking(move || {
        // SAFETY: Transfer the sole ownership of a connected blocking endpoint.
        let mut guest = unsafe {
            GuestIdentityChannel::from_raw_fd(
                guest.into_raw_fd(),
                ready.response().preparation_id(),
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
            Default::default(),
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
    let readiness = readiness();
    let request = preparation_request(ExecutionKind::Bounded);
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
    let error = match PreparedWorker::from_frame(target, &readiness, &request, frame) {
        Ok(_) => panic!("accepted resources for an exited target"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    let mut byte = 0u8;
    // SAFETY: The peer remains owned, and recv writes only to this byte.
    assert_eq!(
        unsafe {
            libc::recv(
                peer.as_raw_fd(),
                (&mut byte as *mut u8).cast(),
                1,
                libc::MSG_DONTWAIT,
            )
        },
        0
    );
}
