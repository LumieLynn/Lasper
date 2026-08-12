use anyhow::Result;
use parking_lot::Mutex;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

pub enum PtyMessage {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

/// Coalesces terminal output notifications while one redraw is queued.
///
/// PTY output can arrive much faster than a terminal frame can be rendered.
/// Keeping at most one notification prevents the shared app event queue from
/// being flooded while preserving a redraw after the current frame.
#[derive(Clone, Debug)]
pub struct RedrawGate {
    pending: Arc<AtomicBool>,
}

/// Reports resize/ioctl failures back to the UI-side session.  The next
/// render retries the latest desired dimensions instead of silently accepting
/// a stale child window size.
#[derive(Clone, Debug)]
pub struct ResizeState {
    failed: Arc<AtomicBool>,
}

impl ResizeState {
    pub fn new() -> Self {
        Self {
            failed: Arc::new(AtomicBool::new(false)),
        }
    }

    fn mark_failed(&self) {
        self.failed.store(true, Ordering::Release);
    }

    pub fn take_failure(&self) -> bool {
        self.failed.swap(false, Ordering::AcqRel)
    }
}

impl RedrawGate {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn clear(&self) {
        self.pending.store(false, Ordering::Release);
    }

    fn notify(&self, tx: &mpsc::Sender<crate::events::AppEvent>) {
        if self
            .pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            && tx
                .try_send(crate::events::AppEvent::TerminalRedraw)
                .is_err()
        {
            // A full or closed queue must not leave the gate permanently set.
            self.clear();
        }
    }
}

pub struct TerminalHandle {
    pub reader: tokio::task::JoinHandle<()>,
    pub writer: tokio::task::JoinHandle<()>,
    pub child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    /// In the elevated path the reader/writer threads own the PTY master
    /// fds wrapped in `File`.  `abort()` kills the threads without running
    /// destructors, so the fds leak unless we close them here.
    elevated_fds: Option<(RawFd, RawFd)>,
}

impl TerminalHandle {
    pub fn abort(&mut self) {
        self.reader.abort();
        self.writer.abort();
        if let Some(ref mut child) = self.child {
            let _ = child.kill();
        }
        if let Some((rfd, wfd)) = self.elevated_fds {
            unsafe {
                libc::close(rfd);
                libc::close(wfd);
            }
        }
    }
}

#[allow(clippy::type_complexity)]
pub fn spawn_terminal(
    cmd_name: &str,
    args: &[&str],
    cols: u16,
    rows: u16,
    app_tx: tokio::sync::mpsc::Sender<crate::events::AppEvent>,
    redraw_gate: RedrawGate,
) -> Result<(
    Arc<Mutex<crate::term::Parser>>,
    mpsc::Sender<PtyMessage>,
    TerminalHandle,
    ResizeState,
)> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(cmd_name);
    cmd.args(args);

    let child = pair.slave.spawn_command(cmd)?;

    // Master handles
    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;

    let (pty_tx, mut pty_rx) = mpsc::channel::<PtyMessage>(1024);
    let resize_state = ResizeState::new();

    // 10,000 lines of scrollback
    let parser = Arc::new(Mutex::new(crate::term::Parser::new(rows, cols, 10000)));

    let parser_clone = parser.clone();
    let app_tx_clone = app_tx.clone();
    let redraw_gate_clone = redraw_gate.clone();

    // Reading thread
    let reader_handle = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            {
                let mut p = parser_clone.lock();
                let mut events = Vec::new();
                p.screen.process(&buf[..n], &mut events);
            }
            redraw_gate_clone.notify(&app_tx_clone);
        }
    });

    let parser_for_write = parser.clone();
    let master_for_write = pair.master;
    let resize_state_for_write = resize_state.clone();

    // Writing/Resize thread
    let writer_handle = tokio::task::spawn_blocking(move || {
        while let Some(msg) = pty_rx.blocking_recv() {
            match msg {
                PtyMessage::Data(bytes) => {
                    let _ = writer.write_all(&bytes);
                    let _ = writer.flush();
                }
                PtyMessage::Resize { cols, rows } => {
                    if let Err(error) = master_for_write.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    }) {
                        resize_state_for_write.mark_failed();
                        log::warn!("failed to resize terminal PTY to {cols}x{rows}: {error}");
                    }
                    let mut p = parser_for_write.lock();
                    p.set_size(rows, cols);
                }
            }
        }
    });

    Ok((
        parser,
        pty_tx,
        TerminalHandle {
            reader: reader_handle,
            writer: writer_handle,
            child: Some(child),
            elevated_fds: None,
        },
        resize_state,
    ))
}

