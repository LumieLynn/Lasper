//! Source-specific deployment, including provider invocation and native configuration.

mod bootstrap;
mod clone;
mod image;
mod oci;

pub(crate) use bootstrap::{BootstrapDeployer, BootstrapStore};
pub(crate) use clone::{CloneDeployer, CLONE_IDENTITY_NOTICE};
pub(crate) use image::{
    inspect_tar_runtime, ImageAcquisitionStore, ImageDeployer, ImageFormat, ImageImportStore,
    ImageSource, TarRuntimeAssessment,
};
pub(crate) use oci::{ensure_pull_oci_available, OciDeployer, OciPullStore};

use crate::adapters::provisioning::engine::diagnostics::DeploymentDiagnostics;
use crate::adapters::provisioning::engine::failure::OperationResult;
use crate::adapters::provisioning::engine::resources::ApplyReport;
use crate::application::provisioning::{DeploymentCancellation, DeploymentResource};
use crate::domain::machine::MachineName;

#[async_trait::async_trait]
pub(crate) trait Deployer: Send + Sync {
    /// Apply this source and record any provider-created resources before returning.
    /// Storage allocation and common system configuration belong to the transaction.
    async fn deploy(
        &self,
        name: &str,
        rootfs: &std::path::Path,
        logs: &DeploymentDiagnostics,
        cancellation: &DeploymentCancellation,
        report: &mut ApplyReport,
    ) -> OperationResult<()>;

    /// Resources whose outcome may change while the source stage is running.
    ///
    /// This list is persisted before dispatch. It may conservatively include
    /// optional effects, but it must include every effect the deployer can
    /// create before it returns an authoritative result.
    fn source_stage_resources(&self, target: &MachineName) -> Vec<DeploymentResource> {
        vec![DeploymentResource::LocalStorage(target.clone())]
    }
}
