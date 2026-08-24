use crate::domain::machine::MachineName;
use crate::domain::session::{SessionId, SessionLifecycle, SessionSize, TerminalAttachmentKind};
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, watch};

pub(crate) const TERMINAL_COMMAND_CAPACITY: usize = 1024;
pub(crate) const TERMINAL_OUTPUT_CAPACITY: usize = 256;
pub(crate) const JOURNAL_OUTPUT_CAPACITY: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalSessionRequest {
    pub id: SessionId,
    pub machine: MachineName,
    pub size: SessionSize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalSessionRequest {
    pub id: SessionId,
    pub machine: MachineName,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct SessionError {
    message: String,
    hint: Option<String>,
}

impl SessionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            hint: None,
        }
    }

    pub fn with_hint(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            hint: Some(hint.into()),
        }
    }

    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }
}

impl From<std::io::Error> for SessionError {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}

#[async_trait]
pub trait SessionPort: Send + Sync + 'static {
    async fn open_terminal(
        &self,
        request: TerminalSessionRequest,
    ) -> Result<TerminalSessionHandle, SessionError>;

    async fn open_journal(
        &self,
        request: JournalSessionRequest,
    ) -> Result<JournalSessionHandle, SessionError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionSendStatus {
    Queued,
    Full,
    Closed,
}

#[derive(Debug)]
pub(crate) enum TerminalCommand {
    Input(Vec<u8>),
    Reply(Vec<u8>),
    Resize(SessionSize),
}

#[derive(Clone)]
pub struct TerminalSessionInput {
    tx: mpsc::Sender<TerminalCommand>,
}

impl TerminalSessionInput {
    pub fn try_input(&self, bytes: Vec<u8>) -> SessionSendStatus {
        send_terminal_command(&self.tx, TerminalCommand::Input(bytes))
    }

    pub async fn send_reply(&self, bytes: Vec<u8>) -> SessionSendStatus {
        match self.tx.send(TerminalCommand::Reply(bytes)).await {
            Ok(()) => SessionSendStatus::Queued,
            Err(_) => SessionSendStatus::Closed,
        }
    }

    pub fn try_resize(&self, size: SessionSize) -> SessionSendStatus {
        send_terminal_command(&self.tx, TerminalCommand::Resize(size))
    }
}

fn send_terminal_command(
    tx: &mpsc::Sender<TerminalCommand>,
    command: TerminalCommand,
) -> SessionSendStatus {
    match tx.try_send(command) {
        Ok(()) => SessionSendStatus::Queued,
        Err(mpsc::error::TrySendError::Full(_)) => SessionSendStatus::Full,
        Err(mpsc::error::TrySendError::Closed(_)) => SessionSendStatus::Closed,
    }
}

pub struct TerminalSessionHandle {
    id: SessionId,
    attachment: TerminalAttachmentKind,
    input: TerminalSessionInput,
    output: Option<mpsc::Receiver<Vec<u8>>>,
    lifecycle: watch::Receiver<SessionLifecycle>,
    resize_failed: Arc<AtomicBool>,
    close: Option<oneshot::Sender<()>>,
}

impl TerminalSessionHandle {
    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn attachment(&self) -> TerminalAttachmentKind {
        self.attachment
    }

    pub fn input(&self) -> TerminalSessionInput {
        self.input.clone()
    }

    pub fn take_output(&mut self) -> Option<mpsc::Receiver<Vec<u8>>> {
        self.output.take()
    }

    pub fn lifecycle(&self) -> SessionLifecycle {
        self.lifecycle.borrow().clone()
    }

    pub fn take_resize_failure(&self) -> bool {
        self.resize_failed.swap(false, Ordering::AcqRel)
    }

    pub fn close(&mut self) {
        if let Some(close) = self.close.take() {
            let _ = close.send(());
        }
    }
}

impl Drop for TerminalSessionHandle {
    fn drop(&mut self) {
        self.close();
    }
}

pub(crate) struct TerminalSessionEndpoint {
    pub commands: mpsc::Receiver<TerminalCommand>,
    pub output: mpsc::Sender<Vec<u8>>,
    pub lifecycle: watch::Sender<SessionLifecycle>,
    pub resize_failed: Arc<AtomicBool>,
    pub close: oneshot::Receiver<()>,
}

