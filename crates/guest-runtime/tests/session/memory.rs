use lasper_guest_protocol::{
    BrokerId, CommandArgv, ExecutionKind, GuestProcess, PreparationId, PrepareRequest,
    PrepareRequestFrame, ProjectionRequests, StartFrame, StartRequest, MAX_COMMAND_ARGUMENT_BYTES,
    MAX_COMMAND_TOTAL_BYTES,
};
use lasper_guest_runtime::session::{
    SessionGuardian, SessionPhase, WorkerBootstrap, WorkerExit, WorkerFailure, WorkerStage,
    WORKER_STACK_BUDGET_BYTES,
};
use std::time::{Duration, Instant};

use super::fixture;

const PREPARATION: PreparationId = PreparationId::new([2; 16]);
const RLIMIT_STACK: i32 = 3;
const RLIMIT_AS: i32 = 9;

#[repr(C)]
struct ResourceLimit {
    current: usize,
    maximum: usize,
}

unsafe extern "C" {
    fn getrlimit(resource: i32, limit: *mut ResourceLimit) -> i32;
    fn setrlimit(resource: i32, limit: *const ResourceLimit) -> i32;
}

pub(super) fn limit_stack() -> std::io::Result<()> {
    limit(RLIMIT_STACK, WORKER_STACK_BUDGET_BYTES)
}

fn limit(resource: i32, bytes: usize) -> std::io::Result<()> {
    let mut limit = ResourceLimit {
        current: 0,
        maximum: 0,
    };
    if unsafe { getrlimit(resource, &mut limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    limit.current = bytes;
    if unsafe { setrlimit(resource, &limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn deny_allocation(bootstrap: WorkerBootstrap<'_>) -> ! {
    if let Err(error) = limit(RLIMIT_AS, 0) {
        bootstrap.fail(WorkerFailure::new(
            WorkerStage::Memory,
            error.raw_os_error().unwrap(),
        ));
    }
    fixture::entry(bootstrap)
}

fn request<'a>(argv: &[&'a str]) -> PrepareRequestFrame<'a> {
    PrepareRequestFrame::new(
        10,
        BrokerId::new([1; 16]),
        PrepareRequest::new(
            "alice",
            ExecutionKind::Bounded,
            GuestProcess::Command(CommandArgv::new(argv).unwrap()),
            None,
            None,
            ProjectionRequests::default(),
        )
        .unwrap(),
    )
}

fn allocation_failure() {
    let request = request(&["/bin/true"]);
    let mut worker =
        unsafe { SessionGuardian::spawn(request, PREPARATION, 0, deny_allocation) }.unwrap();
    let clock = Instant::now();
    loop {
        let state = worker.poll(clock.elapsed().as_millis() as u64).unwrap();
        assert!(
            !state.worker.prepared,
            "allocation failure published readiness"
        );
        if state.phase == SessionPhase::Closed {
            assert_eq!(
                state.worker.failure,
                Some(WorkerFailure::new(WorkerStage::Memory, 12))
            );
            assert_eq!(state.worker.exit, Some(WorkerExit::Exited(126)));
            return;
        }
        assert!(clock.elapsed() < Duration::from_secs(4), "{state:?}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn maximum_argv() {
    let mut arguments: Vec<String> = [
        "/bin/account-shell",
        "-c",
        "printf '%s:%s' \"$#\" \"$HOME\"",
        "budget",
    ]
    .map(str::to_owned)
    .into();
    let used = arguments.iter().map(String::len).sum::<usize>() + arguments.len() - 1;
    let mut remaining = MAX_COMMAND_TOTAL_BYTES - used;
    while remaining != 0 {
        let length = (remaining - 1).min(MAX_COMMAND_ARGUMENT_BYTES);
        arguments.push("x".repeat(length));
        remaining -= length + 1;
    }
    let argv: Vec<_> = arguments.iter().map(String::as_str).collect();
    let request = request(&argv);
    let mut worker =
        unsafe { SessionGuardian::spawn(request.clone(), PREPARATION, 0, fixture::entry) }.unwrap();
    let clock = Instant::now();
    let mut started = false;
    let mut output = Vec::new();
    loop {
        let now = clock.elapsed().as_millis() as u64;
        let state = worker.poll(now).unwrap();
        assert_eq!(state.worker.failure, None, "{state:?}");
        assert_eq!(state.worker.error, None, "{state:?}");
        if !started && state.phase == SessionPhase::Prepared {
            let _ready = fixture::receive_ready(&mut worker, now);
            let start = StartRequest::new(&request, PREPARATION, None, None, None).unwrap();
            worker.start(now, StartFrame::new(11, start)).unwrap();
            started = true;
        }
        let mut bytes = [0; 128];
        match worker.read_stdout(&mut bytes) {
            Ok(length) => output.extend_from_slice(&bytes[..length]),
            Err(lasper_guest_runtime::session::GuardianError::System { errno: 11, .. }) => {}
            Err(error) => panic!("read stdout: {error:?}"),
        }
        if state.phase == SessionPhase::Closed {
            assert!(started);
            assert_eq!(state.worker.exit, Some(WorkerExit::Exited(0)));
            assert_eq!(
                output,
                format!("{}:/home/alice", arguments.len() - 4).as_bytes()
            );
            return;
        }
        assert!(clock.elapsed() < Duration::from_secs(4), "{state:?}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

pub(super) const SCENARIOS: &[(&str, fn())] = &[
    (
        "workspace allocation failure is reported before Prepared",
        allocation_failure,
    ),
    ("maximum argv executes on the budgeted stack", maximum_argv),
];
