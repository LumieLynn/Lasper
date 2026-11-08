use lasper_guest_protocol::{
    ControlFrameHeader, ControlLimits, FeatureSet, HelloRequest, MessageKind,
    CONTROL_FRAME_HEADER_BYTES, HELLO_PAYLOAD_BUFFER_BYTES,
};
use lasper_guest_runtime::session::{GuardianContext, SupervisorStatus};
use std::io::{Read, Write};
use std::os::fd::BorrowedFd;
use std::os::unix::net::UnixStream;
use std::process::Stdio;
use std::time::{Duration, Instant};

use super::{command, guardian_fixture};
use guardian_fixture::GuardianProcess;

unsafe extern "C" {
    fn send(fd: i32, bytes: *const u8, length: usize, flags: i32) -> isize;
    fn setsockopt(fd: i32, level: i32, name: i32, value: *const i32, length: u32) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
}

pub(super) fn before_bootstrap(name: &str) {
    if name != "hello-backpressure" {
        return;
    }
    let arguments: Vec<_> = std::env::args().collect();
    let index = arguments
        .iter()
        .position(|arg| arg == "--control-fd")
        .unwrap();
    let fd = arguments[index + 1].parse::<i32>().unwrap();
    assert_eq!(unsafe { setsockopt(fd, 1, 7, &4096, 4) }, 0);
    let bytes = [b'~'; 1024];
    // Fill the guardian's send queue without reading it from the host side.
    // HelloResponse must then wait on POLLOUT until its own deadline expires.
    loop {
        match unsafe { send(fd, bytes.as_ptr(), bytes.len(), 0x40 | 0x4000) } {
            -1 => {
                assert_eq!(
                    std::io::Error::last_os_error().kind(),
                    std::io::ErrorKind::WouldBlock
                );
                break;
            }
            count => assert!(count > 0),
        }
    }
}

pub(super) fn child(context: &GuardianContext, name: &str) {
    assert!(matches!(name, "hello-probe" | "hello-established"));
    assert_eq!(context.negotiated_limits().get(), ControlLimits::CURRENT);
    assert!(context.hello_completed_at_ms() <= guardian_fixture::monotonic_ms());
    assert!(
        std::fs::read_to_string(format!("/proc/self/task/{}/children", std::process::id()))
            .unwrap()
            .trim()
            .is_empty()
    );
    if name == "hello-established" {
        let fd = unsafe { BorrowedFd::borrow_raw(context.connection().as_raw_fd()) }
            .try_clone_to_owned()
            .unwrap();
        let mut connection = UnixStream::from(fd);
        connection.write_all(b"entered").unwrap();
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(
            context.supervisor_status().unwrap(),
            SupervisorStatus::Alive
        );
        connection.write_all(b"alive").unwrap();
    }
}

fn spawn(name: &str) -> GuardianProcess {
    let mut command = command(name);
    command.stderr(Stdio::piped());
    GuardianProcess::spawn(command).unwrap()
}

fn request() -> Vec<u8> {
    let request = HelloRequest::new(FeatureSet::WORKER_PIDFD, ControlLimits::CURRENT).unwrap();
    let mut payload = [0; HELLO_PAYLOAD_BUFFER_BYTES];
    let length = request.encode_payload(&mut payload).unwrap();
    let header = ControlFrameHeader::new(MessageKind::HelloRequest, 1, length as u32, 0).unwrap();
    let mut bytes = header.encode().to_vec();
    bytes.extend_from_slice(&payload[..length]);
    bytes
}

fn rejected(guardian: &mut GuardianProcess, reason: &str) {
    assert_eq!(guardian.wait().code(), Some(70));
    let mut diagnostic = String::new();
    guardian
        .child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut diagnostic)
        .unwrap();
    assert!(diagnostic.contains(reason), "{diagnostic}");
    assert!(
        guardian.broker.is_some(),
        "broker must outlive the handshake"
    );
}

fn incomplete_hello(length: usize) {
    let mut guardian = spawn("hello-incomplete");
    guardian.release();
    guardian.control.write_all(&request()[..length]).unwrap();
    rejected(&mut guardian, "HelloExpired");
    assert_eq!(guardian.control.read(&mut [0]).unwrap(), 0);
}