pub(crate) fn terminal_session_channel(
    id: SessionId,
    attachment: TerminalAttachmentKind,
) -> (TerminalSessionHandle, TerminalSessionEndpoint) {
    let (command_tx, command_rx) = mpsc::channel(TERMINAL_COMMAND_CAPACITY);
    let (output_tx, output_rx) = mpsc::channel(TERMINAL_OUTPUT_CAPACITY);
    let (lifecycle_tx, lifecycle_rx) = watch::channel(SessionLifecycle::Running);
    let (close_tx, close_rx) = oneshot::channel();
    let resize_failed = Arc::new(AtomicBool::new(false));
    (
        TerminalSessionHandle {
            id,
            attachment,
            input: TerminalSessionInput { tx: command_tx },
            output: Some(output_rx),
            lifecycle: lifecycle_rx,
            resize_failed: Arc::clone(&resize_failed),
            close: Some(close_tx),
        },
        TerminalSessionEndpoint {
            commands: command_rx,
            output: output_tx,
            lifecycle: lifecycle_tx,
            resize_failed,
            close: close_rx,
        },
    )
}

pub struct JournalSessionHandle {
    id: SessionId,
    output: mpsc::Receiver<String>,
    lifecycle: watch::Receiver<SessionLifecycle>,
    close: Option<oneshot::Sender<()>>,
}

impl JournalSessionHandle {
    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn try_recv(&mut self) -> Result<String, mpsc::error::TryRecvError> {
        self.output.try_recv()
    }

    pub fn lifecycle(&self) -> SessionLifecycle {
        self.lifecycle.borrow().clone()
    }

    pub fn close(&mut self) {
        if let Some(close) = self.close.take() {
            let _ = close.send(());
        }
    }
}

impl Drop for JournalSessionHandle {
    fn drop(&mut self) {
        self.close();
    }
}

pub(crate) struct JournalSessionEndpoint {
    pub output: mpsc::Sender<String>,
    pub lifecycle: watch::Sender<SessionLifecycle>,
    pub close: oneshot::Receiver<()>,
}

pub(crate) fn journal_session_channel(
    id: SessionId,
) -> (JournalSessionHandle, JournalSessionEndpoint) {
    let (output_tx, output_rx) = mpsc::channel(JOURNAL_OUTPUT_CAPACITY);
    let (lifecycle_tx, lifecycle_rx) = watch::channel(SessionLifecycle::Running);
    let (close_tx, close_rx) = oneshot::channel();
    (
        JournalSessionHandle {
            id,
            output: output_rx,
            lifecycle: lifecycle_rx,
            close: Some(close_tx),
        },
        JournalSessionEndpoint {
            output: output_tx,
            lifecycle: lifecycle_tx,
            close: close_rx,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_input_and_output_channels_are_bounded() {
        let id = SessionId::new(1).unwrap();
        let (mut handle, mut endpoint) =
            terminal_session_channel(id, TerminalAttachmentKind::Login);
        for _ in 0..TERMINAL_COMMAND_CAPACITY {
            assert_eq!(
                handle.input().try_input(vec![b'x']),
                SessionSendStatus::Queued
            );
        }
        assert_eq!(
            handle.input().try_input(vec![b'x']),
            SessionSendStatus::Full
        );

        for _ in 0..TERMINAL_OUTPUT_CAPACITY {
            endpoint.output.try_send(vec![b'x']).unwrap();
        }
        assert!(matches!(
            endpoint.output.try_send(vec![b'x']),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        assert!(handle.take_output().is_some());
        assert!(matches!(
            endpoint.commands.try_recv(),
            Ok(TerminalCommand::Input(_))
        ));
    }

    #[test]
    fn dropping_a_handle_requests_session_close() {
        let id = SessionId::new(1).unwrap();
        let (handle, mut endpoint) = journal_session_channel(id);
        drop(handle);
        assert!(endpoint.close.try_recv().is_ok());
    }
}
