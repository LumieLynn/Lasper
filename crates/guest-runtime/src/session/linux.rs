pub(super) mod filesystem;

use core::ffi::CStr;
use core::mem::{align_of, offset_of, size_of};

use crate::linux::uapi::{
    errno::{EINTR, EIO},
    file::{
        AT_FDCWD, FD_CLOEXEC, F_DUPFD_CLOEXEC, F_GETFD, F_GETFL, F_SETFD, F_SETFL, O_CLOEXEC,
        O_DIRECTORY, O_NOCTTY, O_NONBLOCK, O_RDWR,
    },
    nr,
    poll::POLLIN,
    process::{CLD_EXITED, PR_SET_CHILD_SUBREAPER, P_PID, WEXITED, WNOHANG, WNOWAIT},
    signal::{
        SIGCHLD, SIGCONT, SIGHUP, SIGINT, SIGPIPE, SIGQUIT, SIGTERM, SIGTSTP, SIGTTIN, SIGTTOU,
        SIG_BLOCK, SIG_SETMASK,
    },
    socket::{AF_UNIX, SOCK_SEQPACKET, SOL_SOCKET, SO_PASSCRED},
    terminal::{TIOCGPTPEER, TIOCSCTTY, TIOCSPGRP, TIOCSPTLCK, TIOCSWINSZ},
    time::CLOCK_MONOTONIC,
};
use crate::socket::close;
use crate::syscall::{syscall1, syscall2, syscall3, syscall4, syscall5};
use lasper_guest_protocol::TerminalSize;

use super::WorkerExit;

pub(super) struct Descriptor(i32);

impl Descriptor {
    pub(super) fn adopt(result: isize) -> Result<Self, i32> {
        let fd = result_or_errno(result)? as i32;
        if fd >= 3 {
            return Ok(Self(fd));
        }
        // Keep private channels out of stdin/stdout/stderr before child setup.
        let duplicate = unsafe { syscall3(nr::FCNTL, fd as usize, F_DUPFD_CLOEXEC, 3) };
        let _ = close(fd);
        result_or_errno(duplicate).map(|fd| Self(fd as i32))
    }

    pub(super) const fn raw(&self) -> i32 {
        self.0
    }

    pub(super) fn into_raw(self) -> i32 {
        let fd = self.0;
        core::mem::forget(self);
        fd
    }

    pub(super) fn nonblocking(&self) -> Result<(), i32> {
        let flags = result_or_errno(unsafe { syscall3(nr::FCNTL, self.0 as usize, F_GETFL, 0) })?;
        result_or_errno(unsafe {
            syscall3(nr::FCNTL, self.0 as usize, F_SETFL, flags | O_NONBLOCK)
        })?;
        Ok(())
    }

    pub(super) fn cloexec(&self) -> Result<(), i32> {
        let flags = result_or_errno(unsafe { syscall3(nr::FCNTL, self.0 as usize, F_GETFD, 0) })?;
        result_or_errno(unsafe {
            syscall3(nr::FCNTL, self.0 as usize, F_SETFD, flags | FD_CLOEXEC)
        })?;
        Ok(())
    }

    pub(super) fn duplicate_to(&self, target: i32) -> Result<(), i32> {
        result_or_errno(unsafe { syscall3(nr::DUP3, self.0 as usize, target as usize, 0) })?;
        Ok(())
    }

    pub(super) fn read(&self, bytes: &mut [u8]) -> Result<usize, i32> {
        loop {
            // SAFETY: The kernel can write only within this exclusive slice.
            let result = unsafe {
                syscall3(
                    nr::READ,
                    self.0 as usize,
                    bytes.as_mut_ptr() as usize,
                    bytes.len(),
                )
            };
            if result != -(EINTR as isize) {
                return result_or_errno(result);
            }
        }
    }

    pub(super) fn write(&self, bytes: &[u8]) -> Result<usize, i32> {
        write(self.0, bytes)
    }

    pub(super) fn write_all(&self, mut bytes: &[u8]) -> Result<(), i32> {
        while !bytes.is_empty() {
            let count = self.write(bytes)?;
            if count == 0 {
                return Err(EIO);
            }
            bytes = &bytes[count..];
        }
        Ok(())
    }
}

