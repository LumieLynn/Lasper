//! Shared contracts used by the daemon's RPC family handlers.

use crate::adapters::lifecycle::error::map_machine_control_error;
use crate::adapters::runtime::source::RuntimeSource;
use crate::adapters::system_operation::{execute_dbus_system_operation, SystemOperation};
use crate::application::machine_lifecycle::{MachineAction, MachineControlOutcome};
use crate::nspawn::models::{ContainerEntry, ImageEntry, MachineName, MachineProperties};

pub(super) enum HandleOutcome {
    Spawned,
    Sync(Result<serde_json::Value, String>),
}

/// The D-Bus surface exposed to RPC handlers.
///
/// Keeping this testable seam local to the daemon avoids coupling handler
/// tests to a live system bus while the wider application capability layer is
/// still being migrated.
#[async_trait::async_trait]
pub(super) trait DaemonDbusExecutor: Send + Sync {
    async fn list_machines(&self) -> crate::nspawn::errors::Result<Vec<ContainerEntry>>;
    async fn list_images(&self) -> crate::nspawn::errors::Result<Vec<ImageEntry>>;
    async fn system_operation(
        &self,
        operation: SystemOperation,
    ) -> crate::nspawn::errors::Result<()>;
    async fn machine_control(
        &self,
        machine: MachineName,
        action: MachineAction,
    ) -> MachineControlOutcome {
        let operation = match action {
            MachineAction::Start => SystemOperation::Start { machine },
            MachineAction::Terminate => SystemOperation::Terminate { machine },
            MachineAction::Poweroff => SystemOperation::Poweroff { machine },
            MachineAction::Reboot => SystemOperation::Reboot { machine },
            MachineAction::Enable => SystemOperation::Enable { machine },
            MachineAction::Disable => SystemOperation::Disable { machine },
            MachineAction::Kill { signal } => SystemOperation::Kill { machine, signal },
        };
        match self.system_operation(operation).await {
            Ok(()) => MachineControlOutcome::Succeeded,
            Err(error) => map_machine_control_error(error),
        }
    }
    async fn get_properties(&self, name: &str) -> crate::nspawn::errors::Result<MachineProperties>;
    async fn is_available(&self) -> bool;
}

#[async_trait::async_trait]
impl DaemonDbusExecutor for crate::adapters::runtime::dbus::DbusBackend {
    async fn list_machines(&self) -> crate::nspawn::errors::Result<Vec<ContainerEntry>> {
        RuntimeSource::list_machines(self).await
    }

    async fn list_images(&self) -> crate::nspawn::errors::Result<Vec<ImageEntry>> {
        RuntimeSource::list_images(self).await
    }

    async fn system_operation(
        &self,
        operation: SystemOperation,
    ) -> crate::nspawn::errors::Result<()> {
        execute_dbus_system_operation(self, operation).await
    }

    async fn get_properties(&self, name: &str) -> crate::nspawn::errors::Result<MachineProperties> {
        RuntimeSource::get_properties(self, name).await
    }

    async fn is_available(&self) -> bool {
        RuntimeSource::is_available(self).await
    }
}
