use lasper_guest_protocol::{
    BrokerId, CommandArgv, ExecutionKind, GuestIdentityReply, GuestProcess, PreparationId,
    PrepareRequest, PrepareRequestFrame, ProjectionObservations, ProjectionRequests, StartFrame,
    StartRequest, TerminalEnvironment, TerminalSize, MAX_COMMAND_TOTAL_BYTES,
    MAX_IDENTITY_MESSAGE_BYTES,
};
use lasper_guest_runtime::session::*;
use std::io::Read;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use std::vec::Vec;

const PREPARATION: PreparationId = PreparationId::new([2; 16]);

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    returned: i16,
}

unsafe extern "C" {
    fn getuid() -> u32;
    fn getgid() -> u32;
    fn kill(pid: i32, signal: i32) -> i32;
    fn getpgid(pid: i32) -> i32;
    fn pause() -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn _exit(code: i32) -> !;
    fn ioctl(fd: i32, operation: usize, ...) -> i32;
    fn poll(fds: *mut PollFd, count: usize, timeout_ms: i32) -> i32;
}

const EAGAIN: i32 = 11;

fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let elapsed = EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64;
    CLOCK_FLOOR
        .fetch_max(elapsed, Ordering::Relaxed)
        .max(elapsed)
}

static CLOCK_FLOOR: AtomicU64 = AtomicU64::new(0);

fn advance_to(time: u64) -> u64 {
    CLOCK_FLOOR.fetch_max(time, Ordering::Relaxed);
    now_ms()
}

