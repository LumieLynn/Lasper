use crate::daemon::server::DaemonServerState;
use crate::domain::session::SessionSize;
use crate::ipc::protocol::session::{
    SpawnJournalctlParams, SpawnTerminalParams, SpawnTerminalResponse, WireSessionId,
    WireSessionLifecycle, WireTerminalAttachmentKind, WireTerminalLaunch,
    WireTerminalLifecycleSource, WireTerminalSize,
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
            let session_process = match state.register(session_id, child_pid) {
                Ok(process) => process,
                Err(error) => {
                    stop_child(&mut child, child_pid);
                    let _ = stream.send_with_fd(error.to_string().as_bytes(), &[]);
                    return;
                }
            };
            let stdout = child.stdout.take().expect("journalctl stdout is piped");
            if let Err(error) =
                stream.send_with_fd(b"ok", &[stdout.as_raw_fd(), status_reader.as_raw_fd()])
            {
                log::error!("Daemon: send_with_fd (journalctl) failed: {error}");
                stop_child(&mut child, child_pid);
                state.finish(session_id, &session_process);
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
                state.finish(session_id, &session_process);
                write_lifecycle(status_writer, lifecycle);
            });
        }
        Err(error) => {
            log::error!("Daemon: spawn journalctl failed: {error}");
            let _ = stream.send_with_fd(error.to_string().as_bytes(), &[]);
        }
    }
}

pub(crate) async fn spawn_terminal(
    mut stream: std::os::unix::net::UnixStream,
    params: SpawnTerminalParams,
    state: Arc<DaemonServerState>,
    machine: crate::adapters::session::MachineSessionTransport,
    invoking_uid: u32,
) {
    if let Err(error) = stream.set_write_timeout(Some(std::time::Duration::from_secs(5))) {
        log::error!("Daemon: failed to set terminal response timeout: {error}");
        return;
    }
    let SpawnTerminalParams {
        session_id,
        name,
        size,
        launch,
    } = params;
    let command = match launch {
        WireTerminalLaunch::DefaultAttachment => {
            match crate::adapters::session::terminal_attach::select(&name) {
                Ok(command) => command,
                Err(error) => {
                    send_session_error(&stream, "terminal attach planning failed", &error);
                    return;
                }
            }
        }
        WireTerminalLaunch::LoginPrompt => {
            let attachment = match crate::adapters::session::terminal_attach::select(&name) {
                Ok(command) => command,
                Err(error) => {
                    send_session_error(&stream, "terminal attach planning failed", &error);
                    return;
                }
            };
            if attachment.kind() == crate::domain::session::TerminalAttachmentKind::Namespace {
                attachment
            } else {
                match machine
                    .open(
                        crate::adapters::session::MachineSessionRequest::login_prompt(name.clone()),
                    )
                    .await
                {
                    Ok(crate::adapters::session::MachineSessionOpening::Dbus(master)) => {
                        send_machine_terminal(&stream, master);
                        return;
                    }
                    Ok(crate::adapters::session::MachineSessionOpening::Cli(command)) => *command,
                    Err(error) => {
                        send_session_error(&stream, "machine login prompt failed", &error);
                        return;
                    }
                }
            }
        }
        WireTerminalLaunch::SelectedUserShell {
            user,
            terminal,
            wayland,
            command,
        } => {
            let command = match command
                .map(crate::application::sessions::GuestCommand::try_from)
                .transpose()
            {
                Ok(command) => command,
                Err(error) => {
                    send_session_error(
                        &stream,
                        "selected-user shell command validation failed",
                        &error,
                    );
                    return;
                }
            };
            let target = crate::application::sessions::ShellTarget::new(name.clone(), user.clone());
            let resolver = crate::adapters::session::WaylandSessionResolver::for_authorized_uid(
                machine.clone(),
                crate::adapters::config::NspawnConfigStore::direct(),
                invoking_uid,
            );
            let environment = match wayland {
                Some(host_socket) => {
                    let result = resolver
                        .prepare(crate::application::sessions::WaylandPreparationRequest {
                            probe_id: next_probe_id(),
                            target,
                            host_socket,
                        })
                        .await;
                    match result {
                        Ok(context) => {
                            crate::application::sessions::TypedSessionEnvironment::wayland(
                                (*terminal).clone(),
                                context,
                            )
                        }
                        Err(error) => {
                            send_session_error(&stream, "Wayland shell validation failed", &error);
                            return;
                        }
                    }
                }
                None => crate::application::sessions::TypedSessionEnvironment::terminal(*terminal),
            };
            let environment = match resolver.environment(&environment).await {
                Ok(environment) => environment,
                Err(error) => {
                    send_session_error(&stream, "Wayland environment validation failed", &error);
                    return;
                }
            };
            let request =
                crate::adapters::session::MachineShellRequest::new(name.clone(), user, environment);
            let request = match command {
                Some(command) => request.with_command(command),
                None => request,
            };
            let request = crate::adapters::session::MachineSessionRequest::shell(request);
            match machine.open(request).await {
                Ok(crate::adapters::session::MachineSessionOpening::Dbus(master)) => {
                    send_machine_terminal(&stream, master);
                    return;
                }
                Ok(crate::adapters::session::MachineSessionOpening::Cli(command)) => *command,
                Err(error) => {
                    send_session_error(&stream, "selected-user shell failed", &error);
                    return;
                }
            }
        }
    };

    let result = tokio::task::spawn_blocking(move || {
        spawn_process_terminal(&mut stream, session_id, name, size, command, state)
    })
    .await;
    if let Err(error) = result {
        log::error!("Daemon terminal worker panicked: {error}");
    }
}

