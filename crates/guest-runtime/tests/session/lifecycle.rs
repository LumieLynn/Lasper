use lasper_guest_protocol::{
    BrokerId, CommandArgv, ExecutionKind, GuestIdentityReply, GuestProcess, PreparationId,
    PrepareRequest, PrepareRequestFrame, ProjectionRequests, StartFrame, StartRequest,
    TerminalEnvironment, TerminalSize, MAX_IDENTITY_MESSAGE_BYTES,
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

fn bounded_stdout(worker: &mut SessionGuardian<'_, '_>) -> Vec<u8> {
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
        ExecutionKind::Bounded,
        &["/bin/sh", "-c", "printf '%s' \"$$\""],
    );
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    assert!(matches!(
        worker.read_stdout(&mut [0; 8]),
        Err(GuardianError::System { errno: EAGAIN, .. })
    ));
    let ready = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    assert_eq!(ready.frame.descriptor_count(), 2);
    let identity: UnixDatagram = ready.descriptors[0].try_clone().unwrap().into();
    identity
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut packet = vec![0; MAX_IDENTITY_MESSAGE_BYTES];
    for nonce in [1, 2] {
        let challenge = ready.frame.identity_challenge([nonce; 32]);
        let length = challenge.encode_packet(&mut packet).unwrap();
        identity.send(&packet[..length]).unwrap();
        let length = identity.recv(&mut packet).unwrap();
        let reply = GuestIdentityReply::decode_for(&packet[..length], challenge, request.request())
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
    let output = bounded_stdout(&mut worker);
    let observation = wait_for(&mut worker, |observation| observation.exit.is_some());
    let mut error = [0; 1024];
    let error_length = worker.read_stderr(&mut error).unwrap();
    assert_eq!(
        output,
        std::format!("{}", worker.pid()).as_bytes(),
        "{observation:?}; {}",
        String::from_utf8_lossy(&error[..error_length])
    );
    assert_eq!(observation.exit, Some(WorkerExit::Exited(0)));
    assert!(matches!(
        observation.exec,
        Some(ExecOutcome::Started | ExecOutcome::Unknown)
    ));
    finish(&mut worker, &request);
}

fn terminal_scenario(context: &mut GuardianContext) {
    let request = request(ExecutionKind::Interactive, &["/bin/sh", "-c", "stty size"]);
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
        Some(GuardianError::ReadyAlreadyTaken)
    );
    worker.start(now_ms(), start(&request)).unwrap();
    finish(&mut worker, &request);
    drop(worker);
    assert_eq!(unsafe { fcntl(pidfd, 1) }, -1);
    assert!(unsafe { fcntl(received.descriptors.last().unwrap().as_raw_fd(), 1) } >= 0);
}

fn abandoned_ready_scenario(context: &mut GuardianContext, fail_delivery: bool) {
    let request = request(
        ExecutionKind::Bounded,
        &["/bin/sh", "-c", "printf must-not-run"],
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
    assert!(bounded_stdout(&mut worker).is_empty());
}

fn expired_ready_scenario(context: &mut GuardianContext) {
    let request = request(
        ExecutionKind::Bounded,
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
    let request = request(ExecutionKind::Bounded, &["/not/a/real/guest/program"]);
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
    let request = request(ExecutionKind::Bounded, &["/bin/true"]);
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
    let request = request(ExecutionKind::Bounded, argv);
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let ready = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    let pidfd = ready.descriptors.last().unwrap().try_clone().unwrap();
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
    let request = request(ExecutionKind::Bounded, &["/not/a/real/guest/program"]);
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let ready = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    let pidfd = ready.descriptors.last().unwrap().try_clone().unwrap();
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
    let request = request(ExecutionKind::Bounded, &["/bin/true"]);
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let ready = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    let pidfd = ready.descriptors.last().unwrap().try_clone().unwrap();
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
        ExecutionKind::Bounded,
        &["/bin/sh", "-c", "printf should-not-run"],
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
    assert!(bounded_stdout(&mut worker).is_empty());
}

fn descendants_scenario(context: &mut GuardianContext) {
    let request = request(ExecutionKind::Bounded, &["/bin/sh", "-c",
        "setsid --fork /bin/sh -c 'trap \"\" HUP TERM; printf \"%s\" \"$$\"; exec >/home/alice/detached.out 2>&1; while :; do sleep 1; done'; exit 23"]);
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let _ready = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    worker.start(now_ms(), start(&request)).unwrap();
    let output = bounded_stdout(&mut worker);
    let mut error = [0; 1024];
    let error_length = worker.read_stderr(&mut error).unwrap();
    assert!(
        !output.is_empty(),
        "descendant did not report its PID: {}",
        String::from_utf8_lossy(&error[..error_length])
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
    let request = request(ExecutionKind::Bounded, &["/bin/true"]);
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
    let request = request(ExecutionKind::Bounded, &["/bin/sh", "-c", "exec sleep 30"]);
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
    assert!(worker
        .next_deadline_ms()
        .is_some_and(|deadline| deadline > now_ms()));
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

fn wrong_start_scenario(context: &mut GuardianContext) {
    let request = request(ExecutionKind::Bounded, &["/bin/true"]);
    let mut worker = context
        .spawn(&request, PREPARATION, now_ms(), entry)
        .unwrap();
    wait_for(&mut worker, |observation| observation.prepared);
    let _ready = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    let wrong = StartRequest::new(&request, PreparationId::new([3; 16]), None, None, None).unwrap();
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
        ExecutionKind::Bounded,
        &["/bin/sh", "-c", "printf should-not-run"],
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
    assert!(bounded_stdout(&mut worker).is_empty());
}

pub(super) const SCENARIOS: &[(&str, super::ScenarioFn)] = &[
    (
        "interactive Ready owns exported endpoints and is one-shot",
        |context| ready_delivery_scenario(context, ExecutionKind::Interactive),
    ),
    (
        "bounded Ready owns exported endpoints and is one-shot",
        |context| ready_delivery_scenario(context, ExecutionKind::Bounded),
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
