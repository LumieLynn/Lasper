use lasper_guest_host_transport::raw::{BrokerConnection, BrokerSession};
use lasper_guest_host_transport::{
    BrokerSessionError, SessionPhase, StartError, StartOptions, StartOutcome,
};
use lasper_guest_protocol::{
    CommandArgv, ControlLimits, ExecutionKind, ExecutionStatus, ExitReason, ExitStatus,
    GuestProcess, PrepareRequest, ProcessEnvironment, TerminalSize,
};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use tokio::time::{timeout, Duration, Instant};

async fn connect(
    target: &Arc<lasper_guest_host_transport::MachineTarget>,
    path: &std::path::Path,
) -> BrokerConnection {
    BrokerConnection::connect(
        Arc::clone(target),
        path.to_str().unwrap(),
        ControlLimits::CURRENT,
        Instant::now() + Duration::from_secs(1),
    )
    .await
    .unwrap()
}

async fn exercise(target: Arc<lasper_guest_host_transport::MachineTarget>, path: &std::path::Path) {
    let connection = connect(&target, path).await;
    let mut session = {
        let command = String::from("printf session-output; printf session-error >&2; exit 7");
        let prepare = request(ExecutionKind::Piped, &["/bin/sh", "-c", &command]);
        BrokerSession::prepare(connection, prepare, Instant::now() + Duration::from_secs(5))
            .await
            .unwrap()
    };
    assert_eq!(session.snapshot().phase(), SessionPhase::Prepared);
    assert_eq!(session.observation().reply().guest_user(), "root");
    let output = drain_pipes(&mut session);
    session.recheck().await.unwrap();
    assert!(matches!(
        session.start(StartOptions::default()).await.unwrap(),
        StartOutcome::Accepted
    ));
    let exit = finish(&mut session).await;
    assert_eq!(exit.exit(), Some(ExitStatus::Exited(7)));
    let (stdout, stderr) = output.await.unwrap();
    assert_eq!(stdout, b"session-output");
    assert_eq!(stderr, b"session-error");
    assert!(matches!(
        session.start(StartOptions::default()).await,
        Err(StartError::NotSubmitted(BrokerSessionError::DriverStopped))
    ));

    let mut session = BrokerSession::prepare(
        connect(&target, path).await,
        request(ExecutionKind::Piped, &["/bin/sleep", "30"]),
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap();
    session.cancel().await.unwrap();
    assert_eq!(
        finish(&mut session).await.execution(),
        lasper_guest_protocol::ExecutionStatus::NotStarted
    );

    let mut session = BrokerSession::prepare(
        connect(&target, path).await,
        request(ExecutionKind::Terminal, &["/bin/sh", "-c", "exit 0"]),
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap();
    let Some(lasper_guest_host_transport::raw::PreparedIo::Terminal(terminal)) = session.take_io()
    else {
        panic!("missing Ready PTY");
    };
    let StartOutcome::Accepted = session
        .start(StartOptions {
            terminal_size: Some(TerminalSize::new(100, 30).unwrap()),
            ..Default::default()
        })
        .await
        .unwrap()
    else {
        panic!("interactive Start was not accepted with a PTY");
    };
    assert!(terminal.as_raw_fd() >= 0);
    assert_eq!(
        finish(&mut session).await.exit(),
        Some(ExitStatus::Exited(0))
    );
    drop(terminal);
}

async fn failed_exec(
    target: &Arc<lasper_guest_host_transport::MachineTarget>,
    path: &std::path::Path,
) {
    let mut session = BrokerSession::prepare(
        connect(target, path).await,
        request(ExecutionKind::Piped, &["/nonexistent/lasper-test-command"]),
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert!(matches!(
        session.start(StartOptions::default()).await.unwrap(),
        StartOutcome::Accepted
    ));
    let exit = finish(&mut session).await;
    assert_eq!(
        exit.execution(),
        ExecutionStatus::Failed {
            errno: libc::ENOENT
        }
    );
    assert_eq!(exit.reason(), ExitReason::ExecFailed);
    assert!(session.snapshot().error().is_none());
}

async fn large_request_and_output(
    target: &Arc<lasper_guest_host_transport::MachineTarget>,
    path: &std::path::Path,
) {
    let mut session = {
        let argument = "argument-".repeat(450);
        let prepare = request(
            ExecutionKind::Piped,
            &[
                "/bin/sh",
                "-c",
                r#"test "${#1}" = 4050 && test "$1" = "$2" && test "$2" = "$3" || exit 19; printf '\377\000'; dd if=/dev/zero bs=1024 count=128 2>/dev/null"#,
                "sh",
                &argument,
                &argument,
                &argument,
            ],
        );
        BrokerSession::prepare(
            connect(target, path).await,
            prepare,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap()
    };
    let output = drain_pipes(&mut session);
    assert!(matches!(
        session.start(StartOptions::default()).await.unwrap(),
        StartOutcome::Accepted
    ));
    let exit = finish(&mut session).await;
    assert_eq!(exit.exit(), Some(ExitStatus::Exited(0)));
    let (stdout, stderr) = output.await.unwrap();
    assert_eq!(stdout.len(), 128 * 1024 + 2);
    assert_eq!(&stdout[..2], &[255, 0]);
    assert!(stderr.is_empty());
}

async fn concurrent_sessions(
    target: &Arc<lasper_guest_host_transport::MachineTarget>,
    path: &std::path::Path,
) {
    let mut first = BrokerSession::prepare(
        connect(target, path).await,
        request(ExecutionKind::Piped, &["/bin/sleep", "30"]),
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap();
    let mut second = BrokerSession::prepare(
        connect(target, path).await,
        request(ExecutionKind::Piped, &["/bin/sleep", "30"]),
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_ne!(first.preparation_id(), second.preparation_id());
    assert!(matches!(
        first.start(StartOptions::default()).await.unwrap(),
        StartOutcome::Accepted
    ));
    assert!(matches!(
        second.start(StartOptions::default()).await.unwrap(),
        StartOutcome::Accepted
    ));
    let first_state = first.subscribe();
    drop(first);
    assert_eq!(
        observe_exit(first_state).await.reason(),
        ExitReason::ClientClosed
    );
    assert_eq!(second.snapshot().phase(), SessionPhase::Accepted);
    second.close().await.unwrap();
    assert_eq!(finish(&mut second).await.reason(), ExitReason::ClientClosed);
}

async fn generic_environment(
    target: &Arc<lasper_guest_host_transport::MachineTarget>,
    path: &std::path::Path,
) {
    let display = "/arbitrary/environment-\"\\\n";
    let prepare = PrepareRequest::new(
        "root",
        ExecutionKind::Terminal,
        GuestProcess::Command(
            CommandArgv::new(&[
                "/bin/sh",
                "-c",
                r#"test "$WAYLAND_DISPLAY" = "$1""#,
                "sh",
                display,
            ])
            .unwrap(),
        ),
        ProcessEnvironment::new([("TERM", ""), ("WAYLAND_DISPLAY", display)]).unwrap(),
        Some(TerminalSize::new(80, 24).unwrap()),
    )
    .unwrap();
    let mut session = BrokerSession::prepare(
        connect(target, path).await,
        prepare,
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap();
    let Some(lasper_guest_host_transport::raw::PreparedIo::Terminal(terminal)) = session.take_io()
    else {
        panic!("missing Ready PTY");
    };
    let StartOutcome::Accepted = session
        .start(StartOptions {
            terminal_size: Some(TerminalSize::new(80, 24).unwrap()),
            ..Default::default()
        })
        .await
        .unwrap()
    else {
        panic!("Start not accepted");
    };
    assert_eq!(
        finish(&mut session).await.exit(),
        Some(ExitStatus::Exited(0))
    );
    drop(terminal);
}

fn drain_pipes(session: &mut BrokerSession) -> tokio::task::JoinHandle<(Vec<u8>, Vec<u8>)> {
    use std::io::Read;
    let Some(lasper_guest_host_transport::raw::PreparedIo::Pipes { stdout, stderr }) =
        session.take_io()
    else {
        panic!("missing Ready pipes");
    };
    tokio::task::spawn_blocking(move || {
        let read = |fd: std::os::fd::OwnedFd| {
            // The fixture dedicates one blocking reader to each data stream.
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
            assert!(flags >= 0);
            assert_eq!(
                unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) },
                0
            );
            let mut bytes = Vec::new();
            std::fs::File::from(fd).read_to_end(&mut bytes).unwrap();
            bytes
        };
        let err = std::thread::spawn(move || read(stderr));
        (read(stdout), err.join().unwrap())
    })
}

fn request<'a>(execution: ExecutionKind, argv: &[&'a str]) -> PrepareRequest<'a> {
    let interactive = execution == ExecutionKind::Terminal;
    PrepareRequest::new(
        "root",
        execution,
        GuestProcess::Command(CommandArgv::new(argv).unwrap()),
        lasper_guest_protocol::ProcessEnvironment::default(),
        interactive.then(|| TerminalSize::new(80, 24).unwrap()),
    )
    .unwrap()
}

async fn finish(session: &mut BrokerSession) -> lasper_guest_protocol::ExitReport {
    observe_exit(session.subscribe()).await
}

async fn observe_exit(
    mut state: tokio::sync::watch::Receiver<lasper_guest_host_transport::SessionSnapshot>,
) -> lasper_guest_protocol::ExitReport {
    timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = state.borrow_and_update().clone();
            if snapshot.phase() == SessionPhase::Finished {
                assert!(snapshot.error().is_none(), "{:?}", snapshot.error());
                return snapshot.report.unwrap();
            }
            state.changed().await.unwrap();
        }
    })
    .await
    .unwrap()
}

