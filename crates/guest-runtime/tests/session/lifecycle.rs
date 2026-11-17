use lasper_guest_protocol::{
    BrokerId, CommandArgv, ExecutionKind, GuestProcess, PreparationId, PrepareRequest,
    PrepareRequestFrame, StartFrame, StartRequest, TerminalSize, MAX_EXECUTION_TIMEOUT_MS,
    MAX_IDENTITY_MESSAGE_BYTES,
};
use lasper_guest_runtime::session::*;
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use std::vec::Vec;

use super::fixture::{self, entry};

const PREPARATION: PreparationId = PreparationId::new([2; 16]);

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    returned: i16,
}

unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
    fn getpgid(pid: i32) -> i32;
    fn pause() -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn ioctl(fd: i32, operation: usize, ...) -> i32;
    fn poll(fds: *mut PollFd, count: usize, timeout_ms: i32) -> i32;
    fn fcntl(fd: i32, operation: i32, ...) -> i32;
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
    let size = if execution == ExecutionKind::Terminal {
        Some(TerminalSize::new(80, 24).unwrap())
    } else {
        None
    };
    PrepareRequestFrame::new(
        10,
        BrokerId::new([1; 16]),
        PrepareRequest::new(
            "alice",
            execution,
            GuestProcess::Command(CommandArgv::new(argv).unwrap()),
            lasper_guest_protocol::ProcessEnvironment::default(),
            size,
        )
        .unwrap(),
    )
}

fn start(request: &PrepareRequestFrame<'_>) -> StartFrame {
    start_with_timeout(request, None)
}

