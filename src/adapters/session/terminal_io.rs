//! Foreground terminal I/O bridge for process-level shell commands.
//!
//! Session creation and machine attachment are deliberately elsewhere.  This
//! module only owns the caller's stdin/stdout, raw-mode guard, and resize
//! forwarding once a `TerminalSessionHandle` has been opened.

use crate::application::sessions::{SessionError, SessionSendStatus, TerminalSessionHandle};
use crate::domain::session::{SessionLifecycle, SessionSize};
use std::io::IsTerminal;

pub(crate) fn inherited_terminal_size() -> Result<SessionSize, SessionError> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(SessionError::new(
            "lasper shell requires an interactive terminal on stdin and stdout",
        ));
    }
    let (cols, rows) = crossterm::terminal::size()
        .map_err(|error| SessionError::new(format!("read terminal size: {error}")))?;
    SessionSize::new(cols, rows)
        .map_err(|error| SessionError::new(format!("validate terminal size: {error}")))
}

/// Forward the caller's terminal byte-for-byte to an application session.
pub(crate) async fn run_inherited_terminal(
    mut handle: TerminalSessionHandle,
) -> Result<i32, SessionError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct RawModeGuard;
    impl RawModeGuard {
        fn enter() -> Result<Self, SessionError> {
            crossterm::terminal::enable_raw_mode()
                .map_err(|error| SessionError::new(format!("enable terminal raw mode: {error}")))?;
            Ok(Self)
        }
    }
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }

    let mut output = handle
        .take_output()
        .ok_or_else(|| SessionError::new("terminal session output is unavailable"))?;
    let input = handle.input();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut input_buffer = [0u8; 4096];
    let mut resize = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
        .map_err(|error| SessionError::new(format!("watch terminal size: {error}")))?;
    let raw_mode = RawModeGuard::enter()?;
    let mut lifecycle = Box::pin(handle.wait());
    let mut completed = None;
    let state = loop {
        if let Some(state) = completed.take() {
            while let Some(chunk) = output.recv().await {
                stdout.write_all(&chunk).await.map_err(|error| {
                    SessionError::new(format!("write terminal output: {error}"))
                })?;
            }
            stdout
                .flush()
                .await
                .map_err(|error| SessionError::new(format!("flush terminal output: {error}")))?;
            break state;
        }
        tokio::select! {
            read = stdin.read(&mut input_buffer) => {
                let read = read.map_err(|error| SessionError::new(format!("read terminal input: {error}")))?;
                if read == 0 {
                    break SessionLifecycle::Closed;
                }
                if input.send_input(input_buffer[..read].to_vec()).await == SessionSendStatus::Closed {
                    break lifecycle.await;
                }
            }
            chunk = output.recv() => {
                match chunk {
                    Some(chunk) => {
                        stdout
                            .write_all(&chunk)
                            .await
                            .map_err(|error| SessionError::new(format!("write terminal output: {error}")))?;
                        stdout
                            .flush()
                            .await
                            .map_err(|error| SessionError::new(format!("flush terminal output: {error}")))?;
                    }
                    None => break lifecycle.await,
                }
            }
            _ = resize.recv() => {
                if let Ok(size) = inherited_terminal_size() {
                    let _ = input.try_resize(size);
                }
            }
            state = &mut lifecycle => completed = Some(state),
        }
    };
    drop(raw_mode);

    match state {
        SessionLifecycle::Exited { success, code } => {
            Ok(code.unwrap_or(if success { 0 } else { 1 }))
        }
        SessionLifecycle::Closed => Ok(0),
        SessionLifecycle::Failed(message) => Err(SessionError::new(message)),
        SessionLifecycle::Running => Err(SessionError::new(
            "terminal session returned without a terminal lifecycle state",
        )),
    }
}
