use lasper_guest_protocol::{
    BrokerId, CommandArgv, ExecutionKind, GuestProcess, PreparationId, PrepareRequest,
    PrepareRequestFrame, ProjectionRequests,
};
use lasper_guest_runtime::session::{
    GuardianContext, GuardianError, SupervisorStatus, WorkerBootstrap,
};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::time::{Duration, Instant};

use super::{command, guardian_fixture::GuardianProcess};

unsafe extern "C" {
    fn fcntl(fd: i32, operation: i32, ...) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
    fn prctl(operation: i32, ...) -> i32;
    fn _exit(code: i32) -> !;
}

fn connection(context: &GuardianContext) -> UnixStream {
    let raw = context.connection().as_raw_fd();
    // SAFETY: The context retains the connection; the clone owns a new FD.
    let fd = unsafe { BorrowedFd::borrow_raw(raw) }
        .try_clone_to_owned()
        .unwrap();
    UnixStream::from(fd)
}

pub(super) fn inherited_owners(name: &str) -> Vec<OwnedFd> {
    if name != "prune" {
        return Vec::new();
    }
    let mut owners = Vec::new();
    let arguments: Vec<_> = std::env::args().collect();
    for index in 0..arguments.len() {
        if arguments[index] == "--unrelated-fd" {
            let raw = arguments[index + 1].parse::<i32>().unwrap();
            assert_eq!(unsafe { fcntl(raw, 2, 1) }, 0);
            // SAFETY: The test parent transferred an otherwise unowned FD.
            owners.push(unsafe { OwnedFd::from_raw_fd(raw) });
        }
    }
    owners
}

