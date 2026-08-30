use crate::daemon::server::DaemonServerState;
use crate::ipc::protocol::session::{
    SpawnJournalctlParams, SpawnTerminalParams, SpawnTerminalResponse, WireSessionLifecycle,
    WireTerminalAttachmentKind,
};
use sendfd::SendWithFd;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::sync::Arc;

pub(crate) fn spawn_journal(
    stream: &mut std::os::unix::net::UnixStream,
    params: SpawnJournalctlParams,
    state: Arc<DaemonServerState>,
) {
    let SpawnJournalctlParams { session_id, name } = params;
    match crate::adapters::process::new_sync_command("journalctl")
        .args([
            "-M",
            name.as_str(),
            "-n",
            "1000",
            "-f",
            "--no-pager",
            "--output=short",
        ])
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
    {
        Ok(mut child) => {
            let child_pid = child.id();
            let (status_reader, status_writer) = match status_pipe() {
                Ok(pipe) => pipe,
                Err(error) => {
                    stop_child(&mut child, child_pid);
                    let _ = stream.send_with_fd(error.to_string().as_bytes(), &[]);
                    return;
                }
            };
            if let Err(error) = state.register(session_id, child_pid) {
                stop_child(&mut child, child_pid);
                let _ = stream.send_with_fd(error.to_string().as_bytes(), &[]);
                return;
            }
            let stdout = child.stdout.take().expect("journalctl stdout is piped");
            if let Err(error) =
                stream.send_with_fd(b"ok", &[stdout.as_raw_fd(), status_reader.as_raw_fd()])
            {
                log::error!("Daemon: send_with_fd (journalctl) failed: {error}");
                stop_child(&mut child, child_pid);
                state.finish(session_id, child_pid);
                return;
            }
            drop(stdout);
            drop(status_reader);
            tokio::task::spawn_blocking(move || {
                let lifecycle = match child.wait() {
                    Ok(status) => WireSessionLifecycle::Exited {
                        success: status.success(),
                        code: status.code(),
                    },
                    Err(error) => WireSessionLifecycle::Failed {
                        message: format!("wait for journal session: {error}"),
                    },
                };
                state.finish(session_id, child_pid);
                write_lifecycle(status_writer, lifecycle);
            });
        }
        Err(error) => {
            log::error!("Daemon: spawn journalctl failed: {error}");
            let _ = stream.send_with_fd(error.to_string().as_bytes(), &[]);
        }
    }
}

pub(crate) fn spawn_terminal(
    stream: &mut std::os::unix::net::UnixStream,
    params: SpawnTerminalParams,
    state: Arc<DaemonServerState>,
) {
    use portable_pty::{native_pty_system, PtySize};

    let SpawnTerminalParams {
        session_id,
        name,
        size,
    } = params;
    let pair = match native_pty_system().openpty(PtySize {
        rows: size.rows(),
        cols: size.cols(),
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(pair) => pair,
        Err(error) => {
            log::error!("Daemon: openpty failed: {error}");
            let _ = stream.send_with_fd(error.to_string().as_bytes(), &[]);
            return;
        }
    };
    let attachment = match crate::adapters::session::terminal_attach::select(&name) {
        Ok(attachment) => attachment,
        Err(error) => {
            log::error!("Daemon: terminal attach planning failed: {error}");
            let _ = stream.send_with_fd(error.to_string().as_bytes(), &[]);
            return;
        }
    };
    let attachment_kind = attachment.kind();
    let terminal_name = name.to_string();
    match pair.slave.spawn_command(attachment.into_pty_command()) {
        Ok(mut child) => {
            drop(pair.slave);
            let Some(child_pid) = child.process_id() else {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stream.send_with_fd(b"terminal child has no process id", &[]);
                return;
            };
            let (status_reader, status_writer) = match status_pipe() {
                Ok(pipe) => pipe,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stream.send_with_fd(error.to_string().as_bytes(), &[]);
                    return;
                }
            };
            if let Err(error) = state.register(session_id, child_pid) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stream.send_with_fd(error.to_string().as_bytes(), &[]);
                return;
            }
            let master_fd = pair.master.as_raw_fd().expect("PTY master has an fd");
            let response = serde_json::to_vec(&SpawnTerminalResponse {
                attach_kind: WireTerminalAttachmentKind::from(attachment_kind),
            })
            .expect("terminal response is serializable");
            if let Err(error) =
                stream.send_with_fd(&response, &[master_fd, status_reader.as_raw_fd()])
            {
                log::error!("Daemon: send_with_fd (terminal) failed: {error}");
                let _ = child.kill();
                let _ = child.wait();
                state.finish(session_id, child_pid);
                return;
            }
            drop(pair.master);
            drop(status_reader);
            tokio::task::spawn_blocking(move || {
                let lifecycle = match child.wait() {
                    Ok(status) => {
                        if status.success() {
                            log::info!(
                                "Terminal attachment for {} ({:?}) exited normally",
                                terminal_name,
                                attachment_kind
                            );
                        } else {
                            log::warn!(
                                "Terminal attachment for {} ({:?}) exited with code {} signal {:?}",
                                terminal_name,
                                attachment_kind,
                                status.exit_code(),
                                status.signal()
                            );
                        }
                        WireSessionLifecycle::Exited {
                            success: status.success(),
                            code: i32::try_from(status.exit_code()).ok(),
                        }
                    }
                    Err(error) => {
                        log::warn!(
                            "Failed to wait for terminal attachment to {} ({:?}): {}",
                            terminal_name,
                            attachment_kind,
                            error
                        );
                        WireSessionLifecycle::Failed {
                            message: format!("wait for terminal session: {error}"),
                        }
                    }
                };
                state.finish(session_id, child_pid);
                write_lifecycle(status_writer, lifecycle);
            });
        }
        Err(error) => {
            log::error!("Daemon: spawn terminal attachment failed: {error}");
            let _ = stream.send_with_fd(error.to_string().as_bytes(), &[]);
        }
    }
}

fn status_pipe() -> std::io::Result<(OwnedFd, std::fs::File)> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let reader = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let writer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    Ok((reader, writer))
}

fn write_lifecycle(mut writer: std::fs::File, lifecycle: WireSessionLifecycle) {
    if serde_json::to_writer(&mut writer, &lifecycle).is_ok() {
        let _ = writer.write_all(b"\n");
        let _ = writer.flush();
    }
}

fn stop_child(child: &mut std::process::Child, pid: u32) {
    let _ = crate::adapters::process::signal_process_group(pid, libc::SIGKILL);
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::protocol::session::WireSessionId;

    #[test]
    fn registry_rejects_duplicate_ids_and_removes_only_the_matching_process() {
        let state = DaemonServerState::default();
        let id = WireSessionId::new(1).unwrap();
        state.register(id, 10).unwrap();
        assert!(state.register(id, 11).is_err());
        state.finish(id, 11);
        assert_eq!(state.len(), 1);
        state.finish(id, 10);
        assert_eq!(state.len(), 0);
    }

    #[tokio::test]
    async fn closing_a_registered_session_terminates_and_reaps_its_process_group() {
        let state = Arc::new(DaemonServerState::default());
        let id = WireSessionId::new(2).unwrap();
        let mut command = crate::adapters::process::new_sync_command("sh");
        command.args(["-c", "exec sleep 30"]).process_group(0);
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        state.register(id, pid).unwrap();

        state.close_and_escalate(id).unwrap();
        let status = child.wait().unwrap();
        assert!(!status.success());
        state.finish(id, pid);
        assert_eq!(state.len(), 0);
    }
}
