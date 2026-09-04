//! Low-level PTY transport for application-owned terminal sessions.

use crate::application::sessions::{
    terminal_session_channel, SessionError, TerminalCommand, TerminalSessionEndpoint,
    TerminalSessionHandle,
};
use crate::domain::session::{SessionId, SessionLifecycle, SessionSize, TerminalAttachmentKind};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::Ordering;

pub(crate) struct RemoteSessionControl {
    pub close: tokio::sync::oneshot::Receiver<()>,
    pub lifecycle: tokio::sync::watch::Sender<SessionLifecycle>,
}

pub(crate) fn spawn_direct_terminal(
    command: CommandBuilder,
    id: SessionId,
    attachment: TerminalAttachmentKind,
    size: SessionSize,
) -> Result<TerminalSessionHandle, SessionError> {
    let pair = native_pty_system()
        .openpty(pty_size(size))
        .map_err(|error| SessionError::new(format!("open terminal PTY: {error}")))?;
    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| SessionError::new(format!("spawn terminal attachment: {error}")))?;
    drop(pair.slave);

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| SessionError::new(format!("clone terminal reader: {error}")))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| SessionError::new(format!("open terminal writer: {error}")))?;
    let killer = child.clone_killer();
    let (handle, endpoint) = terminal_session_channel(id, attachment);
    let TerminalSessionEndpoint {
        commands,
        output,
        lifecycle,
        resize_failed,
        close,
    } = endpoint;

    spawn_reader(reader, output, lifecycle.clone());
    spawn_portable_writer(pair.master, writer, commands, resize_failed);
    spawn_direct_owner(child, killer, close, lifecycle);
    Ok(handle)
}

pub(crate) fn spawn_fd_terminal(
    master_fd: std::os::fd::RawFd,
    id: SessionId,
    attachment: TerminalAttachmentKind,
) -> Result<(TerminalSessionHandle, RemoteSessionControl), SessionError> {
    let master = unsafe { OwnedFd::from_raw_fd(master_fd) };
    let reader_fd = master
        .try_clone()
        .map_err(|error| SessionError::new(format!("clone elevated terminal fd: {error}")))?;
    let reader = std::fs::File::from(reader_fd);
    let writer = std::fs::File::from(master);
    let (handle, endpoint) = terminal_session_channel(id, attachment);
    let TerminalSessionEndpoint {
        commands,
        output,
        lifecycle,
        resize_failed,
        close,
    } = endpoint;

    // The daemon-owned status pipe is authoritative for elevated sessions;
    // PTY EOF alone does not prove the child's exit code.
    spawn_reader(reader, output, lifecycle.clone());
    spawn_fd_writer(writer, commands, resize_failed);
    Ok((handle, RemoteSessionControl { close, lifecycle }))
}

/// Attach a systemd-owned machine PTY. There is no child process for Lasper to wait
/// on, so PTY EOF is the terminal lifecycle boundary.
pub(crate) fn spawn_machine_terminal(
    master: OwnedFd,
    id: SessionId,
    size: SessionSize,
) -> Result<TerminalSessionHandle, SessionError> {
    set_fd_size(master.as_raw_fd(), size).map_err(|error| {
        SessionError::new(format!("set initial machine terminal size: {error}"))
    })?;
    let reader = std::fs::File::from(
        master
            .try_clone()
            .map_err(|error| SessionError::new(format!("clone machine terminal fd: {error}")))?,
    );
    let writer = std::fs::File::from(master);
    let (handle, endpoint) = terminal_session_channel(id, TerminalAttachmentKind::Login);
    let TerminalSessionEndpoint {
        commands,
        output,
        lifecycle,
        resize_failed,
        close,
    } = endpoint;

    spawn_machine_reader(reader, output, lifecycle.clone());
    spawn_fd_writer(writer, commands, resize_failed);
    tokio::spawn(async move {
        if close.await.is_ok() && lifecycle.borrow().is_running() {
            let _ = lifecycle.send(SessionLifecycle::Closed);
        }
    });
    Ok(handle)
}

fn pty_size(size: SessionSize) -> PtySize {
    PtySize {
        rows: size.rows(),
        cols: size.cols(),
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn spawn_reader(
    mut reader: impl Read + Send + 'static,
    output: tokio::sync::mpsc::Sender<Vec<u8>>,
    lifecycle: tokio::sync::watch::Sender<SessionLifecycle>,
) {
    tokio::task::spawn_blocking(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if output.blocking_send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(error) => {
                    if lifecycle.borrow().is_running() {
                        let _ = lifecycle.send(SessionLifecycle::Failed(format!(
                            "terminal output failed: {error}"
                        )));
                    }
                    return;
                }
            }
        }
    });
}

fn spawn_machine_reader(
    mut reader: std::fs::File,
    output: tokio::sync::mpsc::Sender<Vec<u8>>,
    lifecycle: tokio::sync::watch::Sender<SessionLifecycle>,
) {
    tokio::task::spawn_blocking(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    if lifecycle.borrow().is_running() {
                        let _ = lifecycle.send(SessionLifecycle::Exited {
                            success: true,
                            code: None,
                        });
                    }
                    return;
                }
                Ok(read) => {
                    if output.blocking_send(buffer[..read].to_vec()).is_err() {
                        return;
                    }
                }
                Err(error) if error.raw_os_error() == Some(libc::EIO) => {
                    if lifecycle.borrow().is_running() {
                        let _ = lifecycle.send(SessionLifecycle::Exited {
                            success: true,
                            code: None,
                        });
                    }
                    return;
                }
                Err(error) => {
                    if lifecycle.borrow().is_running() {
                        let _ = lifecycle.send(SessionLifecycle::Failed(format!(
                            "machine terminal output failed: {error}"
                        )));
                    }
                    return;
                }
            }
        }
    });
}

