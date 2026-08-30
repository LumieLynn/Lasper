//! Errors raised while preparing a machine for a lifecycle operation.

use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
enum MachinePreparationErrorKind {
    InvalidConfiguration,
    PermissionDenied,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachinePreparationError {
    kind: MachinePreparationErrorKind,
    message: String,
}

impl MachinePreparationError {
    pub(crate) fn invalid_configuration(message: impl Into<String>) -> Self {
        Self {
            kind: MachinePreparationErrorKind::InvalidConfiguration,
            message: message.into(),
        }
    }

    pub(crate) fn permission_denied(message: impl Into<String>) -> Self {
        Self {
            kind: MachinePreparationErrorKind::PermissionDenied,
            message: message.into(),
        }
    }

    pub(crate) fn failed(message: impl Into<String>) -> Self {
        Self {
            kind: MachinePreparationErrorKind::Failed,
            message: message.into(),
        }
    }

    pub fn is_invalid_configuration(&self) -> bool {
        matches!(self.kind, MachinePreparationErrorKind::InvalidConfiguration)
    }

    pub fn is_permission_denied(&self) -> bool {
        matches!(self.kind, MachinePreparationErrorKind::PermissionDenied)
    }
}

impl fmt::Display for MachinePreparationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MachinePreparationError {}
