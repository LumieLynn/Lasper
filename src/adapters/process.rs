//! Command builder helpers.

use crate::domain::secret::SecretBytes;
use std::future::Future;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::pin::Pin;
use std::process::{ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

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

/// Return the most useful diagnostic emitted by a completed command.
///
/// Most systemd client errors are written to stderr, but a few status-style
/// commands communicate failure state through stdout instead. Keep stderr as
/// the authoritative stream when it is present, then retain the other stream
/// and finally the exit status rather than returning an empty error message.
pub(crate) fn command_diagnostic(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return stdout;
    }

    format!("process exited with {}", output.status)
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

    /// Run a short host query with an explicit completion deadline.
    ///
    /// Implementations must terminate the child process group when the
    /// deadline expires. A `TimedOut` error must state whether child exit was
    /// confirmed; callers performing mutations must treat either result as an
    /// unknown operation outcome.
    async fn run_bounded(
        &self,
        program: &str,
        args: Vec<String>,
        timeout: std::time::Duration,
    ) -> std::io::Result<Output>;

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

    async fn run_bounded(
        &self,
        program: &str,
        args: Vec<String>,
        timeout: std::time::Duration,
    ) -> std::io::Result<Output> {
        run_bounded_command(program, args, timeout).await
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

const BOUNDED_TERMINATION_GRACE: std::time::Duration = std::time::Duration::from_secs(1);
const MAX_BOUNDED_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

async fn run_bounded_command(
    program: &str,
    args: Vec<String>,
    timeout: std::time::Duration,
) -> std::io::Result<Output> {
    let mut command = new_command(program);
    command.args(args);
    run_bounded_child_command(command, None, timeout, program, MAX_BOUNDED_OUTPUT_BYTES).await
}

/// Run one configured child in a private process group with bounded captured
/// output. The command's stdin/stdout/stderr configuration is replaced.
pub(crate) async fn run_bounded_child_command(
    mut command: tokio::process::Command,
    input: Option<SecretBytes>,
    timeout: std::time::Duration,
    label: &str,
    output_limit: usize,
) -> std::io::Result<Output> {
    if input.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .as_std_mut()
        .process_group(0);

    let mut child = command.spawn()?;
    let pid = child.id().ok_or_else(|| {
        std::io::Error::other(format!("bounded command {label} has no process id"))
    })?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("bounded child stdout was piped");
    let stderr = child.stderr.take().expect("bounded child stderr was piped");

    let mut completion = Box::pin(async {
        let write_input = async move {
            if let (Some(mut stdin), Some(input)) = (stdin, input) {
                stdin.write_all(input.as_slice()).await?;
                stdin.shutdown().await?;
            }
            Ok::<(), std::io::Error>(())
        };
        let (status, input_result, stdout, stderr) = tokio::join!(
            child.wait(),
            write_input,
            read_bounded_stream(stdout, output_limit),
            read_bounded_stream(stderr, output_limit),
        );
        input_result?;
        Ok::<Output, std::io::Error>(Output {
            status: status?,
            stdout: stdout?,
            stderr: stderr?,
        })
    });

    if let Ok(result) = tokio::time::timeout(timeout, &mut completion).await {
        return result;
    }
    drop(completion);

    log::warn!(
        "bounded command {label} exceeded its {}ms deadline; terminating process group {pid}",
        timeout.as_millis()
    );
    let termination =
        terminate_child_process_group(&mut child, pid, label, BOUNDED_TERMINATION_GRACE).await;
    let detail = match termination {
        Ok(_) => "termination was confirmed".to_string(),
        Err(error) => format!("process exit was not confirmed: {error}"),
    };
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("command {label} exceeded its completion deadline; {detail}"),
    ))
}

pub(crate) async fn terminate_child_process_group(
    child: &mut tokio::process::Child,
    pid: u32,
    label: &str,
    grace: std::time::Duration,
) -> std::io::Result<ExitStatus> {
    let term_error = signal_process_group(pid, libc::SIGTERM).err();
    if let Ok(status) = tokio::time::timeout(grace, child.wait()).await {
        return status;
    }

    let kill_error = signal_process_group(pid, libc::SIGKILL).err();
    match tokio::time::timeout(grace, child.wait()).await {
        Ok(status) => status,
        Err(_) => {
            let signals = match (term_error, kill_error) {
                (Some(term), Some(kill)) => {
                    format!("SIGTERM failed ({term}); SIGKILL failed ({kill})")
                }
                (Some(term), None) => format!("SIGTERM failed ({term})"),
                (None, Some(kill)) => format!("SIGKILL failed ({kill})"),
                (None, None) => "SIGTERM and SIGKILL were delivered".into(),
            };
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("could not confirm that {label} exited after {signals}"),
            ))
        }
    }
}

pub(crate) async fn read_bounded_stream(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<Vec<u8>> {
    const MARKER: &[u8] = b"\n[command output truncated]\n";

    let mut kept = Vec::with_capacity(limit.min(4096));
    let mut chunk = [0u8; 4096];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(kept.len());
        let take = count.min(remaining);
        kept.extend_from_slice(&chunk[..take]);
        truncated |= take < count;
    }
    if truncated {
        let marker_start = limit.saturating_sub(MARKER.len());
        kept.truncate(marker_start);
        kept.extend_from_slice(&MARKER[..MARKER.len().min(limit)]);
    }
    Ok(kept)
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

    #[tokio::test]
    async fn bounded_commands_return_output_before_the_deadline() {
        let output = DefaultCommandRunner
            .run_bounded(
                "sh",
                vec!["-c".into(), "printf ready".into()],
                std::time::Duration::from_secs(1),
            )
            .await
            .unwrap();

        assert!(output.status.success());
        assert_eq!(output.stdout, b"ready");
    }

    #[tokio::test]
    async fn bounded_commands_terminate_their_process_group_on_timeout() {
        let error = DefaultCommandRunner
            .run_bounded(
                "sh",
                vec!["-c".into(), "sleep 30".into()],
                std::time::Duration::from_millis(25),
            )
            .await
            .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("termination was confirmed"));
    }

    #[tokio::test]
    async fn bounded_commands_drain_but_cap_captured_output() {
        let output = DefaultCommandRunner
            .run_bounded(
                "sh",
                vec!["-c".into(), "head -c 9000000 /dev/zero".into()],
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();

        assert!(output.status.success());
        assert_eq!(output.stdout.len(), MAX_BOUNDED_OUTPUT_BYTES);
        assert!(output.stdout.ends_with(b"[command output truncated]\n"));
    }

    #[test]
    fn command_diagnostic_prefers_stderr_then_stdout_then_status() {
        let stderr = Output {
            status: ExitStatus::from_raw(256),
            stdout: b"stdout diagnostic".to_vec(),
            stderr: b"stderr diagnostic\n".to_vec(),
        };
        assert_eq!(command_diagnostic(&stderr), "stderr diagnostic");

        let stdout = Output {
            status: ExitStatus::from_raw(256),
            stdout: b"stdout diagnostic\n".to_vec(),
            stderr: Vec::new(),
        };
        assert_eq!(command_diagnostic(&stdout), "stdout diagnostic");

        let status = Output {
            status: ExitStatus::from_raw(256),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert!(command_diagnostic(&status).contains("exit status"));
    }
}