fn request(execution: ExecutionKind, argv: &[&'static str]) -> PrepareRequestFrame<'static> {
    let (terminal, size) = if execution == ExecutionKind::Interactive {
        (
            Some(TerminalEnvironment::new("xterm", None, None).unwrap()),
            Some(TerminalSize::new(80, 24).unwrap()),
        )
    } else {
        (None, None)
    };
    PrepareRequestFrame::new(
        10,
        BrokerId::new([1; 16]),
        PrepareRequest::new(
            "alice",
            execution,
            GuestProcess::Command(CommandArgv::new(argv).unwrap()),
            terminal,
            size,
            ProjectionRequests::default(),
        )
        .unwrap(),
    )
}

fn start(request: &PrepareRequestFrame<'_>) -> StartFrame {
    StartFrame::new(
        11,
        StartRequest::new(
            request,
            PREPARATION,
            None,
            None,
            request.request().initial_size(),
        )
        .unwrap(),
    )
}

fn control(
    request: &PrepareRequestFrame<'_>,
    kind: lasper_guest_protocol::PreparationControlKind,
) -> lasper_guest_protocol::PreparationControlFrame {
    lasper_guest_protocol::PreparationControlFrame::new(
        12,
        kind,
        lasper_guest_protocol::PreparationControl::new(request.broker_id(), PREPARATION),
    )
}

fn entry(bootstrap: WorkerBootstrap<'_>) -> ! {
    let started = await_worker_start(bootstrap);
    let mut command = [0; MAX_COMMAND_TOTAL_BYTES + 1];
    started.exec_command(&mut command, &[c"PATH=/usr/bin:/bin"])
}

fn await_worker_start(bootstrap: WorkerBootstrap<'_>) -> StartedWorker<'_> {
    let mut packet = [0; MAX_IDENTITY_MESSAGE_BYTES];
    bootstrap.await_start(&mut packet, |challenge, request| {
        GuestIdentityReply::new(
            challenge,
            request.request(),
            request.request().user(),
            unsafe { getuid() },
            unsafe { getgid() },
            ProjectionObservations::default(),
        )
        .map_err(|_| WorkerFailure::new(WorkerStage::Projection, 22))
    })
}

fn failing_entry(bootstrap: WorkerBootstrap<'_>) -> ! {
    bootstrap.fail(WorkerFailure::new(WorkerStage::Account, 2))
}

fn blocked_entry(_: WorkerBootstrap<'_>) -> ! {
    loop {
        unsafe {
            pause();
        }
    }
}

fn blocked_after_start_entry(bootstrap: WorkerBootstrap<'_>) -> ! {
    let _started = await_worker_start(bootstrap);
    loop {
        unsafe {
            pause();
        }
    }
}

fn wait_for_pidfd(pidfd: &OwnedFd) {
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let mut descriptor = PollFd {
            fd: pidfd.as_raw_fd(),
            events: 1,
            returned: 0,
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        // SAFETY: poll borrows one live descriptor slot. The pidfd stays owned
        // here; this observes exit without reaping or polling the guardian.
        let result = unsafe { poll(&mut descriptor, 1, remaining.as_millis() as i32) };
        if result < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        assert_eq!(result, 1, "worker did not exit before the deadline");
        assert_ne!(descriptor.returned & 1, 0, "pidfd was not readable");
        return;
    }
}

fn wait_for(
    worker: &mut SessionGuardian<'_>,
    predicate: impl Fn(WorkerObservation) -> bool,
) -> WorkerObservation {
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let observation = worker.poll(now_ms()).unwrap().worker;
        if predicate(observation) {
            return observation;
        }
        assert!(
            Instant::now() < deadline,
            "worker did not reach expected state: {observation:?}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn duplicate(fd: i32) -> OwnedFd {
    // SAFETY: The guardian owns the borrowed FD throughout this duplication.
    unsafe { BorrowedFd::borrow_raw(fd) }
        .try_clone_to_owned()
        .unwrap()
}

fn bounded_stdout(worker: &mut SessionGuardian<'_>) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut result = Vec::new();
    loop {
        let mut bytes = [0; 1024];
        match worker.read_stdout(&mut bytes) {
            Ok(0) => return result,
            Ok(length) => result.extend_from_slice(&bytes[..length]),
            Err(GuardianError::System { errno: EAGAIN, .. }) => {
                assert!(Instant::now() < deadline, "stdout never closed");
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("read stdout: {error:?}"),
        }
    }
}

fn finish(
    worker: &mut SessionGuardian<'_>,
    request: &PrepareRequestFrame<'_>,
) -> WorkerObservation {
    let deadline = Instant::now() + Duration::from_secs(4);
    if matches!(
        worker.phase(),
        SessionPhase::Preparing
            | SessionPhase::Prepared
            | SessionPhase::Starting
            | SessionPhase::Running
    ) {
        worker
            .control(
                now_ms(),
                control(
                    request,
                    lasper_guest_protocol::PreparationControlKind::Close,
                ),
            )
            .unwrap();
    }
    advance_to(
        worker
            .next_deadline_ms()
            .unwrap_or_else(now_ms)
            .max(now_ms())
            .saturating_add(1_001),
    );
    loop {
        let observation = worker.poll(now_ms()).unwrap().worker;
        if observation.cleanup == CleanupProgress::Complete {
            assert!(
                observation.exit.is_some(),
                "reaping lost the worker exit status"
            );
            assert_eq!(worker.phase(), SessionPhase::Closed);
            assert_eq!(worker.next_deadline_ms(), None);
            return observation;
        }
        assert!(
            Instant::now() < deadline,
            "cleanup remained pending: {observation:?}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn bounded_start_scenario() {
    let request = request(
        ExecutionKind::Bounded,
        &["/bin/sh", "-c", "printf '%s' \"$$\""],
    );
    // SAFETY: This test runs alone in an isolated guardian subprocess. Its
    // entry uses only syscalls and the allocation-free codec after fork.
    let mut worker =
        unsafe { SessionGuardian::spawn(request.clone(), PREPARATION, now_ms(), entry) }.unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    assert!(matches!(
        worker.read_stdout(&mut [0; 8]),
        Err(GuardianError::System { errno: EAGAIN, .. })
    ));
    assert_eq!(worker.ready_frame(now_ms()).unwrap().descriptor_count(), 2);
    let identity: UnixDatagram = duplicate(worker.descriptors().unwrap().as_raw_fds()[0]).into();
    identity
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut packet = [0; MAX_IDENTITY_MESSAGE_BYTES];
    for nonce in [1, 2] {
        let challenge = worker
            .ready_frame(now_ms())
            .unwrap()
            .identity_challenge([nonce; 32]);
        let length = challenge.encode_packet(&mut packet).unwrap();
        identity.send(&packet[..length]).unwrap();
        let length = identity.recv(&mut packet).unwrap();
        let reply = GuestIdentityReply::decode_for(&packet[..length], challenge, request.request())
            .unwrap();
        assert_eq!(reply.guest_user(), "alice");
        assert!(worker.poll(now_ms()).unwrap().worker.prepared);
    }
    worker.release_exported_descriptors();
    worker.start(now_ms(), start(&request)).unwrap();
    assert!(matches!(
        worker.start(now_ms(), start(&request)),
        Err(GuardianError::Lifecycle(
            SessionLifecycleError::InvalidTransition { .. }
        ))
    ));
    let output = bounded_stdout(&mut worker);
    assert_eq!(output, std::format!("{}", worker.pid()).as_bytes());
    let observation = wait_for(&mut worker, |observation| observation.exit.is_some());
    assert_eq!(observation.exit, Some(WorkerExit::Exited(0)));
    assert!(matches!(
        observation.exec,
        Some(ExecOutcome::Started | ExecOutcome::Unknown)
    ));
    finish(&mut worker, &request);
}

fn terminal_scenario() {
    let request = request(ExecutionKind::Interactive, &["/bin/sh", "-c", "stty size"]);
    let mut worker =
        unsafe { SessionGuardian::spawn(request.clone(), PREPARATION, now_ms(), entry) }.unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let descriptors = worker.descriptors().unwrap();
    assert_eq!(descriptors.as_raw_fds().len(), 3);
    let master = duplicate(descriptors.as_raw_fds()[0]);
    let _identity = duplicate(descriptors.as_raw_fds()[1]);
    for operation in [0x5429, 0x540f] {
        // TIOCGSID, TIOCGPGRP
        let mut terminal_pid = 0i32;
        assert_eq!(
            unsafe { ioctl(master.as_raw_fd(), operation, &mut terminal_pid) },
            0
        );
        assert_eq!(terminal_pid, worker.pid());
    }
    worker.release_exported_descriptors();
    let start = StartRequest::new(
        &request,
        PREPARATION,
        None,
        None,
        Some(TerminalSize::new(101, 37).unwrap()),
    )
    .unwrap();
    worker.start(now_ms(), StartFrame::new(11, start)).unwrap();
    let mut master = std::fs::File::from(master);
    let mut output = Vec::new();
    match master.read_to_end(&mut output) {
        Ok(_) => {}
        Err(error) if error.raw_os_error() == Some(5) => {}
        Err(error) => panic!("read PTY: {error}"),
    }
    assert_eq!(output, b"37 101\r\n");
    finish(&mut worker, &request);
}

fn failure_scenario() {
    let request = request(ExecutionKind::Bounded, &["/not/a/real/guest/program"]);
    let mut worker =
        unsafe { SessionGuardian::spawn(request.clone(), PREPARATION, now_ms(), entry) }.unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    worker.start(now_ms(), start(&request)).unwrap();
    let observation = wait_for(&mut worker, |observation| observation.failure.is_some());
    assert_eq!(
        observation.failure,
        Some(WorkerFailure::new(WorkerStage::Exec, 2))
    );
    assert_eq!(observation.exec, Some(ExecOutcome::Failed { errno: 2 }));
    finish(&mut worker, &request);

    let mut worker =
        unsafe { SessionGuardian::spawn(request.clone(), PREPARATION, now_ms(), failing_entry) }
            .unwrap();
    let observation = wait_for(&mut worker, |observation| observation.failure.is_some());
    assert_eq!(
        observation.failure,
        Some(WorkerFailure::new(WorkerStage::Account, 2))
    );
    assert!(!observation.prepared);
    assert_eq!(worker.descriptors().err(), Some(GuardianError::NotPrepared));
    finish(&mut worker, &request);
}

fn completed_before_poll_scenario() {
    let commands: &[(&[&str], WorkerExit)] = &[
        (&["/bin/true"], WorkerExit::Exited(0)),
        (&["/bin/sh", "-c", "exit 37"], WorkerExit::Exited(37)),
        (
            &["/bin/sh", "-c", "kill -TERM $$"],
            WorkerExit::Signaled(15),
        ),
    ];
    for &(argv, exit) in commands {
        let request = request(ExecutionKind::Bounded, argv);
        let mut worker =
            unsafe { SessionGuardian::spawn(request.clone(), PREPARATION, now_ms(), entry) }
                .unwrap();
        wait_for(&mut worker, |observation| observation.prepared);
        let descriptors = worker.descriptors().unwrap();
        let _identity = duplicate(descriptors.as_raw_fds()[0]);
        let pidfd = duplicate(*descriptors.as_raw_fds().last().unwrap());
        worker.release_exported_descriptors();
        worker.start(now_ms(), start(&request)).unwrap();
        wait_for_pidfd(&pidfd);
        assert_eq!(worker.phase(), SessionPhase::Starting);

        let result = worker.poll(now_ms()).unwrap();
        assert_eq!(result.worker.exit, Some(exit));
        assert_eq!(result.worker.exec, Some(ExecOutcome::Unknown));
        assert_eq!(result.worker.failure, None);
        assert_eq!(result.worker.error, None);
        assert_eq!(result.cleanup_reason, Some(CleanupReason::WorkerExited));
        finish(&mut worker, &request);
        assert_eq!(
            worker.poll(now_ms()).unwrap().cleanup_reason,
            result.cleanup_reason
        );
    }
}

fn exec_failure_before_poll_scenario() {
    let request = request(ExecutionKind::Bounded, &["/not/a/real/guest/program"]);
    let mut worker =
        unsafe { SessionGuardian::spawn(request.clone(), PREPARATION, now_ms(), entry) }.unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let pidfd = duplicate(*worker.descriptors().unwrap().as_raw_fds().last().unwrap());
    worker.start(now_ms(), start(&request)).unwrap();
    wait_for_pidfd(&pidfd);

    let result = worker.poll(now_ms()).unwrap();
    assert_eq!(result.worker.exit, Some(WorkerExit::Exited(126)));
    assert_eq!(result.worker.exec, Some(ExecOutcome::Failed { errno: 2 }));
    assert_eq!(
        result.worker.failure,
        Some(WorkerFailure::new(WorkerStage::Exec, 2))
    );
    assert_eq!(
        result.cleanup_reason,
        Some(CleanupReason::ExecFailed { errno: 2 })
    );
    finish(&mut worker, &request);
    assert_eq!(
        worker.poll(now_ms()).unwrap().cleanup_reason,
        result.cleanup_reason
    );
}

fn killed_before_exec_scenario() {
    let request = request(ExecutionKind::Bounded, &["/bin/true"]);
    let mut worker = unsafe {
        SessionGuardian::spawn(
            request.clone(),
            PREPARATION,
            now_ms(),
            blocked_after_start_entry,
        )
    }
    .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let pidfd = duplicate(*worker.descriptors().unwrap().as_raw_fds().last().unwrap());
    worker.start(now_ms(), start(&request)).unwrap();
    assert_eq!(unsafe { kill(worker.pid(), 9) }, 0);
    wait_for_pidfd(&pidfd);

    let result = worker.poll(now_ms()).unwrap();
    assert_eq!(result.worker.exec, Some(ExecOutcome::Unknown));
    assert_eq!(result.worker.exit, Some(WorkerExit::Signaled(9)));
    assert_eq!(result.worker.failure, None);
    assert_eq!(result.cleanup_reason, Some(CleanupReason::WorkerExited));
    finish(&mut worker, &request);
}

fn cancel_scenario() {
    let request = request(
        ExecutionKind::Bounded,
        &["/bin/sh", "-c", "printf should-not-run"],
    );
    let mut worker =
        unsafe { SessionGuardian::spawn(request.clone(), PREPARATION, now_ms(), entry) }.unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    worker
        .control(
            now_ms(),
            control(
                &request,
                lasper_guest_protocol::PreparationControlKind::Cancel,
            ),
        )
        .unwrap();
    assert!(matches!(
        worker.start(now_ms(), start(&request)),
        Err(GuardianError::Lifecycle(
            SessionLifecycleError::InvalidTransition { .. }
        ))
    ));
    finish(&mut worker, &request);
    assert!(bounded_stdout(&mut worker).is_empty());
}

fn descendants_scenario() {
    let request = request(ExecutionKind::Bounded, &["/bin/sh", "-c",
        "setsid /bin/sh -c 'trap \"\" HUP TERM; printf \"%s\" \"$$\"; exec >/dev/null 2>&1; while :; do sleep 1; done' & exit 23"]);
    let mut worker =
        unsafe { SessionGuardian::spawn(request.clone(), PREPARATION, now_ms(), entry) }.unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    worker.start(now_ms(), start(&request)).unwrap();
    let output = bounded_stdout(&mut worker);
    let descendant = std::str::from_utf8(&output)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    assert!(descendant > 0);
    assert_ne!(unsafe { getpgid(descendant) }, worker.pid());
    let observation = wait_for(&mut worker, |observation| observation.exit.is_some());
    assert_eq!(observation.exit, Some(WorkerExit::Exited(23)));
    let result = finish(&mut worker, &request);
    assert_eq!(result.exit, Some(WorkerExit::Exited(23)));
    assert_eq!(unsafe { kill(descendant, 0) }, -1);
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(3));
    assert_eq!(unsafe { waitpid(-1, std::ptr::null_mut(), 1) }, -1);
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(10));
}

fn preparation_timeout_scenario() {
    let request = request(ExecutionKind::Bounded, &["/bin/true"]);
    let mut worker =
        unsafe { SessionGuardian::spawn(request.clone(), PREPARATION, 0, blocked_entry) }.unwrap();
    assert_eq!(worker.phase(), SessionPhase::Preparing);
    assert_eq!(
        worker.ready_frame(advance_to(4_999)).err(),
        Some(GuardianError::NotPrepared)
    );
    assert_eq!(
        worker.poll(advance_to(5_000)).unwrap().phase,
        SessionPhase::Closing
    );
    assert_eq!(
        worker.ready_frame(now_ms()).err(),
        Some(GuardianError::NotPrepared)
    );
    let result = worker.poll(advance_to(6_001)).unwrap();
    assert_eq!(
        result.cleanup_reason,
        Some(CleanupReason::PreparationExpired)
    );
    assert!(matches!(
        result.worker.cleanup,
        CleanupProgress::Pending { .. } | CleanupProgress::Complete
    ));
    if let Some(deadline) = worker.next_deadline_ms() {
        assert!(deadline > now_ms());
    }
    finish(&mut worker, &request);
    assert_eq!(worker.phase(), SessionPhase::Closed);
}

fn lease_and_disconnect_scenario() {
    for connection in [SessionConnection::Client, SessionConnection::Broker] {
        let request = request(ExecutionKind::Bounded, &["/bin/sh", "-c", "exec sleep 30"]);
        let mut worker =
            unsafe { SessionGuardian::spawn(request.clone(), PREPARATION, now_ms(), entry) }
                .unwrap();
        wait_for(&mut worker, |observation| observation.prepared);
        let deadline = worker.next_deadline_ms().unwrap();
        assert_eq!(
            worker
                .ready_frame(advance_to(deadline - 1))
                .unwrap()
                .response()
                .remaining_lease_ms(),
            1
        );
        worker
            .control(
                now_ms(),
                control(
                    &request,
                    lasper_guest_protocol::PreparationControlKind::KeepPrepared,
                ),
            )
            .unwrap();
        assert!(worker.next_deadline_ms().unwrap() > deadline);
        worker.start(advance_to(deadline), start(&request)).unwrap();
        wait_for(&mut worker, |observation| {
            observation.exec == Some(ExecOutcome::Started)
        });
        assert_eq!(worker.phase(), SessionPhase::Running);
        assert_eq!(worker.next_deadline_ms(), None);
        worker.connection_closed(now_ms(), connection);
        assert_eq!(worker.phase(), SessionPhase::Closing);
        finish(&mut worker, &request);
        assert_eq!(worker.phase(), SessionPhase::Closed);
        let reason = match connection {
            SessionConnection::Client => CleanupReason::ClientClosed,
            SessionConnection::Broker => CleanupReason::BrokerExited,
        };
        assert_eq!(worker.poll(now_ms()).unwrap().cleanup_reason, Some(reason));
    }
}

fn wrong_start_scenario() {
    let request = request(ExecutionKind::Bounded, &["/bin/true"]);
    let mut worker =
        unsafe { SessionGuardian::spawn(request.clone(), PREPARATION, now_ms(), entry) }.unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let wrong = StartRequest::new(&request, PreparationId::new([3; 16]), None, None, None).unwrap();
    assert!(matches!(
        worker.start(now_ms(), StartFrame::new(11, wrong)),
        Err(GuardianError::InvalidStart(
            lasper_guest_protocol::SessionPayloadError::PreparationMismatch
        ))
    ));
    assert_eq!(worker.phase(), SessionPhase::Prepared);
    worker.start(now_ms(), start(&request)).unwrap();
    finish(&mut worker, &request);
}

fn worker_exit_before_start_scenario() {
    let request = request(
        ExecutionKind::Bounded,
        &["/bin/sh", "-c", "printf should-not-run"],
    );
    let mut worker =
        unsafe { SessionGuardian::spawn(request.clone(), PREPARATION, now_ms(), entry) }.unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    assert_eq!(unsafe { kill(worker.pid(), 9) }, 0);
    let observation = wait_for(&mut worker, |observation| observation.exit.is_some());
    assert_eq!(observation.exit, Some(WorkerExit::Signaled(9)));
    assert_eq!(observation.exec, None);
    assert!(!observation.prepared);
    assert!(worker.start(now_ms(), start(&request)).is_err());
    finish(&mut worker, &request);
    assert!(bounded_stdout(&mut worker).is_empty());
}

fn isolated(scenario: &str) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--scenario", scenario])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "guardian scenario {scenario} failed: {status}"
            );
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("guardian scenario {scenario} exceeded its deadline");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    if arguments.next().as_deref() != Some("--scenario") {
        for (scenario, _) in SCENARIOS {
            std::println!("guest session: {scenario}");
            isolated(scenario);
        }
        return;
    }
    let scenario = arguments.next().expect("scenario name");
    let (_, run) = SCENARIOS
        .iter()
        .find(|(name, _)| *name == scenario)
        .expect("known scenario");
    run();
}

const SCENARIOS: &[(&str, fn())] = &[
    (
        "assigned worker identity and one-shot Start",
        bounded_start_scenario,
    ),
    ("PTY ownership and Start dimensions", terminal_scenario),
    ("account and exec failures", failure_scenario),
    (
        "command completed before the guardian polled",
        completed_before_poll_scenario,
    ),
    (
        "exec failure retained after worker exit",
        exec_failure_before_poll_scenario,
    ),
    (
        "worker killed before exec after accepting Start",
        killed_before_exec_scenario,
    ),
    ("cancel before execution", cancel_scenario),
    (
        "adopted descendants outside the original process group",
        descendants_scenario,
    ),
    ("preparation deadline", preparation_timeout_scenario),
    (
        "renewed lease and lost client or broker",
        lease_and_disconnect_scenario,
    ),
    (
        "wrong Start preserves the preparation",
        wrong_start_scenario,
    ),
    (
        "assigned worker exit before Start",
        worker_exit_before_start_scenario,
    ),
];
