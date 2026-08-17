pub mod activity;
pub mod handlers;
pub mod inspect;
pub mod manager;
pub mod permission;
pub mod provision;
pub mod system_operation;

pub use activity::HostOperationTracker;
pub use manager::{DefaultManager, NspawnManager};
pub use permission::{DefaultPermissionManager, PermissionLevel, PermissionManager};
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
