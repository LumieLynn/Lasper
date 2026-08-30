//! Shared contracts used by the daemon's RPC family handlers.

use crate::adapters::lifecycle::error::map_machine_control_error;
use crate::adapters::runtime::source::RuntimeSource;
use crate::adapters::system_operation::{execute_dbus_system_operation, SystemOperation};
use crate::application::machine_lifecycle::{
    MachineControlOutcome, MachineRejection, MachineRuntimeAction, NspawnUnitAction,
};
use crate::domain::inspection::MachineProperties;
use crate::domain::machine::MachineName;
use crate::domain::runtime::{ImageEntry, ImageName, MachineEntry};

pub(crate) enum HandleOutcome {
    Spawned,
    Sync(Result<serde_json::Value, String>),
}

/// The D-Bus surface exposed to RPC handlers.
///
/// Keeping this testable seam local to the daemon avoids coupling handler
/// tests to a live system bus while the wider application capability layer is
/// still being migrated.
#[async_trait::async_trait]
pub(crate) trait DaemonDbusExecutor: Send + Sync {
    async fn list_machines(&self) -> crate::nspawn::errors::Result<Vec<MachineEntry>>;
    async fn list_images(&self) -> crate::nspawn::errors::Result<Vec<ImageEntry>>;
    async fn system_operation(
        &self,
        operation: SystemOperation,
    ) -> crate::nspawn::errors::Result<()>;
    async fn nspawn_launch(&self, image: ImageName, machine: MachineName) -> MachineControlOutcome {
        if image.as_str() != machine.as_str() {
            return MachineControlOutcome::Rejected {
                rejection: MachineRejection::InvalidTarget,
                reason: "nspawn launch currently requires matching image and machine names".into(),
            };
        }
        self.machine_control_operation(SystemOperation::Start { machine })
            .await
    }

    async fn machine_runtime_control(
        &self,
        machine: MachineName,
        action: MachineRuntimeAction,
    ) -> MachineControlOutcome {
        let operation = match action {
            MachineRuntimeAction::Terminate => SystemOperation::Terminate { machine },
            MachineRuntimeAction::Poweroff => SystemOperation::Poweroff { machine },
            MachineRuntimeAction::Reboot => SystemOperation::Reboot { machine },
            MachineRuntimeAction::Kill { signal } => SystemOperation::Kill { machine, signal },
        };
        self.machine_control_operation(operation).await
    }

    async fn nspawn_unit_control(
        &self,
        machine: MachineName,
        action: NspawnUnitAction,
    ) -> MachineControlOutcome {
        let operation = match action {
            NspawnUnitAction::Enable => SystemOperation::Enable { machine },
            NspawnUnitAction::Disable => SystemOperation::Disable { machine },
        };
        self.machine_control_operation(operation).await
    }

    async fn machine_control_operation(&self, operation: SystemOperation) -> MachineControlOutcome {
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
    async fn list_machines(&self) -> crate::nspawn::errors::Result<Vec<MachineEntry>> {
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
