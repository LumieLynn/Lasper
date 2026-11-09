use lasper_guest_protocol::{
    GuestIdentityReply, PreparationId, PrepareRequest, MAX_IDENTITY_MESSAGE_BYTES,
};
use lasper_guest_runtime::identity::GuestIdentityChannel;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

pub struct TestWorker {
    pub pid: libc::pid_t,
    pub pidfd: OwnedFd,
    pub terminal: Option<OwnedFd>,
    pub identity: Option<OwnedFd>,
    lifetime: UnixStream,
}

impl TestWorker {
    pub fn spawn(request: PrepareRequest<'static>, preparation: PreparationId) -> Self {
        Self::spawn_with_terminal_release(request, preparation, false)
    }

    pub fn releasing_terminal_on_recheck(
        request: PrepareRequest<'static>,
        preparation: PreparationId,
    ) -> Self {
        Self::spawn_with_terminal_release(request, preparation, true)
    }

    fn spawn_with_terminal_release(
        request: PrepareRequest<'static>,
        preparation: PreparationId,
        release_on_recheck: bool,
    ) -> Self {
        let mut endpoints = [-1; 2];
        // SAFETY: Both result slots are live and receive new owned descriptors.
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    endpoints.as_mut_ptr(),
                )
            },
            0
        );
        let (host, guest) = unsafe {
            (
                OwnedFd::from_raw_fd(endpoints[0]),
                OwnedFd::from_raw_fd(endpoints[1]),
            )
        };
        let (master, slave) = if request.execution()
            == lasper_guest_protocol::ExecutionKind::Interactive
        {
            // SAFETY: posix_openpt creates an owned master; grantpt/unlockpt
            // prepare its slave and TIOCGPTPEER opens that same slave.
            let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
            assert!(fd >= 0, "{}", io::Error::last_os_error());
            let master = unsafe { OwnedFd::from_raw_fd(fd) };
            assert_eq!(unsafe { libc::grantpt(fd) }, 0);
            assert_eq!(unsafe { libc::unlockpt(fd) }, 0);
            let fd = unsafe {
                libc::ioctl(
                    fd,
                    libc::TIOCGPTPEER,
                    libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
                )
            };
            assert!(fd >= 0, "{}", io::Error::last_os_error());
            (Some(master), Some(unsafe { OwnedFd::from_raw_fd(fd) }))
        } else {
            (None, None)
        };
        let (parent, child) = UnixStream::pair().unwrap();
        let mut retained = [
            guest.as_raw_fd(),
            child.as_raw_fd(),
            slave.as_ref().map(AsRawFd::as_raw_fd).unwrap_or(-1),
        ];
        retained.sort_unstable();
        // SAFETY: The child uses raw syscalls and the allocation-free guest
        // codec, then _exit. It does not enter the inherited async runtime.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            unsafe {
                // A fork also inherits sockets from concurrently running
                // tests. Keeping them would hide real close/EOF behavior.
                let mut first = 3u32;
                for descriptor in retained.into_iter().filter(|descriptor| *descriptor >= 3) {
                    let descriptor = descriptor as u32;
                    if descriptor > first
                        && libc::syscall(libc::SYS_close_range, first, descriptor - 1, 0) < 0
                    {
                        libc::_exit(7);
                    }
                    first = descriptor + 1;
                }
                if libc::syscall(libc::SYS_close_range, first, u32::MAX, 0) < 0 {
                    libc::_exit(7);
                }
                if libc::setsid() < 0 {
                    libc::_exit(1);
                }
                if let Some(slave) = &slave {
                    if libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY, 0) < 0
                        || libc::tcsetpgrp(slave.as_raw_fd(), libc::getpid()) < 0
                    {
                        libc::_exit(2);
                    }
                    for descriptor in 0..3 {
                        if libc::dup2(slave.as_raw_fd(), descriptor) < 0 {
                            libc::_exit(3);
                        }
                    }
                    libc::close(slave.as_raw_fd());
                }
                if libc::write(child.as_raw_fd(), [1u8].as_ptr().cast(), 1) != 1 {
                    libc::_exit(4);
                }
            }
            let mut channel =
                unsafe { GuestIdentityChannel::from_raw_fd(guest.into_raw_fd(), preparation) };
            let mut bytes = [0; MAX_IDENTITY_MESSAGE_BYTES];
            let mut observations = 0;
            while let Ok(challenge) = channel.receive_challenge(&mut bytes) {
                if release_on_recheck && observations == 1 {
                    // Keep the assigned sender alive, but remove its terminal
                    // ownership while the host is awaiting the next reply.
                    unsafe {
                        libc::signal(libc::SIGHUP, libc::SIG_IGN);
                        if libc::ioctl(0, libc::TIOCNOTTY, 0) < 0 {
                            libc::_exit(8);
                        }
                    }
                }
                let Ok(reply) = GuestIdentityReply::new(
                    challenge,
                    &request,
                    "alice",
                    unsafe { libc::getuid() },
                    unsafe { libc::getgid() },
                    Default::default(),
                ) else {
                    unsafe { libc::_exit(5) };
                };
                if channel.send_reply(&reply, &mut bytes).is_err() {
                    unsafe { libc::_exit(6) };
                }
                observations += 1;
            }
            // A rejected observation shuts down the identity channel. Keep
            // the fixture alive until its owner ends it, so tests can still
            // distinguish a resource mismatch from an exited worker.
            let mut byte = 0u8;
            loop {
                // SAFETY: The retained endpoint writes at most this one byte.
                let result =
                    unsafe { libc::read(child.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) };
                if result == 0 {
                    break;
                }
                if result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                unsafe { libc::_exit(9) };
            }
            unsafe { libc::_exit(0) };
        }
        drop(child);
        drop(slave);
        drop(guest);
        // This test owns waitpid; even an early-exit child keeps its PID until
        // the pidfd is opened, matching the guardian's required reap ordering.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        assert!(fd >= 0, "{}", io::Error::last_os_error());
        let mut worker = Self {
            pid,
            pidfd: unsafe { OwnedFd::from_raw_fd(fd as i32) },
            terminal: master,
            identity: Some(host),
            lifetime: parent,
        };
        worker
            .lifetime
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        worker.lifetime.read_exact(&mut [0; 1]).unwrap();
        worker
    }
}

impl Drop for TestWorker {
    fn drop(&mut self) {
        // SAFETY: This unreaped child cannot have its PID reused. Stop it even
        // when the parent test failed before it could close the channel.
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
            while libc::waitpid(self.pid, std::ptr::null_mut(), 0) < 0 {
                if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    break;
                }
            }
        }
    }
}
