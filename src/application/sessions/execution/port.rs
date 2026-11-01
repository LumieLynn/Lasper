//! Execution contracts shared by local and daemon session delivery.
//! Provider-specific attachments stay behind the prepared owner's associated
//! type; application code only needs their locally owned stream.

use super::{GuestExecutionRequest, NativeReadiness, ObservedMachineInstance};
use crate::application::sessions::{SessionError, TerminalSessionHandle};
use crate::domain::machine::MachineName;
use crate::domain::session::{SessionId, SessionSize};
use async_trait::async_trait;

pub(crate) struct GuestExecutionObservation {
    pub(crate) native: NativeReadiness,
    pub(crate) systemd_init: bool,
    pub(crate) instance: Option<ObservedMachineInstance>,
}

#[async_trait]
pub(crate) trait GuestExecutionPort: Send + Sync {
    type Prepared: PreparedGuestExecution;

    /// Observe only: no guest command and no outstanding prepared worker.
    async fn observe(
        &self,
        machine: &MachineName,
        deadline: tokio::time::Instant,
    ) -> Result<GuestExecutionObservation, SessionError>;

    /// The provider owns the deadline and cancellation cleanup. Callers must
    /// not wrap this wait in a competing timeout that interrupts child reaping.
    async fn prepare(
        &self,
        request: GuestExecutionRequest,
        deadline: tokio::time::Instant,
    ) -> Result<Self::Prepared, GuestExecutionError>;
}

#[async_trait]
pub(crate) trait PreparedGuestExecution: Send {
    type Session: Send;

    /// Consumes the preparation. An error after submission must not select a
    /// different provider and execute the user's request again.
    async fn open(self) -> Result<Self::Session, GuestExecutionError>;

    async fn open_local(
        self,
        id: SessionId,
        size: SessionSize,
    ) -> Result<TerminalSessionHandle, GuestExecutionError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GuestExecutionFailure {
    NotAttempted,
    Rejected,
    OutcomeUnknown,
}

#[derive(Debug, thiserror::Error)]
#[error("{cause}")]
pub(crate) struct GuestExecutionError {
    pub(crate) failure: GuestExecutionFailure,
    cause: SessionError,
}

impl GuestExecutionError {
    pub(crate) fn new(failure: GuestExecutionFailure, cause: SessionError) -> Self {
        Self { failure, cause }
    }

    pub(crate) fn context(mut self, context: &str) -> Self {
        self.cause = self.cause.context(context);
        self
    }
}

impl From<GuestExecutionError> for SessionError {
    fn from(error: GuestExecutionError) -> Self {
        error.cause.with_execution_failure(error.failure)
    }
}

impl From<GuestExecutionError> for crate::application::sessions::TerminalOpenError {
    fn from(error: GuestExecutionError) -> Self {
        Self::Terminal(error.into())
    }
}
