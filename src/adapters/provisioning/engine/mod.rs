//! Deployment trait and orchestrator.

mod bootstrap_args;
pub(crate) mod bootstrap_operation;
pub mod builders;
mod capabilities;
pub(crate) mod image_operation;
pub(crate) mod oci_operation;
mod orchestrator;
mod rollback;
mod stream;
mod tar_limits;

#[cfg(test)]
mod tests;

pub(crate) use crate::application::provisioning::{
    DeploymentCancellation, DeploymentEvent as DeployLogEvent,
};
pub use bootstrap_operation::BootstrapStore;
use capabilities::{
    capture_uncommitted_effects, finish_manifest, persist_applying, persist_cleanup_pending,
    persist_committed,
};
pub(crate) use capabilities::{
    AppliedResource, ApplyReport, Deployer, DirectProvisioningCapabilities,
};
pub use image_operation::ImageImportStore;
pub use oci_operation::OciPullStore;
pub(crate) use orchestrator::{is_cancelled_outcome, run_deployment};
pub(crate) use stream::{
    process_state_unknown, send_deploy_log, send_deploy_progress, send_deploy_stream_log,
    stream_deploy_command,
};
