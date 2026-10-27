//! Tar and raw disk image deployment implementations.

use async_trait::async_trait;

use crate::adapters::error::NspawnError;
use crate::adapters::provisioning::engine::failure::OperationResult as Result;
use crate::adapters::provisioning::engine::{
    check_deployment_cancellation, send_deploy_log, AppliedResource, ApplyReport, DeployLogEvent,
    Deployer, DeploymentCancellation, ImageAcquisitionStore, ImageImportStore, ImageSource,
};
use crate::domain::machine::MachineName;

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
    fn source_stage_resources(
        &self,
        target: &crate::domain::machine::MachineName,
    ) -> Vec<crate::application::provisioning::DeploymentResource> {
        use crate::application::provisioning::DeploymentResource;
        vec![match self.format {
            ImageFormat::Tar => DeploymentResource::LocalStorage(target.clone()),
            ImageFormat::Raw => DeploymentResource::ExternalImage(target.clone()),
        }]
    }

    async fn deploy(
        &self,
        name: &str,
        rootfs: &std::path::Path,
        logs: tokio::sync::mpsc::Sender<DeployLogEvent>,
        cancellation: &DeploymentCancellation,
        report: &mut ApplyReport,
    ) -> Result<()> {
        check_deployment_cancellation(cancellation)?;
        let source = self
            .acquisition
            .acquire(&self.source, &logs, cancellation)
            .await?;
        check_deployment_cancellation(cancellation)?;
        match self.format {
            ImageFormat::Raw => {
                send_deploy_log(&logs, "Importing typed RAW machine image...").await;
                let machine = MachineName::new(name)
                    .map_err(|error| NspawnError::Validation(error.to_string()))?;
                self.image_import.import_raw(machine, source).await?;
                report.record_created(AppliedResource::ExternalImage);
                check_deployment_cancellation(cancellation)?;
            }
            ImageFormat::Tar => {
                send_deploy_log(&logs, "Extracting typed rootfs archive...").await;
                let target =
                    crate::adapters::rootfs::RootfsTarget::from_provisioned_path(name, rootfs)?;
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
                    send_deploy_log(&logs, format!("WARNING: {warning}")).await;
                }
                check_deployment_cancellation(cancellation)?;
            }
        }
        Ok(())
    }
}
