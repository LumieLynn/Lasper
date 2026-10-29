//! Source-specific deployment, including provider invocation and native configuration.

mod bootstrap;
mod clone;
mod image;
mod oci;

pub(crate) use bootstrap::{BootstrapCommand, BootstrapDeployer};
pub(crate) use clone::{CloneDeployer, CLONE_IDENTITY_NOTICE};
pub(crate) use image::{
    inspect_tar_runtime, ArchiveImport, ImageSource, RawImageImport, TarRuntimeAssessment,
};
pub(crate) use oci::{ensure_pull_oci_available, OciDeployer};

use crate::adapters::provisioning::engine::diagnostics::DeploymentDiagnostics;
use crate::adapters::provisioning::engine::failure::OperationResult;
use crate::adapters::storage::rootfs::RootfsTarget;
use crate::application::provisioning::DeploymentCancellation;

/// Sources that populate storage already allocated and mounted by the transaction.
pub(crate) enum RootfsSource {
    Bootstrap(Box<BootstrapDeployer>),
    Archive(ArchiveImport),
}

impl RootfsSource {
    pub(crate) async fn deploy(
        &self,
        target: &RootfsTarget,
        logs: &DeploymentDiagnostics,
        cancellation: &DeploymentCancellation,
    ) -> OperationResult<()> {
        match self {
            Self::Bootstrap(source) => source.deploy(target, logs, cancellation).await,
            Self::Archive(source) => source.deploy(target, logs, cancellation).await,
        }
    }
}
