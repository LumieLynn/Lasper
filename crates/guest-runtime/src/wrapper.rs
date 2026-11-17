//! Starts a broker child, then replaces the wrapper with the payload through `execve`.
//!
//! If the wrapper starts as PID 1, the payload remains PID 1 after `execve`.

use core::ffi::{c_char, CStr};

use crate::broker::{self, BrokerError, BrokerResult, Startup, STARTUP_TIMEOUT_MS};
use crate::diagnostic::Diagnostic;
use crate::failure::FailureClass;
use crate::linux::uapi::{
    errno::{EACCES, EEXIST, EINTR, EINVAL, EIO, ENAMETOOLONG, ENOENT, ETIMEDOUT},
    file::{O_CLOEXEC, O_DIRECTORY, O_NOFOLLOW, PATH_MAX, S_IFDIR, S_IFMT},
    nr,
    signal::{SIGCHLD, SIGKILL},
    socket::MSG_NOSIGNAL,
};
use crate::linux::{self, filesystem, result_or_errno, Descriptor, Pipe, PollFd};
use crate::paths;
use crate::syscall::{syscall1, syscall3, syscall4, syscall5};

/// Starts the broker child, waits for Ready, then executes the payload with `execve`.
///
/// Before calling `execve`, the parent preserves its cwd, stdio, inherited FDs,
/// signal dispositions, signal mask and umask.
///
/// # Safety
///
/// `argv` and `envp` must be readable, aligned, null-terminated arrays of valid
/// C-string pointers. `argv[0]` must be present. The arrays and their strings
/// must remain valid and unmodified until `execve` succeeds or this call returns.
///
/// Call from a single-threaded process with exclusive child creation and reaping.
/// The child must have `WORKER_STACK_BUDGET_BYTES` plus space for the broker frames.
pub unsafe fn run(
    directory: &CStr,
    executable: &CStr,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> Result<core::convert::Infallible, BrokerError> {
    if !executable.to_bytes().starts_with(b"/") {
        return Err(BrokerError {
            class: FailureClass::Usage,
            operation: "wrapper requires an absolute payload executable",
            errno: EINVAL,
        });
    }
    let original =
        filesystem::stat_path(executable).at_class(FailureClass::Exec, "stat wrapper payload")?;
    let this = filesystem::stat_path(paths::PROC_SELF_EXE).at("stat wrapper executable")?;
    if original.device() == this.device() && original.inode == this.inode {
        return Err(BrokerError {
            class: FailureClass::Usage,
            operation: "wrapper payload resolves to wrapper itself",
            errno: EINVAL,
        });
    }
    prepare_directory(directory).at_class(
        FailureClass::Configuration,
        "prepare private broker directory",
    )?;
    let ready = Pipe::new().at("create broker Ready channel")?;
    let (status_writer, status_reader) =
        linux::seqpacket_pair().at("create payload exec-status channel")?;
    let parent = linux::pidfd(linux::pid()).at("pin wrapper lifetime")?;
    let mut child_fd = -1i32;
    // CLONE_PIDFD pins the child at creation, even if inherited SIGCHLD settings
    // allow the kernel to reap it before a separate pidfd_open could run.
    let pid = result_or_errno(unsafe {
        syscall5(
            nr::CLONE,
            (0x1000 | SIGCHLD) as usize,
            0,
            &mut child_fd as *mut _ as usize,
            0,
            0,
        )
    })
    .at("fork guest broker")? as i32;
    if pid == 0 {
        drop(ready.reader);
        drop(status_writer);
        let startup = Startup {
            ready: ready.writer,
            exec_status: status_reader,
            parent,
        };
        let result = linux::setup_session(None)
            .at("detach broker session")
            .and_then(|()| {
                // SAFETY: This child has no children of its own and inherits the
                // caller's stack budget. Unused channel ends have been closed,
                // and the child exits without resuming the wrapper's callers.
                unsafe { broker::run_wrapped(directory, startup) }
            });
        if let Err(error) = result {
            // The parent only sees Ready fail. This diagnostic identifies the
            // child's failing operation.
            use core::fmt::Write;
            let _ = writeln!(
                Diagnostic,
                "lasper guest broker: {:?}: {} (errno {})",
                error.class, error.operation, error.errno
            );
        }
        linux::exit(result.map_or_else(|error| error.class.exit_code(), |()| 0));
    }
    let child = Child {
        pid,
        fd: Descriptor::adopt(child_fd as isize).at("retain broker pidfd")?,
    };
    drop(ready.writer);
    drop(status_reader);
    drop(parent);
    await_ready(&ready.reader, &child.fd).at("wait for broker Ready")?;
    drop(ready.reader);
    let result = unsafe {
        syscall3(
            nr::EXECVE,
            executable.as_ptr() as usize,
            argv as usize,
            envp as usize,
        )
    };
    let errno = if result < 0 { -result as i32 } else { EIO };
    report_exec_error(&status_writer, errno);
    drop(status_writer);
    // The bounded poll gives the broker time to unlink its listener before
    // Child forces cleanup.
    let mut exited = [PollFd::new(child.fd.raw())];
    let _ = linux::poll(&mut exited, Some(STARTUP_TIMEOUT_MS));
    Err(BrokerError {
        class: FailureClass::Exec,
        operation: "exec wrapper payload",
        errno,
    })
}

fn report_exec_error(channel: &Descriptor, errno: i32) {
    let error = errno.to_ne_bytes();
    // MSG_NOSIGNAL keeps a closed channel from killing the wrapper while leaving
    // its signal disposition unchanged.
    while crate::socket::send(channel.raw(), &error, MSG_NOSIGNAL) == -(EINTR as isize) {}
}

fn prepare_directory(path: &CStr) -> Result<(), i32> {
    // Parent directories may be shared with other software. Missing parents use
    // 0755, and existing directories retain their ownership and permissions.
    let old_mask = result_or_errno(unsafe { syscall1(nr::UMASK, 0o022) })?;
    let directory = create_directory_tree(path);
    result_or_errno(unsafe { syscall1(nr::UMASK, old_mask) })?;
    let directory = directory?;
    let stat = filesystem::ownership(&directory)?;
    if stat.uid != 0 || stat.mode & S_IFMT != S_IFDIR || stat.mode & 0o7777 != 0o700 {
        return Err(EACCES);
    }
    Ok(())
}

fn create_directory_tree(path: &CStr) -> Result<Descriptor, i32> {
    if !path.to_bytes().starts_with(b"/") {
        return Err(EINVAL);
    }
    let mut parent = linux::open(paths::ROOT_DIRECTORY, true)?;
    let mut components = path
        .to_bytes()
        .split(|byte| *byte == b'/')
        .filter(|component| !component.is_empty())
        .peekable();
    let mut name = [0; PATH_MAX];
    while let Some(component) = components.next() {
        if component.len() >= name.len() {
            return Err(ENAMETOOLONG);
        }
        name[..component.len()].copy_from_slice(component);
        name[component.len()] = 0;
        let last = components.peek().is_none();
        let flags = O_DIRECTORY | O_CLOEXEC | if last { O_NOFOLLOW } else { 0 };
        let open = || {
            Descriptor::adopt(unsafe {
                syscall4(
                    nr::OPENAT,
                    parent.raw() as usize,
                    name.as_ptr() as usize,
                    flags,
                    0,
                )
            })
        };
        parent = match open() {
            Ok(directory) => directory,
            Err(ENOENT) => {
                match result_or_errno(unsafe {
                    syscall3(
                        nr::MKDIRAT,
                        parent.raw() as usize,
                        name.as_ptr() as usize,
                        if last { 0o700 } else { 0o755 },
                    )
                }) {
                    Ok(_) | Err(EEXIST) => {}
                    Err(errno) => return Err(errno),
                }
                open()?
            }
            Err(errno) => return Err(errno),
        };
    }
    Ok(parent)
}

fn await_ready(ready: &Descriptor, child: &Descriptor) -> Result<(), i32> {
    let deadline = linux::monotonic_ms()?.saturating_add(STARTUP_TIMEOUT_MS);
    loop {
        let remaining = deadline.saturating_sub(linux::monotonic_ms()?);
        if remaining == 0 {
            return Err(ETIMEDOUT);
        }
        let mut events = [PollFd::new(ready.raw()), PollFd::new(child.raw())];
        match linux::poll(&mut events, Some(remaining)) {
            Ok(()) => {}
            Err(EINTR) => continue,
            Err(errno) => return Err(errno),
        }
        if events[1].ready() {
            return Err(EIO);
        }
        if events[0].ready() {
            let mut byte = [0];
            return if ready.read(&mut byte)? == 1 && byte[0] == 1 {
                Ok(())
            } else {
                Err(EIO)
            };
        }
    }
}

/// Cleans up the broker on the wrapper's failure path.
///
/// Dropping this guard attempts `SIGKILL` through the pidfd, then calls `wait4`
/// even if signaling failed. The wait can block indefinitely; the bounded poll
/// in `run` does not limit it. Successful `execve` bypasses `Drop`.
///
/// Signal errors and non-`EINTR` wait errors are ignored to preserve the wrapper
/// error. Interrupted waits are retried.
struct Child {
    pid: i32,
    fd: Descriptor,
}
impl Drop for Child {
    fn drop(&mut self) {
        unsafe {
            syscall4(
                nr::PIDFD_SEND_SIGNAL,
                self.fd.raw() as usize,
                SIGKILL as usize,
                0,
                0,
            )
        };
        loop {
            let result = unsafe { syscall4(nr::WAIT4, self.pid as usize, 0, 0, 0) };
            if result != -(EINTR as isize) {
                break;
            }
        }
    }
}

#[cfg(test)]
#[path = "../tests/wrapper/status.rs"]
mod tests;
