//! Host effects available to direct deployment execution.

use crate::adapters::config::{NspawnConfigStore, SystemdUnitStore};
use crate::adapters::platform::nvidia::NvidiaStateStore;
use crate::adapters::process::CommandRunner;
use crate::adapters::provisioning::engine::failure::{OperationFailure, OperationResult};
use crate::adapters::provisioning::engine::guest::GuestInitializer;
use crate::adapters::system_operation::SystemOperationStore;
use crate::adapters::trusted_state::TrustedStateRoot;

/// Attach the attempted host operation while retaining the adapter's original error chain.
pub(super) trait HostResultExt<T> {
    fn host_context(self, operation: &'static str) -> OperationResult<T>;
}

impl<T, E> HostResultExt<T> for Result<T, E>
where
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    fn host_context(self, operation: &'static str) -> OperationResult<T> {
        self.map_err(|source| OperationFailure::host(operation, source))
    }
}

/// Direct host capabilities used by the provisioning implementation.
///
/// The elevated route submits the complete deployment to the daemon, whose
/// worker constructs this same direct bundle. Keeping this type route-specific
/// prevents a deployment stage from selecting a daemon transport internally.
#[derive(Clone)]
pub(crate) struct DirectProvisioningCapabilities {
    pub(super) system_operations: SystemOperationStore,
    pub(super) nspawn: NspawnConfigStore,
    pub(super) systemd_unit: SystemdUnitStore,
    pub(super) guest: GuestInitializer,
    pub(super) commands: std::sync::Arc<dyn CommandRunner>,
    pub(super) nvidia_state: NvidiaStateStore,
}

impl DirectProvisioningCapabilities {
    pub(crate) fn from_direct(
        command_runner: std::sync::Arc<dyn CommandRunner>,
        state_root: TrustedStateRoot,
    ) -> Self {
        Self {
            system_operations: SystemOperationStore::direct(command_runner.clone()),
            nspawn: NspawnConfigStore::direct(),
            systemd_unit: SystemdUnitStore::direct(),
            guest: GuestInitializer::new(command_runner.clone()),
            commands: command_runner,
            nvidia_state: NvidiaStateStore::direct(state_root),
        }
    }

    pub(crate) fn system_operations(&self) -> &SystemOperationStore {
        &self.system_operations
    }

    pub(crate) fn nspawn(&self) -> &NspawnConfigStore {
        &self.nspawn
    }

    pub(crate) fn systemd_unit(&self) -> &SystemdUnitStore {
        &self.systemd_unit
    }
}
