//! Shared contracts used by the daemon's RPC family handlers.

use crate::adapters::lifecycle::error::map_machine_control_error;
use crate::adapters::runtime::source::RuntimeSource;
use crate::adapters::system_operation::{execute_dbus_system_operation, SystemOperation};
use crate::application::machine_lifecycle::{
    MachineControlOutcome, MachineRejection, MachineRuntimeAction, NspawnUnitAction,
};
use crate::application::runtime::RuntimeResult;
use crate::domain::inspection::MachineProperties;
use crate::domain::machine::MachineName;
use crate::domain::runtime::{ImageEntry, ImageName, MachineEntry};

pub(crate) enum HandleOutcome {
    Spawned,
    Sync(Result<serde_json::Value, String>),
}

/// Read-only runtime queries exposed to daemon handlers.
///
/// Keeping this testable seam local to the daemon avoids coupling query
/// handlers to a live system bus while the wider application capability layer
/// is still being migrated.
#[async_trait::async_trait]
pub(crate) trait DaemonRuntimeQueries: Send + Sync {
    async fn list_machines(&self) -> RuntimeResult<Vec<MachineEntry>>;
    async fn list_images(&self) -> RuntimeResult<Vec<ImageEntry>>;
    async fn get_properties(&self, name: &str) -> RuntimeResult<MachineProperties>;
    async fn is_available(&self) -> bool;
}

/// Host mutation surface exposed to command handlers.
///
/// Query and job handlers should not depend on this trait. Keeping the
/// mutation defaults here also makes the D-Bus executor seam explicit without
/// turning it into a catch-all daemon capability.
#[async_trait::async_trait]
pub(crate) trait DaemonSystemExecutor: Send + Sync {
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
}

#[async_trait::async_trait]
impl DaemonRuntimeQueries for crate::adapters::runtime::dbus::DbusBackend {
    async fn list_machines(&self) -> RuntimeResult<Vec<MachineEntry>> {
        RuntimeSource::list_machines(self)
            .await
            .map_err(crate::adapters::runtime::map_runtime_error)
    }

    async fn list_images(&self) -> RuntimeResult<Vec<ImageEntry>> {
        RuntimeSource::list_images(self)
            .await
            .map_err(crate::adapters::runtime::map_runtime_error)
    }

    async fn get_properties(&self, name: &str) -> RuntimeResult<MachineProperties> {
        RuntimeSource::get_properties(self, name)
            .await
            .map_err(crate::adapters::runtime::map_runtime_error)
    }

    async fn is_available(&self) -> bool {
        RuntimeSource::is_available(self).await
    }
}

#[async_trait::async_trait]
impl DaemonSystemExecutor for crate::adapters::runtime::dbus::DbusBackend {
    async fn system_operation(
        &self,
        operation: SystemOperation,
    ) -> crate::nspawn::errors::Result<()> {
        execute_dbus_system_operation(self, operation).await
    }
}
