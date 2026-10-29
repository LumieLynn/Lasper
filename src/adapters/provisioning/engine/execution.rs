//! Host resources selected from a validated deployment specification.

use crate::adapters::provisioning::engine::failure::{OperationFailure, OperationResult};
use crate::adapters::provisioning::engine::sources::{
    CloneDeployer, OciDeployer, RawImageImport, RootfsSource,
};
use crate::adapters::storage::StorageBackend;
use crate::application::provisioning::{DeploymentCancellation, DeploymentResource};
use crate::domain::machine::{CreateUser, MachineConfig, MachineName};
use crate::domain::provisioning::DeploymentIntegration;
use crate::domain::wayland::WaylandGrantIntent;

pub(crate) struct DeploymentExecution {
    pub target: MachineName,
    pub method: DeploymentMethod,
}

/// Each path carries only the storage and configuration it can apply.
/// The transaction retains allocated storage until cleanup has completed.
pub(crate) enum DeploymentMethod {
    Rootfs {
        source: RootfsSource,
        storage: Box<dyn StorageBackend>,
        configuration: Box<SystemConfiguration>,
    },
    RawImage {
        source: RawImageImport,
        configuration: Box<SystemConfiguration>,
    },
    Clone(CloneDeployer),
    Oci(OciDeployer),
}

impl DeploymentMethod {
    pub(crate) fn storage(&self) -> Option<&dyn StorageBackend> {
        match self {
            Self::Rootfs { storage, .. } => Some(storage.as_ref()),
            Self::RawImage { .. } | Self::Clone(_) | Self::Oci(_) => None,
        }
    }

    pub(crate) fn source_stage_resources(&self, target: &MachineName) -> Vec<DeploymentResource> {
        match self {
            Self::Rootfs { .. } => vec![DeploymentResource::LocalStorage(target.clone())],
            Self::RawImage { .. } => vec![DeploymentResource::ExternalImage(target.clone())],
            Self::Clone(source) => source.source_stage_resources(target),
            Self::Oci(source) => source.source_stage_resources(target),
        }
    }
}

pub(crate) struct SystemConfiguration {
    pub machine: MachineConfig,
    pub users: Vec<CreateUser>,
    pub wayland: Vec<WaylandGrantIntent>,
    pub integration: DeploymentIntegration,
}

pub(crate) fn check_deployment_cancellation(
    cancellation: &DeploymentCancellation,
) -> OperationResult<()> {
    cancellation
        .checkpoint()
        .map_err(|_| OperationFailure::Cancelled)
}
