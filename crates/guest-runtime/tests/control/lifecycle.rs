use super::{bootstrap::GuardianProcess, client::Client, wire};
use lasper_guest_protocol::*;
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

fn delayed_prepare(privileged: bool) {
    let mut guardian =
        GuardianProcess::spawn(super::command("delayed-prepare", privileged)).unwrap();
    guardian.release();
    guardian.hello();
    std::thread::sleep(Duration::from_millis(4_100));
    let mut client = Client::prepare(guardian, ExecutionKind::Piped, &["/bin/true"]);
    // Cross the old Hello+5s deadline after Ready, while retaining a fresh lease.
    std::thread::sleep(Duration::from_millis(1_100));
    assert!(matches!(
        client
            .control(PreparationControlKind::KeepPrepared)
            .status(),
        ControlReplyStatus::Prepared { .. }
    ));
    client.start();
    client.finish();
    client.wait();
}

fn closing_rejects_input(privileged: bool) {
    let mut client = Client::prepared(
        "closing-input",
        privileged,
        ExecutionKind::Piped,
        &["/bin/true"],
    );
    assert_eq!(
        client.control(PreparationControlKind::Cancel).status(),
        ControlReplyStatus::CleanupRequested
    );
    send_late_commands(&mut client);
    let frame = client.finish();
    assert_eq!(
        exit(&client, &frame).report().reason(),
        ExitReason::Cancelled
    );
    client.wait();
}

fn running_rejects_prepared_operations(privileged: bool) {
    let mut client = Client::prepared(
        "running-order",
        privileged,
        ExecutionKind::Piped,
        &["/bin/sleep", "30"],
    );
    client.start();
    for invalid in [false, true] {
        for operation in [
            GuestOperation::Prepare,
            GuestOperation::KeepPrepared,
            GuestOperation::Recheck,
            GuestOperation::Start,
            GuestOperation::Cancel,
        ] {
            let id = client.next_id;
            client.next_id += 1;
            let mut bytes = [0; 4096];
            let preparation = if invalid {
                PreparationId::new([9; 16])
            } else {
                client.ready.response().preparation_id()
            };
            let (header, length) = match operation {
                GuestOperation::Prepare => {
                    let request = PrepareRequestFrame::new(
                        id,
                        client.request.broker_id(),
                        client.request.request().clone(),
                    );
                    let length = super::client::protocol()
                        .encode_prepare_frame(&request, &mut bytes)
                        .unwrap();
                    let header =
                        ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES]).unwrap();
                    bytes.copy_within(CONTROL_FRAME_HEADER_BYTES..length, 0);
                    (header, length - CONTROL_FRAME_HEADER_BYTES)
                }
                GuestOperation::Start => super::client::protocol()
                    .encode_start_frame(
                        &StartFrame::new(
                            id,
                            StartRequest::new(&client.request, preparation, None, None).unwrap(),
                        ),
                        &mut bytes,
                    )
                    .unwrap(),
                _ => super::client::protocol()
                    .encode_control_request(
                        &PreparationControlFrame::new(
                            id,
                            match operation {
                                GuestOperation::KeepPrepared => {
                                    PreparationControlKind::KeepPrepared
                                }
                                GuestOperation::Recheck => PreparationControlKind::Recheck,
                                GuestOperation::Cancel => PreparationControlKind::Cancel,
                                _ => unreachable!(),
                            },
                            PreparationControl::new(client.request.broker_id(), preparation),
                        ),
                        &mut bytes,
                    )
                    .unwrap(),
            };
            let (header, payload) = if invalid && operation == GuestOperation::Prepare {
                (
                    ControlFrameHeader::new_with_version(
                        header.protocol_version(),
                        header.kind(),
                        id,
                        2,
                        0,
                    )
                    .unwrap(),
                    b"{}".as_slice(),
                )
            } else {
                (header, &bytes[..length])
            };
            wire::send(client.guardian.control.as_raw_fd(), header, payload, &[]).unwrap();
            let frame = client.receive();
            let rejected = super::client::protocol()
                .decode_error(
                    header,
                    &client.request,
                    Some(client.ready.response().preparation_id()),
                    frame.header,
                    &frame.payload,
                    frame.descriptors.len(),
                )
                .unwrap();
            assert_eq!(
                rejected.code(),
                if invalid {
                    GuestErrorCode::InvalidRequest
                } else {
                    GuestErrorCode::InvalidState
                },
                "{operation:?}, invalid={invalid}"
            );
            assert_eq!(rejected.operation(), operation);
        }
    }
    assert_eq!(
        client.control(PreparationControlKind::Close).status(),
        ControlReplyStatus::CleanupRequested
    );
    client.finish();
    client.wait();
}

fn cleanup_progress(privileged: bool) {
    let mut client = Client::prepared(
        "cleanup-progress",
        privileged,
        ExecutionKind::Piped,
        &[
            "/bin/sh",
            "-c",
            "trap '' HUP TERM; : > /home/alice/started; while :; do /bin/sleep 30; done",
        ],
    );
    client.start();
    let marker = std::env::temp_dir().join(format!(
        "lasper-account-test-{}/home/alice/started",
        client.guardian.child.id()
    ));
    let deadline = Instant::now() + Duration::from_secs(3);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "worker did not install its signal dispositions"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        client.control(PreparationControlKind::Close).status(),
        ControlReplyStatus::CleanupRequested
    );
    send_late_commands(&mut client);
    let frame = client.receive();
    let first = exit(&client, &frame).report();
    assert_eq!(first.cleanup(), CleanupStatus::InProgress);
    assert_eq!(first.reason(), ExitReason::ClientClosed);
    assert!(!first.is_final());
    let mut reports = vec![first];
    loop {
        let frame = client.receive();
        let report = exit(&client, &frame).report();
        assert!(
            !reports.contains(&report),
            "unchanged cleanup report was sent again"
        );
        if report.is_final() {
            assert_eq!(report.cleanup(), CleanupStatus::Complete);
            assert_eq!(report.exit(), Some(ExitStatus::Signaled(9)));
            break;
        }
        reports.push(report);
    }
    client.wait();
}

