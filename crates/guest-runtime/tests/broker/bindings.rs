use super::{client, fixture::Broker, wire};
use lasper_guest_protocol::*;
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

fn control(
    stream: &UnixStream,
    request: &PrepareRequestFrame<'_>,
    ready: &PrepareReadyFrame,
    id: u64,
    token: PreparationControl,
    rejected: bool,
) {
    let protocol = client::protocol();
    let command = PreparationControlFrame::new(id, PreparationControlKind::Recheck, token);
    let mut bytes = [0; 512];
    let (header, length) = protocol
        .encode_control_request(&command, &mut bytes)
        .unwrap();
    wire::send(stream.as_raw_fd(), header, &bytes[..length], &[]).unwrap();
    let response = wire::receive(stream.as_raw_fd()).unwrap();
    if rejected {
        let error = protocol
            .decode_error(
                header,
                request,
                Some(ready.response().preparation_id()),
                response.header,
                &response.payload,
                response.descriptors.len(),
            )
            .unwrap();
        assert_eq!(error.code(), GuestErrorCode::InvalidRequest);
    } else {
        let reply = protocol
            .decode_control_reply(
                &command,
                response.header,
                &response.payload,
                response.descriptors.len(),
            )
            .unwrap();
        assert!(matches!(
            reply.status(),
            ControlReplyStatus::Prepared { .. }
        ));
    }
}

fn finish(stream: &UnixStream, request: &PrepareRequestFrame<'_>, ready: &PrepareReadyFrame) {
    // Recheck has consumed IDs 11 onward. Start uses a fresh ID on this same
    // connection; rejection must not have replaced or released its worker.
    let protocol = client::protocol();
    let start = StartFrame::new(
        20,
        StartRequest::new(request, ready.response().preparation_id(), None, None).unwrap(),
    );
    let mut bytes = [0; 512];
    let (header, length) = protocol.encode_start_frame(&start, &mut bytes).unwrap();
    wire::send(stream.as_raw_fd(), header, &bytes[..length], &[]).unwrap();
    let frame = wire::receive(stream.as_raw_fd()).unwrap();
    protocol
        .decode_start_accepted(
            20,
            request,
            ready.response().preparation_id(),
            frame.header,
            &frame.payload,
            frame.descriptors.len(),
        )
        .unwrap();
    client::finish(stream, request, ready);
}

fn assert_worker_exited(descriptors: &[OwnedFd]) {
    let mut entry = libc::pollfd {
        fd: descriptors[1].as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut entry, 1, 0) }, 1);
    assert_ne!(entry.revents & libc::POLLIN, 0);
}

fn cross_connection(privileged: bool) {
    let mut broker = Broker::with_guest(privileged);
    let mut first = broker.connect();
    let mut second = broker.connect();
    let broker_id = client::hello(&mut first);
    assert_eq!(client::hello(&mut second), broker_id);
    let request_a = client::request_command(&first, broker_id, &["/bin/true"]);
    let request_b = client::request_command(&second, broker_id, &["/bin/true"]);
    let (ready_a, fds_a) = client::ready(&first, &request_a);
    let (ready_b, fds_b) = client::ready(&second, &request_b);
    let token_a = PreparationControl::new(broker_id, ready_a.response().preparation_id());
    let token_b = PreparationControl::new(broker_id, ready_b.response().preparation_id());
    assert_ne!(token_a.preparation_id(), token_b.preparation_id());
    for (stream, request, ready, foreign, local) in [
        (&first, &request_a, &ready_a, token_b, token_a),
        (&second, &request_b, &ready_b, token_a, token_b),
    ] {
        control(stream, request, ready, 11, foreign, true);
        control(stream, request, ready, 12, local, false);
        finish(stream, request, ready);
    }
    assert_worker_exited(&fds_a);
    assert_worker_exited(&fds_b);
    drop((first, second));
    broker.wait_children(0);
    assert!(broker.stop().success());
}

fn previous_generation(privileged: bool) {
    let mut broker = Broker::with_guest(privileged);
    let mut old = broker.connect();
    let old_id = client::hello(&mut old);
    let old_request = client::request_command(&old, old_id, &["/bin/true"]);
    let (old_ready, old_fds) = client::ready(&old, &old_request);
    assert!(broker.stop().success());
    assert_worker_exited(&old_fds);
    // A stopped broker can report cleanup on the old connection, but the new
    // generation must never reuse that connection or its preparation binding.
    let mut tail = Vec::new();
    old.read_to_end(&mut tail).unwrap();
    std::fs::remove_dir_all(
        std::env::temp_dir().join(format!("lasper-account-test-{}", broker.child.id())),
    )
    .unwrap();
    broker.restart(privileged);
    let mut stream = broker.connect();
    let new_id = client::hello(&mut stream);
    assert_ne!(old_id, new_id);
    let request = client::request_command(&stream, new_id, &["/bin/true"]);
    let (ready, descriptors) = client::ready(&stream, &request);
    let local = PreparationControl::new(new_id, ready.response().preparation_id());
    for (id, token) in [
        (
            11,
            PreparationControl::new(old_id, old_ready.response().preparation_id()),
        ),
        // Keep the new preparation valid to isolate broker generation checking.
        (12, PreparationControl::new(old_id, local.preparation_id())),
        (
            13,
            PreparationControl::new(new_id, old_ready.response().preparation_id()),
        ),
    ] {
        control(&stream, &request, &ready, id, token, true);
    }
    control(&stream, &request, &ready, 14, local, false);
    finish(&stream, &request, &ready);
    assert_worker_exited(&descriptors);
    drop(stream);
    broker.wait_children(0);
    assert!(broker.stop().success());
}

pub const SCENARIOS: &[super::Scenario] = &[
    (
        "preparation bindings cannot cross connections of the same broker",
        cross_connection,
    ),
    (
        "restarted broker rejects old generation and preparation bindings",
        previous_generation,
    ),
];
