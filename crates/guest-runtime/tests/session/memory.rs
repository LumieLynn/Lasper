use lasper_guest_protocol::{
    BrokerId, CommandArgv, ExecutionKind, GuestProcess, PreparationId, PrepareRequest,
    PrepareRequestFrame, StartFrame, StartRequest, MAX_COMMAND_ARGUMENT_BYTES,
    MAX_COMMAND_TOTAL_BYTES,
};
use lasper_guest_runtime::session::{
    GuardianContext, SessionPhase, WorkerBootstrap, WorkerExit, WorkerFailure, WorkerStage,
    WORKER_STACK_BUDGET_BYTES,
};
use std::io::Read;
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

fn deny_environment_allocation(bootstrap: WorkerBootstrap<'_>) -> ! {
    let capacities = lasper_guest_protocol::V1_0_CAPACITIES;
    let pointer_offset = (capacities.prepare_unescape_bytes()
        + capacities.merged_environment_bytes())
    .next_multiple_of(core::mem::align_of::<*const u8>());
    let bytes = (pointer_offset
        + (6 + capacities.environment_entries + 1) * core::mem::size_of::<*const u8>())
    .next_multiple_of(64 * 1024);
    let instruction = |code, k, jf| libc::sock_filter { code, jt: 0, jf, k };
    // This fault targets the environment mapping only. WorkerMemory and account
    // setup still succeed, so the test distinguishes the two allocation stages.
    let filter = [
        instruction((libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, 0, 0),
        instruction(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            libc::SYS_mmap as u32,
            3,
        ),
        instruction(
            (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            (core::mem::offset_of!(libc::seccomp_data, args) + core::mem::size_of::<u64>()) as u32,
            0,
        ),
        instruction(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            bytes as u32,
            1,
        ),
        instruction(
            (libc::BPF_RET | libc::BPF_K) as u16,
            libc::SECCOMP_RET_ERRNO | libc::ENOMEM as u32,
            0,
        ),
        instruction(
            (libc::BPF_RET | libc::BPF_K) as u16,
            libc::SECCOMP_RET_ALLOW,
            0,
        ),
    ];
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr().cast_mut(),
    };
    // SAFETY: Both prctl calls borrow only this live filter; the child exits
    // without returning to the test harness and the kernel copies the program.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
        || unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) } != 0
    {
        bootstrap.fail(WorkerFailure::new(WorkerStage::Signals, libc::EIO));
    }
    fixture::entry(bootstrap)
}

fn request<'a>(argv: &[&'a str]) -> PrepareRequestFrame<'a> {
    PrepareRequestFrame::new(
        10,
        BrokerId::new([1; 16]),
        PrepareRequest::new(
            "alice",
            ExecutionKind::Piped,
            GuestProcess::Command(CommandArgv::new(argv).unwrap()),
            lasper_guest_protocol::ProcessEnvironment::default(),
            None,
        )
        .unwrap(),
    )
}

fn allocation_failure(context: &mut GuardianContext) {
    allocation_failure_with(context, deny_allocation);
}

fn allocation_failure_with(context: &mut GuardianContext, entry: fn(WorkerBootstrap<'_>) -> !) {
    let request = request(&["/bin/true"]);
    let mut worker = context.spawn(&request, PREPARATION, 0, entry).unwrap();
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
            assert_eq!(state.worker.exit, Some(WorkerExit::Exited(71)));
            return;
        }
        assert!(clock.elapsed() < Duration::from_secs(4), "{state:?}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn maximum_argv(context: &mut GuardianContext) {
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
    assert_eq!(
        run_command(context, &request),
        format!("{}:/home/alice", arguments.len() - 4).as_bytes()
    );
}

fn maximum_environment(context: &mut GuardianContext) {
    use lasper_guest_protocol::{
        ProcessEnvironment, MAX_PREPARE_UNESCAPE_BYTES, MAX_PROCESS_ENVIRONMENT_BYTES,
    };
    let entries = [("HOME", "/chosen"), ("PAYLOAD", ""), ("TERM", "")];
    let overhead: usize = entries
        .iter()
        .map(|(name, value)| name.len() + value.len() + 2)
        .sum();
    let value = "\n".repeat(MAX_PROCESS_ENVIRONMENT_BYTES - overhead);
    let request = PrepareRequestFrame::new(
        10,
        BrokerId::new([1; 16]),
        PrepareRequest::new(
            "alice",
            ExecutionKind::Piped,
            GuestProcess::Command(
                CommandArgv::new(&[
                    "/bin/account-shell",
                    "-c",
                    "printf '%s:%s:<%s>' \"${#PAYLOAD}\" \"$HOME\" \"${TERM-unset}\"",
                ])
                .unwrap(),
            ),
            ProcessEnvironment::new([("HOME", "/chosen"), ("PAYLOAD", &value), ("TERM", "")])
                .unwrap(),
            None,
        )
        .unwrap(),
    );
    let protocol = context.negotiated_protocol();
    let mut payload = vec![0; lasper_guest_protocol::MAX_CONTROL_FRAME_BYTES as usize];
    let length = protocol
        .encode_prepare_frame(&request, &mut payload)
        .unwrap();
    let frame = lasper_guest_protocol::ControlFrame::decode(&payload[..length]).unwrap();
    let decoded = protocol
        .decode_prepare_frame(
            frame.header(),
            frame.payload(),
            0,
            request.broker_id(),
            &mut vec![0; MAX_PREPARE_UNESCAPE_BYTES],
        )
        .unwrap();
    assert_eq!(
        run_command(context, &decoded),
        format!("{}:/chosen:<>", value.len()).as_bytes()
    );
}

fn run_command(context: &mut GuardianContext, request: &PrepareRequestFrame<'_>) -> Vec<u8> {
    let mut worker = context
        .spawn(request, PREPARATION, 0, fixture::entry)
        .unwrap();
    let clock = Instant::now();
    let mut started = false;
    let mut output = Vec::new();
    let mut stdout = None;
    loop {
        let now = clock.elapsed().as_millis() as u64;
        let state = worker.poll(now).unwrap();
        assert_eq!(state.worker.failure, None, "{state:?}");
        assert_eq!(state.worker.error, None, "{state:?}");
        if !started && state.phase == SessionPhase::Prepared {
            let ready = fixture::receive_ready(&mut worker, context.connection(), now);
            stdout = Some(ready.stdout());
            let start = StartRequest::new(request, PREPARATION, None, None).unwrap();
            worker.start(now, StartFrame::new(11, start)).unwrap();
            started = true;
        }
        let mut bytes = [0; 128];
        match stdout
            .as_mut()
            .map(|pipe| pipe.read(&mut bytes))
            .transpose()
        {
            Ok(Some(length)) => output.extend_from_slice(&bytes[..length]),
            Ok(None) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("read stdout: {error:?}"),
        }
        if state.phase == SessionPhase::Closed {
            assert!(started);
            assert_eq!(state.worker.exit, Some(WorkerExit::Exited(0)));
            return output;
        }
        assert!(clock.elapsed() < Duration::from_secs(4), "{state:?}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

pub(super) const SCENARIOS: &[(&str, super::ScenarioFn)] = &[
    (
        "workspace allocation failure is reported before Prepared",
        allocation_failure,
    ),
    (
        "environment allocation failure is reported before Prepared",
        |context| allocation_failure_with(context, deny_environment_allocation),
    ),
    ("maximum argv executes on the budgeted stack", maximum_argv),
    (
        "maximum escaped environment executes on the budgeted stack",
        maximum_environment,
    ),
];
