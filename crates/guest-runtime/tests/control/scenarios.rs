use super::{bootstrap::GuardianProcess, client::Client, wire};
use lasper_guest_protocol::*;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

fn bounded(privileged: bool) {
    let mut client = Client::prepared(
        "bounded",
        privileged,
        ExecutionKind::Piped,
        &["/bin/sh", "-c", "printf '\\377out'; printf err >&2; exit 7"],
    );
    client.start();
    let readers = client.drain_output();
    let frame = client.finish();
    let exit = super::client::protocol()
        .decode_exit(
            &client.request,
            client.ready.response().preparation_id(),
            frame.header,
            &frame.payload,
            0,
        )
        .unwrap();
    assert_eq!(exit.report().exit(), Some(ExitStatus::Exited(7)));
    let (stdout, stderr) = (readers.0.join().unwrap(), readers.1.join().unwrap());
    assert_eq!(stdout, b"\xffout");
    assert_eq!(stderr, b"err");
    client.wait();
}

fn maximum_prepare(privileged: bool) {
    let mut arguments: Vec<String> = [
        "/bin/sh",
        "-c",
        "printf '%s\\n' \"$#\"; printf '%s\\n' \"$@\"; printf '%s' \"$V\"",
        "budget",
    ]
    .map(str::to_owned)
    .into();
    let used = arguments.iter().map(String::len).sum::<usize>() + arguments.len() - 1;
    let mut remaining = MAX_COMMAND_TOTAL_BYTES - used;
    while remaining != 0 {
        let length = (remaining - 1).min(MAX_COMMAND_ARGUMENT_BYTES);
        arguments.push("\\\"\n".repeat(length.div_ceil(3))[..length].to_owned());
        remaining -= length + 1;
    }
    let argv: Vec<_> = arguments.iter().map(String::as_str).collect();
    let value_length = MAX_PROCESS_ENVIRONMENT_BYTES - "V=".len() - 1;
    let value = "\n\"\\".repeat(value_length.div_ceil(3))[..value_length].to_owned();
    let request = PrepareRequestFrame::new(
        10,
        BrokerId::new([1; 16]),
        PrepareRequest::new(
            "alice",
            ExecutionKind::Piped,
            GuestProcess::Command(CommandArgv::new(&argv).unwrap()),
            ProcessEnvironment::new([("V", value.as_str())]).unwrap(),
            None,
        )
        .unwrap(),
    );
    // command() sets the stack limit before exec. Send the request through the
    // production receive/decode path, not directly to GuardianContext::spawn.
    let mut guardian =
        GuardianProcess::spawn(super::command("maximum-prepare", privileged)).unwrap();
    guardian.release();
    guardian.hello();
    let mut client = Client::prepare_request(guardian, request);
    client.start();
    let readers = client.drain_output();
    let frame = client.finish();
    let exit = super::client::protocol()
        .decode_exit(
            &client.request,
            client.ready.response().preparation_id(),
            frame.header,
            &frame.payload,
            frame.descriptors.len(),
        )
        .unwrap();
    assert_eq!(exit.report().command_exit(), Some(ExitStatus::Exited(0)));
    assert_eq!(exit.report().failure(), None);
    let mut expected = format!("{}\n", arguments.len() - 4).into_bytes();
    for argument in &arguments[4..] {
        expected.extend_from_slice(argument.as_bytes());
        expected.push(b'\n');
    }
    expected.extend_from_slice(value.as_bytes());
    assert_eq!(readers.0.join().unwrap(), expected);
    assert!(readers.1.join().unwrap().is_empty());
    client.wait();
}

