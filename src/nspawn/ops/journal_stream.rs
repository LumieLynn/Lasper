//! Narrow journal-stream capability retained until the session vertical slice.

use crate::nspawn::models::MachineName;
use crate::nspawn::sys::daemon::{pipe_reader, ElevatedDaemon};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;

pub trait JournalStreamSource: Send + Sync + 'static {
    fn spawn(
        &self,
        machine: MachineName,
        tx: tokio::sync::mpsc::Sender<String>,
        fatal: Arc<AtomicBool>,
    ) -> tokio::task::JoinHandle<()>;
}

pub(crate) struct DefaultJournalStreamSource {
    daemon: Option<Arc<ElevatedDaemon>>,
}

impl DefaultJournalStreamSource {
    pub(crate) fn new(daemon: Option<Arc<ElevatedDaemon>>) -> Self {
        Self { daemon }
    }
}

impl JournalStreamSource for DefaultJournalStreamSource {
    fn spawn(
        &self,
        machine: MachineName,
        tx: tokio::sync::mpsc::Sender<String>,
        fatal: Arc<AtomicBool>,
    ) -> tokio::task::JoinHandle<()> {
        if let Some(daemon) = self.daemon.clone() {
            return tokio::spawn(async move {
                let stdout_fd = match daemon.spawn_journalctl(machine.as_str()).await {
                    Ok(fd) => fd,
                    Err(error) => {
                        fatal.store(true, Ordering::Relaxed);
                        let _ = tx.send(format!("Log stream error: {error}")).await;
                        return;
                    }
                };
                match pipe_reader(stdout_fd) {
                    Ok(receiver) => {
                        let mut lines = tokio::io::BufReader::new(receiver).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            if tx.send(line).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        fatal.store(true, Ordering::Relaxed);
                        let _ = tx.send(format!("Log stream error: {error}")).await;
                    }
                }
            });
        }

        tokio::spawn(async move {
            let mut child = match crate::nspawn::sys::new_command("journalctl")
                .args([
                    "-M",
                    machine.as_str(),
                    "-n",
                    "1000",
                    "-f",
                    "--no-pager",
                    "--output=short",
                ])
                .spawn()
            {
                Ok(child) => child,
                Err(error) => {
                    fatal.store(true, Ordering::Relaxed);
                    let _ = tx.send(format!("Log stream error: {error}")).await;
                    if error.kind() == std::io::ErrorKind::PermissionDenied {
                        let _ = tx
                            .send(
                                "Hint: add yourself to the 'systemd-journal' group: sudo usermod -a -G systemd-journal $USER"
                                    .into(),
                            )
                            .await;
                    }
                    return;
                }
            };

            let stdout = child.stdout.take().expect("journalctl stdout piped");
            let mut stderr = child.stderr.take().expect("journalctl stderr piped");
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            loop {
                tokio::select! {
                    line = lines.next_line() => {
                        match line {
                            Ok(Some(line)) => {
                                if tx.send(line).await.is_err() {
                                    break;
                                }
                            }
                            _ => break,
                        }
                    },
                    _ = child.wait() => break,
                }
            }

            use tokio::io::AsyncReadExt;
            let mut buffer = Vec::new();
            if stderr.read_to_end(&mut buffer).await.is_ok() && !buffer.is_empty() {
                fatal.store(true, Ordering::Relaxed);
                let _ = tx
                    .send(format!("Log stream: {}", String::from_utf8_lossy(&buffer)))
                    .await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_does_not_require_a_privileged_daemon() {
        let source = DefaultJournalStreamSource::new(None);
        assert!(source.daemon.is_none());
    }
}
