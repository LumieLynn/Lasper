//! Terminal launch planning and delivery of machine or process PTYs.

use crate::adapters::session::terminal_attach::{self, TerminalAttachCommand};
use crate::adapters::session::{
    MachinePty, MachineSessionOpening, MachineSessionRequest, MachineSessionTransport,
    MachineShellEnvironment, MachineShellRequest, WaylandSessionResolver,
};
use crate::application::sessions::{GuestCommand, SessionError, ShellTarget, TerminalOpenError};
use crate::daemon::sessions::process::{self, SessionChild};
use crate::daemon::sessions::{handoff, lifecycle, SessionRegistry};
use crate::domain::machine::MachineName;
use crate::domain::session::{SessionId, TerminalAttachmentKind};
use crate::ipc::protocol::session::{
    SpawnTerminalParams, SpawnTerminalResponse, WireSessionId, WireTerminalAttachmentKind,
    WireTerminalLaunch, WireTerminalLifecycleSource, WireTerminalSize, WireTerminalStartupError,
};
use anyhow::Context;
use sendfd::SendWithFd;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

pub(crate) async fn open(
    stream: UnixStream,
    params: SpawnTerminalParams,
    sessions: Arc<SessionRegistry>,
    machine: MachineSessionTransport,
    invoking_uid: u32,
) {
    let SpawnTerminalParams {
        session_id,
        name,
        size,
        launch,
    } = params;
    let probe_id = SessionId::new(session_id.get()).expect("wire session id is non-zero");
    let opening = prepare(&name, launch, machine, invoking_uid, probe_id).await;
    handoff::respond(stream, move |stream| {
        let opening = match opening {
            Ok(opening) => opening,
            Err(error) => {
                log::warn!("Daemon terminal preparation failed: {error}");
                let response = serde_json::to_vec(&WireTerminalStartupError::from(error))?;
                stream.send_with_fd(&response, &[])?;
                return Ok(());
            }
        };
        match opening {
            MachineSessionOpening::Dbus(pty) => deliver_machine_terminal(stream, pty),
            MachineSessionOpening::SystemdTools(command) => {
                spawn_process_terminal(stream, session_id, name, size, *command, sessions)
            }
        }
    })
    .await;
}

async fn prepare(
    name: &MachineName,
    launch: WireTerminalLaunch,
    machine: MachineSessionTransport,
    invoking_uid: u32,
    probe_id: SessionId,
) -> Result<MachineSessionOpening, TerminalOpenError> {
    match launch {
        WireTerminalLaunch::DefaultAttachment => {
            let command = terminal_attach::select(name).map_err(|error| {
                SessionError::new(format!("terminal attach planning failed: {error}"))
            })?;
            Ok(MachineSessionOpening::SystemdTools(Box::new(command)))
        }
        WireTerminalLaunch::LoginPrompt => {
            let attachment = terminal_attach::select(name).map_err(|error| {
                SessionError::new(format!("terminal attach planning failed: {error}"))
            })?;
            if attachment.kind() == TerminalAttachmentKind::Namespace {
                Ok(MachineSessionOpening::SystemdTools(Box::new(attachment)))
            } else {
                machine
                    .open(MachineSessionRequest::login_prompt(name.clone()))
                    .await
                    .map_err(Into::into)
            }
        }
        WireTerminalLaunch::SelectedUserShell {
            user,
            terminal,
            wayland,
            x11_display,
            command,
        } => {
            let command = command
                .map(GuestCommand::try_from)
                .transpose()
                .map_err(|error| {
                    SessionError::new(format!(
                        "selected-user shell command validation failed: {error}"
                    ))
                })?;
            let resolver = WaylandSessionResolver::for_authorized_uid(
                machine.clone(),
                crate::adapters::config::NspawnConfigStore::direct(),
                invoking_uid,
            );
            let wayland_display = resolver
                .prepare_display(
                    ShellTarget::new(name.clone(), user.clone()),
                    wayland.map(|socket| *socket),
                    probe_id,
                )
                .await
                .map_err(TerminalOpenError::WaylandPreparation)?;
            let environment =
                MachineShellEnvironment::shell(*terminal, wayland_display.as_deref(), x11_display)
                    .map_err(|error| {
                        SessionError::new(format!(
                            "selected-user shell environment validation failed: {error}"
                        ))
                    })?;
            let request = MachineShellRequest::new(name.clone(), user, environment);
            let request = match command {
                Some(command) => request.with_command(command),
                None => request,
            };
            machine
                .open(MachineSessionRequest::shell(request))
                .await
                .map_err(Into::into)
        }
    }
}

