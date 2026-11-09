use lasper_guest_runtime::session::{GuardianBootstrap, GuardianContext, GUARDIAN_START_BYTE};
use std::io::{PipeWriter, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

#[path = "hello.rs"]
mod hello;

unsafe extern "C" {
    fn fcntl(fd: i32, operation: i32, ...) -> i32;
    fn syscall(number: isize, ...) -> isize;
    fn clock_gettime(clock: i32, value: *mut Timespec) -> i32;
}

#[repr(C)]
struct Timespec {
    seconds: i64,
    nanoseconds: i64,
}

pub fn monotonic_ms() -> u64 {
    let mut time = Timespec {
        seconds: 0,
        nanoseconds: 0,
    };
    assert_eq!(unsafe { clock_gettime(1, &mut time) }, 0);
    time.seconds as u64 * 1000 + time.nanoseconds as u64 / 1_000_000
}

pub struct GuardianProcess {
    pub child: Child,
    pub control: UnixStream,
    pub broker: Option<PipeWriter>,
    _pidfd: OwnedFd,
}

impl GuardianProcess {
    pub fn spawn(mut command: Command) -> std::io::Result<Self> {
        let (control, child_control) = UnixStream::pair()?;
        let (child_broker, broker) = std::io::pipe()?;
        let fds = [child_control.as_raw_fd(), child_broker.as_raw_fd()];
        command.args([
            "--control-fd",
            &fds[0].to_string(),
            "--broker-fd",
            &fds[1].to_string(),
            "--accepted-at-ms",
            &monotonic_ms().to_string(),
        ]);
        // The exec child receives only these two test connection endpoints.
        unsafe {
            command.pre_exec(move || {
                for fd in fds {
                    let flags = fcntl(fd, 1);
                    if flags == -1 || fcntl(fd, 2, flags & !1) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        drop(child_control);
        drop(child_broker);
        // The child is not reaped until after this assignment handle is opened.
        let raw_pidfd = unsafe { syscall(434, child.id() as i32, 0) };
        if raw_pidfd == -1 {
            let error = std::io::Error::last_os_error();
            drop(broker);
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        // SAFETY: pidfd_open returned one new, exclusively owned descriptor.
        let pidfd = unsafe { OwnedFd::from_raw_fd(raw_pidfd as i32) };
        Ok(Self {
            child,
            control,
            broker: Some(broker),
            _pidfd: pidfd,
        })
    }

    pub fn release(&mut self) {
        self.broker
            .as_mut()
            .unwrap()
            .write_all(&[GUARDIAN_START_BYTE])
            .unwrap();
    }

    pub fn hello(&mut self) {
        hello::exchange(&mut self.control);
    }

    pub fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "guardian test exceeded its deadline"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for GuardianProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub fn enter(entry: fn(GuardianContext) -> i32) -> ! {
    let arguments: Vec<_> = std::env::args().collect();
    let value = |name| {
        let index = arguments
            .iter()
            .position(|argument| argument == name)
            .unwrap();
        arguments[index + 1].parse::<u64>().unwrap()
    };
    // SAFETY: This exec child is single-threaded and has no children or FD
    // owners for the inherited endpoints. The runner holds broker Go and wait.
    unsafe {
        GuardianBootstrap::run(
            value("--control-fd") as i32,
            value("--broker-fd") as i32,
            lasper_guest_protocol::BrokerId::new([1; 16]),
            value("--accepted-at-ms"),
            entry,
        )
    }
}
