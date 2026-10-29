//! Import an acquired artifact into a mounted rootfs or a managed RAW image.

mod acquisition;
mod archive;
mod raw;

pub(crate) use acquisition::ImageSource;
pub(crate) use archive::{inspect_tar_runtime, TarRuntimeAssessment};

use crate::adapters::process::CommandRunner;
use crate::adapters::provisioning::engine::diagnostics::DeploymentDiagnostics;
use crate::adapters::provisioning::engine::execution::check_deployment_cancellation;
use crate::adapters::provisioning::engine::failure::OperationResult;
use crate::adapters::provisioning::engine::host::HostResultExt;
use crate::adapters::provisioning::engine::resources::{AppliedResource, ApplyReport};
use crate::adapters::storage::rootfs::RootfsTarget;
use crate::application::provisioning::DeploymentCancellation;
use crate::domain::machine::MachineName;
use acquisition::acquire_image;
use archive::{import_tar_image, ImportTarRequest};
use raw::validate_raw_system_image;
use std::sync::Arc;

pub(crate) struct ArchiveImport {
    pub source: ImageSource,
    pub allow_unsafe_remote: bool,
}

impl ArchiveImport {
    pub(crate) async fn deploy(
        &self,
        target: &RootfsTarget,
        logs: &DeploymentDiagnostics,
        cancellation: &DeploymentCancellation,
    ) -> OperationResult<()> {
        check_deployment_cancellation(cancellation)?;
        let source = acquire_image(&self.source, logs, cancellation).await?;
        check_deployment_cancellation(cancellation)?;
        validate_source(&source)?;
        logs.message("Extracting rootfs archive...");
        let request = ImportTarRequest {
            target: target.clone(),
            source_origin: self.source.tar_origin(),
            allow_unsafe_remote: self.allow_unsafe_remote,
        };
        import_tar_image(request, source, logs, cancellation).await?;
        check_deployment_cancellation(cancellation)
    }
}

pub(crate) struct RawImageImport {
    pub source: ImageSource,
    pub commands: Arc<dyn CommandRunner>,
}

impl RawImageImport {
    pub(crate) async fn deploy(
        &self,
        machine: &MachineName,
        logs: &DeploymentDiagnostics,
        cancellation: &DeploymentCancellation,
        report: &mut ApplyReport,
    ) -> OperationResult<()> {
        check_deployment_cancellation(cancellation)?;
        let source = acquire_image(&self.source, logs, cancellation).await?;
        check_deployment_cancellation(cancellation)?;
        validate_source(&source)?;
        logs.message("Importing RAW machine image...");
        let path = crate::adapters::storage::image_ops::import_raw_image(machine, source)
            .await
            .host_context("publish imported raw image")?;
        report.record_created(AppliedResource::ExternalImage);
        check_deployment_cancellation(cancellation)?;
        validate_raw_system_image(&path, self.commands.as_ref()).await?;
        check_deployment_cancellation(cancellation)
    }
}

fn validate_source(source: &std::fs::File) -> OperationResult<()> {
    crate::adapters::storage::image_ops::validate_import_source(source)
        .host_context("validate import source descriptor")
}
