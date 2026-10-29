//! Import of a system image from an acquired local or remote artifact.

mod acquisition;
mod archive;
mod raw;

pub(crate) use acquisition::{ImageAcquisitionStore, ImageSource};
pub(crate) use archive::{inspect_tar_runtime, TarRuntimeAssessment};

use crate::adapters::provisioning::engine::diagnostics::DeploymentDiagnostics;
use crate::adapters::provisioning::engine::execution::check_deployment_cancellation;
use crate::adapters::provisioning::engine::failure::{OperationFailure, OperationResult};
use crate::adapters::provisioning::engine::host::HostResultExt;
use crate::adapters::provisioning::engine::resources::{AppliedResource, ApplyReport};
use crate::adapters::provisioning::engine::sources::Deployer;
use crate::adapters::storage::rootfs::RootfsTarget;
use crate::application::provisioning::{DeploymentCancellation, DeploymentResource};
use crate::domain::machine::MachineName;
use archive::{import_tar_image, ImageImportReport, ImportTarRequest, TarSourceOrigin};
use async_trait::async_trait;
use raw::import_raw_system_image;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    Tar,
    Raw,
}

pub struct ImageDeployer {
    pub source: ImageSource,
    pub format: ImageFormat,
    pub acquisition: ImageAcquisitionStore,
    pub image_import: ImageImportStore,
    pub allow_unsafe_remote_tar: bool,
}

#[async_trait]
impl Deployer for ImageDeployer {
    fn source_stage_resources(&self, target: &MachineName) -> Vec<DeploymentResource> {
        vec![match self.format {
            ImageFormat::Tar => DeploymentResource::LocalStorage(target.clone()),
            ImageFormat::Raw => DeploymentResource::ExternalImage(target.clone()),
        }]
    }

    async fn deploy(
        &self,
        name: &str,
        rootfs: &std::path::Path,
        logs: &DeploymentDiagnostics,
        cancellation: &DeploymentCancellation,
        report: &mut ApplyReport,
    ) -> OperationResult<()> {
        check_deployment_cancellation(cancellation)?;
        let source = self
            .acquisition
            .acquire(&self.source, logs, cancellation)
            .await?;
        check_deployment_cancellation(cancellation)?;
        match self.format {
            ImageFormat::Raw => {
                logs.message("Importing typed RAW machine image...");
                let machine = MachineName::new(name)
                    .map_err(|error| OperationFailure::InvalidInput(error.to_string()))?;
                self.image_import.import_raw(machine, source).await?;
                report.record_created(AppliedResource::ExternalImage);
                check_deployment_cancellation(cancellation)?;
            }
            ImageFormat::Tar => {
                logs.message("Extracting typed rootfs archive...");
                let target = RootfsTarget::from_provisioned_path(name, rootfs)
                    .host_context("resolve archive target")?;
                let report = self
                    .image_import
                    .import_tar(
                        target,
                        source,
                        self.source.tar_origin(),
                        self.allow_unsafe_remote_tar,
                        cancellation,
                    )
                    .await?;
                for warning in report.warnings {
                    logs.message(format!("WARNING: {warning}"));
                }
                check_deployment_cancellation(cancellation)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct ImageImportStore;

impl ImageImportStore {
    pub fn new() -> Self {
        Self
    }

    pub(crate) async fn import_raw(
        &self,
        machine: MachineName,
        source: std::fs::File,
    ) -> OperationResult<()> {
        validate_source(&source)?;
        import_raw_system_image(machine, source).await
    }

    pub(crate) async fn import_tar(
        &self,
        target: RootfsTarget,
        source: std::fs::File,
        source_origin: TarSourceOrigin,
        allow_unsafe_remote: bool,
        cancellation: &DeploymentCancellation,
    ) -> OperationResult<ImageImportReport> {
        validate_source(&source)?;
        let request = ImportTarRequest {
            target,
            source_origin,
            allow_unsafe_remote,
        };
        import_tar_image(request, source, cancellation).await
    }
}

fn validate_source(source: &std::fs::File) -> OperationResult<()> {
    crate::adapters::storage::image_ops::validate_import_source(source)
        .host_context("validate import source descriptor")
}
