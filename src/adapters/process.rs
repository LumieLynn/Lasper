//! Command builder helpers.

use std::future::Future;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::pin::Pin;
use std::process::{ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

static DAEMON_CHILD_LIFECYCLE: AtomicBool = AtomicBool::new(false);

/// Mark commands created after daemon startup so the kernel terminates their
/// direct child when the root daemon itself disappears unexpectedly.
pub(crate) fn enable_daemon_child_lifecycle() {
    DAEMON_CHILD_LIFECYCLE.store(true, Ordering::Release);
}

fn install_parent_death_signal(command: &mut std::process::Command) {
    if !DAEMON_CHILD_LIFECYCLE.load(Ordering::Acquire) {
        return;
    }
    let daemon_pid = unsafe { libc::getpid() };
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Close the fork/exec race: if the daemon died before this child
            // installed the death signal, terminate the child immediately.
            if libc::getppid() != daemon_pid {
                libc::_exit(1);
            }
            Ok(())
        });
    }
}

/// Handle to a spawned command: stderr merged into stdout via `2>&1`,
/// exit status retrievable via [`SpawnedProcess::wait`].
pub struct SpawnedProcess {
    pub stdout: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    wait_fn: Option<Pin<Box<dyn Future<Output = std::io::Result<ExitStatus>> + Send>>>,
    signal_fn: Option<SignalFn>,
    completion_wins_cancellation: bool,
}

type SignalFn = std::sync::Arc<
    dyn Fn(i32) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>> + Send + Sync,
>;

impl SpawnedProcess {
    pub fn new_cancellable(
        stdout: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        wait_fn: impl Future<Output = std::io::Result<ExitStatus>> + Send + 'static,
        signal_fn: impl Fn(i32) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self {
            stdout,
            wait_fn: Some(Box::pin(wait_fn)),
            signal_fn: Some(std::sync::Arc::new(signal_fn)),
            completion_wins_cancellation: false,
        }
    }

    /// Marks operations whose authoritative backend may complete while a
    /// cancellation request is racing with its final completion signal.
    pub(crate) fn with_completion_wins_cancellation(mut self) -> Self {
        self.completion_wins_cancellation = true;
        self
    }

    pub(crate) fn completion_wins_cancellation(&self) -> bool {
        self.completion_wins_cancellation
    }

    pub async fn wait(mut self) -> std::io::Result<ExitStatus> {
        // Drain any unread stdout before waiting — avoids pipe deadlocks.
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 4096];
        while self.stdout.read(&mut buf).await? > 0 {}
        drop(self.stdout);

        self.wait_fn.take().unwrap().await
    }

    pub async fn terminate_and_wait(self) -> std::io::Result<ExitStatus> {
        const STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

        let signal = self.signal_fn.clone().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "spawned command does not support cancellation",
            )
        })?;
        if let Err(signal_error) = signal(libc::SIGTERM).await {
            // A failed signal request does not prove that the child is still
            // running. Waiting is the only safe way to establish its state.
            return tokio::time::timeout(STOP_TIMEOUT, self.wait())
                .await
                .map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!(
                            "termination request failed ({signal_error}) and process exit was not confirmed"
                        ),
                    )
                })?;
        }

        let mut wait = Box::pin(self.wait());
        match tokio::time::timeout(STOP_TIMEOUT, &mut wait).await {
            Ok(status) => status,
            Err(_) => {
                signal(libc::SIGKILL).await?;
                tokio::time::timeout(STOP_TIMEOUT, wait)
                    .await
                    .map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "process exit was not confirmed after SIGKILL",
                        )
                    })?
            }
        }
    }
}

pub(crate) fn signal_process_group(pid: u32, signal: i32) -> std::io::Result<()> {
    let result = unsafe { libc::kill(-(pid as libc::pid_t), signal) };
    if result == 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }
}