fn spawn_process_terminal(
    stream: &mut std::os::unix::net::UnixStream,
    session_id: WireSessionId,
    name: crate::domain::machine::MachineName,
    size: WireTerminalSize,
    attachment: crate::adapters::session::terminal_attach::TerminalAttachCommand,
    state: Arc<DaemonServerState>,
) {
    use portable_pty::{native_pty_system, PtySize};

    let size: SessionSize = size.into_session_size();
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
    let attachment_kind = attachment.kind();
    let terminal_name = name.to_string();
    let command = match attachment.into_pty_command() {
        Ok(command) => command,
        Err(error) => {
            log::warn!("Daemon: terminal attachment validation failed: {error}");
            let _ = stream.send_with_fd(error.to_string().as_bytes(), &[]);
            return;
        }
    };
    match pair.slave.spawn_command(command) {
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
            let session_process = match state.register(session_id, child_pid) {
                Ok(process) => process,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stream.send_with_fd(error.to_string().as_bytes(), &[]);
                    return;
                }
            };
            let master_fd = pair.master.as_raw_fd().expect("PTY master has an fd");
            let response = serde_json::to_vec(&SpawnTerminalResponse {
                attach_kind: WireTerminalAttachmentKind::from(attachment_kind),
                lifecycle: WireTerminalLifecycleSource::DaemonStatus,
            })
            .expect("terminal response is serializable");
            if let Err(error) =
                stream.send_with_fd(&response, &[master_fd, status_reader.as_raw_fd()])
            {
                log::error!("Daemon: send_with_fd (terminal) failed: {error}");
                let _ = child.kill();
                let _ = child.wait();
                state.finish(session_id, &session_process);
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
                state.finish(session_id, &session_process);
                write_lifecycle(status_writer, lifecycle);
            });
        }
        Err(error) => {
            log::error!("Daemon: spawn terminal attachment failed: {error}");
            let _ = stream.send_with_fd(error.to_string().as_bytes(), &[]);
        }
    }
}