fn original_deadline() {
    let mut command = command("hello-incomplete");
    command.stderr(Stdio::piped()).args([
        "--accepted-at-ms",
        &guardian_fixture::monotonic_ms()
            .saturating_sub(600)
            .to_string(),
    ]);
    let started = Instant::now();
    let mut guardian = GuardianProcess::spawn(command).unwrap();
    guardian.release();
    guardian.control.write_all(&request()[..3]).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    guardian.control.write_all(&request()[3..8]).unwrap();
    rejected(&mut guardian, "HelloExpired");
    assert!(started.elapsed() < Duration::from_millis(900));
}

fn blocked_response() {
    let mut guardian = spawn("hello-backpressure");
    guardian.release();
    guardian.control.write_all(&request()).unwrap();
    rejected(&mut guardian, "HelloExpired");
    let mut bytes = Vec::new();
    guardian.control.read_to_end(&mut bytes).unwrap();
    assert!(!bytes.is_empty());
    assert!(bytes.iter().all(|&byte| byte == b'~'));
}

fn broker_lost() {
    let mut guardian = spawn("hello-incomplete");
    guardian.release();
    guardian
        .control
        .write_all(&request()[..CONTROL_FRAME_HEADER_BYTES + 1])
        .unwrap();
    guardian.broker.take();
    assert_eq!(guardian.wait().code(), Some(70));
    let mut diagnostic = String::new();
    guardian
        .child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut diagnostic)
        .unwrap();
    assert!(diagnostic.contains("BrokerDisconnected"), "{diagnostic}");
}

fn supervisor_stop() {
    let mut guardian = spawn("hello-incomplete");
    guardian.release();
    guardian.control.write_all(&request()[..5]).unwrap();
    // Wait for bootstrap's signal mask rather than racing exec's default SIGTERM.
    let deadline = Instant::now() + Duration::from_millis(800);
    loop {
        let status =
            std::fs::read_to_string(format!("/proc/{}/status", guardian.child.id())).unwrap();
        let mask = status
            .lines()
            .find_map(|line| line.strip_prefix("SigBlk:\t"))
            .unwrap();
        if u64::from_str_radix(mask.trim(), 16).unwrap() & (1 << 14) != 0 {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(unsafe { kill(guardian.child.id() as i32, 15) }, 0);
    rejected(&mut guardian, "SupervisorStopped(15)");
}

fn invalid_payload() {
    let mut guardian = spawn("hello-invalid");
    guardian.release();
    let header = ControlFrameHeader::new(MessageKind::HelloRequest, 1, 1, 0).unwrap();
    guardian.control.write_all(&header.encode()).unwrap();
    guardian.control.write_all(b"{").unwrap();
    rejected(&mut guardian, "HelloPayload(InvalidJson)");
}

fn hello_only() {
    let mut guardian = spawn("hello-probe");
    guardian.release();
    guardian.hello();
    assert!(guardian.wait().success());
    assert_eq!(guardian.control.read(&mut [0]).unwrap(), 0);
}

fn established_connection() {
    let mut guardian = spawn("hello-established");
    guardian.release();
    guardian.hello();
    let mut bytes = [0; 7];
    guardian.control.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"entered");
    let mut bytes = [0; 5];
    guardian.control.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"alive");
    assert!(guardian.wait().success());
}

pub(super) const SCENARIOS: &[(&str, fn())] = &[
    ("Go without Hello expires with a live broker", || {
        incomplete_hello(0)
    }),
    ("partial Hello header expires", || incomplete_hello(7)),
    ("partial Hello payload expires", || {
        incomplete_hello(CONTROL_FRAME_HEADER_BYTES + 1)
    }),
    (
        "Go and partial progress do not reset the accept deadline",
        original_deadline,
    ),
    ("blocked HelloResponse expires", blocked_response),
    ("broker EOF interrupts partial Hello", broker_lost),
    ("supervisor stop interrupts partial Hello", supervisor_stop),
    ("invalid Hello never enters the session", invalid_payload),
    ("Hello-only probe creates no worker", hello_only),
    (
        "completed Hello is not an established-session idle timeout",
        established_connection,
    ),
];