pub(super) fn write(fd: i32, bytes: &[u8]) -> Result<usize, i32> {
    loop {
        // SAFETY: The input slice remains readable for the syscall.
        let result =
            unsafe { syscall3(nr::WRITE, fd as usize, bytes.as_ptr() as usize, bytes.len()) };
        if result != -(EINTR as isize) {
            return result_or_errno(result);
        }
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        let _ = close(self.0);
    }
}

pub(super) struct Pipe {
    pub(super) reader: Descriptor,
    pub(super) writer: Descriptor,
}

impl Pipe {
    pub(super) fn new() -> Result<Self, i32> {
        let mut fds = [-1i32; 2];
        // SAFETY: pipe2 initializes both slots on success.
        result_or_errno(unsafe { syscall2(nr::PIPE2, fds.as_mut_ptr() as usize, O_CLOEXEC) })?;
        let reader = Descriptor::adopt(fds[0] as isize);
        let writer = Descriptor::adopt(fds[1] as isize);
        Ok(Self {
            reader: reader?,
            writer: writer?,
        })
    }
}

pub(super) fn identity_pair() -> Result<(Descriptor, Descriptor), i32> {
    let mut fds = [-1i32; 2];
    // SAFETY: socketpair initializes two FD slots; SOCK_CLOEXEC applies to both.
    result_or_errno(unsafe {
        syscall4(
            nr::SOCKETPAIR,
            AF_UNIX,
            SOCK_SEQPACKET | O_CLOEXEC,
            0,
            fds.as_mut_ptr() as usize,
        )
    })?;
    let parent = Descriptor::adopt(fds[0] as isize);
    let child = Descriptor::adopt(fds[1] as isize);
    let (parent, child) = (parent?, child?);
    let enabled = 1i32;
    // SAFETY: SO_PASSCRED reads this one integer during the call.
    result_or_errno(unsafe {
        syscall5(
            nr::SETSOCKOPT,
            parent.raw() as usize,
            SOL_SOCKET as usize,
            SO_PASSCRED,
            &enabled as *const _ as usize,
            size_of::<i32>(),
        )
    })?;
    Ok((parent, child))
}

pub(super) fn open(path: &CStr, directory: bool) -> Result<Descriptor, i32> {
    let flags = O_CLOEXEC | if directory { O_DIRECTORY } else { 0 };
    // SAFETY: path includes its terminating NUL and no mode argument is needed.
    Descriptor::adopt(unsafe { syscall4(nr::OPENAT, AT_FDCWD, path.as_ptr() as usize, flags, 0) })
}

pub(super) fn null_device() -> Result<Descriptor, i32> {
    Descriptor::adopt(unsafe {
        syscall4(
            nr::OPENAT,
            AT_FDCWD,
            c"/dev/null".as_ptr() as usize,
            O_CLOEXEC | O_RDWR,
            0,
        )
    })
}

pub(super) struct Terminal {
    pub(super) master: Descriptor,
    pub(super) slave: Descriptor,
}

impl Terminal {
    pub(super) fn new(size: TerminalSize) -> Result<Self, i32> {
        let master = Descriptor::adopt(unsafe {
            syscall4(
                nr::OPENAT,
                AT_FDCWD,
                c"/dev/ptmx".as_ptr() as usize,
                O_RDWR | O_NOCTTY | O_CLOEXEC,
                0,
            )
        })?;
        let unlock = 0i32;
        result_or_errno(unsafe {
            syscall3(
                nr::IOCTL,
                master.raw() as usize,
                TIOCSPTLCK,
                &unlock as *const _ as usize,
            )
        })?;
        // TIOCGPTPEER opens this master's slave without resolving /dev/pts/N.
        let slave = Descriptor::adopt(unsafe {
            syscall3(
                nr::IOCTL,
                master.raw() as usize,
                TIOCGPTPEER,
                O_RDWR | O_NOCTTY | O_CLOEXEC,
            )
        })?;
        set_terminal_size(master.raw(), size)?;
        Ok(Self { master, slave })
    }
}

pub(super) fn set_terminal_size(fd: i32, size: TerminalSize) -> Result<(), i32> {
    let dimensions = [size.rows(), size.cols(), 0, 0];
    // SAFETY: winsize is four u16 fields on both supported Linux architectures.
    result_or_errno(unsafe {
        syscall3(
            nr::IOCTL,
            fd as usize,
            TIOCSWINSZ,
            dimensions.as_ptr() as usize,
        )
    })?;
    Ok(())
}

