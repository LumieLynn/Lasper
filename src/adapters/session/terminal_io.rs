//! Process-level I/O bridges for selected-user shell commands.
//!
//! Session creation and machine attachment are deliberately elsewhere.  This
//! module owns either the caller's interactive terminal or the output-only
//! launcher stream once a `TerminalSessionHandle` has been opened.

use crate::application::sessions::{SessionError, SessionSendStatus, TerminalSessionHandle};
use crate::domain::session::{SessionLifecycle, SessionSize};
use std::io::IsTerminal;

const DEFAULT_LAUNCHER_COLUMNS: u16 = 80;
const DEFAULT_LAUNCHER_ROWS: u16 = 24;

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

/// Choose the initial PTY geometry required by `OpenMachineShell` without
/// claiming ownership of an interactive terminal.
pub(crate) fn launcher_terminal_size() -> SessionSize {
    if std::io::stdout().is_terminal() {
        if let Ok((cols, rows)) = crossterm::terminal::size() {
            if let Ok(size) = SessionSize::new(cols, rows) {
                return size;
            }
        }
    }

    let cols = environment_dimension("COLUMNS").unwrap_or(DEFAULT_LAUNCHER_COLUMNS);
    let rows = environment_dimension("LINES").unwrap_or(DEFAULT_LAUNCHER_ROWS);
    SessionSize::new(cols, rows).expect("launcher fallback terminal dimensions are non-zero")
}

fn environment_dimension(name: &str) -> Option<u16> {
    std::env::var(name)
        .ok()
        .as_deref()
        .and_then(parse_dimension)
}

fn parse_dimension(value: &str) -> Option<u16> {
    value.parse().ok().filter(|value| *value > 0)
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

    terminal_exit_code(state)
}

/// Forward a launcher session's output without reading stdin or taking over a
/// terminal. The process stays alive until the guest command and PTY output
/// have both completed.
pub(crate) async fn run_launcher_terminal(
    handle: TerminalSessionHandle,
) -> Result<i32, SessionError> {
    run_launcher_terminal_to(handle, tokio::io::stdout()).await
}

async fn run_launcher_terminal_to<W>(
    mut handle: TerminalSessionHandle,
    mut destination: W,
) -> Result<i32, SessionError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    let mut output = handle
        .take_output()
        .ok_or_else(|| SessionError::new("launcher session output is unavailable"))?;
    let mut lifecycle = Box::pin(handle.wait());
    let mut completed = None;
    let state = loop {
        if let Some(state) = completed.take() {
            while let Some(chunk) = output.recv().await {
                destination.write_all(&chunk).await.map_err(|error| {
                    SessionError::new(format!("write launcher output: {error}"))
                })?;
            }
            destination
                .flush()
                .await
                .map_err(|error| SessionError::new(format!("flush launcher output: {error}")))?;
            break state;
        }

        tokio::select! {
            chunk = output.recv() => {
                match chunk {
                    Some(chunk) => {
                        destination.write_all(&chunk).await.map_err(|error| {
                            SessionError::new(format!("write launcher output: {error}"))
                        })?;
                        destination.flush().await.map_err(|error| {
                            SessionError::new(format!("flush launcher output: {error}"))
                        })?;
                    }
                    None => break lifecycle.await,
                }
            }
            state = &mut lifecycle => completed = Some(state),
        }
    };

    terminal_exit_code(state)
}

fn terminal_exit_code(state: SessionLifecycle) -> Result<i32, SessionError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::session::pty::spawn_direct_terminal;
    use crate::domain::session::{SessionId, TerminalAttachmentKind};
    use portable_pty::CommandBuilder;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn launcher_forwards_output_and_preserves_command_status() {
        let mut command = CommandBuilder::new("sh");
        command.args(["-c", "printf launcher-ready; exit 7"]);
        let id = SessionId::new(3).unwrap();
        let size = SessionSize::new(80, 24).unwrap();
        let handle =
            spawn_direct_terminal(command, id, TerminalAttachmentKind::Login, size).unwrap();
        let (destination, mut captured) = tokio::io::duplex(1024);

        let status = run_launcher_terminal_to(handle, destination).await.unwrap();
        let mut output = Vec::new();
        captured.read_to_end(&mut output).await.unwrap();

        assert_eq!(status, 7);
        assert!(String::from_utf8_lossy(&output).contains("launcher-ready"));
    }

    #[test]
    fn launcher_dimensions_reject_invalid_values() {
        assert_eq!(parse_dimension(""), None);
        assert_eq!(parse_dimension("0"), None);
        assert_eq!(parse_dimension("65536"), None);
        assert_eq!(parse_dimension("24"), Some(24));
    }
}
