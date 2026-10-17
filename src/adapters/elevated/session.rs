use super::ElevatedDaemon;
use crate::application::sessions::{
    MappedGuestIdentity, ObservedGuestIdentity, ObservedMachineInstance, ObservedNamespaceIdentity,
    SessionError, WaylandPreparationRequest, WaylandSessionContext, X11ProjectionContext,
    X11ProjectionProbeRequest,
};
use crate::domain::machine::MachineName;
use crate::domain::session::{SessionLifecycle, SessionSize, TerminalAttachmentKind};
use crate::ipc::protocol::session::{
    CloseSessionParams, PrepareWaylandParams, PrepareWaylandResponse, ProbeX11ProjectionParams,
    ProbeX11ProjectionResponse, SpawnJournalctlParams, SpawnTerminalParams, SpawnTerminalResponse,
    WireSessionId, WireSessionLifecycle, WireTerminalLaunch, WireTerminalLifecycleSource,
    WireTerminalSize,
};
use crate::ipc::protocol::FdOperation;
use std::os::fd::OwnedFd;
use std::time::Duration;

const JOURNAL_FD_HANDOFF_TIMEOUT: Duration = Duration::from_secs(15);
// Selected-user shells may try multiple configured Wayland targets. Each
// machine1 shell call has a 60-second D-Bus deadline, so the handoff needs a
// larger outer cap than one machine1 call while still bounding a stuck peer.
const TERMINAL_FD_HANDOFF_TIMEOUT: Duration = Duration::from_secs(150);

pub(crate) struct SpawnedTerminalPty {
    pub master_fd: OwnedFd,
    pub attach_kind: TerminalAttachmentKind,
    pub lifecycle: Option<tokio::sync::oneshot::Receiver<SessionLifecycle>>,
    pub machine_removed: Option<tokio::sync::oneshot::Receiver<SessionLifecycle>>,
}