#[allow(clippy::type_complexity)]
pub fn spawn_terminal_with_fd(
    master_fd: std::os::unix::io::RawFd,
    cols: u16,
    rows: u16,
    app_tx: tokio::sync::mpsc::Sender<crate::events::AppEvent>,
    redraw_gate: RedrawGate,
) -> Result<(
    Arc<Mutex<crate::term::Parser>>,
    mpsc::Sender<PtyMessage>,
    TerminalHandle,
    ResizeState,
)> {
    use std::io::{Read, Write};
    use std::os::unix::io::FromRawFd;

    // Dup the fd: one for reading, one for writing+resize.
    // elevated_fds is the canonical owner — it closes the fds in abort().
    // ManuallyDrop on the File wrappers prevents double-close since
    // spawn_blocking threads may be aborted without running destructors.
    let reader_fd = master_fd;
    let writer_fd = unsafe { libc::dup(master_fd) };
    let elevated_fds = Some((reader_fd, writer_fd));
    if writer_fd < 0 {
        return Err(anyhow::anyhow!(
            "dup failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    let mut reader = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(reader_fd) });
    let mut writer = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(writer_fd) });

    let (pty_tx, mut pty_rx) = mpsc::channel::<PtyMessage>(1024);
    let resize_state = ResizeState::new();

    let parser = Arc::new(Mutex::new(crate::term::Parser::new(rows, cols, 10000)));

    let parser_clone = parser.clone();
    let app_tx_clone = app_tx.clone();
    let redraw_gate_clone = redraw_gate.clone();

    // Reading thread
    let reader_handle = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let mut p = parser_clone.lock();
                    let mut events = Vec::new();
                    p.screen.process(&buf[..n], &mut events);
                    redraw_gate_clone.notify(&app_tx_clone);
                }
                Err(_) => break,
            }
        }
    });

    let parser_for_write = parser.clone();
    let resize_fd = master_fd; // original fd, still valid (we dup'd for writer)
    let resize_state_for_write = resize_state.clone();

    // Writing/Resize thread
    let writer_handle = tokio::task::spawn_blocking(move || {
        while let Some(msg) = pty_rx.blocking_recv() {
            match msg {
                PtyMessage::Data(bytes) => {
                    let _ = writer.write_all(&bytes);
                    let _ = writer.flush();
                }
                PtyMessage::Resize { cols, rows } => {
                    let ws = libc::winsize {
                        ws_row: rows,
                        ws_col: cols,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    };
                    let result = unsafe { libc::ioctl(resize_fd, libc::TIOCSWINSZ, &ws) };
                    if result < 0 {
                        resize_state_for_write.mark_failed();
                        log::warn!(
                            "failed to resize elevated terminal PTY to {cols}x{rows}: {}",
                            std::io::Error::last_os_error()
                        );
                    }
                    let mut p = parser_for_write.lock();
                    p.set_size(rows, cols);
                }
            }
        }
    });

    Ok((
        parser,
        pty_tx,
        TerminalHandle {
            reader: reader_handle,
            writer: writer_handle,
            child: None,
            elevated_fds,
        },
        resize_state,
    ))
}

#[cfg(test)]
mod tests {
    use super::RedrawGate;
    use crate::events::AppEvent;

    #[test]
    fn redraw_notifications_are_coalesced_until_consumed() {
        let gate = RedrawGate::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);

        gate.notify(&tx);
        gate.notify(&tx);
        assert!(matches!(rx.try_recv(), Ok(AppEvent::TerminalRedraw)));
        assert!(rx.try_recv().is_err());

        gate.clear();
        gate.notify(&tx);
        assert!(matches!(rx.try_recv(), Ok(AppEvent::TerminalRedraw)));
    }
}
