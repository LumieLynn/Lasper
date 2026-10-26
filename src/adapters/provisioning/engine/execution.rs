//! Host resources selected from a validated deployment specification.

use crate::adapters::provisioning::engine::Deployer;
use crate::adapters::storage::StorageBackend;
use crate::domain::machine::{CreateUser, MachineConfig, MachineName};
use crate::domain::provisioning::DeploymentIntegration;
use crate::domain::wayland::WaylandGrantIntent;

pub(crate) struct DeploymentExecution {
    pub target: MachineName,
    pub source: Box<dyn Deployer>,
    pub storage: StoragePreparation,
    pub configuration: Option<SystemConfiguration>,
}

pub(crate) enum StoragePreparation {
    Allocate(Box<dyn StorageBackend>),
    ProviderImage,
}

impl StoragePreparation {
    pub(crate) fn backend(&self) -> Option<&dyn StorageBackend> {
        match self {
            Self::Allocate(backend) => Some(backend.as_ref()),
            Self::ProviderImage => None,
        }
    }
}

pub(crate) struct SystemConfiguration {
    pub machine: MachineConfig,
    pub users: Vec<CreateUser>,
    pub wayland: Vec<WaylandGrantIntent>,
    pub integration: DeploymentIntegration,
}
