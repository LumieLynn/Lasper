//! Production assembly for application services and host adapters.

mod execution;
mod permission;

pub(crate) use execution::ExecutionContext;
pub(crate) use permission::{DefaultPermissionManager, PermissionLevel, PermissionManager};

use crate::application::operations::ExecutionRoute;
use crate::application::provisioning::ProvisioningService;
use crate::application::sessions::SessionService;
use crate::application::{
    HostOperationTracker, ImageLifecycleService, MachineLifecycleService, OperationRegistry,
    ResourceInspectionService, RuntimeCatalog,
};
use std::sync::Arc;

pub(crate) struct ApplicationServices {
    pub session: Arc<SessionService>,
    pub runtime: Arc<RuntimeCatalog>,
    pub machine_lifecycle: Arc<MachineLifecycleService>,
    pub image_lifecycle: Arc<ImageLifecycleService>,
    pub provisioning: Arc<ProvisioningService>,
    pub provisioning_preparation:
        Arc<crate::application::provisioning::ProvisioningPreparationService>,
    pub resource_inspection: Arc<ResourceInspectionService>,
    pub host_operations: HostOperationTracker,
}

pub(crate) fn compose_application_services(
    level: PermissionLevel,
    cli_mode: bool,
    execution: &Arc<ExecutionContext>,
) -> ApplicationServices {
    let daemon = execution.daemon_ref().cloned();
    let session_route = match level {
        PermissionLevel::User => crate::adapters::session::SessionRoute::Direct(
            crate::adapters::session::DirectTerminalPolicy::LoginOnly,
        ),
        PermissionLevel::Root => crate::adapters::session::SessionRoute::Direct(
            crate::adapters::session::DirectTerminalPolicy::Automatic,
        ),
        PermissionLevel::Elevated => crate::adapters::session::SessionRoute::Elevated(
            daemon
                .as_ref()
                .cloned()
                .expect("validated elevated composition has a daemon"),
        ),
    };
    let session = crate::adapters::session::compose_session_service(session_route);
    let fallback_inspector = cli_mode.then(|| execution.machine_inspection.clone());
    let primary_runtime = if cli_mode {
        crate::adapters::runtime::PrimaryRuntimeRoute::Disabled
    } else {
        match level {
            PermissionLevel::User | PermissionLevel::Root => {
                crate::adapters::runtime::PrimaryRuntimeRoute::DirectDbus
            }
            PermissionLevel::Elevated => {
                crate::adapters::runtime::PrimaryRuntimeRoute::ElevatedDbus(
                    daemon
                        .as_ref()
                        .cloned()
                        .expect("validated elevated composition has a daemon"),
                )
            }
        }
    };
    let runtime = crate::adapters::runtime::compose_runtime_catalog(
        execution.local_cmd.clone(),
        fallback_inspector,
        primary_runtime,
    );
    let operations = OperationRegistry::new();
    let control_route = select_control_route(level, cli_mode);
    let image_route = match control_route {
        ExecutionRoute::DirectDbus => {
            crate::adapters::lifecycle::image::ImageLifecycleRoute::DirectDbus
        }
        ExecutionRoute::LocalCli => {
            crate::adapters::lifecycle::image::ImageLifecycleRoute::LocalCli
        }
        ExecutionRoute::ElevatedDbus => {
            crate::adapters::lifecycle::image::ImageLifecycleRoute::Elevated {
                daemon: daemon
                    .as_ref()
                    .cloned()
                    .expect("validated elevated composition has a daemon"),
                transport: crate::application::image_lifecycle::ImageRemoveTransport::Dbus,
            }
        }
        ExecutionRoute::ElevatedCli => {
            crate::adapters::lifecycle::image::ImageLifecycleRoute::Elevated {
                daemon: daemon
                    .as_ref()
                    .cloned()
                    .expect("validated elevated composition has a daemon"),
                transport: crate::application::image_lifecycle::ImageRemoveTransport::Cli,
            }
        }
    };
    let image_lifecycle = Arc::new(crate::adapters::lifecycle::image::compose_image_lifecycle(
        Arc::clone(&runtime),
        Arc::clone(&operations),
        image_route,
        crate::adapters::lifecycle::image::ImageLifecycleAdapters {
            local_cmd: execution.local_cmd.clone(),
            system_operations: execution.system_operations.clone(),
            nspawn: execution.nspawn.clone(),
            systemd_unit: execution.systemd_unit.clone(),
            nvidia_state: execution.nvidia_state.clone(),
        },
    ));
    let machine_route = match control_route {
        ExecutionRoute::DirectDbus => {
            crate::adapters::lifecycle::machine::MachineLifecycleRoute::DirectDbus
        }
        ExecutionRoute::LocalCli => {
            crate::adapters::lifecycle::machine::MachineLifecycleRoute::LocalCli
        }
        ExecutionRoute::ElevatedDbus => {
            crate::adapters::lifecycle::machine::MachineLifecycleRoute::Elevated {
                daemon: daemon
                    .as_ref()
                    .cloned()
                    .expect("validated elevated composition has a daemon"),
                transport: crate::application::machine_lifecycle::MachineControlTransport::Dbus,
            }
        }
        ExecutionRoute::ElevatedCli => {
            crate::adapters::lifecycle::machine::MachineLifecycleRoute::Elevated {
                daemon: daemon
                    .as_ref()
                    .cloned()
                    .expect("validated elevated composition has a daemon"),
                transport: crate::application::machine_lifecycle::MachineControlTransport::Cli,
            }
        }
    };
    let machine_lifecycle = crate::adapters::lifecycle::machine::compose_machine_lifecycle(
        Arc::clone(&runtime),
        Arc::clone(&operations),
        machine_route,
        crate::adapters::lifecycle::machine::MachineLifecycleAdapters {
            local_cmd: execution.local_cmd.clone(),
            system_operations: execution.system_operations.clone(),
            nspawn: execution.nspawn.clone(),
            systemd_unit: execution.systemd_unit.clone(),
            nvidia_state: execution.nvidia_state.clone(),
            rootfs: execution.rootfs.clone(),
        },
    );
    let provisioning = crate::adapters::provisioning::compose_provisioning_service(
        execution,
        Arc::clone(&operations),
        Arc::clone(&runtime),
    );
    let provisioning_preparation =
        crate::adapters::provisioning::compose_provisioning_preparation_service();
    let resource_inspection = Arc::new(ResourceInspectionService::new(Arc::new(
        crate::adapters::inspection::StoreResourceInspection::new(
            execution.local_cmd.clone(),
            execution.nspawn.clone(),
            execution.systemd_unit.clone(),
        ),
    )));

    ApplicationServices {
        session,
        runtime,
        machine_lifecycle,
        image_lifecycle,
        provisioning,
        provisioning_preparation,
        resource_inspection,
        host_operations: execution.host_operations.clone(),
    }
}