pub(super) fn setup_session(slave: Option<&Descriptor>) -> Result<(), i32> {
    result_or_errno(unsafe { syscall1(nr::SETSID, 0) })?;
    if let Some(slave) = slave {
        result_or_errno(unsafe { syscall3(nr::IOCTL, slave.raw() as usize, TIOCSCTTY, 0) })?;
        let pid = pid();
        result_or_errno(unsafe {
            syscall3(
                nr::IOCTL,
                slave.raw() as usize,
                TIOCSPGRP,
                &pid as *const _ as usize,
            )
        })?;
    }
    Ok(())
}

pub(super) fn pid() -> i32 {
    unsafe { syscall1(nr::GETPID, 0) as i32 }
}

pub(super) fn tid() -> i32 {
    unsafe { syscall1(nr::GETTID, 0) as i32 }
}

pub(super) fn set_subreaper() -> Result<(), i32> {
    result_or_errno(unsafe { syscall5(nr::PRCTL, PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) })?;
    Ok(())
}

#[repr(C)]
struct SignalAction {
    handler: usize,
    flags: usize,
    restorer: usize,
    mask: u64,
}

const _: () = {
    assert!(size_of::<SignalAction>() == 32);
    assert!(align_of::<SignalAction>() == 8);
    assert!(offset_of!(SignalAction, handler) == 0);
    assert!(offset_of!(SignalAction, flags) == 8);
    assert!(offset_of!(SignalAction, restorer) == 16);
    assert!(offset_of!(SignalAction, mask) == 24);
};

fn signal_disposition(signal: i32, ignore: bool) -> Result<(), i32> {
    let action = SignalAction {
        handler: usize::from(ignore),
        flags: 0,
        restorer: 0,
        mask: 0,
    };
    // SAFETY: This is the kernel sigaction layout, with an eight-byte sigset.
    result_or_errno(unsafe {
        syscall4(
            nr::RT_SIGACTION,
            signal as usize,
            &action as *const _ as usize,
            0,
            8,
        )
    })?;
    Ok(())
}

pub(super) fn guardian_signals() -> Result<(), i32> {
    // Children must remain waitable, including when the broker ignored SIGCHLD.
    for signal in [SIGCHLD, SIGINT, SIGQUIT, SIGTERM] {
        signal_disposition(signal, false)?;
    }
    Ok(())
}

pub(super) fn ignore_sigpipe() -> Result<(), i32> {
    signal_disposition(SIGPIPE, true)
}

pub(super) fn block_signals(mask: u64) -> Result<(), i32> {
    // SAFETY: mask is a live kernel-sized signal set, borrowed for this syscall.
    result_or_errno(unsafe {
        syscall4(
            nr::RT_SIGPROCMASK,
            SIG_BLOCK,
            &mask as *const _ as usize,
            0,
            8,
        )
    })?;
    Ok(())
}

pub(super) fn signal_fd(mask: u64) -> Result<Descriptor, i32> {
    Descriptor::adopt(unsafe {
        syscall4(
            nr::SIGNALFD4,
            usize::MAX,
            &mask as *const _ as usize,
            8,
            O_NONBLOCK | O_CLOEXEC,
        )
    })
}

pub(super) fn validate_descriptor(fd: i32) -> Result<(), i32> {
    result_or_errno(unsafe { syscall3(nr::FCNTL, fd as usize, F_GETFD, 0) })?;
    Ok(())
}

pub(super) fn worker_signals() -> Result<(), i32> {
    for signal in [
        SIGHUP, SIGINT, SIGQUIT, SIGPIPE, SIGTERM, SIGCHLD, SIGCONT, SIGTSTP, SIGTTIN, SIGTTOU,
    ] {
        signal_disposition(signal, false)?;
    }
    let empty = 0u64;
    result_or_errno(unsafe {
        syscall4(
            nr::RT_SIGPROCMASK,
            SIG_SETMASK,
            &empty as *const _ as usize,
            0,
            8,
        )
    })?;
    Ok(())
}

/// The caller must use only fork-safe operations in the child until exec/exit.
pub(super) unsafe fn fork() -> Result<i32, i32> {
    // A null child stack gives fork semantics. No VM, files, or signal handlers
    // are shared; the last two clone arguments are null on both architectures.
    result_or_errno(unsafe { syscall5(nr::CLONE, SIGCHLD as usize, 0, 0, 0, 0) })
        .map(|pid| pid as i32)
}