fn send_machine_terminal(
    stream: &std::os::unix::net::UnixStream,
    pty: crate::adapters::session::MachinePty,
) {
    let crate::adapters::session::MachinePty {
        master,
        machine_removed,
    } = pty;
    let status = if machine_removed.is_some() {
        match status_pipe() {
            Ok(pipe) => Some(pipe),
            Err(error) => {
                send_session_error(stream, "create machine status pipe", &error);
                return;
            }
        }
    } else {
        None
    };
    let response = serde_json::to_vec(&SpawnTerminalResponse {
        attach_kind: WireTerminalAttachmentKind::Login,
        lifecycle: if status.is_some() {
            WireTerminalLifecycleSource::MachineRemoved
        } else {
            WireTerminalLifecycleSource::PtyEof
        },
    })
    .expect("terminal response is serializable");
    let mut fds = vec![master.as_raw_fd()];
    if let Some((reader, _)) = &status {
        fds.push(reader.as_raw_fd());
    }
    if let Err(error) = stream.send_with_fd(&response, &fds) {
        log::error!("Daemon: send_with_fd (machine terminal) failed: {error}");
        return;
    }
    if let (Some(removed), Some((_reader, writer))) = (machine_removed, status) {
        tokio::spawn(async move {
            let result = tokio::select! {
                result = removed => result,
                _ = status_reader_closed(&writer) => return,
            };
            let lifecycle = match result {
                Ok(crate::domain::session::SessionLifecycle::Exited { success, code }) => {
                    WireSessionLifecycle::Exited { success, code }
                }
                _ => WireSessionLifecycle::Failed {
                    message: "machine removal monitor closed".into(),
                },
            };
            write_lifecycle(writer, lifecycle);
        });
    }
}

async fn status_reader_closed(writer: &std::fs::File) {
    // A login has no daemon-owned child to reap. Release its D-Bus signal
    // subscription when the client closes the status pipe instead.
    loop {
        let mut fd = libc::pollfd {
            fd: writer.as_raw_fd(),
            events: 0,
            revents: 0,
        };
        if unsafe { libc::poll(&mut fd, 1, 0) } < 0
            || fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

fn send_session_error(
    stream: &std::os::unix::net::UnixStream,
    context: &str,
    error: &dyn std::fmt::Display,
) {
    let message = format!("{context}: {error}");
    log::warn!("Daemon: {message}");
    let _ = stream.send_with_fd(message.as_bytes(), &[]);
}

fn next_probe_id() -> crate::domain::session::SessionId {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_PROBE_ID: AtomicU64 = AtomicU64::new(1);
    loop {
        let value = NEXT_PROBE_ID.fetch_add(1, Ordering::Relaxed);
        if let Ok(id) = crate::domain::session::SessionId::new(value) {
            return id;
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
        let mut first = crate::adapters::process::new_sync_command("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let first_process = state.register(id, first.id()).unwrap();
        let mut second = crate::adapters::process::new_sync_command("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let second_id = WireSessionId::new(2).unwrap();
        let second_process = state.register(second_id, second.id()).unwrap();
        let duplicate_error = state.register(id, u32::MAX).unwrap_err();
        assert_eq!(duplicate_error.kind(), std::io::ErrorKind::AlreadyExists);
        state.finish(id, &second_process);
        assert_eq!(state.len(), 2);
        state.finish(id, &first_process);
        assert_eq!(state.len(), 1);
        state.finish(second_id, &second_process);
        assert_eq!(state.len(), 0);
        let _ = first.kill();
        let _ = first.wait();
        let _ = second.kill();
        let _ = second.wait();
    }

    #[tokio::test]
    async fn closing_a_registered_session_terminates_and_reaps_its_process_group() {
        let state = Arc::new(DaemonServerState::default());
        let id = WireSessionId::new(2).unwrap();
        let mut command = crate::adapters::process::new_sync_command("sh");
        command.args(["-c", "exec sleep 30"]).process_group(0);
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        let process = state.register(id, pid).unwrap();

        state.close_and_escalate(id).unwrap();
        let status = child.wait().unwrap();
        assert!(!status.success());
        state.finish(id, &process);
        assert_eq!(state.len(), 0);
    }

    #[tokio::test]
    async fn machine_status_monitor_stops_when_its_client_reader_closes() {
        let (reader, writer) = status_pipe().unwrap();
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(20),
            status_reader_closed(&writer),
        )
        .await
        .is_err());
        drop(reader);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            status_reader_closed(&writer),
        )
        .await
        .unwrap();
    }
}
