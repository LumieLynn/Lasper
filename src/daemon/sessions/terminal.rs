//! Terminal launch planning and delivery of machine or process PTYs.

use crate::adapters::session::terminal_attach::{self, TerminalAttachCommand};
use crate::adapters::session::{
    MachinePty, MachineSessionOpening, MachineSessionRequest, MachineSessionTransport,
    MachineShellEnvironment, MachineShellRequest, WaylandSessionResolver,
};
use crate::application::sessions::{GuestCommand, ShellTarget, WaylandPreparationRequest};
use crate::daemon::sessions::process::{self, SessionChild};
use crate::daemon::sessions::{handoff, lifecycle, SessionRegistry};
use crate::domain::machine::MachineName;
use crate::domain::session::{SessionId, TerminalAttachmentKind};
use crate::ipc::protocol::session::{
    SpawnTerminalParams, SpawnTerminalResponse, WireSessionId, WireTerminalAttachmentKind,
    WireTerminalLaunch, WireTerminalLifecycleSource, WireTerminalSize,
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
    let opening = prepare(&name, launch, machine, invoking_uid).await;
    handoff::respond(stream, move |stream| match opening? {
        MachineSessionOpening::Dbus(pty) => deliver_machine_terminal(stream, pty),
        MachineSessionOpening::SystemdTools(command) => {
            spawn_process_terminal(stream, session_id, name, size, *command, sessions)
        }
    })
    .await;
}

async fn prepare(
    name: &MachineName,
    launch: WireTerminalLaunch,
    machine: MachineSessionTransport,
    invoking_uid: u32,
) -> anyhow::Result<MachineSessionOpening> {
    match launch {
        WireTerminalLaunch::DefaultAttachment => {
            let command =
                terminal_attach::select(name).context("terminal attach planning failed")?;
            Ok(MachineSessionOpening::SystemdTools(Box::new(command)))
        }
        WireTerminalLaunch::LoginPrompt => {
            let attachment =
                terminal_attach::select(name).context("terminal attach planning failed")?;
            if attachment.kind() == TerminalAttachmentKind::Namespace {
                Ok(MachineSessionOpening::SystemdTools(Box::new(attachment)))
            } else {
                machine
                    .open(MachineSessionRequest::login_prompt(name.clone()))
                    .await
                    .context("machine login prompt failed")
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
                .context("selected-user shell command validation failed")?;
            let resolver = WaylandSessionResolver::for_authorized_uid(
                machine.clone(),
                crate::adapters::config::NspawnConfigStore::direct(),
                invoking_uid,
            );
            let wayland_context = match wayland {
                Some(host_socket) => Some(
                    resolver
                        .prepare(WaylandPreparationRequest {
                            probe_id: next_probe_id(),
                            target: ShellTarget::new(name.clone(), user.clone()),
                            host_socket: *host_socket,
                        })
                        .await
                        .context("Wayland shell validation failed")?,
                ),
                None => None,
            };
            let wayland_display = resolver
                .guest_display(wayland_context.as_ref())
                .await
                .context("Wayland environment validation failed")?;
            let environment =
                MachineShellEnvironment::shell(*terminal, wayland_display.as_deref(), x11_display)
                    .context("selected-user shell environment validation failed")?;
            let request = MachineShellRequest::new(name.clone(), user, environment);
            let request = match command {
                Some(command) => request.with_command(command),
                None => request,
            };
            machine
                .open(MachineSessionRequest::shell(request))
                .await
                .context("selected-user shell failed")
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

fn next_probe_id() -> SessionId {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_PROBE_ID: AtomicU64 = AtomicU64::new(1);
    loop {
        let value = NEXT_PROBE_ID.fetch_add(1, Ordering::Relaxed);
        if let Ok(id) = SessionId::new(value) {
            return id;
        }
    }
}