fn output_pressure(privileged: bool) {
    let mut client = Client::prepared(
        "output-pressure",
        privileged,
        ExecutionKind::Piped,
        &[
            "/bin/sh",
            "-c",
            "printf '%200000s' x; printf '%200000s' y >&2",
        ],
    );
    client.start();
    let readers = client.drain_output();
    let frame = client.finish();
    let exit = super::client::protocol()
        .decode_exit(
            &client.request,
            client.ready.response().preparation_id(),
            frame.header,
            &frame.payload,
            0,
        )
        .unwrap();
    assert_eq!(exit.report().exit(), Some(ExitStatus::Exited(0)));
    assert_eq!(readers.0.join().unwrap().len(), 200000);
    assert_eq!(readers.1.join().unwrap().len(), 200000);
    client.wait();
}

fn late_stderr(privileged: bool) {
    let mut client = Client::prepared(
        "late-stderr",
        privileged,
        ExecutionKind::Piped,
        &[
            "/bin/sh",
            "-c",
            "printf '%200000s' x; printf 'late error' >&2; exit 7",
        ],
    );
    client.start();
    let readers = client.drain_output();
    let frame = client.finish();
    let exit = super::client::protocol()
        .decode_exit(
            &client.request,
            client.ready.response().preparation_id(),
            frame.header,
            &frame.payload,
            0,
        )
        .unwrap();
    assert_eq!(readers.0.join().unwrap().len(), 200000);
    assert_eq!(readers.1.join().unwrap(), b"late error");
    assert_eq!(exit.report().command_exit(), Some(ExitStatus::Exited(7)));
    client.wait();
}

