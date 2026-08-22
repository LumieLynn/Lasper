mod contract;
mod preparation;
mod service;
mod wayland;

pub use contract::{
    DeploymentError, DeploymentEvent, DeploymentExecutor, DeploymentId, DeploymentJobHandle,
    DeploymentPreflight, DeploymentProgress, DeploymentRequest, DeploymentSecrets,
    DeploymentSource, DeploymentStatus, DeploymentStorage, DeploymentSubmission, RemoteTarSafety,
    SourcePreflight, UserSecret,
};
pub use preparation::{
    HostCapability, HostGpuDevice, HostHardwareSnapshot, ImagePartitionInfo, ImagePartitionProbe,
    InterfaceValidation, ProvisioningHostSnapshot, ProvisioningPreparationPort,
    ProvisioningPreparationService, StorageBackendKind, UnclassifiedNvidiaFile,
};
pub use service::ProvisioningService;

pub(crate) use contract::{
    DeploymentCancellation, DeploymentCancellationRequested, DeploymentJobContext,
};
pub(crate) use wayland::{resolve_wayland_bind_policy, resolve_wayland_grant};

#[cfg(test)]
pub(crate) use contract::deployment_job_channel;
