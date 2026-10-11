//! Backend services retained by the TUI application.
//!
//! The application state and the service graph have different lifetimes and
//! responsibilities.  Keeping the graph here makes that boundary explicit:
//! workspace and overlays own UI state, while `AppServices` is the set of
//! application operations the UI can request.

use crate::application::provisioning::{ProvisioningPreparationService, ProvisioningService};
use crate::application::sessions::SessionService;
use crate::application::{
    HostOperationTracker, ImageLifecycleService, MachineLifecycleService,
    ResourceInspectionService, RuntimeCatalog,
};
use crate::composition::ApplicationServices;
use crate::tui::features::configuration::ConfigurationPageExecutor;
use std::sync::Arc;

pub(crate) struct AppServices {
    pub(crate) configuration: Arc<crate::application::configuration::ConfigurationService>,
    pub(crate) configuration_executor: ConfigurationPageExecutor,
    pub(crate) x11_access: Arc<crate::application::x11::X11AccessService>,
    pub(crate) session_service: Arc<SessionService>,
    pub(crate) runtime_catalog: Arc<RuntimeCatalog>,
    pub(crate) machine_lifecycle: Arc<MachineLifecycleService>,
    pub(crate) image_lifecycle: Arc<ImageLifecycleService>,
    pub(crate) provisioning: Arc<ProvisioningService>,
    pub(crate) provisioning_preparation: Arc<ProvisioningPreparationService>,
    pub(crate) resource_inspection: Arc<ResourceInspectionService>,
    pub(crate) host_operations: HostOperationTracker,
}

impl AppServices {
    pub(crate) fn new(services: ApplicationServices) -> Self {
        let ApplicationServices {
            configuration,
            session: session_service,
            x11_access,
            runtime: runtime_catalog,
            machine_lifecycle,
            image_lifecycle,
            provisioning,
            provisioning_preparation,
            resource_inspection,
            host_operations,
        } = services;
        let configuration_executor = ConfigurationPageExecutor::new(
            session_service.clone(),
            x11_access.clone(),
            host_operations.clone(),
        );

        Self {
            configuration,
            configuration_executor,
            x11_access,
            session_service,
            runtime_catalog,
            machine_lifecycle,
            image_lifecycle,
            provisioning,
            provisioning_preparation,
            resource_inspection,
            host_operations,
        }
    }
}