/// Open a pidfd that pins the kernel identity of `pid`.
///
/// The descriptor is useful for liveness checks and parent monitoring. It does
/// not make a later `kill(-pid, ...)` process-group operation atomic; callers
/// must keep that residual race explicit in their ownership policy.
pub(crate) fn open_pidfd(pid: u32) -> std::io::Result<OwnedFd> {
    let pid = libc::pid_t::try_from(pid).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "process PID is out of range",
        )
    })?;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let pidfd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
    let flags = unsafe { libc::fcntl(pidfd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(pidfd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(pidfd)
}

/// Trait abstracting over command execution.
#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run a command, capturing stdout/stderr after it exits.
    async fn run(&self, program: &str, args: Vec<String>) -> std::io::Result<Output>;

    /// Spawn a command for streaming.  Stderr merged into stdout.
    /// Use [`SpawnedProcess::wait`] for the exit status.
    async fn spawn(&self, program: &str, args: Vec<String>) -> std::io::Result<SpawnedProcess>;
}

/// Default implementation — direct [`tokio::process::Command`].
pub struct DefaultCommandRunner;

#[async_trait::async_trait]
impl CommandRunner for DefaultCommandRunner {
    async fn run(&self, program: &str, args: Vec<String>) -> std::io::Result<Output> {
        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        new_command(program).args(&args_refs).output().await
    }

    async fn spawn(&self, program: &str, args: Vec<String>) -> std::io::Result<SpawnedProcess> {
        let mut argv = vec![program.to_string()];
        argv.extend(args);
        let mut command = new_command("sh");
        command
            .arg("-c")
            .arg("exec \"$@\" 2>&1")
            .arg("--")
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        let mut child = command.spawn()?;
        let pid = child.id().expect("spawned command has pid");
        let stdout = Box::new(child.stdout.take().expect("stdout piped"));
        Ok(SpawnedProcess::new_cancellable(
            stdout,
            async move { child.wait_with_output().await.map(|o| o.status) },
            move |signal| Box::pin(async move { signal_process_group(pid, signal) }),
        ))
    }
}

/// Creates a new `tokio::process::Command` with `LC_ALL=C` set
/// with stdin detached and stdout/stderr piped by default so non-interactive
/// host operations cannot compete with the TUI for its controlling terminal.
pub fn new_command(program: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.env("LC_ALL", "C");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    install_parent_death_signal(cmd.as_std_mut());
    cmd
}

/// Creates a new `std::process::Command` with `LC_ALL=C` set
/// with stdin detached and stdout/stderr piped by default so non-interactive
/// host operations cannot compete with the TUI for its controlling terminal.
pub fn new_sync_command(program: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    cmd.env("LC_ALL", "C");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    install_parent_death_signal(&mut cmd);
    cmd
}

/// Cap the size of regular files written by a child process.
///
/// This is a per-file kernel limit. Callers that need a total tree or
/// archive budget must enforce that budget separately.
pub(crate) fn limit_file_size(command: &mut std::process::Command, limit: u64) {
    unsafe {
        command.pre_exec(move || {
            let mut resource = std::mem::zeroed::<libc::rlimit>();
            if libc::getrlimit(libc::RLIMIT_FSIZE, &mut resource) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            resource.rlim_cur = resource.rlim_cur.min(limit as libc::rlim_t);
            if libc::setrlimit(libc::RLIMIT_FSIZE, &resource) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

/// Logs the captured stdout/stderr of a finished command.
///
/// - stdout → `log::debug!`
/// - stderr → `log::warn!` on failure, `log::debug!` on success
pub fn log_output(label: &str, output: &Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !stdout.trim().is_empty() {
        for line in stdout.trim().lines() {
            log::debug!("[{}] {}", label, line);
        }
    }

    if !stderr.trim().is_empty() {
        if output.status.success() {
            for line in stderr.trim().lines() {
                log::debug!("[{} stderr] {}", label, line);
            }
        } else {
            for line in stderr.trim().lines() {
                log::warn!("[{} stderr] {}", label, line);
            }
        }
    }
}

/// Extension trait for `tokio::process::Command` that provides
/// [`logged_output`](CommandLogged::logged_output) — a drop-in replacement
/// for `.output()` that routes captured stdout/stderr through the `log` crate.
#[async_trait::async_trait]
pub trait CommandLogged {
    /// Runs the command, logs its stdout/stderr, and returns the `Output`.
    async fn logged_output(&mut self, label: &str) -> std::io::Result<Output>;
}

#[async_trait::async_trait]
impl CommandLogged for tokio::process::Command {
    async fn logged_output(&mut self, label: &str) -> std::io::Result<Output> {
        let output = self.output().await?;
        log_output(label, &output);
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn child_file_size_limit_caps_regular_file_output() {
        let destination = tempfile::tempfile().unwrap();
        let mut command = new_sync_command("head");
        command
            .args(["-c", "2048", "/dev/zero"])
            .stdout(Stdio::from(destination.try_clone().unwrap()));
        limit_file_size(&mut command, 1024);

        let status = command.status().unwrap();

        assert!(!status.success());
        assert!(destination.metadata().unwrap().len() <= 1024);
    }

    #[tokio::test]
    async fn streamed_commands_use_the_stable_locale() {
        use tokio::io::AsyncReadExt;

        let mut spawned = DefaultCommandRunner
            .spawn("sh", vec!["-c".into(), "printf '%s' \"$LC_ALL\"".into()])
            .await
            .unwrap();
        let mut output = String::new();
        spawned.stdout.read_to_string(&mut output).await.unwrap();
        let status = spawned.wait().await.unwrap();

        assert!(status.success());
        assert_eq!(output, "C");
    }

    #[tokio::test]
    async fn cancellable_process_waits_for_its_process_group_to_exit() {
        let spawned = DefaultCommandRunner
            .spawn("sh", vec!["-c".into(), "echo ready; sleep 30".into()])
            .await
            .unwrap();

        let status = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            spawned.terminate_and_wait(),
        )
        .await
        .expect("cancelled process did not exit")
        .unwrap();

        assert!(!status.success());
    }

    #[tokio::test]
    async fn failed_termination_request_still_waits_for_a_confirmed_exit() {
        let spawned = SpawnedProcess::new_cancellable(
            Box::new(tokio::io::empty()),
            async { Ok(ExitStatus::from_raw(0)) },
            |_| {
                Box::pin(async {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "signal rejected",
                    ))
                })
            },
        );

        let status = spawned.terminate_and_wait().await.unwrap();

        assert!(status.success());
    }
}