fn select_control_route(level: PermissionLevel, cli_mode: bool) -> ExecutionRoute {
    match (level, cli_mode) {
        (PermissionLevel::User | PermissionLevel::Root, false) => ExecutionRoute::DirectDbus,
        (PermissionLevel::User | PermissionLevel::Root, true) => ExecutionRoute::LocalCli,
        (PermissionLevel::Elevated, false) => ExecutionRoute::ElevatedDbus,
        (PermissionLevel::Elevated, true) => ExecutionRoute::ElevatedCli,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_route_covers_authority_and_cli_modes() {
        assert_eq!(
            select_control_route(PermissionLevel::User, false),
            ExecutionRoute::DirectDbus
        );
        assert_eq!(
            select_control_route(PermissionLevel::Root, false),
            ExecutionRoute::DirectDbus
        );
        assert_eq!(
            select_control_route(PermissionLevel::User, true),
            ExecutionRoute::LocalCli
        );
        assert_eq!(
            select_control_route(PermissionLevel::Root, true),
            ExecutionRoute::LocalCli
        );
        assert_eq!(
            select_control_route(PermissionLevel::Elevated, false),
            ExecutionRoute::ElevatedDbus
        );
        assert_eq!(
            select_control_route(PermissionLevel::Elevated, true),
            ExecutionRoute::ElevatedCli
        );
    }
}
