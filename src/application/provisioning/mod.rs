mod contract;
mod service;

pub use contract::{
    DeploymentError, DeploymentEvent, DeploymentExecutor, DeploymentId, DeploymentJobHandle,
    DeploymentPreflight, DeploymentProgress, DeploymentRequest, DeploymentSecrets,
    DeploymentSource, DeploymentStatus, DeploymentStorage, DeploymentSubmission, RemoteTarSafety,
    SourcePreflight, UserSecret,
};
pub use service::ProvisioningService;

pub(crate) use contract::{
    DeploymentCancellation, DeploymentCancellationRequested, DeploymentJobContext,
};

#[cfg(test)]
pub(crate) use contract::deployment_job_channel;
