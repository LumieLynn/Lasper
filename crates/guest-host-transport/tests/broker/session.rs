use lasper_guest_host_transport::{
    BrokerConnection, BrokerSession, BrokerSessionError, SessionPhase, SessionStartOptions,
};
use lasper_guest_protocol::{
    CommandArgv, ControlLimits, ExecutionKind, ExecutionStatus, ExitReason, ExitStatus,
    GuestProcess, PrepareRequest, ProjectionRequests, TerminalEnvironment, TerminalSize,
    WaylandProjectionRequest, MAX_CAPTURED_OUTPUT_BYTES,
};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
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
        let prepare = request(ExecutionKind::Bounded, &["/bin/sh", "-c", &command]);
        BrokerSession::prepare(connection, prepare, Instant::now() + Duration::from_secs(5))
            .await
            .unwrap()
    };
    assert_eq!(session.snapshot().phase(), SessionPhase::Prepared);
    assert_eq!(session.observation().reply().guest_user(), "root");
    session.recheck().await.unwrap();
    assert!(session
        .start(SessionStartOptions::default())
        .await
        .unwrap()
        .is_none());
    let exit = finish(&mut session).await;
    assert_eq!(exit.report().exit(), Some(ExitStatus::Exited(7)));
    let output = exit.output().unwrap();
    assert_eq!(output.stdout().bytes(), b"session-output");
    assert_eq!(output.stderr().bytes(), b"session-error");
    assert!(output.stdout().complete() && output.stderr().complete());
    assert!(matches!(
        session.start(SessionStartOptions::default()).await,
        Err(BrokerSessionError::DriverStopped)
    ));

    let mut session = BrokerSession::prepare(
        connect(&target, path).await,
        request(ExecutionKind::Bounded, &["/bin/sleep", "30"]),
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap();
    session.cancel().await.unwrap();
    assert_eq!(
        finish(&mut session).await.report().execution(),
        lasper_guest_protocol::ExecutionStatus::NotStarted
    );

    let mut session = BrokerSession::prepare(
        connect(&target, path).await,
        request(ExecutionKind::Interactive, &["/bin/sh", "-c", "exit 0"]),
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap();
    let terminal = session
        .start(SessionStartOptions {
            terminal_size: Some(TerminalSize::new(100, 30).unwrap()),
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap();
    assert!(terminal.as_raw_fd() >= 0);
    assert_eq!(
        finish(&mut session).await.report().exit(),
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
        request(
            ExecutionKind::Bounded,
            &["/nonexistent/lasper-test-command"],
        ),
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap();
    session.start(SessionStartOptions::default()).await.unwrap();
    let exit = finish(&mut session).await;
    assert_eq!(
        exit.report().execution(),
        ExecutionStatus::Failed {
            errno: libc::ENOENT
        }
    );
    assert_eq!(exit.report().reason(), ExitReason::ExecFailed);
    assert!(session.snapshot().error().is_none());
}

async fn large_request_and_output(
    target: &Arc<lasper_guest_host_transport::MachineTarget>,
    path: &std::path::Path,
) {
    let mut session = {
        let argument = "argument-".repeat(450);
        let prepare = request(
            ExecutionKind::Bounded,
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
    session.start(SessionStartOptions::default()).await.unwrap();
    let exit = finish(&mut session).await;
    assert_eq!(exit.report().exit(), Some(ExitStatus::Exited(0)));
    let output = exit.output().unwrap();
    assert_eq!(output.stdout().bytes().len(), MAX_CAPTURED_OUTPUT_BYTES);
    assert_eq!(&output.stdout().bytes()[..2], &[255, 0]);
    assert!(output.stdout().truncated() && output.stdout().complete());
    assert!(output.stderr().complete());
}

async fn concurrent_sessions(
    target: &Arc<lasper_guest_host_transport::MachineTarget>,
    path: &std::path::Path,
) {
    let mut first = BrokerSession::prepare(
        connect(target, path).await,
        request(ExecutionKind::Bounded, &["/bin/sleep", "30"]),
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap();
    let mut second = BrokerSession::prepare(
        connect(target, path).await,
        request(ExecutionKind::Bounded, &["/bin/sleep", "30"]),
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_ne!(first.preparation_id(), second.preparation_id());
    first.start(SessionStartOptions::default()).await.unwrap();
    second.start(SessionStartOptions::default()).await.unwrap();
    let first_state = first.subscribe();
    drop(first);
    assert_eq!(
        observe_exit(first_state).await.report().reason(),
        ExitReason::ClientClosed
    );
    assert_eq!(second.snapshot().phase(), SessionPhase::Started);
    second.close().await.unwrap();
    assert_eq!(
        finish(&mut second).await.report().reason(),
        ExitReason::ClientClosed
    );
}

async fn changed_projection(
    target: &Arc<lasper_guest_host_transport::MachineTarget>,
    path: &std::path::Path,
) {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("wayland-\"\\");
    let old = UnixListener::bind(&socket).unwrap();
    let display = socket.to_str().unwrap();
    let prepare = PrepareRequest::new(
        "root",
        ExecutionKind::Interactive,
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
        Some(TerminalEnvironment::new("xterm-256color", None, None).unwrap()),
        Some(TerminalSize::new(80, 24).unwrap()),
        ProjectionRequests::new(Some(WaylandProjectionRequest::new(display).unwrap()), None),
    )
    .unwrap();
    let mut session = BrokerSession::prepare(
        connect(target, path).await,
        prepare,
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap();
    let previous = session.observation().reply().projections();
    std::fs::remove_file(&socket).unwrap();
    let _new = UnixListener::bind(&socket).unwrap();
    let start = || SessionStartOptions {
        wayland_display: Some(display.into()),
        terminal_size: Some(TerminalSize::new(80, 24).unwrap()),
        ..Default::default()
    };
    assert!(matches!(
        session.start(start()).await,
        Err(BrokerSessionError::ProjectionChanged)
    ));
    assert_eq!(session.snapshot().phase(), SessionPhase::Prepared);
    assert_eq!(session.observation().reply().projections(), previous);
    assert_ne!(
        session.recheck().await.unwrap().reply().projections(),
        previous
    );
    let terminal = session.start(start()).await.unwrap().unwrap();
    assert_eq!(
        finish(&mut session).await.report().exit(),
        Some(ExitStatus::Exited(0))
    );
    drop(terminal);
    drop(old);
}

fn request<'a>(execution: ExecutionKind, argv: &[&'a str]) -> PrepareRequest<'a> {
    let interactive = execution == ExecutionKind::Interactive;
    PrepareRequest::new(
        "root",
        execution,
        GuestProcess::Command(CommandArgv::new(argv).unwrap()),
        interactive.then(|| TerminalEnvironment::new("xterm-256color", None, None).unwrap()),
        interactive.then(|| TerminalSize::new(80, 24).unwrap()),
        ProjectionRequests::default(),
    )
    .unwrap()
}

async fn finish(session: &mut BrokerSession) -> Arc<lasper_guest_host_transport::SessionExit> {
    observe_exit(session.subscribe()).await
}

async fn observe_exit(
    mut state: tokio::sync::watch::Receiver<lasper_guest_host_transport::SessionSnapshot>,
) -> Arc<lasper_guest_host_transport::SessionExit> {
    timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = state.borrow_and_update().clone();
            assert_ne!(
                snapshot.phase(),
                SessionPhase::Failed,
                "{:?}",
                snapshot.error()
            );
            if snapshot.phase() == SessionPhase::Closed {
                return snapshot.into_exit().unwrap();
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
    changed_projection(&target, &broker.socket()).await;
    let mut prepared = BrokerSession::prepare(
        connect(&target, &broker.socket()).await,
        request(ExecutionKind::Bounded, &["/bin/true"]),
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap();
    broker.stop().await;
    assert_eq!(
        finish(&mut prepared).await.report().reason(),
        ExitReason::BrokerExited
    );
    println!("host broker: 8 session scenarios passed");
}