fn control_and_cancel(privileged: bool) {
    let mut client = Client::prepared(
        "control-cancel",
        privileged,
        ExecutionKind::Piped,
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
    let exit = super::client::protocol()
        .decode_exit(
            &client.request,
            client.ready.response().preparation_id(),
            frame.header,
            &frame.payload,
            0,
        )
        .unwrap();
    assert_eq!(exit.report().execution(), ExecutionStatus::NotStarted);
    assert_eq!(exit.report().reason(), ExitReason::Cancelled);
    let readers = client.drain_output();
    assert!(readers.0.join().unwrap().is_empty());
    assert!(readers.1.join().unwrap().is_empty());
    client.wait();
}

fn execution_timeout(privileged: bool, execution: ExecutionKind) {
    let mut client = Client::prepared(
        "execution-timeout",
        privileged,
        execution,
        &["/bin/sleep", "30"],
    );
    client.start_with_timeout(Some(200));
    let frame = client.finish();
    let exit = super::client::protocol()
        .decode_exit(
            &client.request,
            client.ready.response().preparation_id(),
            frame.header,
            &frame.payload,
            frame.descriptors.len(),
        )
        .unwrap();
    assert_eq!(exit.report().reason(), ExitReason::ExecutionTimedOut);
    assert_eq!(exit.report().cleanup(), CleanupStatus::Complete);
    assert!(exit.report().exit().is_some());
    assert_eq!(exit.report().command_exit(), None);
    client.wait();
}

fn bad_start(privileged: bool) {
    let mut client = Client::prepared(
        "bad-start",
        privileged,
        ExecutionKind::Piped,
        &["/bin/true"],
    );
    let start = StartFrame::new(
        11,
        StartRequest::new(&client.request, PreparationId::new([9; 16]), None, None).unwrap(),
    );
    let mut bytes = [0; 1024];
    let (header, length) = super::client::protocol()
        .encode_start_frame(&start, &mut bytes)
        .unwrap();
    wire::send(
        client.guardian.control.as_raw_fd(),
        header,
        &bytes[..length],
        &[],
    )
    .unwrap();
    let response = client.receive();
    let error = super::client::protocol()
        .decode_error(
            header,
            &client.request,
            Some(client.ready.response().preparation_id()),
            response.header,
            &response.payload,
            0,
        )
        .unwrap();
    assert_eq!(error.code(), GuestErrorCode::InvalidRequest);
    let start = StartFrame::new(
        12,
        StartRequest::new(
            &client.request,
            client.ready.response().preparation_id(),
            None,
            None,
        )
        .unwrap(),
    );
    let (header, length) = super::client::protocol()
        .encode_start_frame(&start, &mut bytes)
        .unwrap();
    wire::send(
        client.guardian.control.as_raw_fd(),
        header,
        &bytes[..length],
        &[],
    )
    .unwrap();
    let response = client.receive();
    let error = super::client::protocol()
        .decode_error(
            header,
            &client.request,
            Some(client.ready.response().preparation_id()),
            response.header,
            &response.payload,
            0,
        )
        .unwrap();
    assert_eq!(error.code(), GuestErrorCode::InvalidState);
    client.next_id = 13;
    client.control(PreparationControlKind::Close);
    client.finish();
    client.wait();
}

fn exec_failure(privileged: bool) {
    let mut client = Client::prepared(
        "exec-failure",
        privileged,
        ExecutionKind::Piped,
        &["/does/not/exist"],
    );
    client.start();
    let frame = client.finish();
    let exit = super::client::protocol()
        .decode_exit(
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
    assert_eq!(
        exit.report().failure().unwrap().stage(),
        GuestFailureStage::Exec
    );
    assert_eq!(exit.report().failure().unwrap().errno(), 2);
    client.wait();
}

fn control_disconnect(privileged: bool) {
    let mut client = Client::prepared("disconnect", privileged, ExecutionKind::Piped, &["/bin/sh", "-c", "trap '' TERM HUP; /usr/bin/setsid /bin/sh -c 'trap \"\" TERM HUP; /bin/sleep 30' & wait"]);
    client.start();
    std::thread::sleep(Duration::from_millis(100));
    let worker_fd = client.descriptors[1].as_raw_fd();
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
        ExecutionKind::Piped,
        &["/bin/sleep", "30"],
    );
    client.start();
    client.guardian.broker.take();
    let frame = client.finish();
    let exit = super::client::protocol()
        .decode_exit(
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
        ExecutionKind::Piped,
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
    let (header, _) = super::client::protocol()
        .encode_control_request(&command, &mut bytes)
        .unwrap();
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
    let request = super::client::request(ExecutionKind::Piped, &["/bin/true"]);
    let mut bytes = [0; 8192];
    let length = super::client::protocol()
        .encode_prepare_frame(&request, &mut bytes)
        .unwrap();
    let header = ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES]).unwrap();
    wire::send(
        guardian.control.as_raw_fd(),
        header,
        &bytes[CONTROL_FRAME_HEADER_BYTES..length],
        &[],
    )
    .unwrap();
    let response = wire::receive(guardian.control.as_raw_fd()).unwrap();
    let error = super::client::protocol()
        .decode_error(
            header,
            &request,
            None,
            response.header,
            &response.payload,
            0,
        )
        .unwrap();
    assert_eq!(error.errno(), Some(2));
    assert_eq!(error.stage(), Some(GuestFailureStage::Account));
    assert!(guardian.wait().success());
    std::fs::remove_dir_all(
        std::env::temp_dir().join(format!("lasper-account-test-{}", guardian.child.id())),
    )
    .unwrap();
}

pub const SCENARIOS: &[super::Scenario] = &[
    (
        "maximum argv and escaped environment execute together on the budgeted stack",
        maximum_prepare,
    ),
    (
        "bounded execution reports its explicit timeout",
        |privileged| execution_timeout(privileged, ExecutionKind::Piped),
    ),
    (
        "interactive execution reports its explicit timeout",
        |privileged| execution_timeout(privileged, ExecutionKind::Terminal),
    ),
    (
        "bounded command retains binary output and wait status",
        bounded,
    ),
    (
        "large stdout and stderr streams drain independently",
        output_pressure,
    ),
    (
        "stderr remains readable after large stdout output",
        late_stderr,
    ),
    (
        "lease, Recheck and Cancel use the assigned preparation",
        control_and_cancel,
    ),
    ("a rejected Start permits only Close", bad_start),
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