fn partial_input_during_cleanup(privileged: bool) {
    let mut client = Client::prepared(
        "partial-input-cleanup",
        privileged,
        ExecutionKind::Piped,
        &["/bin/true"],
    );
    let header = ControlFrameHeader::new_with_version(
        lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::Recheck,
        11,
        100,
        0,
    )
    .unwrap();
    client
        .guardian
        .control
        .write_all(&header.encode()[..3])
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    client.guardian.broker.take();
    let frame = client.finish();
    assert_eq!(
        exit(&client, &frame).report().reason(),
        ExitReason::BrokerExited
    );
    client.wait();
}

fn closed_output(privileged: bool, large: bool, close_writers: bool) {
    let mut client = Client::prepared(
        "held-output",
        privileged,
        ExecutionKind::Piped,
        if large {
            &["/bin/output-helper", "--hold-output", "large"]
        } else {
            &["/bin/output-helper", "--hold-output"]
        },
    );
    let socket = std::env::temp_dir().join(format!(
        "lasper-account-test-{}/home/alice/output.sock",
        client.guardian.child.id()
    ));
    let listener = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666)).unwrap();
    listener.set_nonblocking(true).unwrap();
    client.start();
    let readers = client.drain_output();
    let limit = Instant::now() + Duration::from_secs(4);
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < limit,
                    "output helper did not export its writers"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("accept output helper: {error}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let writers = wire::receive(stream.as_raw_fd()).unwrap().descriptors;
    assert_eq!(writers.len(), 2);
    wait_until_reaped(&client);
    // No descendant owns these writers now. Cleanup is complete, but EOF is not.
    let started = Instant::now();
    let mut stdout = std::fs::File::from(writers[0].try_clone().unwrap());
    stdout.write_all(b"after-cleanup").unwrap();
    drop(stdout);
    let held: Option<Vec<OwnedFd>> = if close_writers {
        drop(writers);
        None
    } else {
        Some(writers)
    };
    let frame = client.finish();
    let report = exit(&client, &frame);
    assert!(report.report().is_final());
    // Status completion is independent of writers retained outside the guest.

    assert!(started.elapsed() < Duration::from_secs(3));
    drop(held);
    let stdout = readers.0.join().unwrap();
    let stderr = readers.1.join().unwrap();
    if large {
        assert!(stdout.len() > 64 * 1024);
        assert!(stderr.len() > 64 * 1024);
    } else {
        assert_eq!(stdout, b"after-cleanup");
    }
    client.wait();
}

fn send_late_commands(client: &mut Client) {
    for _ in 0..16 {
        client.send_control(PreparationControlKind::KeepPrepared);
        client.send_control(PreparationControlKind::Recheck);
        let start = StartFrame::new(
            client.next_id,
            StartRequest::new(
                &client.request,
                client.ready.response().preparation_id(),
                None,
                None,
            )
            .unwrap(),
        );
        client.next_id += 1;
        client.send(|bytes| {
            super::client::protocol()
                .encode_start_frame(&start, bytes)
                .unwrap()
        });
    }
}

fn wait_until_reaped(client: &Client) {
    let pid = client.guardian.child.id();
    let path = format!("/proc/{pid}/task/{pid}/children");
    let deadline = Instant::now() + Duration::from_secs(4);
    while !std::fs::read_to_string(&path).unwrap().trim().is_empty() {
        assert!(Instant::now() < deadline, "guardian did not finish reaping");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn exit(client: &Client, frame: &wire::Frame) -> ExitFrame {
    super::client::protocol()
        .decode_exit(
            &client.request,
            client.ready.response().preparation_id(),
            frame.header,
            &frame.payload,
            frame.descriptors.len(),
        )
        .unwrap()
}

pub fn export_output(large: bool) {
    if large {
        std::io::stdout().write_all(&vec![b'x'; 200_000]).unwrap();
        std::io::stderr().write_all(&vec![b'y'; 200_000]).unwrap();
    }
    let stream = UnixStream::connect("/home/alice/output.sock").unwrap();
    // Fixture-only FD transfer; the parent holds both writers outside the session tree.
    let header = ControlFrameHeader::new_with_version(
        lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::PrepareReady,
        1,
        0,
        2,
    )
    .unwrap();
    wire::send(stream.as_raw_fd(), header, &[], &[1, 2]).unwrap();
}

pub const SCENARIOS: &[super::Scenario] = &[
    (
        "running rejects Keep, Recheck and repeated Start but still accepts Close",
        running_rejects_prepared_operations,
    ),
    (
        "cleanup progress precedes final Exit without duplicate reports or late command replies",
        cleanup_progress,
    ),
    (
        "a late Prepare does not shorten Ready or lease deadlines",
        delayed_prepare,
    ),
    (
        "Closing does not consume queued Keep, Recheck or Start",
        closing_rejects_input,
    ),
    (
        "cleanup preserves Exit delivery with a partial inbound frame",
        partial_input_during_cleanup,
    ),
    (
        "host can drain output after final Exit without accepting commands",
        |p| closed_output(p, false, true),
    ),
    (
        "final Exit does not wait for externally held pipe writers",
        |p| closed_output(p, false, false),
    ),
    (
        "large output and late commands cannot postpone final Exit",
        |p| closed_output(p, true, false),
    ),
];
