use anyhow::Result;
use parking_lot::Mutex;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::sync::Arc;
use tokio::sync::mpsc;

pub enum PtyMessage {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

pub struct TerminalHandle {
    pub reader: tokio::task::JoinHandle<()>,
    pub writer: tokio::task::JoinHandle<()>,
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl TerminalHandle {
    pub fn abort(&mut self) {
        self.reader.abort();
        self.writer.abort();
        let _ = self.child.kill();
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
            child,
        },
    ))
}
