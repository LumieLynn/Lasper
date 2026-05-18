pub mod handlers;
pub mod inspect;
pub mod manager;
pub mod permission;
pub mod provision;

pub use manager::{DefaultManager, NspawnManager};
pub use permission::{DefaultPermissionManager, PermissionLevel, PermissionManager};

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
    DeployStarted,
    DeployFailed(String),
    HardwareDiscovered {
        nvidia_state: crate::nspawn::platform::nvidia::state::NvidiaState,
        nvidia_devices: Vec<String>,
        host_gpus: Vec<crate::nspawn::platform::gpu::GpuDevice>,
    },
    DiscoveryStarted,
    DiscoveryFailed(String),
}
