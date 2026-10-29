mod identity;
mod job;
mod resource;
mod result;
mod secrets;

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
pub(crate) use resource::ResourceApplyStatus;
pub use result::DeploymentResult;
pub(crate) use secrets::DeploymentSecretsWire;
pub use secrets::{DeploymentSecrets, DeploymentStartError, DeploymentSubmission, UserSecret};

#[cfg(test)]
pub(crate) use job::MemoryDeploymentClaimControl;

#[cfg(test)]
mod tests;