fn spawn_process_terminal(
    stream: &UnixStream,
    session_id: WireSessionId,
    name: MachineName,
    size: WireTerminalSize,
    attachment: TerminalAttachCommand,
    sessions: Arc<SessionRegistry>,
) -> anyhow::Result<()> {
    let size = size.into_session_size();
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: size.rows(),
            cols: size.cols(),
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("open terminal PTY")?;
    let master_fd = pair.master.as_raw_fd().context("PTY master has no fd")?;
    let kind = attachment.kind();
    let command = attachment
        .into_pty_command()
        .context("terminal attachment validation failed")?;
    let response = serde_json::to_vec(&SpawnTerminalResponse {
        attach_kind: WireTerminalAttachmentKind::from(kind),
        lifecycle: WireTerminalLifecycleSource::DaemonStatus,
    })?;
    let child = pair
        .slave
        .spawn_command(command)
        .context("spawn terminal attachment")?;
    drop(pair.slave);
    process::deliver(
        SessionChild::Terminal { child, name, kind },
        sessions,
        session_id,
        stream,
        &response,
        master_fd,
    )
}

fn deliver_machine_terminal(stream: &UnixStream, pty: MachinePty) -> anyhow::Result<()> {
    let MachinePty {
        master,
        machine_removed,
    } = pty;
    let status = machine_removed
        .as_ref()
        .map(|_| lifecycle::channel())
        .transpose()
        .context("create machine status pipe")?;
    let response = serde_json::to_vec(&SpawnTerminalResponse {
        attach_kind: WireTerminalAttachmentKind::Login,
        lifecycle: if status.is_some() {
            WireTerminalLifecycleSource::MachineRemoved
        } else {
            WireTerminalLifecycleSource::PtyEof
        },
    })?;
    let mut fds = vec![master.as_raw_fd()];
    if let Some((reader, _)) = &status {
        fds.push(reader.as_raw_fd());
    }
    stream
        .send_with_fd(&response, &fds)
        .context("deliver machine terminal descriptors")?;
    if let (Some(removed), Some((_reader, writer))) = (machine_removed, status) {
        lifecycle::monitor_machine(removed, writer);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::sessions::{InteractiveShellEnvironment, ValidatedGuestUserName};
    use crate::domain::session::SessionSize;
    use crate::domain::wayland::{HostWaylandSocket, SocketRevision, WaylandDisplay};
    use sendfd::RecvWithFd;

    #[tokio::test]
    async fn rejected_wayland_preparation_returns_a_typed_failure_without_terminal_fds() {
        let directory = tempfile::tempdir().unwrap();
        let uid = uzers::get_current_uid();
        let socket = HostWaylandSocket::from_verified_parts(
            WaylandDisplay::new("wayland-0").unwrap(),
            directory.path().to_path_buf(),
            directory.path().join("wayland-0"),
            uid,
            uid,
            uzers::get_current_gid(),
            0o755,
            SocketRevision {
                device: 0,
                inode: 0,
                ctime_seconds: 0,
                ctime_nanoseconds: 0,
            },
        )
        .unwrap();
        let (server, client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        open(
            server,
            SpawnTerminalParams {
                session_id: WireSessionId::new(1).unwrap(),
                name: MachineName::new("unused-machine").unwrap(),
                size: SessionSize::new(80, 24).unwrap().into(),
                launch: WireTerminalLaunch::SelectedUserShell {
                    user: ValidatedGuestUserName::new("alice").unwrap(),
                    terminal: Box::new(InteractiveShellEnvironment::default()),
                    wayland: Some(Box::new(socket)),
                    x11_display: None,
                    command: None,
                },
            },
            Arc::new(SessionRegistry::default()),
            MachineSessionTransport::SystemdTools,
            uid,
        )
        .await;

        let mut message = [0; 4096];
        let mut fds = [-1; 2];
        let (bytes, fd_count) = client.recv_with_fd(&mut message, &mut fds).unwrap();
        assert_eq!(fd_count, 0);
        let failure: WireTerminalStartupError = serde_json::from_slice(&message[..bytes]).unwrap();
        assert!(matches!(
            TerminalOpenError::from(failure),
            TerminalOpenError::WaylandPreparation(_)
        ));
    }
}
