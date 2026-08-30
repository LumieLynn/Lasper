//! Tar and raw disk image deployment implementations.

use async_trait::async_trait;

use crate::adapters::error::{NspawnError, Result};
use crate::adapters::provisioning::engine::{
    check_deployment_cancellation, send_deploy_log, AppliedResource, ApplyReport, DeployLogEvent,
    Deployer, DeploymentCancellation, ImageAcquisitionStore, ImageImportStore, ImageSource,
};
use crate::application::provisioning::MachineProvisioningConfig;
use crate::domain::machine::MachineName;
use crate::domain::source::{ArtifactFormat, ArtifactSpec};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    Tar,
    Raw,
}

impl ImageFormat {
    pub fn from_artifact(artifact: &ArtifactSpec) -> Self {
        match artifact.resolved_format() {
            ArtifactFormat::Tar => Self::Tar,
            ArtifactFormat::Raw => Self::Raw,
            ArtifactFormat::Auto => unreachable!("format was resolved"),
        }
    }
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
    fn is_external_storage_managed(&self) -> bool {
        self.format == ImageFormat::Raw
    }

    async fn deploy(
        &self,
        name: &str,
        _cfg: &MachineProvisioningConfig,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_format_is_resolved_before_privileged_import() {
        assert_eq!(
            ImageFormat::from_artifact(&ArtifactSpec {
                path: "rootfs.raw.xz".into(),
                format: ArtifactFormat::Auto,
            }),
            ImageFormat::Raw
        );
        assert_eq!(
            ImageFormat::from_artifact(&ArtifactSpec {
                path: "rootfs.tar.xz".into(),
                format: ArtifactFormat::Auto,
            }),
            ImageFormat::Tar
        );
    }
}
