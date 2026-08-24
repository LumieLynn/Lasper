use crate::adapters::elevated::{pipe_reader, ElevatedDaemon};
use crate::application::sessions::{
    journal_session_channel, JournalSessionHandle, JournalSessionRequest, SessionError,
    SessionPort, TerminalSessionHandle, TerminalSessionRequest,
};
use crate::domain::session::SessionLifecycle;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;

pub(crate) struct ElevatedSessionAdapter {
    daemon: Arc<ElevatedDaemon>,
}

impl ElevatedSessionAdapter {
    pub(crate) fn new(daemon: Arc<ElevatedDaemon>) -> Self {
        Self { daemon }
    }
}

#[async_trait]
impl SessionPort for ElevatedSessionAdapter {
    async fn open_terminal(
        &self,
        request: TerminalSessionRequest,
    ) -> Result<TerminalSessionHandle, SessionError> {
        let spawned = self
            .daemon
            .spawn_terminal(
                request.id.get(),
                request.machine.as_str(),
                request.size.cols(),
                request.size.rows(),
            )
            .await
            .map_err(|error| SessionError::new(format!("open elevated terminal: {error}")))?;
        let lifecycle = spawned.lifecycle;
        let (handle, control) = match crate::adapters::session::pty::spawn_fd_terminal(
            spawned.master_fd,
            request.id,
            spawned.attach_kind,
        ) {
            Ok(session) => session,
            Err(error) => {
                if let Err(close_error) = self.daemon.close_session(request.id.get()).await {
                    log::warn!(
                        "failed to close elevated terminal after local setup error: {close_error}"
                    );
                }
                return Err(error);
            }
        };
        let daemon = Arc::clone(&self.daemon);
        tokio::spawn(async move {
            tokio::select! {
                close = control.close => {
                    if close.is_ok() {
                        let _ = control.lifecycle.send(SessionLifecycle::Closed);
                        if let Err(error) = daemon.close_session(request.id.get()).await {
                            log::warn!("failed to close elevated terminal session: {error}");
                        }
                    }
                }
                lifecycle = lifecycle => {
                    let state = lifecycle.unwrap_or_else(|_| {
                        SessionLifecycle::Failed(
                            "elevated terminal lifecycle channel closed".to_string(),
                        )
                    });
                    if control.lifecycle.borrow().is_running() {
                        let _ = control.lifecycle.send(state);
                    }
                }
            }
        });
        Ok(handle)
    }

    async fn open_journal(
        &self,
        request: JournalSessionRequest,
    ) -> Result<JournalSessionHandle, SessionError> {
        let spawned = self
            .daemon
            .spawn_journalctl(request.id.get(), request.machine.as_str())
            .await
            .map_err(|error| SessionError::new(format!("open elevated journal: {error}")))?;
        let receiver = match pipe_reader(spawned.output_fd) {
            Ok(receiver) => receiver,
            Err(error) => {
                if let Err(close_error) = self.daemon.close_session(request.id.get()).await {
                    log::warn!(
                        "failed to close elevated journal after local setup error: {close_error}"
                    );
                }
                return Err(SessionError::new(format!("open journal output: {error}")));
            }
        };
        let (handle, endpoint) = journal_session_channel(request.id);
        let daemon = Arc::clone(&self.daemon);
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(receiver).lines();
            let mut close = Box::pin(endpoint.close);
            let mut lifecycle = Box::pin(spawned.lifecycle);
            let mut lifecycle_state = None;
            let mut output_open = true;
            loop {
                if !output_open {
                    if let Some(state) = lifecycle_state.take() {
                        if endpoint.lifecycle.borrow().is_running() {
                            let _ = endpoint.lifecycle.send(state);
                        }
                        return;
                    }
                }
                tokio::select! {
                    _ = &mut close => {
                        let _ = endpoint.lifecycle.send(SessionLifecycle::Closed);
                        if let Err(error) = daemon.close_session(request.id.get()).await {
                            log::warn!("failed to close elevated journal session: {error}");
                        }
                        return;
                    }
                    result = &mut lifecycle, if lifecycle_state.is_none() => {
                        lifecycle_state = Some(result.unwrap_or_else(|_| {
                            SessionLifecycle::Failed(
                                "elevated journal lifecycle channel closed".to_string(),
                            )
                        }));
                    }
                    line = lines.next_line(), if output_open => {
                        match line {
                            Ok(Some(line)) => {
                                tokio::select! {
                                    _ = &mut close => {
                                        let _ = endpoint.lifecycle.send(SessionLifecycle::Closed);
                                        let _ = daemon.close_session(request.id.get()).await;
                                        return;
                                    }
                                    result = endpoint.output.send(line) => {
                                        if result.is_err() {
                                            let _ = daemon.close_session(request.id.get()).await;
                                            return;
                                        }
                                    }
                                }
                            }
                            Ok(None) => {
                                output_open = false;
                            }
                            Err(error) => {
                                let _ = endpoint.lifecycle.send(SessionLifecycle::Failed(format!(
                                    "read elevated journal stream: {error}"
                                )));
                                let _ = daemon.close_session(request.id.get()).await;
                                return;
                            }
                        }
                    }
                }
            }
        });
        Ok(handle)
    }
}
