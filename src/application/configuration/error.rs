//! Configuration failures remain semantic across direct and elevated execution.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum ConfigurationError {
    #[error("Permission denied: {0}")]
    PermissionDenied(String),
    #[error("Invalid configuration request: {0}")]
    InvalidInput(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Failed(String),
    #[error(
        "Configuration save outcome is unknown: {0}; refresh and review the file before retrying"
    )]
    OutcomeUnknown(String),
}

impl ConfigurationError {
    pub fn failed(message: impl std::fmt::Display) -> Self {
        Self::Failed(message.to_string())
    }

    pub fn invalid_input(message: impl std::fmt::Display) -> Self {
        Self::InvalidInput(message.to_string())
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported(message.into())
    }

    pub fn outcome_unknown(message: impl std::fmt::Display) -> Self {
        Self::OutcomeUnknown(message.to_string())
    }

    pub fn is_outcome_unknown(&self) -> bool {
        matches!(self, Self::OutcomeUnknown(_))
    }
}
