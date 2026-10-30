use crate::adapters::session::execution::NativeExecution;
use crate::adapters::session::projection::{WaylandSessionResolver, X11SessionResolver};
use crate::adapters::session::shell::SelectedUserShell;
use crate::application::sessions::{
    journal_session_channel, GuestTarget, JournalSessionHandle, JournalSessionRequest,
    SessionError, SessionPort, TerminalLaunch, TerminalOpenError, TerminalSessionHandle,
    TerminalSessionRequest, WaylandPreparationRequest, WaylandSessionContext, X11ProjectionContext,
    X11ProjectionProbeRequest,
};
use crate::domain::session::SessionLifecycle;
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

const MAX_JOURNAL_DIAGNOSTICS: usize = 64 * 1024;

pub(crate) struct DirectSessionAdapter {
    machine: NativeExecution,
    wayland: WaylandSessionResolver,
    x11: X11SessionResolver,
}

impl DirectSessionAdapter {
    pub(crate) fn new(
        machine: NativeExecution,
        nspawn: crate::adapters::config::NspawnConfigStore,
    ) -> Self {
        Self {
            machine: machine.clone(),
            wayland: WaylandSessionResolver::new(machine.clone(), nspawn.clone()),
            x11: X11SessionResolver::new(machine, nspawn),
        }
    }
}

#[async_trait]
impl SessionPort for DirectSessionAdapter {
    async fn automatic_wayland(
        &self,
        machine: &crate::domain::machine::MachineName,
    ) -> Result<Option<crate::domain::wayland::HostWaylandSocket>, SessionError> {
        self.wayland.automatic_wayland(machine).await
    }

    async fn discover_host_wayland_sockets(
        &self,
    ) -> Vec<crate::domain::wayland::HostWaylandSocket> {
        crate::adapters::platform::wayland::discover_wayland_sockets().await
    }

    async fn open_terminal(
        &self,
        request: TerminalSessionRequest,
    ) -> Result<TerminalSessionHandle, TerminalOpenError> {
        let TerminalSessionRequest {
            id,
            machine,
            size,
            launch,
        } = request;
        match launch {
            TerminalLaunch::SelectedUserShell {
                user,
                environment,
                command,
            } => {
                let request = SelectedUserShell {
                    target: GuestTarget::new(machine, user),
                    terminal: environment.terminal_environment().clone(),
                    wayland: environment.wayland_socket().cloned(),
                    x11_display: environment.x11_context().map(|context| context.display()),
                    command,
                }
                .prepare(&self.wayland, id)
                .await?;
                // Wayland preparation may open a guest probe. Recheck the
                // desktop endpoint afterwards, immediately before execution.
                if let Some(context) = environment.x11_context() {
                    crate::adapters::platform::x11::revalidate_for_desktop(
                        context.projection().host_socket(),
                    )
                    .await
                    .map_err(|error| {
                        SessionError::new(format!(
                            "revalidate prepared X11 display before opening shell: {error}",
                        ))
                    })?;
                }
                self.machine
                    .open_local(request, id, size)
                    .await
                    .map_err(Into::into)
            }
            TerminalLaunch::LoginPrompt => self
                .machine
                .login(&machine)
                .await?
                .into_local(id, size)
                .map_err(Into::into),
        }
    }

    async fn prepare_wayland(
        &self,
        request: WaylandPreparationRequest,
    ) -> Result<WaylandSessionContext, SessionError> {
        self.wayland.prepare(request).await
    }

    async fn probe_x11_projection(
        &self,
        request: X11ProjectionProbeRequest,
    ) -> Result<X11ProjectionContext, SessionError> {
        crate::adapters::platform::x11::revalidate_for_desktop(&request.host_socket)
            .await
            .map_err(|error| {
                SessionError::new(format!("revalidate selected X11 display: {error}"))
            })?;
        self.x11.probe(request).await
    }

