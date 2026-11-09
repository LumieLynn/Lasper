extern crate std;

use crate::session::{GuardianBootstrap, GuardianContext, GUARDIAN_START_BYTE};
use lasper_guest_protocol::BrokerId;
use std::io::{PipeWriter, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

#[path = "hello.rs"]
mod hello;
#[path = "control.rs"]
pub mod wire;

pub struct ForkedGuardian {
    pub control: UnixStream,
    broker: PipeWriter,
    pid: i32,
    reaped: bool,
}

impl ForkedGuardian {
    pub fn spawn(entry: fn(GuardianContext) -> i32) -> Self {
        let (control, child_control) = UnixStream::pair().unwrap();
        let (child_broker, broker) = std::io::pipe().unwrap();
        let accepted_at = crate::session::linux::monotonic_ms().unwrap();
        // The child never returns to the multithreaded test harness. Its entry
        // uses only fork-safe runtime code and never drops inherited Rust owners.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // SAFETY: Bootstrap adopts these endpoints, prunes inherited FDs,
            // and exits without unwinding into the fork caller.
            unsafe {
                GuardianBootstrap::run(
                    child_control.as_raw_fd(),
                    child_broker.as_raw_fd(),
                    BrokerId::new([1; 16]),
                    accepted_at,
                    entry,
                )
            }
        }
        drop(child_control);
        drop(child_broker);
        let mut guardian = Self {
            control,
            broker,
            pid,
            reaped: false,
        };
        guardian.broker.write_all(&[GUARDIAN_START_BYTE]).unwrap();
        hello::exchange(&mut guardian.control);
        guardian
    }

    pub fn wait(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            let mut status = 0;
            let result = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
            if result == self.pid {
                self.reaped = true;
                assert!(libc::WIFEXITED(status));
                assert_eq!(libc::WEXITSTATUS(status), 0);
                return;
            }
            assert_eq!(result, 0);
            assert!(Instant::now() < deadline, "guardian did not finish cleanup");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for ForkedGuardian {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        // On a failed assertion, remove stranded children before their guardian.
        if let Ok(children) = std::fs::read_to_string(std::format!(
            "/proc/{}/task/{}/children",
            self.pid,
            self.pid
        )) {
            for pid in children
                .split_whitespace()
                .filter_map(|pid| pid.parse::<i32>().ok())
            {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
            libc::waitpid(self.pid, core::ptr::null_mut(), 0);
        }
    }
}
