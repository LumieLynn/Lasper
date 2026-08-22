//! Production assembly for application services and host adapters.

mod execution;
mod permission;

pub(crate) use execution::ExecutionContext;
pub(crate) use permission::{DefaultPermissionManager, PermissionLevel, PermissionManager};

use crate::application::provisioning::ProvisioningService;
use crate::application::sessions::SessionService;
use crate::application::{
    ImageLifecycleService, MachineLifecycleService, OperationRegistry, RuntimeCatalog,
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
}

pub(crate) fn compose_application_services(
    level: PermissionLevel,
    cli_mode: bool,
    execution: &Arc<ExecutionContext>,
) -> ApplicationServices {
    let session = crate::adapters::session::compose_session_service(level, execution);
    let runtime = crate::adapters::runtime::compose_runtime_catalog(level, cli_mode, execution);
    let operations = OperationRegistry::new();
    let image_lifecycle = Arc::new(crate::adapters::lifecycle::image::compose_image_lifecycle(
        Arc::clone(&runtime),
        Arc::clone(&operations),
        level,
        cli_mode,
        execution,
    ));
    let machine_lifecycle = crate::adapters::lifecycle::machine::compose_machine_lifecycle(
        Arc::clone(&runtime),
        operations,
        level,
        cli_mode,
        execution,
    );
    let provisioning = crate::adapters::provisioning::compose_provisioning_service(execution);
    let provisioning_preparation =
        crate::adapters::provisioning::compose_provisioning_preparation_service();

    ApplicationServices {
        session,
        runtime,
        machine_lifecycle,
        image_lifecycle,
        provisioning,
        provisioning_preparation,
    }
}
