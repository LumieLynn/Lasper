//! Command builder helpers.

use std::pin::Pin;
use std::future::Future;
use std::process::{ExitStatus, Output, Stdio};

/// Handle to a spawned command: stderr merged into stdout via `2>&1`,
/// exit status retrievable via [`SpawnedProcess::wait`].
pub struct SpawnedProcess {
    pub stdout: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    wait_fn: Option<Pin<Box<dyn Future<Output = std::io::Result<ExitStatus>> + Send>>>,
}

impl SpawnedProcess {
    pub fn new(
        stdout: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        wait_fn: impl Future<Output = std::io::Result<ExitStatus>> + Send + 'static,
    ) -> Self {
        Self {
            stdout,
            wait_fn: Some(Box::pin(wait_fn)),
        }
    }

    pub async fn wait(mut self) -> std::io::Result<ExitStatus> {
        // Drain any unread stdout before waiting — avoids pipe deadlocks.
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 4096];
        while self.stdout.read(&mut buf).await? > 0 {}
        drop(self.stdout);

        self.wait_fn.take().unwrap().await
    }
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
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("exec \"$@\" 2>&1")
            .arg("--")
            .args(&argv)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdout = Box::new(child.stdout.take().expect("stdout piped"));
        Ok(SpawnedProcess::new(stdout, async move {
            child.wait_with_output().await.map(|o| o.status)
        }))
    }
}

/// Creates a new `tokio::process::Command` with `LC_ALL=C` set
/// and stdout/stderr piped by default to prevent leaking output
/// into the TUI's raw-mode terminal.
pub fn new_command(program: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.env("LC_ALL", "C");
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd
}

/// Creates a new `std::process::Command` with `LC_ALL=C` set
/// and stdout/stderr piped by default to prevent leaking output
/// into the TUI's raw-mode terminal.
pub fn new_sync_command(program: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    cmd.env("LC_ALL", "C");
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd
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