fn start_with_timeout(request: &PrepareRequestFrame<'_>, timeout_ms: Option<u64>) -> StartFrame {
    StartFrame::new(
        11,
        StartRequest::new(
            request,
            PREPARATION,
            request.request().initial_size(),
            timeout_ms,
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

fn blocked_entry(_: WorkerBootstrap<'_>) -> ! {
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
    worker: &mut SessionGuardian<'_, '_>,
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

fn pipe_output(mut pipe: std::fs::File) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut output = Vec::new();
    loop {
        let mut bytes = [0; 1024];
        match pipe.read(&mut bytes) {
            Ok(0) => return output,
            Ok(length) => output.extend_from_slice(&bytes[..length]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "pipe never closed");
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("read pipe: {error}"),
        }
    }
}

fn finish(
    worker: &mut SessionGuardian<'_, '_>,
    request: &PrepareRequestFrame<'_>,
) -> WorkerObservation {
    let deadline = Instant::now() + Duration::from_secs(4);
    if matches!(
        worker.phase(),
        SessionPhase::Preparing
            | SessionPhase::Prepared
            | SessionPhase::DeliveringReady
            | SessionPhase::AwaitingStart
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
            assert!(worker.next_deadline_ms().is_none());
            return observation;
        }
        assert!(
            Instant::now() < deadline,
            "cleanup remained pending: {observation:?}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn bounded_start_scenario(context: &mut GuardianContext) {
    let request = request(
        ExecutionKind::Piped,
        &["/bin/sh", "-c", "printf '%s' \"$$\""],
    );
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let ready = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    assert_eq!(ready.frame.descriptor_count(), 4);
    assert_eq!(
        ready.stdout().read(&mut [0; 8]).unwrap_err().raw_os_error(),
        Some(EAGAIN)
    );
    let identity: UnixDatagram = ready.descriptors[0].try_clone().unwrap().into();
    identity
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut packet = vec![0; MAX_IDENTITY_MESSAGE_BYTES];
    let profile = ready.profile;
    for nonce in [1, 2] {
        let challenge = ready.frame.identity_challenge([nonce; 32]);
        let length = profile
            .encode_identity_challenge(challenge, &mut packet)
            .unwrap();
        identity.send(&packet[..length]).unwrap();
        let length = identity.recv(&mut packet).unwrap();
        let reply = profile
            .decode_identity_reply(&packet[..length], challenge, request.request())
            .unwrap();
        assert_eq!(reply.guest_user(), "alice");
        assert_eq!(
            (reply.guest_uid(), reply.guest_gid()),
            (fixture::get().uid, fixture::get().gid)
        );
        assert!(worker.poll(now_ms()).unwrap().worker.prepared);
    }
    worker.start(now_ms(), start(&request)).unwrap();
    assert!(matches!(
        worker.start(now_ms(), start(&request)),
        Err(GuardianError::Lifecycle(
            SessionLifecycleError::InvalidTransition { .. }
        ))
    ));
    let output = pipe_output(ready.stdout());
    let observation = wait_for(&mut worker, |observation| observation.exit.is_some());
    let error = pipe_output(ready.stderr());
    assert_eq!(
        output,
        std::format!("{}", worker.pid()).as_bytes(),
        "{observation:?}; {}",
        String::from_utf8_lossy(&error)
    );
    assert_eq!(observation.exit, Some(WorkerExit::Exited(0)));
    assert!(matches!(
        observation.exec,
        Some(ExecOutcome::Started | ExecOutcome::Unknown)
    ));
    finish(&mut worker, &request);
}

fn terminal_scenario(context: &mut GuardianContext) {
    let request = request(ExecutionKind::Terminal, &["/bin/sh", "-c", "stty size"]);
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let ready = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    assert_eq!(ready.descriptors.len(), 3);
    let master = ready.descriptors[0].try_clone().unwrap();
    for operation in [0x5429, 0x540f] {
        // TIOCGSID, TIOCGPGRP
        let mut terminal_pid = 0i32;
        assert_eq!(
            unsafe { ioctl(master.as_raw_fd(), operation, &mut terminal_pid) },
            0
        );
        assert_eq!(terminal_pid, worker.pid());
    }
    let start = StartRequest::new(
        &request,
        PREPARATION,
        Some(TerminalSize::new(101, 37).unwrap()),
        None,
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

fn ready_delivery_scenario(context: &mut GuardianContext, execution: ExecutionKind) {
    let request = request(execution, &["/bin/true"]);
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    assert_eq!(worker.phase(), SessionPhase::Prepared);
    assert!(matches!(
        worker.start(now_ms(), start(&request)),
        Err(GuardianError::Lifecycle(
            SessionLifecycleError::InvalidTransition {
                phase: SessionPhase::Prepared
            }
        ))
    ));
    assert_eq!(worker.phase(), SessionPhase::Prepared);
    let pidfd = worker.poll_descriptors()[1];
    let received = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    assert_eq!(
        received.descriptors.len(),
        received.frame.descriptor_count() as usize
    );
    assert_eq!(worker.phase(), SessionPhase::AwaitingStart);
    assert!(unsafe { fcntl(pidfd, 1) } >= 0);
    assert_eq!(
        worker
            .deliver_ready(
                now_ms(),
                &mut lasper_guest_runtime::transport::IoBudget::new()
            )
            .err(),
        Some(GuardianError::ReadyAlreadyDelivered)
    );
    worker.start(now_ms(), start(&request)).unwrap();
    finish(&mut worker, &request);
    drop(worker);
    assert_eq!(unsafe { fcntl(pidfd, 1) }, -1);
    assert!(unsafe { fcntl(received.descriptors.last().unwrap().as_raw_fd(), 1) } >= 0);
}

fn abandoned_ready_scenario(context: &mut GuardianContext, fail_delivery: bool) {
    let request = request(
        ExecutionKind::Piped,
        &[
            "/bin/sh",
            "-c",
            "printf must-not-run > /home/alice/unexpected-execution",
        ],
    );
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let budget = if fail_delivery { 1 } else { 0 };
    assert!(worker
        .deliver_ready(
            now_ms(),
            &mut lasper_guest_runtime::transport::IoBudget::with_bytes(budget)
        )
        .unwrap()
        .is_pending());
    assert_eq!(worker.phase(), SessionPhase::DeliveringReady);
    worker.connection_closed(now_ms(), SessionConnection::Client);
    assert!(worker.start(now_ms(), start(&request)).is_err());
    assert_eq!(
        worker.poll(now_ms()).unwrap().cleanup_reason,
        Some(CleanupReason::ClientClosed)
    );
    finish(&mut worker, &request);
    assert!(!fixture::get()
        .path("home/alice/unexpected-execution")
        .exists());
}

fn expired_ready_scenario(context: &mut GuardianContext) {
    let request = request(
        ExecutionKind::Piped,
        &["/bin/sh", "-c", "printf must-not-run"],
    );
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let deadline = worker.next_deadline_ms().unwrap();
    assert_eq!(
        worker
            .deliver_ready(
                advance_to(deadline),
                &mut lasper_guest_runtime::transport::IoBudget::new()
            )
            .err(),
        Some(GuardianError::NotPrepared)
    );
    assert_eq!(worker.phase(), SessionPhase::Closing);
    assert_eq!(
        finish(&mut worker, &request).cleanup,
        CleanupProgress::Complete
    );
}

fn exec_failure_scenario(context: &mut GuardianContext) {
    let request = request(ExecutionKind::Piped, &["/not/a/real/guest/program"]);
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let _ready = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    worker.start(now_ms(), start(&request)).unwrap();
    let observation = wait_for(&mut worker, |observation| observation.failure.is_some());
    assert_eq!(
        observation.failure,
        Some(WorkerFailure::new(WorkerStage::Exec, 2))
    );
    assert_eq!(observation.exec, Some(ExecOutcome::Failed { errno: 2 }));
    finish(&mut worker, &request);
}

fn account_failure_scenario(context: &mut GuardianContext) {
    let request = request(ExecutionKind::Piped, &["/bin/true"]);
    std::fs::remove_file(fixture::get().path("etc/passwd")).unwrap();
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    let observation = wait_for(&mut worker, |observation| observation.failure.is_some());
    assert_eq!(
        observation.failure,
        Some(WorkerFailure::new(WorkerStage::Account, 2))
    );
    assert!(!observation.prepared);
    assert_eq!(
        worker
            .deliver_ready(
                now_ms(),
                &mut lasper_guest_runtime::transport::IoBudget::new()
            )
            .err(),
        Some(GuardianError::NotPrepared)
    );
    finish(&mut worker, &request);
}

fn completed_before_poll_scenario(
    context: &mut GuardianContext,
    argv: &[&'static str],
    exit: WorkerExit,
) {
    let request = request(ExecutionKind::Piped, argv);
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let ready = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    let pidfd = ready.descriptors[1].try_clone().unwrap();
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

fn exec_failure_before_poll_scenario(context: &mut GuardianContext) {
    let request = request(ExecutionKind::Piped, &["/not/a/real/guest/program"]);
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let ready = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    let pidfd = ready.descriptors[1].try_clone().unwrap();
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

fn killed_before_exec_scenario(context: &mut GuardianContext) {
    let request = request(ExecutionKind::Piped, &["/bin/true"]);
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let ready = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    let pidfd = ready.descriptors[1].try_clone().unwrap();
    assert_eq!(unsafe { kill(worker.pid(), 19) }, 0);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let mut status = 0;
        // Observe SIGSTOP without reaping the worker. Start can now be accepted
        // by the guardian, but the worker cannot consume it before SIGKILL.
        let result = unsafe { waitpid(worker.pid(), &mut status, 1 | 2) };
        assert!(result >= 0);
        if result == worker.pid() {
            assert_eq!(status & 0xff, 0x7f);
            assert_eq!((status >> 8) & 0xff, 19);
            break;
        }
        assert!(Instant::now() < deadline, "worker did not stop");
        std::thread::sleep(Duration::from_millis(2));
    }
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

fn cancel_scenario(context: &mut GuardianContext) {
    let request = request(
        ExecutionKind::Piped,
        &[
            "/bin/sh",
            "-c",
            "printf should-not-run > /home/alice/unexpected-execution",
        ],
    );
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
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
    assert!(!fixture::get()
        .path("home/alice/unexpected-execution")
        .exists());
}

fn descendants_scenario(context: &mut GuardianContext) {
    let request = request(ExecutionKind::Piped, &["/bin/sh", "-c",
        "setsid --fork /bin/sh -c 'trap \"\" HUP TERM; printf \"%s\" \"$$\"; exec >/home/alice/detached.out 2>&1; while :; do sleep 1; done'; exit 23"]);
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let ready = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    worker.start(now_ms(), start(&request)).unwrap();
    let output = pipe_output(ready.stdout());
    let error = pipe_output(ready.stderr());
    assert!(
        !output.is_empty(),
        "descendant did not report its PID: {}",
        String::from_utf8_lossy(&error)
    );
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

fn preparation_timeout_scenario(context: &mut GuardianContext) {
    let request = request(ExecutionKind::Piped, &["/bin/true"]);
    let accepted_at = context.hello_completed_at_ms() + 4_900;
    let mut worker = context
        .spawn(&request, PREPARATION, accepted_at, blocked_entry)
        .unwrap();
    assert_eq!(worker.phase(), SessionPhase::Preparing);
    assert_eq!(worker.next_deadline_ms(), Some(accepted_at + 5_000));
    assert_eq!(
        worker
            .deliver_ready(
                advance_to(accepted_at + 4_999),
                &mut lasper_guest_runtime::transport::IoBudget::new()
            )
            .err(),
        Some(GuardianError::NotPrepared)
    );
    assert_eq!(
        worker.poll(advance_to(accepted_at + 5_000)).unwrap().phase,
        SessionPhase::Closing
    );
    assert_eq!(
        worker
            .deliver_ready(
                now_ms(),
                &mut lasper_guest_runtime::transport::IoBudget::new()
            )
            .err(),
        Some(GuardianError::NotPrepared)
    );
    let result = worker.poll(advance_to(accepted_at + 6_001)).unwrap();
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

fn lease_and_disconnect_scenario(context: &mut GuardianContext, connection: SessionConnection) {
    let request = request(ExecutionKind::Piped, &["/bin/sh", "-c", "exec sleep 30"]);
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let deadline = worker.next_deadline_ms().unwrap();
    let ready = fixture::receive_ready(&mut worker, context.connection(), advance_to(deadline - 1));
    assert!((1..=45_000).contains(&ready.frame.response().remaining_lease_ms()));
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
        SessionConnection::Supervisor => CleanupReason::SupervisorStopped,
    };
    assert_eq!(worker.poll(now_ms()).unwrap().cleanup_reason, Some(reason));
}

fn unlimited_execution_scenario(context: &mut GuardianContext, execution: ExecutionKind) {
    let request = request(execution, &["/bin/sleep", "30"]);
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let _ready = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    let accepted_at = now_ms();
    worker.start(accepted_at, start(&request)).unwrap();
    assert_eq!(worker.next_deadline_ms(), None);
    wait_for(&mut worker, |observation| {
        observation.exec == Some(ExecOutcome::Started)
    });

    let observation = worker.poll(advance_to(accepted_at + 120_000)).unwrap();
    assert_eq!(observation.phase, SessionPhase::Running);
    assert_eq!(observation.cleanup_reason, None);
    assert_eq!(observation.worker.exit, None);
    assert_eq!(worker.next_deadline_ms(), None);
    assert_eq!(
        finish(&mut worker, &request).cleanup,
        CleanupProgress::Complete
    );
}

fn execution_timeout_scenario(context: &mut GuardianContext, execution: ExecutionKind) {
    let request = request(
        execution,
        &[
            "/bin/sh",
            "-c",
            "trap '' HUP TERM; setsid --fork /bin/sh -c 'printf \"%s\" \"$$\" > /home/alice/timeout-child; exec /bin/sleep 30'; exec /bin/sleep 30",
        ],
    );
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let _ready = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    let start = start_with_timeout(&request, Some(1_000));
    let accepted_at = advance_to(now_ms() + 10_000);
    assert_eq!(
        worker.poll(accepted_at).unwrap().phase,
        SessionPhase::AwaitingStart
    );
    worker.start(accepted_at, start).unwrap();
    assert_eq!(worker.next_deadline_ms(), Some(accepted_at + 1_000));
    wait_for(&mut worker, |observation| {
        observation.exec == Some(ExecOutcome::Started)
    });
    let deadline = Instant::now() + Duration::from_secs(4);
    let descendant = loop {
        if let Ok(contents) =
            std::fs::read_to_string(fixture::get().path("home/alice/timeout-child"))
        {
            if let Ok(pid) = contents.parse::<i32>() {
                break pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "worker did not start its descendant"
        );
        std::thread::sleep(Duration::from_millis(2));
    };
    assert!(descendant > 0);
    assert_ne!(unsafe { getpgid(descendant) }, worker.pid());
    assert_eq!(
        worker.poll(advance_to(accepted_at + 999)).unwrap().phase,
        SessionPhase::Running
    );
    let observation = worker.poll(advance_to(accepted_at + 1_000)).unwrap();
    assert_eq!(
        observation.cleanup_reason,
        Some(CleanupReason::ExecutionTimedOut)
    );
    assert!(matches!(
        observation.phase,
        SessionPhase::Closing | SessionPhase::CleanupPending
    ));
    let finished = finish(&mut worker, &request);
    assert_eq!(finished.cleanup, CleanupProgress::Complete);
    assert_eq!(
        worker.poll(now_ms()).unwrap().cleanup_reason,
        Some(CleanupReason::ExecutionTimedOut)
    );
    assert_eq!(unsafe { kill(descendant, 0) }, -1);
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(3));
    assert_eq!(unsafe { waitpid(-1, std::ptr::null_mut(), 1) }, -1);
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(10));
}

fn maximum_execution_timeout_scenario(context: &mut GuardianContext, execution: ExecutionKind) {
    let request = request(execution, &["/bin/sleep", "30"]);
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let _ready = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    let accepted_at = advance_to(now_ms() + 10_000);
    worker
        .start(
            accepted_at,
            start_with_timeout(&request, Some(MAX_EXECUTION_TIMEOUT_MS)),
        )
        .unwrap();
    let deadline = accepted_at + MAX_EXECUTION_TIMEOUT_MS;
    assert_eq!(worker.next_deadline_ms(), Some(deadline));
    wait_for(&mut worker, |observation| {
        observation.exec == Some(ExecOutcome::Started)
    });
    let observation = worker.poll(advance_to(accepted_at + 120_000)).unwrap();
    assert_eq!(observation.phase, SessionPhase::Running);
    assert_eq!(observation.cleanup_reason, None);
    assert_eq!(worker.next_deadline_ms(), Some(deadline));
    finish(&mut worker, &request);
}

fn wrong_start_scenario(context: &mut GuardianContext) {
    let request = request(ExecutionKind::Piped, &["/bin/true"]);
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let _ready = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    let wrong = StartRequest::new(&request, PreparationId::new([3; 16]), None, None).unwrap();
    assert!(matches!(
        worker.start(now_ms(), StartFrame::new(11, wrong)),
        Err(GuardianError::InvalidStart(
            lasper_guest_protocol::SessionPayloadError::PreparationMismatch
        ))
    ));
    assert_eq!(worker.phase(), SessionPhase::AwaitingStart);
    worker.start(now_ms(), start(&request)).unwrap();
    finish(&mut worker, &request);
}

fn worker_exit_before_start_scenario(context: &mut GuardianContext) {
    let request = request(
        ExecutionKind::Piped,
        &[
            "/bin/sh",
            "-c",
            "printf should-not-run > /home/alice/unexpected-execution",
        ],
    );
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    assert_eq!(unsafe { kill(worker.pid(), 9) }, 0);
    let observation = wait_for(&mut worker, |observation| observation.exit.is_some());
    assert_eq!(observation.exit, Some(WorkerExit::Signaled(9)));
    assert_eq!(observation.exec, None);
    assert!(!observation.prepared);
    assert!(worker.start(now_ms(), start(&request)).is_err());
    finish(&mut worker, &request);
    assert!(!fixture::get()
        .path("home/alice/unexpected-execution")
        .exists());
}

pub(super) const SCENARIOS: &[(&str, super::ScenarioFn)] = &[
    (
        "interactive Ready owns exported endpoints and is one-shot",
        |context| ready_delivery_scenario(context, ExecutionKind::Terminal),
    ),
    (
        "bounded Ready owns exported endpoints and is one-shot",
        |context| ready_delivery_scenario(context, ExecutionKind::Piped),
    ),
    ("abandoned Ready enters cleanup", |context| {
        abandoned_ready_scenario(context, false)
    }),
    ("failed Ready delivery enters cleanup", |context| {
        abandoned_ready_scenario(context, true)
    }),
    ("expired Ready enters cleanup", expired_ready_scenario),
    (
        "assigned worker identity and one-shot Start",
        bounded_start_scenario,
    ),
    ("PTY ownership and Start dimensions", terminal_scenario),
    ("bounded execution is unlimited by default", |context| {
        unlimited_execution_scenario(context, ExecutionKind::Piped)
    }),
    ("interactive execution is unlimited by default", |context| {
        unlimited_execution_scenario(context, ExecutionKind::Terminal)
    }),
    (
        "bounded execution timeout starts at Start and reaps descendants",
        |context| execution_timeout_scenario(context, ExecutionKind::Piped),
    ),
    (
        "interactive execution timeout starts at Start and reaps descendants",
        |context| execution_timeout_scenario(context, ExecutionKind::Terminal),
    ),
    (
        "bounded execution accepts the protocol's maximum timeout",
        |context| maximum_execution_timeout_scenario(context, ExecutionKind::Piped),
    ),
    (
        "interactive execution accepts the protocol's maximum timeout",
        |context| maximum_execution_timeout_scenario(context, ExecutionKind::Terminal),
    ),
    ("guest exec failure", exec_failure_scenario),
    ("guest account failure", account_failure_scenario),
    (
        "successful command completed before the guardian polled",
        |context| completed_before_poll_scenario(context, &["/bin/true"], WorkerExit::Exited(0)),
    ),
    (
        "failed command completed before the guardian polled",
        |context| {
            completed_before_poll_scenario(
                context,
                &["/bin/sh", "-c", "exit 37"],
                WorkerExit::Exited(37),
            )
        },
    ),
    (
        "signaled command completed before the guardian polled",
        |context| {
            completed_before_poll_scenario(
                context,
                &["/bin/sh", "-c", "kill -TERM $$"],
                WorkerExit::Signaled(15),
            )
        },
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
    ("renewed lease and lost client", |context| {
        lease_and_disconnect_scenario(context, SessionConnection::Client)
    }),
    ("renewed lease and lost broker", |context| {
        lease_and_disconnect_scenario(context, SessionConnection::Broker)
    }),
    (
        "wrong Start preserves the preparation",
        wrong_start_scenario,
    ),
    (
        "assigned worker exit before Start",
        worker_exit_before_start_scenario,
    ),
];
