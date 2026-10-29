mod contract;
mod ownership;
mod plan;
mod preparation;
mod recovery;
mod service;
mod state;

pub use contract::{
    DeploymentClaimStatus, DeploymentError, DeploymentEvent, DeploymentExecutor, DeploymentId,
    DeploymentJobHandle, DeploymentPreflight, DeploymentProgress, DeploymentResult,
    DeploymentSecrets, DeploymentStartError, DeploymentStatus, DeploymentSubmission,
    RemoteTarSafety, SourcePreflight, UserSecret,
};
pub use plan::DeploymentPlan;
pub use preparation::{
    HostCapability, HostGpuDevice, HostHardwareSnapshot, ImagePartitionInfo, ImagePartitionProbe,
    InterfaceValidation, ProvisioningHostSnapshot, ProvisioningPreparationPort,
    ProvisioningPreparationService, StorageBackendKind, UnclassifiedNvidiaFile,
};
pub use service::ProvisioningService;

#[cfg(test)]
pub(crate) use contract::MemoryDeploymentClaimControl;
pub(crate) use contract::{
    deployment_job_channel, DeploymentCancellation, DeploymentClaimControl, DeploymentJobContext,
    DeploymentRequestId, DeploymentSecretsWire, ResourceApplyStatus,
};
pub(crate) use ownership::{DeploymentResource, ResourceDisposition, ResourceLedger};
pub(crate) use plan::PlanFingerprint;
pub(crate) use recovery::{
    DeploymentRecoveryEvidence, DeploymentRecoveryObservation, DeploymentRecoveryProbe,
    DeploymentRecoveryReport,
};
pub(crate) use service::run_deployment_executor;
pub(crate) use state::{
    DeploymentCrashManifest, DeploymentManifestState, DeploymentStage, DeploymentStateError,
    DeploymentStatePort, DeploymentStateSession,
};

#[cfg(test)]
pub(crate) use recovery::MemoryDeploymentRecoveryProbe;
#[cfg(test)]
pub(crate) use state::MemoryDeploymentStatePort;
