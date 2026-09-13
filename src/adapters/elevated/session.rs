use super::ElevatedDaemon;
use crate::application::sessions::{
    ObservedGuestIdentity, SessionError, WaylandPreparationRequest, WaylandSessionContext,
};
use crate::domain::machine::MachineName;
use crate::domain::session::{SessionLifecycle, SessionSize, TerminalAttachmentKind};
use crate::ipc::protocol::session::{
    CloseSessionParams, PrepareWaylandParams, PrepareWaylandResponse, SpawnJournalctlParams,
    SpawnTerminalParams, SpawnTerminalResponse, WireSessionId, WireSessionLifecycle,
    WireTerminalLaunch, WireTerminalLifecycleSource, WireTerminalSize,
};
use crate::ipc::protocol::FdOperation;
use sendfd::RecvWithFd;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd, RawFd};

pub(crate) struct SpawnedTerminalPty {
    pub master_fd: RawFd,
    pub attach_kind: TerminalAttachmentKind,
    pub lifecycle: Option<tokio::sync::oneshot::Receiver<SessionLifecycle>>,
    pub machine_removed: Option<tokio::sync::oneshot::Receiver<SessionLifecycle>>,
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
        size: SessionSize,
        launch: WireTerminalLaunch,
    ) -> std::io::Result<SpawnedTerminalPty> {
        let request = SpawnTerminalParams {
            session_id: WireSessionId::new(session_id)?,
            name: MachineName::try_from(name)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?,
            size: WireTerminalSize::from(size),
            launch,
        };
        let socket = self.open_fd_channel(FdOperation::Terminal(request)).await?;
        let (message, fds, fd_count) =
            tokio::task::spawn_blocking(move || receive_terminal_fds(&socket)).await??;
        if fd_count == 0 {
            return Err(daemon_fd_error(message.as_bytes()));
        }
        match serde_json::from_str::<SpawnTerminalResponse>(&message) {
            Ok(response) => {
                let mut machine_removed = None;
                let lifecycle = match response.lifecycle {
                    WireTerminalLifecycleSource::DaemonStatus if fd_count == 2 => {
                        Some(monitor_lifecycle(fds[1]))
                    }
                    WireTerminalLifecycleSource::PtyEof if fd_count == 1 => None,
                    WireTerminalLifecycleSource::MachineRemoved if fd_count == 2 => {
                        machine_removed = Some(monitor_lifecycle(fds[1]));
                        None
                    }
                    expected => {
                        close_fds(&fds[..fd_count]);
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "daemon terminal response has {fd_count} fds for {expected:?} lifecycle"
                            ),
                        ));
                    }
                };
                Ok(SpawnedTerminalPty {
                    master_fd: fds[0],
                    attach_kind: response.attach_kind.into(),
                    lifecycle,
                    machine_removed,
                })
            }
            Err(error) => {
                close_fds(&fds[..fd_count]);
                Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
            }
        }
    }

    pub(crate) async fn prepare_wayland(
        &self,
        request: WaylandPreparationRequest,
    ) -> Result<WaylandSessionContext, SessionError> {
        let host_socket = request.host_socket.clone();
        let params = PrepareWaylandParams {
            probe_id: WireSessionId::new(request.probe_id.get())
                .map_err(|error| SessionError::new(error.to_string()))?,
            machine: request.target.machine().clone(),
            user: request.target.user().clone(),
            host_socket: request.host_socket,
        };
        let params = serde_json::to_value(params)
            .map_err(|error| SessionError::new(format!("encode Wayland validation: {error}")))?;
        let result = self
            .rpc_call("prepare_wayland", params)
            .await
            .map_err(|error| {
                SessionError::new(format!("validate Wayland through daemon: {error}"))
            })?;
        let response: PrepareWaylandResponse = serde_json::from_value(result)
            .map_err(|error| SessionError::new(format!("decode Wayland validation: {error}")))?;
        match response {
            PrepareWaylandResponse::Ready {
                guest_socket,
                uid,
                gid,
            } => Ok(WaylandSessionContext::verified(
                host_socket,
                guest_socket,
                ObservedGuestIdentity::new(uid, gid),
            )),
            PrepareWaylandResponse::Failed { message, hint } => match hint {
                Some(hint) => Err(SessionError::with_hint(message, hint)),
                None => Err(SessionError::new(message)),
            },
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

fn receive_terminal_fds(
    socket: &std::os::unix::net::UnixStream,
) -> std::io::Result<(String, [RawFd; 2], usize)> {
    let mut buffer = [0u8; 512];
    let mut fds = [-1 as RawFd; 2];
    let (read, fd_count) = socket.recv_with_fd(&mut buffer, &mut fds)?;
    let message = match String::from_utf8(buffer[..read].to_vec()) {
        Ok(message) => message,
        Err(error) => {
            close_fds(&fds[..fd_count]);
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error));
        }
    };
    Ok((message, fds, fd_count))
}

fn monitor_lifecycle(status_fd: RawFd) -> tokio::sync::oneshot::Receiver<SessionLifecycle> {
    let status_fd = unsafe { OwnedFd::from_raw_fd(status_fd) };
    let (mut tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let lifecycle = tokio::select! {
            _ = tx.closed() => return,
            result = read_lifecycle(status_fd) => result.unwrap_or_else(|error| SessionLifecycle::Failed(error.to_string())),
        };
        let _ = tx.send(lifecycle);
    });
    rx
}

async fn read_lifecycle(status_fd: OwnedFd) -> std::io::Result<SessionLifecycle> {
    use tokio::io::AsyncReadExt;

    const MAX_STATUS_BYTES: u64 = 4096;
    let receiver = super::pipe_reader(status_fd.into_raw_fd())?;
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
    use sendfd::SendWithFd;
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
            read_lifecycle(unsafe { OwnedFd::from_raw_fd(reader) })
                .await
                .unwrap(),
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
        assert!(read_lifecycle(unsafe { OwnedFd::from_raw_fd(reader) })
            .await
            .is_err());
        task.await.unwrap();
    }

    #[test]
    fn terminal_fd_receiver_accepts_a_systemd_owned_pty_without_status_pipe() {
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let (master, _writer) = pipe();
        let response = serde_json::to_vec(&SpawnTerminalResponse {
            attach_kind: crate::ipc::protocol::session::WireTerminalAttachmentKind::Login,
            lifecycle: WireTerminalLifecycleSource::PtyEof,
        })
        .unwrap();

        server.send_with_fd(&response, &[master]).unwrap();
        unsafe { libc::close(master) };
        let (message, fds, count) = receive_terminal_fds(&client).unwrap();

        let decoded: SpawnTerminalResponse = serde_json::from_str(&message).unwrap();
        assert!(matches!(
            decoded.lifecycle,
            WireTerminalLifecycleSource::PtyEof
        ));
        assert_eq!(count, 1);
        close_fds(&fds[..count]);
    }
}