pub(crate) struct SpawnedJournalStream {
    pub output_fd: OwnedFd,
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
        let (message, fds) = tokio::task::spawn_blocking(move || {
            super::receive_fd_response(&socket, "journal stream", JOURNAL_FD_HANDOFF_TIMEOUT)
        })
        .await??;
        if message.trim() != "ok" {
            return Err(daemon_fd_error(message.as_bytes()));
        }
        if fds.len() != 2 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("daemon returned {} journal fds; expected 2", fds.len()),
            ));
        }
        let mut fds = fds.into_iter();
        let output_fd = fds.next().expect("journal fd count checked");
        let status_fd = fds.next().expect("journal fd count checked");
        Ok(SpawnedJournalStream {
            output_fd,
            lifecycle: monitor_lifecycle(status_fd),
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
        let (message, fds) = tokio::task::spawn_blocking(move || {
            super::receive_fd_response(&socket, "terminal", TERMINAL_FD_HANDOFF_TIMEOUT)
        })
        .await??;
        if fds.is_empty() {
            return Err(daemon_fd_error(message.as_bytes()));
        }
        let fd_count = fds.len();
        let mut fds = fds.into_iter();
        let master_fd = fds.next().expect("terminal fd count checked");
        let status_fd = fds.next();
        match serde_json::from_str::<SpawnTerminalResponse>(&message) {
            Ok(response) => {
                let mut machine_removed = None;
                let lifecycle = match response.lifecycle {
                    WireTerminalLifecycleSource::DaemonStatus if fd_count == 2 => Some(
                        monitor_lifecycle(status_fd.expect("terminal fd count checked")),
                    ),
                    WireTerminalLifecycleSource::PtyEof if fd_count == 1 => None,
                    WireTerminalLifecycleSource::MachineRemoved if fd_count == 2 => {
                        machine_removed = Some(monitor_lifecycle(
                            status_fd.expect("terminal fd count checked"),
                        ));
                        None
                    }
                    expected => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "daemon terminal response has {fd_count} fds for {expected:?} lifecycle"
                            ),
                        ));
                    }
                };
                Ok(SpawnedTerminalPty {
                    master_fd,
                    attach_kind: response.attach_kind.into(),
                    lifecycle,
                    machine_removed,
                })
            }
            Err(error) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
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

    pub(crate) async fn probe_x11_projection(
        &self,
        request: X11ProjectionProbeRequest,
    ) -> Result<X11ProjectionContext, SessionError> {
        let host_socket = request.host_socket.clone();
        let params = ProbeX11ProjectionParams {
            probe_id: WireSessionId::new(request.probe_id.get())
                .map_err(|error| SessionError::new(error.to_string()))?,
            machine: request.target.machine().clone(),
            user: request.target.user().clone(),
            host_socket: request.host_socket,
        };
        let params = serde_json::to_value(params)
            .map_err(|error| SessionError::new(format!("encode X11 validation: {error}")))?;
        let result = self
            .rpc_call("probe_x11_projection", params)
            .await
            .map_err(|error| SessionError::new(format!("validate X11 through daemon: {error}")))?;
        let response: ProbeX11ProjectionResponse = serde_json::from_value(result)
            .map_err(|error| SessionError::new(format!("decode X11 validation: {error}")))?;
        match response {
            ProbeX11ProjectionResponse::Ready {
                guest_mount,
                guest_client_path,
                mount_writable,
                client_writable,
                guest_uid,
                guest_gid,
                host_uid,
                host_gid,
                leader_pid,
                leader_start_time,
                namespaces,
            } => {
                let instance = ObservedMachineInstance::from_process(
                    leader_pid,
                    leader_start_time,
                    namespaces.map(|namespaces| {
                        (
                            ObservedNamespaceIdentity::new(namespaces.pid.0, namespaces.pid.1),
                            ObservedNamespaceIdentity::new(namespaces.user.0, namespaces.user.1),
                        )
                    }),
                );
                Ok(X11ProjectionContext::verified(
                    host_socket,
                    guest_mount,
                    guest_client_path,
                    crate::application::sessions::X11FilesystemAccess::observed(
                        mount_writable,
                        client_writable,
                    ),
                    MappedGuestIdentity::verified(
                        ObservedGuestIdentity::new(guest_uid, guest_gid),
                        host_uid,
                        host_gid,
                        instance,
                    ),
                ))
            }
            ProbeX11ProjectionResponse::Failed { message, hint } => match hint {
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

fn monitor_lifecycle(status_fd: OwnedFd) -> tokio::sync::oneshot::Receiver<SessionLifecycle> {
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
    let receiver = super::pipe_reader_owned(status_fd)?;
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
    use std::os::fd::{AsRawFd, FromRawFd, RawFd};

    fn pipe() -> (RawFd, std::fs::File) {
        let mut fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let writer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
        (fds[0], writer)
    }

    fn assert_pipe_reader_closed(writer: &std::fs::File) {
        // Observe the pipe through an FD we still own. Checking the closed
        // reader's number races with FD reuse by parallel tests.
        let mut descriptor = libc::pollfd {
            fd: writer.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 0) }, 1);
        assert_ne!(descriptor.revents & libc::POLLERR, 0);
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
        let (message, fds) =
            super::super::receive_fd_response(&client, "terminal", Duration::from_secs(1)).unwrap();

        let decoded: SpawnTerminalResponse = serde_json::from_str(&message).unwrap();
        assert!(matches!(
            decoded.lifecycle,
            WireTerminalLifecycleSource::PtyEof
        ));
        assert_eq!(fds.len(), 1);
        drop(fds);
    }

    #[test]
    fn terminal_fd_receiver_times_out_without_a_daemon_response() {
        let (_server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let error =
            super::super::receive_fd_response(&client, "terminal", Duration::from_millis(10))
                .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("terminal FD response"));
    }

    #[test]
    fn received_descriptors_are_closed_when_the_response_is_discarded() {
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let (read_fd, writer) = pipe();
        server.send_with_fd(b"ok", &[read_fd]).unwrap();
        unsafe { libc::close(read_fd) };

        let (_, received) =
            super::super::receive_fd_response(&client, "journal stream", Duration::from_secs(1))
                .unwrap();
        drop(received);
        assert_pipe_reader_closed(&writer);
    }

    #[test]
    fn dropping_lifecycle_read_future_closes_its_owned_fd() {
        let (read_fd, writer) = pipe();
        let status_fd = unsafe { OwnedFd::from_raw_fd(read_fd) };
        let future = read_lifecycle(status_fd);
        drop(future);
        assert_pipe_reader_closed(&writer);
    }
}
