use core::mem::size_of;
use std::io::Read;
use std::process::Stdio;

use super::{command, guardian_fixture::GuardianProcess};

const PTRACE_TRACEME: u32 = 0;
const PTRACE_DETACH: u32 = 17;
const PTRACE_SYSCALL: u32 = 24;
const PTRACE_SETOPTIONS: u32 = 0x4200;
const PTRACE_GET_SYSCALL_INFO: u32 = 0x420e;
const PTRACE_O_TRACESYSGOOD: usize = 1;
const SIGSTOP: i32 = 19;
const SYSCALL_STOP: i32 = 5 | 0x80;

#[cfg(target_arch = "x86_64")]
const RT_SIGACTION: u64 = 13;
#[cfg(target_arch = "aarch64")]
const RT_SIGACTION: u64 = 134;
#[cfg(target_arch = "x86_64")]
const FCNTL: u64 = 72;
#[cfg(target_arch = "aarch64")]
const FCNTL: u64 = 25;

unsafe extern "C" {
    fn ptrace(request: u32, ...) -> isize;
    fn raise(signal: i32) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
}

#[repr(C)]
#[derive(Default)]
struct SyscallInfo {
    kind: u8,
    reserved: [u8; 3],
    architecture: u32,
    instruction_pointer: u64,
    stack_pointer: u64,
    data: [u64; 8],
}

fn trace(request: u32, pid: i32, address: usize, data: usize) -> isize {
    // Requests below either take integer arguments or borrow a live SyscallInfo.
    let result = unsafe { ptrace(request, pid, address, data) };
    assert!(result >= 0, "ptrace: {}", std::io::Error::last_os_error());
    result
}

pub(super) fn before_bootstrap(name: &str) {
    if name.starts_with("signal-setup-") {
        trace(PTRACE_TRACEME, 0, 0, 0);
        assert_eq!(unsafe { raise(SIGSTOP) }, 0);
    }
}

fn expect_stop(pid: i32, signal: i32) {
    let mut status = 0;
    assert_eq!(unsafe { waitpid(pid, &mut status, 0) }, pid);
    assert_eq!(
        status & 0xff,
        0x7f,
        "child exited while tracing: {status:#x}"
    );
    assert_eq!((status >> 8) & 0xff, signal);
}

#[derive(Clone, Copy)]
enum SetupPoint {
    BeforeFdValidation,
    BeforeIgnoringPipe,
    AfterDispositionReset(i32),
}

impl SetupPoint {
    fn matches(self, info: &SyscallInfo) -> bool {
        match self {
            Self::BeforeFdValidation => info.data[0] == FCNTL && info.data[2] == 1,
            Self::BeforeIgnoringPipe => info.data[0] == RT_SIGACTION && info.data[1] == 13,
            Self::AfterDispositionReset(signal) => {
                info.data[0] == RT_SIGACTION && info.data[1] == signal as u64
            }
        }
    }
}

fn pause_setup(pid: i32, point: SetupPoint) {
    expect_stop(pid, SIGSTOP);
    trace(PTRACE_SETOPTIONS, pid, 0, PTRACE_O_TRACESYSGOOD);
    let mut matched_entry = false;
    for _ in 0..1024 {
        trace(PTRACE_SYSCALL, pid, 0, 0);
        expect_stop(pid, SYSCALL_STOP);
        let mut info = SyscallInfo::default();
        let length = trace(
            PTRACE_GET_SYSCALL_INFO,
            pid,
            size_of::<SyscallInfo>(),
            &mut info as *mut SyscallInfo as usize,
        );
        assert!(length >= 24);
        match info.kind {
            1 => {
                assert!(length >= 80);
                matched_entry = point.matches(&info);
                if matched_entry && !matches!(point, SetupPoint::AfterDispositionReset(_)) {
                    return;
                }
            }
            2 if matched_entry => {
                assert!(length >= 33);
                assert_eq!(info.data[0], 0, "restoring signal disposition failed");
                return;
            }
            2 => {}
            other => panic!("unexpected syscall stop {other}"),
        }
    }
    panic!("guardian did not reach the signal setup checkpoint");
}

fn stop_during_setup(signal: i32, point: SetupPoint) {
    let mut command = command(&format!("signal-setup-{signal}"));
    command.stderr(Stdio::piped());
    let mut guardian = GuardianProcess::spawn(command).unwrap();
    let pid = guardian.child.id() as i32;
    pause_setup(pid, point);

    // Queue the signal before signalfd exists, at a deterministic syscall stop.
    assert_eq!(unsafe { kill(pid, signal) }, 0);
    guardian.release();
    trace(PTRACE_DETACH, pid, 0, 0);

    assert_eq!(guardian.wait().code(), Some(70));
    let mut diagnostic = String::new();
    guardian
        .child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut diagnostic)
        .unwrap();
    assert!(
        diagnostic.contains(&format!("SupervisorStopped({signal})")),
        "{diagnostic}"
    );
    assert_eq!(guardian.control.read(&mut [0]).unwrap(), 0);
}

fn pipe_during_setup(point: SetupPoint) {
    let mut guardian = GuardianProcess::spawn(command("signal-setup-pipe")).unwrap();
    let pid = guardian.child.id() as i32;
    pause_setup(pid, point);
    assert_eq!(unsafe { kill(pid, 13) }, 0);
    guardian.release();
    trace(PTRACE_DETACH, pid, 0, 0);

    guardian.hello();
    let mut ready = [0; 5];
    guardian.control.read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"ready");
    assert!(guardian.wait().success());
}

pub(super) const SCENARIOS: &[(&str, fn())] = &[
    (
        "SIGINT before FD validation remains pending for signalfd",
        || stop_during_setup(2, SetupPoint::BeforeFdValidation),
    ),
    (
        "SIGQUIT before FD validation remains pending for signalfd",
        || stop_during_setup(3, SetupPoint::BeforeFdValidation),
    ),
    (
        "SIGTERM before FD validation remains pending for signalfd",
        || stop_during_setup(15, SetupPoint::BeforeFdValidation),
    ),
    (
        "SIGPIPE pending before its disposition changes is discarded",
        || pipe_during_setup(SetupPoint::BeforeIgnoringPipe),
    ),
    (
        "SIGPIPE during FD validation does not stop the guardian",
        || pipe_during_setup(SetupPoint::BeforeFdValidation),
    ),
    (
        "SIGINT during signal setup remains pending for signalfd",
        || stop_during_setup(2, SetupPoint::AfterDispositionReset(2)),
    ),
    (
        "SIGQUIT during signal setup remains pending for signalfd",
        || stop_during_setup(3, SetupPoint::AfterDispositionReset(3)),
    ),
    (
        "SIGTERM during signal setup remains pending for signalfd",
        || stop_during_setup(15, SetupPoint::AfterDispositionReset(15)),
    ),
];
