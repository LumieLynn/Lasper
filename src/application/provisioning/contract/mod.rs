mod config;
mod identity;
mod job;
mod request;
mod resource;
mod secrets;

pub use config::MachineProvisioningConfig;
pub use identity::DeploymentId;
pub(crate) use identity::DeploymentRequestId;
pub(crate) use job::{
    deployment_job_channel, DeploymentCancellation, DeploymentClaimControl, DeploymentJobContext,
};
pub use job::{
    DeploymentClaimStatus, DeploymentError, DeploymentEvent, DeploymentExecutor,
    DeploymentJobHandle, DeploymentPreflight, DeploymentProgress, DeploymentStatus,
    RemoteTarSafety, SourcePreflight,
};
pub use request::{DeploymentRequest, DeploymentSource, DeploymentStorage};
pub(crate) use resource::ResourceApplyStatus;
pub(crate) use secrets::DeploymentSecretsWire;
pub use secrets::{DeploymentSecrets, DeploymentSubmission, UserSecret};

#[cfg(test)]
pub(crate) use job::MemoryDeploymentClaimControl;

#[cfg(test)]
mod tests;
