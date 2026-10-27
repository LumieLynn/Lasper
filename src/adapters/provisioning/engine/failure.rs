//! Operation failures and the cleanup evidence attached by the deployment coordinator.

use crate::adapters::error::NspawnError;
use crate::adapters::process::process_exit_is_unconfirmed;
use crate::application::provisioning::DeploymentError;
use std::fmt;

pub(super) type OperationResult<T> = Result<T, OperationFailure>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum OperationFailure {
    #[error(transparent)]
    Adapter(#[from] NspawnError),
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
    pub(super) fn process_exit_unconfirmed(operation: &str, source: std::io::Error) -> Self {
        Self::ProcessExitUnconfirmed {
            operation: operation.into(),
            source,
        }
    }

    pub(super) fn outcome_is_unconfirmed(&self) -> bool {
        match self {
            Self::ProcessExitUnconfirmed { .. } => true,
            Self::Adapter(NspawnError::SystemOperationOutcomeUnknown(_)) => true,
            Self::Adapter(error) => process_exit_is_unconfirmed(error),
            Self::Cancelled => false,
        }
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
