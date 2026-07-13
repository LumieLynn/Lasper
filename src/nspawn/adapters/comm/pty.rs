use anyhow::Result;
use parking_lot::Mutex;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::os::unix::io::RawFd;
use std::sync::Arc;
use tokio::sync::mpsc;

pub enum PtyMessage {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
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
) -> Result<(
    Arc<Mutex<crate::term::Parser>>,
    mpsc::Sender<PtyMessage>,
    TerminalHandle,
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

    // 10,000 lines of scrollback
    let parser = Arc::new(Mutex::new(crate::term::Parser::new(rows, cols, 10000)));

    let parser_clone = parser.clone();
    let app_tx_clone = app_tx.clone();

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
            let _ = app_tx_clone.try_send(crate::events::AppEvent::TerminalRedraw);
        }
    });

    let parser_for_write = parser.clone();
    let master_for_write = pair.master;

    // Writing/Resize thread
    let writer_handle = tokio::task::spawn_blocking(move || {
        while let Some(msg) = pty_rx.blocking_recv() {
            match msg {
                PtyMessage::Data(bytes) => {
                    let _ = writer.write_all(&bytes);
                    let _ = writer.flush();
                }
                PtyMessage::Resize { cols, rows } => {
                    let _ = master_for_write.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
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
    ))
}

#[allow(clippy::type_complexity)]
pub fn spawn_terminal_with_fd(
    master_fd: std::os::unix::io::RawFd,
    cols: u16,
    rows: u16,
    app_tx: tokio::sync::mpsc::Sender<crate::events::AppEvent>,
) -> Result<(
    Arc<Mutex<crate::term::Parser>>,
    mpsc::Sender<PtyMessage>,
    TerminalHandle,
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

    let parser = Arc::new(Mutex::new(crate::term::Parser::new(rows, cols, 10000)));

    let parser_clone = parser.clone();
    let app_tx_clone = app_tx.clone();

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
                    let _ = app_tx_clone.try_send(crate::events::AppEvent::TerminalRedraw);
                }
                Err(_) => break,
            }
        }
    });

    let parser_for_write = parser.clone();
    let resize_fd = master_fd; // original fd, still valid (we dup'd for writer)

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
                    unsafe {
                        libc::ioctl(resize_fd, libc::TIOCSWINSZ, &ws);
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
    ))
}