fn spawn_portable_writer(
    master: Box<dyn portable_pty::MasterPty + Send>,
    mut writer: Box<dyn Write + Send>,
    mut commands: tokio::sync::mpsc::Receiver<TerminalCommand>,
    resize_failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    tokio::task::spawn_blocking(move || {
        while let Some(command) = commands.blocking_recv() {
            match command {
                TerminalCommand::Input(bytes) | TerminalCommand::Reply(bytes) => {
                    if writer
                        .write_all(&bytes)
                        .and_then(|_| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
                TerminalCommand::Resize(size) => {
                    if let Err(error) = master.resize(pty_size(size)) {
                        resize_failed.store(true, Ordering::Release);
                        log::warn!(
                            "failed to resize terminal PTY to {}x{}: {error}",
                            size.cols(),
                            size.rows()
                        );
                    }
                }
            }
        }
    });
}

fn spawn_fd_writer(
    mut writer: std::fs::File,
    mut commands: tokio::sync::mpsc::Receiver<TerminalCommand>,
    resize_failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    tokio::task::spawn_blocking(move || {
        while let Some(command) = commands.blocking_recv() {
            match command {
                TerminalCommand::Input(bytes) | TerminalCommand::Reply(bytes) => {
                    if writer
                        .write_all(&bytes)
                        .and_then(|_| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
                TerminalCommand::Resize(size) => {
                    if let Err(error) = set_fd_size(writer.as_raw_fd(), size) {
                        resize_failed.store(true, Ordering::Release);
                        log::warn!(
                            "failed to resize elevated terminal PTY to {}x{}: {}",
                            size.cols(),
                            size.rows(),
                            error
                        );
                    }
                }
            }
        }
    });
}

fn set_fd_size(fd: std::os::fd::RawFd, size: SessionSize) -> std::io::Result<()> {
    let dimensions = libc::winsize {
        ws_row: size.rows(),
        ws_col: size.cols(),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &dimensions) } < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn spawn_direct_owner(
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    mut killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    close: tokio::sync::oneshot::Receiver<()>,
    lifecycle: tokio::sync::watch::Sender<SessionLifecycle>,
) {
    let mut wait = Box::pin(tokio::task::spawn_blocking(move || child.wait()));
    tokio::spawn(async move {
        tokio::select! {
            _ = close => {
                if let Err(error) = killer.kill() {
                    log::warn!("failed to close terminal child: {error}");
                }
                if let Err(error) = (&mut wait).await {
                    log::warn!("terminal child wait task failed after close: {error}");
                }
                let _ = lifecycle.send(SessionLifecycle::Closed);
            }
            result = &mut wait => {
                let state = match result {
                    Ok(Ok(status)) => SessionLifecycle::Exited {
                        success: status.success(),
                        code: i32::try_from(status.exit_code()).ok(),
                    },
                    Ok(Err(error)) => SessionLifecycle::Failed(format!(
                        "wait for terminal child: {error}"
                    )),
                    Err(error) => SessionLifecycle::Failed(format!(
                        "terminal child wait task failed: {error}"
                    )),
                };
                if lifecycle.borrow().is_running() {
                    let _ = lifecycle.send(state);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use portable_pty::CommandBuilder;
    use std::time::Duration;

    async fn wait_until_finished(handle: &TerminalSessionHandle) -> SessionLifecycle {
        for _ in 0..100 {
            let state = handle.lifecycle();
            if !state.is_running() {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("terminal session did not reach a terminal lifecycle state")
    }

    #[tokio::test]
    async fn direct_terminal_reports_child_exit() {
        let mut command = CommandBuilder::new("sh");
        command.args(["-c", "printf ready"]);
        let id = SessionId::new(1).unwrap();
        let size = SessionSize::new(80, 24).unwrap();
        let mut handle =
            spawn_direct_terminal(command, id, TerminalAttachmentKind::Login, size).unwrap();
        let mut output = handle.take_output().unwrap();
        let bytes = tokio::time::timeout(Duration::from_secs(1), output.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("ready"));
        assert!(matches!(
            wait_until_finished(&handle).await,
            SessionLifecycle::Exited { success: true, .. }
        ));
        handle.close();
    }

    #[tokio::test]
    async fn direct_terminal_close_reports_closed_after_child_reap() {
        let mut command = CommandBuilder::new("sh");
        command.args(["-c", "exec sleep 30"]);
        let id = SessionId::new(2).unwrap();
        let size = SessionSize::new(80, 24).unwrap();
        let mut handle =
            spawn_direct_terminal(command, id, TerminalAttachmentKind::Login, size).unwrap();
        let _ = handle.take_output();
        handle.close();
        assert!(matches!(
            wait_until_finished(&handle).await,
            SessionLifecycle::Closed
        ));
    }
}