pub(super) fn pidfd(pid: i32) -> Result<Descriptor, i32> {
    loop {
        let result = unsafe { syscall2(nr::PIDFD_OPEN, pid as usize, 0) };
        if result != -(EINTR as isize) {
            return Descriptor::adopt(result);
        }
    }
}

pub(super) fn signal(pid: i32, signal: i32) -> Result<(), i32> {
    result_or_errno(unsafe { syscall2(nr::KILL, pid as usize, signal as usize) })?;
    Ok(())
}

#[repr(C, align(8))]
struct WaitInfo {
    signal: i32,
    errno: i32,
    code: i32,
    padding: i32,
    pid: i32,
    uid: u32,
    status: i32,
    rest: [u8; 100],
}

const _: () = {
    assert!(size_of::<WaitInfo>() == 128);
    assert!(align_of::<WaitInfo>() == 8);
    assert!(offset_of!(WaitInfo, signal) == 0);
    assert!(offset_of!(WaitInfo, errno) == 4);
    assert!(offset_of!(WaitInfo, code) == 8);
    assert!(offset_of!(WaitInfo, pid) == 16);
    assert!(offset_of!(WaitInfo, uid) == 20);
    assert!(offset_of!(WaitInfo, status) == 24);
    assert!(offset_of!(WaitInfo, rest) == 28);
};

pub(super) fn observe_exit(pid: i32) -> Result<Option<WorkerExit>, i32> {
    let mut info = WaitInfo {
        signal: 0,
        errno: 0,
        code: 0,
        padding: 0,
        pid: 0,
        uid: 0,
        status: 0,
        rest: [0; 100],
    };
    loop {
        // SAFETY: waitid fills the whole siginfo_t. WNOWAIT leaves the child
        // unreaped so its PID/PGID cannot be reused before group cleanup.
        let result = unsafe {
            syscall5(
                nr::WAITID,
                P_PID,
                pid as usize,
                &mut info as *mut _ as usize,
                WEXITED | WNOHANG | WNOWAIT,
                0,
            )
        };
        if result != -(EINTR as isize) {
            result_or_errno(result)?;
            return Ok((info.pid != 0).then_some(if info.code == CLD_EXITED {
                WorkerExit::Exited(info.status)
            } else {
                WorkerExit::Signaled(info.status)
            }));
        }
    }
}

pub(super) struct ReapedChild {
    pub(super) pid: i32,
    pub(super) exit: WorkerExit,
}

pub(super) fn reap_one() -> Result<Option<ReapedChild>, i32> {
    loop {
        let mut status = 0i32;
        // SAFETY: wait4 writes one status integer. This is the only reaper;
        // retain the status even if the child exited after the WNOWAIT query.
        let result = unsafe {
            syscall4(
                nr::WAIT4,
                -1isize as usize,
                &mut status as *mut _ as usize,
                WNOHANG,
                0,
            )
        };
        if result != -(EINTR as isize) {
            let pid = result_or_errno(result)? as i32;
            return Ok((pid != 0).then_some(ReapedChild {
                pid,
                exit: if status & 0x7f == 0 {
                    WorkerExit::Exited((status >> 8) & 0xff)
                } else {
                    WorkerExit::Signaled(status & 0x7f)
                },
            }));
        }
    }
}

#[repr(C)]
pub(super) struct PollFd {
    pub(super) fd: i32,
    pub(super) events: i16,
    pub(super) returned: i16,
}

const _: () = {
    assert!(size_of::<PollFd>() == 8);
    assert!(align_of::<PollFd>() == 4);
    assert!(offset_of!(PollFd, fd) == 0);
    assert!(offset_of!(PollFd, events) == 4);
    assert!(offset_of!(PollFd, returned) == 6);
};

impl PollFd {
    pub(super) const fn new(fd: i32) -> Self {
        Self {
            fd,
            events: POLLIN,
            returned: 0,
        }
    }

    pub(super) const fn ready(&self) -> bool {
        self.returned != 0
    }
}

#[repr(C)]
struct Timespec {
    seconds: i64,
    nanoseconds: i64,
}

