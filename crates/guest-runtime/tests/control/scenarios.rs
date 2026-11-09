use super::{bootstrap::GuardianProcess, client::Client, wire};
use lasper_guest_protocol::*;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

fn bounded(privileged: bool) {
    let mut client = Client::prepared(
        "bounded",
        privileged,
        ExecutionKind::Bounded,
        &["/bin/sh", "-c", "printf '\\377out'; printf err >&2; exit 7"],
    );
    client.start();
    let frame = client.finish();
    let exit = ExitFrame::decode_for(
        &client.request,
        client.ready.response().preparation_id(),
        frame.header,
        &frame.payload,
        0,
    )
    .unwrap();
    assert_eq!(exit.report().exit(), Some(ExitStatus::Exited(7)));
    let output = exit.output().unwrap();
    let mut stdout = [0; 32];
    assert_eq!(output.stdout().write_into(&mut stdout).unwrap(), 4);
    assert_eq!(&stdout[..4], b"\xffout");
    assert!(output.stdout().complete());
    assert!(!output.stdout().truncated());
    assert_eq!(output.stderr().len(), 3);
    client.wait();
}

fn output_pressure(privileged: bool) {
    let mut client = Client::prepared(
        "output-pressure",
        privileged,
        ExecutionKind::Bounded,
        &[
            "/bin/sh",
            "-c",
            "printf '%200000s' x; printf '%200000s' y >&2",
        ],
    );
    client.start();
    let frame = client.finish();
    let exit = ExitFrame::decode_for(
        &client.request,
        client.ready.response().preparation_id(),
        frame.header,
        &frame.payload,
        0,
    )
    .unwrap();
    assert_eq!(exit.report().exit(), Some(ExitStatus::Exited(0)));
    let output = exit.output().unwrap();
    assert_eq!(
        output.stdout().len() + output.stderr().len(),
        MAX_CAPTURED_OUTPUT_BYTES
    );
    assert!(output.stdout().truncated() && output.stderr().truncated());
    assert!(output.stdout().complete() && output.stderr().complete());
    client.wait();
}

fn control_and_cancel(privileged: bool) {
    let mut client = Client::prepared(
        "control-cancel",
        privileged,
        ExecutionKind::Bounded,
        &["/bin/sh", "-c", "printf should-not-run"],
    );
    for kind in [
        PreparationControlKind::KeepPrepared,
        PreparationControlKind::Recheck,
    ] {
        assert!(matches!(
            client.control(kind).status(),
            ControlReplyStatus::Prepared {
                remaining_lease_ms: 1..=MAX_PREPARED_LEASE_MS
            }
        ));
    }
    assert_eq!(
        client.control(PreparationControlKind::Cancel).status(),
        ControlReplyStatus::CleanupRequested
    );
    let frame = client.finish();
    let exit = ExitFrame::decode_for(
        &client.request,
        client.ready.response().preparation_id(),
        frame.header,
        &frame.payload,
        0,
    )
    .unwrap();
    assert_eq!(exit.report().execution(), ExecutionStatus::NotStarted);
    assert_eq!(exit.report().reason(), ExitReason::Cancelled);
    assert!(exit.output().unwrap().stdout().is_empty());
    client.wait();
}

fn bad_start(privileged: bool) {
    let mut client = Client::prepared(
        "bad-start",
        privileged,
        ExecutionKind::Bounded,
        &["/bin/true"],
    );
    let start = StartFrame::new(
        11,
        StartRequest::new(
            &client.request,
            PreparationId::new([9; 16]),
            None,
            None,
            None,
        )
        .unwrap(),
    );
    let mut bytes = [0; 1024];
    let (header, length) = start.encode_parts(&mut bytes).unwrap();
    wire::send(
        client.guardian.control.as_raw_fd(),
        header,
        &bytes[..length],
        &[],
    )
    .unwrap();
    let response = client.receive();
    let error = ErrorFrame::decode_for(
        header,
        &client.request,
        Some(client.ready.response().preparation_id()),
        response.header,
        &response.payload,
        0,
    )
    .unwrap();
    assert_eq!(error.code(), GuestErrorCode::InvalidRequest);
    client.next_id = 12;
    client.start();
    client.finish();
    client.wait();
}

