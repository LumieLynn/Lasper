//! Operation failures and the cleanup evidence attached by the deployment coordinator.

use crate::adapters::process::process_exit_is_unconfirmed;
use crate::adapters::system_operation::SystemOperationError;
use crate::application::provisioning::{DeploymentError, DeploymentStateError};
use std::fmt;
use std::path::PathBuf;

pub(super) type OperationResult<T> = Result<T, OperationFailure>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum OperationFailure {
    #[error("invalid deployment input: {0}")]
    InvalidInput(String),
    #[error("required tool '{0}' was not found on PATH")]
    MissingTool(String),
    #[error("I/O error in {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
    #[error("{operation} failed: {command}. {diagnostic}")]
    CommandFailed {
        operation: String,
        command: String,
        diagnostic: String,
    },
    #[error("{operation}: {source}")]
    Host {
        operation: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error(transparent)]
    State(#[from] DeploymentStateError),
    #[error(transparent)]
    System(#[from] SystemOperationError),
    #[error("OCI transfer D-Bus operation failed: {0}")]
    ImportTransport(#[from] zbus::Error),
    #[error("{operation} worker did not complete: {source}")]
    Worker {
        operation: &'static str,
        #[source]
        source: tokio::task::JoinError,
    },
    #[error("deployment state is inconsistent: {0}")]
    InconsistentState(String),
    #[error("deployment resource conflict: {0}")]
    ResourceConflict(String),
    #[error("deployment cleanup failed before completion")]
    CleanupFailed,
    #[error("Deployment cancelled")]
    Cancelled,
    #[error("could not confirm that {operation} exited: {source}")]
    ProcessExitUnconfirmed {
        operation: String,
        #[source]
        source: std::io::Error,
    },
}

impl OperationFailure {
    pub(super) fn host(
        operation: &'static str,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        Self::Host {
            operation,
            source: source.into(),
        }
    }

    pub(super) fn command_failed(
        operation: impl Into<String>,
        command: impl Into<String>,
        output: &std::process::Output,
    ) -> Self {
        Self::CommandFailed {
            operation: operation.into(),
            command: command.into(),
            diagnostic: crate::adapters::process::command_diagnostic(output),
        }
    }

    pub(super) fn command_exit(
        operation: impl Into<String>,
        command: impl Into<String>,
        status: std::process::ExitStatus,
    ) -> Self {
        Self::CommandFailed {
            operation: operation.into(),
            command: command.into(),
            diagnostic: status.to_string(),
        }
    }

    pub(super) fn process_exit_unconfirmed(operation: &str, source: std::io::Error) -> Self {
        Self::ProcessExitUnconfirmed {
            operation: operation.into(),
            source,
        }
    }

    pub(super) fn outcome_is_unconfirmed(&self) -> bool {
        matches!(
            self,
            Self::ProcessExitUnconfirmed { .. }
                | Self::System(SystemOperationError::OutcomeUnknown(_))
        ) || process_exit_is_unconfirmed(self)
    }
}

/// The original operation failure stays separate from cleanup failures until presentation.
#[derive(Debug)]
pub(crate) struct DeploymentFailure {
    cause: Box<OperationFailure>,
    cleanup_errors: Vec<String>,
}

impl DeploymentFailure {
    pub(super) fn new(cause: OperationFailure, cleanup_errors: Vec<String>) -> Self {
        Self {
            cause: Box::new(cause),
            cleanup_errors,
        }
    }

    pub(crate) fn into_application_error(self) -> DeploymentError {
        if self.cause.outcome_is_unconfirmed() || !self.cleanup_errors.is_empty() {
            DeploymentError::reconciliation_required(self.to_string())
        } else if matches!(*self.cause, OperationFailure::Cancelled) {
            DeploymentError::cancelled(self.to_string())
        } else {
            DeploymentError::failed(self.to_string())
        }
    }
}

impl fmt::Display for DeploymentFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.cause)?;
        if !self.cleanup_errors.is_empty() {
            write!(
                f,
                "; cleanup incomplete: {}",
                self.cleanup_errors.join("; ")
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for DeploymentFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

#[cfg(test)]
mod tests;