pub(super) fn child(context: &mut GuardianContext, name: &str) {
    if name == "prune" {
        let expected: BTreeSet<_> = context.poll_descriptors().into_iter().collect();
        let directory = format!("/proc/{}/fd", std::process::id());
        let mut actual = BTreeSet::new();
        for entry in std::fs::read_dir(&directory).unwrap() {
            let entry = entry.unwrap();
            let fd: i32 = entry.file_name().to_str().unwrap().parse().unwrap();
            if fd >= 3
                && std::fs::read_link(entry.path()).unwrap().as_os_str() != directory.as_str()
            {
                actual.insert(fd);
            }
        }
        assert_eq!(actual, expected);
        for fd in expected {
            assert_eq!(unsafe { fcntl(fd, 1) } & 1, 1);
        }
        let mut subreaper = 0i32;
        assert_eq!(unsafe { prctl(37, &mut subreaper) }, 0);
        assert_eq!(subreaper, 1);
        // Force a closed inherited slot to be reused, then exercise the signal
        // owner. No old owner may run its destructor while this entry is live.
        let _replacement = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(
            context.supervisor_status().unwrap(),
            SupervisorStatus::Alive
        );
    }
    if name == "one-worker" {
        let request = PrepareRequestFrame::new(
            1,
            BrokerId::new([1; 16]),
            PrepareRequest::new(
                "root",
                ExecutionKind::Bounded,
                GuestProcess::Command(CommandArgv::new(&["/bin/true"]).unwrap()),
                None,
                None,
                ProjectionRequests::default(),
            )
            .unwrap(),
        );
        let wrong_broker = PrepareRequestFrame::new(
            request.request_id(),
            BrokerId::new([9; 16]),
            request.request().clone(),
        );
        assert_eq!(
            context
                .spawn(&wrong_broker, PreparationId::new([2; 16]), 0, exit_worker)
                .err(),
            Some(GuardianError::MismatchedBroker {
                expected: context.broker_id(),
                actual: BrokerId::new([9; 16]),
            })
        );
        let worker = context
            .spawn(&request, PreparationId::new([2; 16]), 0, exit_worker)
            .unwrap();
        assert_eq!(
            context
                .spawn(&request, PreparationId::new([3; 16]), 0, exit_worker)
                .err(),
            Some(GuardianError::SessionAlreadyCreated)
        );
        drop(worker);
        assert_eq!(
            context
                .spawn(&request, PreparationId::new([4; 16]), 0, exit_worker)
                .err(),
            Some(GuardianError::SessionAlreadyCreated)
        );
    }
    connection(context).write_all(b"ready").unwrap();
    if matches!(name, "broker-eof" | "stop" | "bad-broker") {
        let mut pending = context.connection().begin_receive().unwrap();
        assert!(pending.advance().unwrap().is_pending());
        let deadline = Instant::now() + Duration::from_secs(3);
        let expected = if name == "stop" {
            SupervisorStatus::Stopped(15)
        } else {
            SupervisorStatus::BrokerDisconnected
        };
        loop {
            let status = match context.supervisor_status() {
                Err(GuardianError::UnexpectedBrokerData) if name == "bad-broker" => {
                    assert_eq!(
                        context.supervisor_status(),
                        Err(GuardianError::UnexpectedBrokerData)
                    );
                    break;
                }
                result => result.unwrap(),
            };
            if status != SupervisorStatus::Alive {
                assert_eq!(status, expected);
                assert_eq!(context.supervisor_status().unwrap(), expected);
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

fn exit_worker(_: WorkerBootstrap<'_>) -> ! {
    unsafe { _exit(0) }
}

fn expect_ready(guardian: &mut GuardianProcess) {
    guardian.hello();
    guardian
        .control
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut marker = [0; 5];
    guardian.control.read_exact(&mut marker).unwrap();
    assert_eq!(&marker, b"ready");
}

fn waits_for_go() {
    let mut guardian = GuardianProcess::spawn(command("gate")).unwrap();
    guardian
        .control
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let error = guardian.control.read(&mut [0]).unwrap_err();
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    guardian.release();
    expect_ready(&mut guardian);
    assert!(guardian.wait().success());
}

fn prunes_inherited_descriptors() {
    let (mut unrelated_reader, unrelated_writer) = std::io::pipe().unwrap();
    let (mut unrelated_peer, unrelated_connection) = UnixStream::pair().unwrap();
    let path = std::env::temp_dir().join(format!("lasper-listener-{}", std::process::id()));
    let listener = UnixListener::bind(&path).unwrap();
    let fds = [
        unrelated_writer.as_raw_fd(),
        unrelated_connection.as_raw_fd(),
        listener.as_raw_fd(),
    ];
    let mut command = command("prune");
    for fd in fds {
        command.args(["--unrelated-fd", &fd.to_string()]);
    }
    unsafe {
        command.pre_exec(move || {
            for fd in fds {
                if fcntl(fd, 2, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut guardian = GuardianProcess::spawn(command).unwrap();
    drop(unrelated_writer);
    drop(unrelated_connection);
    drop(listener);
    unrelated_peer
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    assert_eq!(unrelated_peer.read(&mut [0]).unwrap(), 0);
    let flags = unsafe { fcntl(unrelated_reader.as_raw_fd(), 3) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { fcntl(unrelated_reader.as_raw_fd(), 4, flags | 0o4000) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match unrelated_reader.read(&mut [0]) {
            Ok(0) => break,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "guardian retained an unrelated liveness writer"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            other => panic!("unexpected unrelated pipe input: {other:?}"),
        }
    }
    guardian.release();
    expect_ready(&mut guardian);
    assert!(guardian.wait().success());
    std::fs::remove_file(path).unwrap();
}

fn rejected_startup(name: &str, action: impl FnOnce(&mut GuardianProcess)) {
    let mut command = command(name);
    if name == "expired" {
        command.args(["--accepted-at-ms", "0"]);
    }
    let mut guardian = GuardianProcess::spawn(command).unwrap();
    action(&mut guardian);
    assert_eq!(guardian.wait().code(), Some(70));
    assert_eq!(guardian.control.read(&mut [0]).unwrap(), 0);
}

fn broker_eof_before_go() {
    rejected_startup("gate", |guardian| {
        guardian.broker.take();
    });
}
fn invalid_go() {
    rejected_startup("gate", |guardian| {
        guardian.broker.as_mut().unwrap().write_all(&[99]).unwrap();
    });
}
fn expired_startup() {
    rejected_startup("expired", |_| {});
}

fn supervisor_loss(name: &str) {
    let mut guardian = GuardianProcess::spawn(command(name)).unwrap();
    guardian.release();
    expect_ready(&mut guardian);
    if name == "stop" {
        assert_eq!(unsafe { kill(guardian.child.id() as i32, 15) }, 0);
    } else if name == "bad-broker" {
        guardian.broker.as_mut().unwrap().write_all(&[99]).unwrap();
    } else {
        guardian.broker.take();
    }
    assert!(guardian.wait().success());
}

fn one_worker() {
    let mut guardian = GuardianProcess::spawn(command("one-worker")).unwrap();
    guardian.release();
    expect_ready(&mut guardian);
    assert!(guardian.wait().success());
}

pub(super) const SCENARIOS: &[(&str, fn())] = &[
    ("waits for broker Go", waits_for_go),
    (
        "closes inherited listener, connection and liveness writer before Go",
        prunes_inherited_descriptors,
    ),
    ("broker EOF refuses startup", broker_eof_before_go),
    ("invalid Go refuses startup", invalid_go),
    ("startup uses the original accept deadline", expired_startup),
    ("broker EOF stays observable after Go", || {
        supervisor_loss("broker-eof")
    }),
    ("stop signal stays observable after consumption", || {
        supervisor_loss("stop")
    }),
    (
        "invalid broker data cannot be consumed and forgotten",
        || supervisor_loss("bad-broker"),
    ),
    ("one guardian cannot create a second worker", one_worker),
];