    async fn open_journal(
        &self,
        request: JournalSessionRequest,
    ) -> Result<JournalSessionHandle, SessionError> {
        let mut child = crate::adapters::process::new_command("journalctl")
            .args([
                "-M",
                request.machine.as_str(),
                "-n",
                "1000",
                "-f",
                "--no-pager",
                "--output=short",
            ])
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                let message = format!("start journal stream: {error}");
                if error.kind() == std::io::ErrorKind::PermissionDenied {
                    SessionError::with_hint(
                        message,
                        "Add the current user to the systemd-journal group to read machine logs.",
                    )
                } else {
                    SessionError::new(message)
                }
            })?;
        let stdout = child.stdout.take().expect("journalctl stdout is piped");
        let stderr = child.stderr.take().expect("journalctl stderr is piped");
        let stderr_task = tokio::spawn(async move {
            let mut error_output = Vec::new();
            let result = stderr
                .take((MAX_JOURNAL_DIAGNOSTICS + 1) as u64)
                .read_to_end(&mut error_output)
                .await;
            (result, error_output)
        });
        let (handle, endpoint) = journal_session_channel(request.id);
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            let mut close = Box::pin(endpoint.close);
            let status = loop {
                tokio::select! {
                    _ = &mut close => {
                        if let Err(error) = child.kill().await {
                            if error.kind() != std::io::ErrorKind::InvalidInput {
                                log::warn!("failed to close journal session: {error}");
                            }
                        }
                        let _ = child.wait().await;
                        let _ = endpoint.lifecycle.send(SessionLifecycle::Closed);
                        return;
                    }
                    line = lines.next_line() => {
                        match line {
                            Ok(Some(line)) => {
                                tokio::select! {
                                    _ = &mut close => {
                                        let _ = child.kill().await;
                                        let _ = child.wait().await;
                                        let _ = endpoint.lifecycle.send(SessionLifecycle::Closed);
                                        return;
                                    }
                                    result = endpoint.output.send(line) => {
                                        if result.is_err() {
                                            let _ = child.kill().await;
                                            let _ = child.wait().await;
                                            return;
                                        }
                                    }
                                }
                            }
                            Ok(None) => break child.wait().await,
                            Err(error) => {
                                let _ = child.kill().await;
                                let _ = child.wait().await;
                                let _ = endpoint.lifecycle.send(SessionLifecycle::Failed(format!(
                                    "read journal stream: {error}"
                                )));
                                return;
                            }
                        }
                    }
                    result = child.wait() => break result,
                }
            };

            let mut error_output = match stderr_task.await {
                Ok((Ok(_), output)) => output,
                Ok((Err(error), _)) => {
                    let message = format!("read journal diagnostics: {error}");
                    let _ = endpoint.lifecycle.send(SessionLifecycle::Failed(message));
                    return;
                }
                Err(error) => {
                    let message = format!("journal diagnostics task failed: {error}");
                    let _ = endpoint.lifecycle.send(SessionLifecycle::Failed(message));
                    return;
                }
            };
            if !error_output.is_empty() {
                let truncated = error_output.len() > MAX_JOURNAL_DIAGNOSTICS;
                error_output.truncate(MAX_JOURNAL_DIAGNOSTICS);
                let message = format!(
                    "Log stream{}: {}",
                    if truncated {
                        " (diagnostics truncated)"
                    } else {
                        ""
                    },
                    String::from_utf8_lossy(&error_output).trim()
                );
                let _ = endpoint.output.try_send(message.clone());
                let _ = endpoint.lifecycle.send(SessionLifecycle::Failed(message));
                return;
            }
            let state = match status {
                Ok(status) => SessionLifecycle::Exited {
                    success: status.success(),
                    code: status.code(),
                },
                Err(error) => SessionLifecycle::Failed(format!("wait for journal stream: {error}")),
            };
            let _ = endpoint.lifecycle.send(state);
        });
        Ok(handle)
    }
}
