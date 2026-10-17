//! Preparation required before submitting a machine start.

use crate::application::machine_lifecycle::MachineControlTransport;
use crate::application::{ResourceClaim, ResourceKey};
use crate::domain::machine::MachineName;
use crate::domain::nvidia::NvidiaCdiSource;
use serde::{Deserialize, Serialize};
use std::fmt;

#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait MachineStartPreparation: Send + Sync + 'static {
    async fn prepare(&self, machine: &MachineName) -> Result<(), MachinePreparationError>;
}

/// One host operation, including any NVIDIA reconciliation and manager reload.
/// The daemon owns the whole operation instead of exposing each store write as
/// a separate stage of an application-owned privileged workflow.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MachineStartPreparationRequest {
    pub machine: MachineName,
    pub nvidia_cdi_source: NvidiaCdiSource,
    pub transport: MachineControlTransport,
}

impl MachineStartPreparationRequest {
    pub(crate) fn resource_claims(&self) -> Vec<ResourceClaim> {
        vec![
            ResourceClaim::exclusive(ResourceKey::for_machine(&self.machine)),
            ResourceClaim::exclusive(ResourceKey::SystemdManager),
        ]
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MachinePreparationErrorKind {
    InvalidConfiguration,
    PermissionDenied,
    Busy,
    Failed,
    OutcomeUnknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

    pub(crate) fn busy(message: impl Into<String>) -> Self {
        Self {
            kind: MachinePreparationErrorKind::Busy,
            message: message.into(),
        }
    }

    pub(crate) fn outcome_unknown(message: impl Into<String>) -> Self {
        Self {
            kind: MachinePreparationErrorKind::OutcomeUnknown,
            message: message.into(),
        }
    }

    pub fn is_invalid_configuration(&self) -> bool {
        matches!(self.kind, MachinePreparationErrorKind::InvalidConfiguration)
    }

    pub fn is_permission_denied(&self) -> bool {
        matches!(self.kind, MachinePreparationErrorKind::PermissionDenied)
    }

    pub fn is_busy(&self) -> bool {
        matches!(self.kind, MachinePreparationErrorKind::Busy)
    }

    pub fn is_outcome_unknown(&self) -> bool {
        matches!(self.kind, MachinePreparationErrorKind::OutcomeUnknown)
    }
}

impl fmt::Display for MachinePreparationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MachinePreparationError {}
