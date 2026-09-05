use crate::application::sessions::{
    journal_session_channel, JournalSessionHandle, JournalSessionRequest, SessionError,
    SessionPort, TerminalLaunch, TerminalSessionHandle, TerminalSessionRequest,
    WaylandPreparationRequest, WaylandSessionContext,
};
use crate::domain::session::SessionLifecycle;
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

const MAX_JOURNAL_DIAGNOSTICS: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirectTerminalPolicy {
    LoginOnly,
    Automatic,
}

pub(crate) struct DirectSessionAdapter {
    terminal_policy: DirectTerminalPolicy,
    wayland: super::wayland::WaylandSessionResolver,
}

impl DirectSessionAdapter {
    pub(crate) fn new(
        terminal_policy: DirectTerminalPolicy,
        machine: super::MachineSessionTransport,
        nspawn: crate::adapters::config::NspawnConfigStore,
    ) -> Self {
        Self {
            terminal_policy,
            wayland: super::wayland::WaylandSessionResolver::new(machine, nspawn),
        }
    }
}

#[async_trait]
impl SessionPort for DirectSessionAdapter {
    async fn discover_host_wayland_sockets(
        &self,
    ) -> Vec<crate::domain::wayland::HostWaylandSocket> {
        crate::adapters::platform::capabilities::discover_wayland_sockets().await
    }

    async fn open_terminal(
        &self,
        request: TerminalSessionRequest,
    ) -> Result<TerminalSessionHandle, SessionError> {
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
                self.wayland
                    .open_selected_user_shell(id, machine, user, *environment, command, size)
                    .await
            }
            // User mode keeps the native machine1 login prompt on the chosen
            // D-Bus/CLI route; root mode retains its existing namespace-aware
            // attachment policy for the legacy direct path.
            TerminalLaunch::LoginPrompt => match self.terminal_policy {
                DirectTerminalPolicy::LoginOnly => {
                    self.wayland.open_login_prompt(id, machine, size).await
                }
                DirectTerminalPolicy::Automatic => {
                    let attachment = crate::adapters::session::terminal_attach::select(&machine)
                        .map_err(|error| {
                            SessionError::new(format!("plan terminal attachment: {error}"))
                        })?;
                    let kind = attachment.kind();
                    let command = attachment.into_pty_command().map_err(|error| {
                        SessionError::new(format!("validate terminal attachment: {error}"))
                    })?;
                    crate::adapters::session::pty::spawn_direct_terminal(command, id, kind, size)
                }
            },
        }
    }

    async fn prepare_wayland(
        &self,
        request: WaylandPreparationRequest,
    ) -> Result<WaylandSessionContext, SessionError> {
        self.wayland.prepare(request).await
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
