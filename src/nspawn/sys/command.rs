//! Command builder helpers.
//!
//! All commands have stdout/stderr piped to prevent leaking into the TUI's
//! raw-mode terminal. Use [`CommandLogged::logged_output`] to run a command
//! and automatically route its output through the `log` crate.

use std::process::{Output, Stdio};

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