const _: () = {
    assert!(size_of::<Timespec>() == 16);
    assert!(align_of::<Timespec>() == 8);
    assert!(offset_of!(Timespec, seconds) == 0);
    assert!(offset_of!(Timespec, nanoseconds) == 8);
};

pub(super) fn monotonic_ms() -> Result<u64, i32> {
    let mut time = Timespec {
        seconds: 0,
        nanoseconds: 0,
    };
    // SAFETY: The kernel initializes the complete timespec on success.
    result_or_errno(unsafe {
        syscall2(
            nr::CLOCK_GETTIME,
            CLOCK_MONOTONIC,
            &mut time as *mut _ as usize,
        )
    })?;
    Ok((time.seconds as u64).saturating_mul(1000) + time.nanoseconds as u64 / 1_000_000)
}

pub(super) fn poll(fds: &mut [PollFd], timeout_ms: Option<u64>) -> Result<(), i32> {
    let timeout = timeout_ms.map(|ms| Timespec {
        seconds: (ms / 1000).min(i64::MAX as u64) as i64,
        nanoseconds: ((ms % 1000) * 1_000_000) as i64,
    });
    let pointer = timeout
        .as_ref()
        .map_or(0, |value| value as *const _ as usize);
    // SAFETY: ppoll has exclusive access to the live FD array and borrows timeout.
    result_or_errno(unsafe {
        syscall5(
            nr::PPOLL,
            fds.as_mut_ptr() as usize,
            fds.len(),
            pointer,
            0,
            8,
        )
    })?;
    Ok(())
}

pub(super) fn read_directory(fd: &Descriptor, output: &mut [u8]) -> Result<usize, i32> {
    loop {
        let result = unsafe {
            syscall3(
                nr::GETDENTS64,
                fd.raw() as usize,
                output.as_mut_ptr() as usize,
                output.len(),
            )
        };
        if result != -(EINTR as isize) {
            return result_or_errno(result);
        }
    }
}

pub(super) fn for_directory_entries(
    mut bytes: &[u8],
    mut visit: impl FnMut(&[u8]) -> Result<(), i32>,
) -> Result<(), i32> {
    while !bytes.is_empty() {
        if bytes.len() < 20 {
            return Err(EIO);
        }
        let length = u16::from_ne_bytes([bytes[16], bytes[17]]) as usize;
        if length < 20 || length > bytes.len() {
            return Err(EIO);
        }
        let name = &bytes[19..length];
        let end = name.iter().position(|byte| *byte == 0).ok_or(EIO)?;
        visit(&name[..end])?;
        bytes = &bytes[length..];
    }
    Ok(())
}

pub(super) fn parse_pid(bytes: &[u8]) -> Option<i32> {
    if bytes.is_empty() {
        return None;
    }
    let mut value = 0i32;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(i32::from(byte - b'0'))?;
    }
    Some(value)
}

pub(super) fn close_unrelated(retained: &[i32]) -> Result<(), i32> {
    let directory = open(c"/proc/self/fd", true)?;
    let mut bytes = [0; 4096];
    loop {
        let length = read_directory(&directory, &mut bytes)?;
        if length == 0 {
            return Ok(());
        }
        for_directory_entries(&bytes[..length], |name| {
            if let Some(fd) = parse_pid(name) {
                if fd >= 3 && fd != directory.raw() && !retained.contains(&fd) {
                    let _ = close(fd);
                }
            }
            Ok(())
        })?;
    }
}

pub(super) unsafe fn execve(program: &CStr, argv: &[*const u8], env: &[*const u8]) -> i32 {
    // SAFETY: The caller owns each NUL-terminated string and both pointer arrays
    // have a final null slot. All remain live until exec replaces this process.
    let result = unsafe {
        syscall3(
            nr::EXECVE,
            program.as_ptr() as usize,
            argv.as_ptr() as usize,
            env.as_ptr() as usize,
        )
    };
    -result as i32
}

pub(super) fn exit(code: i32) -> ! {
    unsafe {
        syscall1(nr::EXIT_GROUP, code as usize);
    }
    // exit_group does not return, including when seccomp terminates the process.
    loop {
        core::hint::spin_loop();
    }
}

pub(super) fn result_or_errno(result: isize) -> Result<usize, i32> {
    if result < 0 {
        Err(-result as i32)
    } else {
        Ok(result as usize)
    }
}

#[cfg(test)]
#[path = "../../tests/session/filesystem.rs"]
mod tests;
