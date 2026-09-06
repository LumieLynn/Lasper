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

/// Login PTYs survive getty hangups until machine removal or explicit close.
/// Shell PTYs finish after a sustained hangup, allowing startup/reset gaps.
pub(crate) fn spawn_machine_terminal(
    pty: super::MachinePty,
    id: SessionId,
    size: SessionSize,
) -> Result<TerminalSessionHandle, SessionError> {
    let super::MachinePty {
        master,
        machine_removed,
    } = pty;
    set_fd_size(master.as_raw_fd(), size).map_err(|error| {
        SessionError::new(format!("set initial machine terminal size: {error}"))
    })?;
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(SessionError::new(format!(
            "set machine PTY nonblocking: {}",
            std::io::Error::last_os_error()
        )));
    }
    let master = tokio::io::unix::AsyncFd::new(master)
        .map_err(|error| SessionError::new(format!("watch machine PTY: {error}")))?;
    let (handle, endpoint) = terminal_session_channel(id, TerminalAttachmentKind::Login);
    tokio::spawn(async move {
        let TerminalSessionEndpoint {
            mut commands,
            output,
            lifecycle,
            resize_failed,
            mut close,
        } = endpoint;
        let login = machine_removed.is_some();
        let removed = async move {
            match machine_removed {
                Some(removed) => removed.await.unwrap_or_else(|_| {
                    SessionLifecycle::Failed("machine removal monitor closed".into())
                }),
                None => std::future::pending().await,
            }
        };
        tokio::pin!(removed);
        let started = tokio::time::Instant::now();
        let mut hangup_since = None;
        let mut read_retry_at = None;
        let mut write_retry_at = None;
        let mut pending_input: Vec<u8> = Vec::new();
        let mut buffer = [0u8; 4096];
        let state = loop {
            let now = tokio::time::Instant::now();
            if !login
                && hangup_since.is_some_and(|since| {
                    now.duration_since(since) >= std::time::Duration::from_millis(250)
                })
                && now.duration_since(started) >= std::time::Duration::from_secs(1)
            {
                break SessionLifecycle::Exited {
                    success: true,
                    code: None,
                };
            }
            tokio::select! {
                _ = &mut close => break SessionLifecycle::Closed,
                state = &mut removed => break state,
                _ = output.closed() => break SessionLifecycle::Closed,
                command = commands.recv(), if pending_input.is_empty() => match command {
                    Some(TerminalCommand::Input(bytes) | TerminalCommand::Reply(bytes)) => pending_input = bytes,
                    Some(TerminalCommand::Resize(size)) => {
                        if set_fd_size(master.as_raw_fd(), size).is_err() { resize_failed.store(true, Ordering::Release); }
                    }
                    None => break SessionLifecycle::Closed,
                },
                _ = wait_for_machine_pty_retry(read_retry_at), if read_retry_at.is_some() => {
                    read_retry_at = None;
                },
                _ = wait_for_machine_pty_retry(write_retry_at), if write_retry_at.is_some() => {
                    write_retry_at = None;
                },
                (result, read_closed) = read_machine_pty(&master, &mut buffer), if read_retry_at.is_none() => match result {
                    Ok(0) => {
                        hangup_since.get_or_insert(tokio::time::Instant::now());
                        read_retry_at = Some(machine_pty_retry_deadline());
                    }
                    Ok(count) => {
                        hangup_since = None;
                        tokio::select! {
                            _ = &mut close => break SessionLifecycle::Closed,
                            state = &mut removed => break state,
                            result = output.send(buffer[..count].to_vec()) => {
                                if result.is_err() { break SessionLifecycle::Closed; }
                            }
                        }
                    }
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => {
                        hangup_since.get_or_insert(tokio::time::Instant::now());
                        read_retry_at = Some(machine_pty_retry_deadline());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        hangup_since = None;
                        if read_closed {
                            read_retry_at = Some(machine_pty_retry_deadline());
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {},
                    Err(error) => break SessionLifecycle::Failed(format!("machine terminal output failed: {error}")),
                },
                result = write_machine_pty(&master, &pending_input), if !pending_input.is_empty() && write_retry_at.is_none() => match result {
                    Ok(0) => break SessionLifecycle::Failed("machine PTY write returned zero".into()),
                    Ok(count) => {
                        pending_input.drain(..count);
                    }
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => {
                        write_retry_at = Some(machine_pty_retry_deadline());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {},
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {},
                    Err(error) => break SessionLifecycle::Failed(format!("machine terminal input failed: {error}")),
                },
            }
        };
        // Dropping the single owner closes both directions, even if input or
        // output was idle or backpressured when close was requested.
        drop(master);
        let _ = lifecycle.send(state);
    });
    Ok(handle)
}

fn machine_pty_retry_deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + std::time::Duration::from_millis(25)
}

async fn wait_for_machine_pty_retry(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

// AsyncFd::async_io internally retries WouldBlock. A reopened PTY may retain
// cached HUP readiness, so return to the owner's timed retry instead of
// spinning inside that helper and starving close/resize/timer processing.
async fn read_machine_pty(
    master: &tokio::io::unix::AsyncFd<OwnedFd>,
    buffer: &mut [u8],
) -> (std::io::Result<usize>, bool) {
    let mut ready = match master.readable().await {
        Ok(ready) => ready,
        Err(error) => return (Err(error), false),
    };
    let read_closed = ready.ready().is_read_closed();
    let result = ready
        .try_io(|fd| {
            let count =
                unsafe { libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
            if count < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(count as usize)
            }
        })
        .unwrap_or_else(|_| Err(std::io::ErrorKind::WouldBlock.into()));
    (result, read_closed)
}

async fn write_machine_pty(
    master: &tokio::io::unix::AsyncFd<OwnedFd>,
    bytes: &[u8],
) -> std::io::Result<usize> {
    let mut ready = master.writable().await?;
    ready
        .try_io(|fd| {
            let count = unsafe { libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
            if count < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(count as usize)
            }
        })
        .unwrap_or_else(|_| Err(std::io::ErrorKind::WouldBlock.into()))
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
    use std::os::unix::fs::OpenOptionsExt;
    use std::time::Duration;

    fn machine_pty() -> (OwnedFd, std::fs::File, std::path::PathBuf) {
        let mut master = -1;
        let mut slave = -1;
        let mut name = [0 as libc::c_char; 128];
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    name.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
            .to_str()
            .unwrap();
        (
            unsafe { OwnedFd::from_raw_fd(master) },
            unsafe { std::fs::File::from_raw_fd(slave) },
            name.into(),
        )
    }

    fn open_test_machine(
        master: OwnedFd,
        removed: Option<tokio::sync::oneshot::Receiver<SessionLifecycle>>,
    ) -> TerminalSessionHandle {
        spawn_machine_terminal(
            super::super::MachinePty {
                master,
                machine_removed: removed,
            },
            SessionId::new(3).unwrap(),
            SessionSize::new(80, 24).unwrap(),
        )
        .unwrap()
    }

    async fn wait_until_finished(handle: &TerminalSessionHandle) -> SessionLifecycle {
        for _ in 0..300 {
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

    #[tokio::test]
    async fn machine_login_survives_getty_restart_until_machine_removal() {
        let (master, slave, path) = machine_pty();
        let (removed, removal) = tokio::sync::oneshot::channel();
        let mut handle = open_test_machine(master, Some(removal));
        let mut output = handle.take_output().unwrap();
        drop(slave);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(handle.lifecycle().is_running());

        for _ in 0..2 {
            let mut slave = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
                .open(&path)
                .unwrap();
            slave.write_all(b"login: ").unwrap();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), output.recv())
                    .await
                    .unwrap()
                    .unwrap(),
                b"login: "
            );
            drop(slave);
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(handle.lifecycle().is_running());
        }
        removed
            .send(SessionLifecycle::Exited {
                success: true,
                code: None,
            })
            .unwrap();
        assert!(matches!(
            wait_until_finished(&handle).await,
            SessionLifecycle::Exited { .. }
        ));
    }

    #[tokio::test]
    async fn machine_terminal_accepts_input_after_a_completed_write() {
        let (master, _slave, _) = machine_pty();
        let (removed, removal) = tokio::sync::oneshot::channel();
        let mut handle = open_test_machine(master, Some(removal));
        let input = handle.input();

        assert_eq!(
            input.send_input(vec![b'x']).await,
            crate::application::sessions::SessionSendStatus::Queued
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(handle.lifecycle().is_running());
        assert_eq!(
            input.send_input(vec![b'y']).await,
            crate::application::sessions::SessionSendStatus::Queued
        );

        removed
            .send(SessionLifecycle::Exited {
                success: true,
                code: None,
            })
            .unwrap();
        assert!(matches!(
            wait_until_finished(&handle).await,
            SessionLifecycle::Exited { .. }
        ));
        handle.close();
    }

    #[tokio::test]
    async fn machine_login_round_trips_are_not_throttled_between_echoes() {
        let (master, _slave, _) = machine_pty();
        let (_removed, removal) = tokio::sync::oneshot::channel();
        let mut handle = open_test_machine(master, Some(removal));
        let mut output = handle.take_output().unwrap();
        let input = handle.input();

        tokio::time::timeout(Duration::from_millis(800), async {
            for _ in 0..64 {
                assert_eq!(
                    input.send_input(vec![b'x']).await,
                    crate::application::sessions::SessionSendStatus::Queued
                );
                let echoed = output.recv().await.unwrap();
                assert!(echoed.contains(&b'x'));
            }
        })
        .await
        .expect("D-Bus PTY input was throttled between echoed characters");
        handle.close();
    }

    #[tokio::test]
    async fn machine_shell_recovers_a_reset_hangup_after_initial_output() {
        let (master, mut slave, path) = machine_pty();
        let mut handle = open_test_machine(master, None);
        let mut output = handle.take_output().unwrap();
        slave.write_all(b"reset").unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), output.recv())
                .await
                .unwrap()
                .unwrap(),
            b"reset"
        );
        drop(slave);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(handle.lifecycle().is_running());
        let mut slave = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        slave.write_all(b"shell ready").unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), output.recv())
                .await
                .unwrap()
                .unwrap(),
            b"shell ready"
        );
        drop(slave);
        assert!(matches!(
            wait_until_finished(&handle).await,
            SessionLifecycle::Exited { .. }
        ));
    }

    #[tokio::test]
    async fn machine_shell_without_output_eventually_exits() {
        let (master, slave, _) = machine_pty();
        let handle = open_test_machine(master, None);
        drop(slave);
        assert!(matches!(
            wait_until_finished(&handle).await,
            SessionLifecycle::Exited { .. }
        ));
    }

    #[tokio::test]
    async fn closing_idle_machine_login_releases_the_pty_and_monitor() {
        let (master, _slave, _) = machine_pty();
        let (mut removed, removal) = tokio::sync::oneshot::channel();
        let mut handle = open_test_machine(master, Some(removal));
        let mut output = handle.take_output().unwrap();
        handle.close();
        assert_eq!(wait_until_finished(&handle).await, SessionLifecycle::Closed);
        assert!(tokio::time::timeout(Duration::from_secs(1), output.recv())
            .await
            .unwrap()
            .is_none());
        tokio::time::timeout(Duration::from_secs(1), removed.closed())
            .await
            .unwrap();
    }
}