pub(super) async fn run() {
    println!("host broker: session owns Prepare, identity, Start, cancellation and final output");
    let mut broker = super::fixture::Broker::with_accounts();
    let target = super::target::target_for(broker.pid()).await;
    broker.start();
    exercise(Arc::clone(&target), &broker.socket()).await;
    failed_exec(&target, &broker.socket()).await;
    large_request_and_output(&target, &broker.socket()).await;
    concurrent_sessions(&target, &broker.socket()).await;
    generic_environment(&target, &broker.socket()).await;
    client_api(&target, &broker.socket()).await;
    let mut prepared = BrokerSession::prepare(
        connect(&target, &broker.socket()).await,
        request(ExecutionKind::Piped, &["/bin/true"]),
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap();
    broker.stop().await;
    assert_eq!(
        finish(&mut prepared).await.reason(),
        ExitReason::BrokerExited
    );
    println!("host broker: 9 session scenarios passed");
}

async fn client_api(
    target: &Arc<lasper_guest_host_transport::MachineTarget>,
    path: &std::path::Path,
) {
    use lasper_guest_host_transport::{
        ClientOptions, GuestClient, Prepared, ProcessSpec, Program, SessionIo, StartOptions,
        StartSubmission, Stdio,
    };
    use tokio::io::AsyncReadExt;
    let value = "a\n\\\"".repeat((lasper_guest_protocol::MAX_PROCESS_ENVIRONMENT_BYTES - 13) / 4);
    let client = GuestClient::new(
        Arc::clone(target),
        ClientOptions {
            endpoint: path.to_str().unwrap().into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        client
            .probe(Instant::now() + Duration::from_secs(2))
            .await
            .unwrap()
            .hello
            .profile()
            .version(),
        lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 }
    );
    let Prepared { mut session, io } = client
        .prepare(
            ProcessSpec {
                user: "root".into(),
                program: Program::Argv(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "printf '%s' \"$EXTRA\"; printf diagnostic >&2; exit 7".into(),
                ]),
                stdio: Stdio::Piped,
                environment: [
                    ("EXTRA".into(), value.clone()),
                    ("TERM".into(), String::new()),
                ]
                .into(),
            },
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(matches!(
        session
            .start(
                StartOptions::default(),
                Instant::now() + Duration::from_secs(5)
            )
            .await
            .unwrap(),
        StartOutcome::Accepted
    ));
    let SessionIo::Pipes {
        mut stdout,
        mut stderr,
    } = io
    else {
        panic!("expected pipes");
    };
    let drain = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (out_result, err_result) =
            tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
        out_result.unwrap();
        err_result.unwrap();
        (out, err)
    };
    let (completion, (out, err)) = tokio::join!(session.wait(), drain);
    assert_eq!(out, value.as_bytes());
    assert_eq!(err, b"diagnostic");
    assert_eq!(completion.submission, StartSubmission::Accepted);
    assert_eq!(
        completion.report.unwrap().exit(),
        Some(ExitStatus::Exited(7))
    );
    assert!(completion.error.is_none());
    let (first, second) = tokio::join!(session.close(), session.close());
    assert_eq!(first.report, second.report);
}