fn exec_failure(privileged: bool) {
    let mut client = Client::prepared(
        "exec-failure",
        privileged,
        ExecutionKind::Bounded,
        &["/does/not/exist"],
    );
    client.start();
    let frame = client.finish();
    let exit = ExitFrame::decode_for(
        &client.request,
        client.ready.response().preparation_id(),
        frame.header,
        &frame.payload,
        0,
    )
    .unwrap();
    assert_eq!(
        exit.report().execution(),
        ExecutionStatus::Failed { errno: 2 }
    );
    assert_eq!(exit.report().reason(), ExitReason::ExecFailed);
    client.wait();
}

fn control_disconnect(privileged: bool) {
    let mut client = Client::prepared("disconnect", privileged, ExecutionKind::Bounded, &["/bin/sh", "-c", "trap '' TERM HUP; /usr/bin/setsid /bin/sh -c 'trap \"\" TERM HUP; /bin/sleep 30' & wait"]);
    client.start();
    std::thread::sleep(Duration::from_millis(100));
    let worker_fd = client.descriptors.last().unwrap().as_raw_fd();
    client
        .guardian
        .control
        .shutdown(std::net::Shutdown::Both)
        .unwrap();
    client.wait();
    let mut poll = libc::pollfd {
        fd: worker_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut poll, 1, 0) }, 1);
}

fn broker_disconnect(privileged: bool) {
    let mut client = Client::prepared(
        "broker-disconnect",
        privileged,
        ExecutionKind::Bounded,
        &["/bin/sleep", "30"],
    );
    client.start();
    client.guardian.broker.take();
    let frame = client.finish();
    let exit = ExitFrame::decode_for(
        &client.request,
        client.ready.response().preparation_id(),
        frame.header,
        &frame.payload,
        0,
    )
    .unwrap();
    assert_eq!(exit.report().reason(), ExitReason::BrokerExited);
    client.wait();
}

fn partial_control(privileged: bool) {
    let mut client = Client::prepared(
        "partial-control",
        privileged,
        ExecutionKind::Bounded,
        &["/bin/true"],
    );
    let command = PreparationControlFrame::new(
        11,
        PreparationControlKind::KeepPrepared,
        PreparationControl::new(
            client.request.broker_id(),
            client.ready.response().preparation_id(),
        ),
    );
    let mut bytes = [0; 256];
    let (header, _) = command.encode_parts(&mut bytes).unwrap();
    client
        .guardian
        .control
        .write_all(&header.encode()[..3])
        .unwrap();
    let start = Instant::now();
    client.wait();
    assert!((Duration::from_secs(4)..Duration::from_secs(9)).contains(&start.elapsed()));
}

fn account_failure(privileged: bool) {
    let mut guardian =
        GuardianProcess::spawn(super::command("account-failure", privileged)).unwrap();
    guardian.release();
    guardian.hello();
    let request = super::client::request(ExecutionKind::Bounded, &["/bin/true"]);
    let mut bytes = [0; 8192];
    let length = request.encode_into(&mut bytes).unwrap();
    let header = ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES]).unwrap();
    wire::send(
        guardian.control.as_raw_fd(),
        header,
        &bytes[CONTROL_FRAME_HEADER_BYTES..length],
        &[],
    )
    .unwrap();
    let response = wire::receive(guardian.control.as_raw_fd()).unwrap();
    let error = ErrorFrame::decode_for(
        header,
        &request,
        None,
        response.header,
        &response.payload,
        0,
    )
    .unwrap();
    assert_eq!(error.errno(), Some(2));
    assert!(guardian.wait().success());
    std::fs::remove_dir_all(
        std::env::temp_dir().join(format!("lasper-account-test-{}", guardian.child.id())),
    )
    .unwrap();
}

pub const SCENARIOS: &[super::Scenario] = &[
    (
        "bounded command retains binary output and wait status",
        bounded,
    ),
    (
        "output pipes keep draining beyond the capture budget",
        output_pressure,
    ),
    (
        "lease, Recheck and Cancel use the assigned preparation",
        control_and_cancel,
    ),
    (
        "a rejected Start preserves the worker for a valid Start",
        bad_start,
    ),
    ("exec failure follows StartAccepted", exec_failure),
    (
        "control disconnect reaps the worker and adopted descendants",
        control_disconnect,
    ),
    (
        "broker EOF is recorded separately from client close",
        broker_disconnect,
    ),
    (
        "a partial control header expires without losing cleanup",
        partial_control,
    ),
    (
        "account setup failure returns Prepare Error and reaps",
        account_failure,
    ),
];
