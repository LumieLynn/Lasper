use super::ElevatedDaemon;
use crate::daemon::protocol::session::{
    CloseSessionParams, SpawnJournalctlParams, SpawnTerminalParams, SpawnTerminalResponse,
    WireSessionId, WireSessionLifecycle,
};
use crate::daemon::protocol::FdOperation;
use crate::domain::machine::MachineName;
use crate::domain::session::{SessionLifecycle, TerminalAttachmentKind};
use crate::nspawn::models::TerminalSize;
use sendfd::RecvWithFd;
use std::os::fd::RawFd;

pub(crate) struct SpawnedTerminalPty {
    pub master_fd: RawFd,
    pub attach_kind: TerminalAttachmentKind,
    pub lifecycle: tokio::sync::oneshot::Receiver<SessionLifecycle>,
}

pub(crate) struct SpawnedJournalStream {
    pub output_fd: RawFd,
    pub lifecycle: tokio::sync::oneshot::Receiver<SessionLifecycle>,
}

impl ElevatedDaemon {
    pub(crate) async fn spawn_journalctl(
        &self,
        session_id: u64,
        name: &str,
    ) -> std::io::Result<SpawnedJournalStream> {
        let request = SpawnJournalctlParams {
            session_id: WireSessionId::new(session_id)?,
            name: MachineName::try_from(name)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?,
        };
        let socket = self
            .open_fd_channel(FdOperation::Journalctl(request))
            .await?;
        let (message, fds) =
            tokio::task::spawn_blocking(move || receive_two_fds(&socket)).await??;
        if message.trim() != "ok" {
            close_fds(&fds);
            return Err(daemon_fd_error(message.as_bytes()));
        }
        Ok(SpawnedJournalStream {
            output_fd: fds[0],
            lifecycle: monitor_lifecycle(fds[1]),
        })
    }

    pub(crate) async fn spawn_terminal(
        &self,
        session_id: u64,
        name: &str,
        cols: u16,
        rows: u16,
    ) -> std::io::Result<SpawnedTerminalPty> {
        let request = SpawnTerminalParams {
            session_id: WireSessionId::new(session_id)?,
            name: MachineName::try_from(name)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?,
            size: TerminalSize::new(cols, rows)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?,
        };
        let socket = self.open_fd_channel(FdOperation::Terminal(request)).await?;
        let (message, fds) =
            tokio::task::spawn_blocking(move || receive_two_fds(&socket)).await??;
        match serde_json::from_str::<SpawnTerminalResponse>(&message) {
            Ok(response) => Ok(SpawnedTerminalPty {
                master_fd: fds[0],
                attach_kind: response.attach_kind.into(),
                lifecycle: monitor_lifecycle(fds[1]),
            }),
            Err(error) => {
                close_fds(&fds);
                Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
            }
        }
    }

    pub(crate) async fn close_session(&self, session_id: u64) -> std::io::Result<()> {
        let params = serde_json::to_value(CloseSessionParams {
            session_id: WireSessionId::new(session_id)?,
        })
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        self.rpc_call("close_session", params).await?;
        Ok(())
    }
}

fn receive_two_fds(
    socket: &std::os::unix::net::UnixStream,
) -> std::io::Result<(String, [RawFd; 2])> {
    let mut buffer = [0u8; 512];
    let mut fds = [-1 as RawFd; 2];
    let (read, fd_count) = socket.recv_with_fd(&mut buffer, &mut fds)?;
    if fd_count != fds.len() {
        close_fds(&fds[..fd_count]);
        if fd_count == 0 {
            return Err(daemon_fd_error(&buffer[..read]));
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("daemon returned {fd_count} session fds; expected 2"),
        ));
    }
    let message = match String::from_utf8(buffer[..read].to_vec()) {
        Ok(message) => message,
        Err(error) => {
            close_fds(&fds);
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error));
        }
    };
    Ok((message, fds))
}

fn monitor_lifecycle(status_fd: RawFd) -> tokio::sync::oneshot::Receiver<SessionLifecycle> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let lifecycle = read_lifecycle(status_fd)
            .await
            .unwrap_or_else(|error| SessionLifecycle::Failed(error.to_string()));
        let _ = tx.send(lifecycle);
    });
    rx
}

async fn read_lifecycle(status_fd: RawFd) -> std::io::Result<SessionLifecycle> {
    use tokio::io::AsyncReadExt;

    const MAX_STATUS_BYTES: u64 = 4096;
    let receiver = super::pipe_reader(status_fd)?;
    let mut bytes = Vec::new();
    receiver
        .take(MAX_STATUS_BYTES + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > MAX_STATUS_BYTES as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "daemon session status exceeds size limit",
        ));
    }
    let lifecycle: WireSessionLifecycle = serde_json::from_slice(&bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    Ok(lifecycle.into())
}

fn close_fds(fds: &[RawFd]) {
    for fd in fds.iter().copied().filter(|fd| *fd >= 0) {
        unsafe {
            libc::close(fd);
        }
    }
}

fn daemon_fd_error(message: &[u8]) -> std::io::Error {
    std::io::Error::other(format!(
        "daemon error: {}",
        String::from_utf8_lossy(message).trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::{FromRawFd, RawFd};

    fn pipe() -> (RawFd, std::fs::File) {
        let mut fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let writer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
        (fds[0], writer)
    }

    #[tokio::test]
    async fn lifecycle_pipe_decodes_typed_exit_state() {
        let (reader, mut writer) = pipe();
        let encoded = serde_json::to_vec(&WireSessionLifecycle::Exited {
            success: false,
            code: Some(143),
        })
        .unwrap();
        let task = tokio::task::spawn_blocking(move || {
            writer.write_all(&encoded).unwrap();
        });

        assert_eq!(
            read_lifecycle(reader).await.unwrap(),
            SessionLifecycle::Exited {
                success: false,
                code: Some(143)
            }
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn lifecycle_pipe_rejects_oversized_state() {
        let (reader, mut writer) = pipe();
        let task = tokio::task::spawn_blocking(move || {
            let _ = writer.write_all(&vec![b'x'; 5000]);
        });
        assert!(read_lifecycle(reader).await.is_err());
        task.await.unwrap();
    }
}
