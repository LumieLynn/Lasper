//! Application-level failures for runtime discovery and inspection.

use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeError {
    InvalidInput(String),
    Unavailable(String),
    PermissionDenied(String),
    Failed(String),
}

impl RuntimeError {
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }

    pub fn permission_denied(message: impl Into<String>) -> Self {
        Self::PermissionDenied(message.into())
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }

    pub fn is_permission_denied(&self) -> bool {
        matches!(self, Self::PermissionDenied(_))
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message)
            | Self::Unavailable(message)
            | Self::PermissionDenied(message)
            | Self::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RuntimeError {}

pub type RuntimeResult<T> = std::result::Result<T, RuntimeError>;
