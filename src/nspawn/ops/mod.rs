pub mod activity;
pub mod handlers;
pub mod image_lifecycle;
pub(crate) mod image_lifecycle_adapter;
pub mod inspect;
pub mod journal_stream;
pub mod machine_lifecycle;
pub(crate) mod machine_lifecycle_adapter;
pub mod permission;
pub mod provision;
pub mod registry;
pub mod route;
pub mod runtime_catalog;
pub(crate) mod runtime_catalog_adapter;
pub mod system_operation;

pub use activity::HostOperationTracker;
pub use image_lifecycle::{ImageLifecycleService, ImageRemovalOutcome};
pub use journal_stream::JournalStreamSource;
pub use machine_lifecycle::{
    MachineAction, MachineLifecycleOutcome, MachineLifecycleResult, MachineLifecycleService,
};
pub use permission::{DefaultPermissionManager, PermissionLevel, PermissionManager};
pub use registry::{OperationRegistry, ResourceClaim, ResourceKey};
pub use runtime_catalog::{RuntimeCatalog, RuntimeQuery, RuntimeUpdate};
pub use system_operation::SystemOperationStore;

#[derive(Clone)]
pub enum BackendCommand {
    SubmitConfig(Box<crate::ui::wizard::context::WizardContext>),
    ValidateInterface { name: String, is_bridge_mode: bool },
    DiscoverHardware,
}

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code, clippy::large_enum_variant)]
pub enum BackendResponse {
    ValidationSuccess,
    ValidationError(String),
    ValidationWarning(String),
    TarImportRiskConfirmationRequired(String),
    DeployStarted,
    DeployFailed(String),
    DeployCancelled(String),
    HardwareDiscovered {
        nvidia_state: crate::nspawn::platform::nvidia::state::NvidiaState,
        nvidia_devices: Vec<String>,
        host_gpus: Vec<crate::nspawn::platform::gpu::GpuDevice>,
    },
    DiscoveryStarted,
    DiscoveryFailed(String),
}
