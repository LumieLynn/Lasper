use crate::application::provisioning::{
    DeploymentError, DeploymentJobContext, DeploymentPreflight, DeploymentRequest,
    DeploymentSource, DeploymentStorage, DeploymentSubmission, ProvisioningPort,
    ProvisioningService,
};
use crate::nspawn::adapters::storage::{
    DirectoryBackend, DiskImageBackend, StorageBackend, SubvolumeBackend,
};
use crate::nspawn::errors::NspawnError;
use crate::nspawn::ops::provision::builders::{bootstrap, clone, image, oci};
use crate::nspawn::ops::provision::Deployer;
use async_trait::async_trait;
use std::sync::Arc;

pub(crate) fn compose_provisioning_service(
    exec_ctx: &Arc<crate::nspawn::sys::ExecutionContext>,
) -> Arc<ProvisioningService> {
    Arc::new(ProvisioningService::new(Arc::new(
        NspawnProvisioningAdapter {
            exec_ctx: Arc::clone(exec_ctx),
        },
    )))
}

struct NspawnProvisioningAdapter {
    exec_ctx: Arc<crate::nspawn::sys::ExecutionContext>,
}

#[async_trait]
impl ProvisioningPort for NspawnProvisioningAdapter {
    async fn preflight(
        &self,
        request: &DeploymentRequest,
    ) -> Result<DeploymentPreflight, DeploymentError> {
        if !request
            .source
            .is_unacknowledged_remote_tar(request.allow_unsafe_remote_tar)
        {
            return Ok(DeploymentPreflight::Ready);
        }

        let assessment = self
            .exec_ctx
            .image_import
            .assess_tar_runtime()
            .await
            .map_err(|error| {
                DeploymentError::failed(format!("Could not inspect the host tar runtime: {error}"))
            })?;
        Ok(match assessment.risk {
            Some(risk) => DeploymentPreflight::ConfirmationRequired(risk),
            None => DeploymentPreflight::Ready,
        })
    }

    async fn run(
        &self,
        submission: DeploymentSubmission,
        job: DeploymentJobContext,
    ) -> Result<(), DeploymentError> {
        let (request, secrets) = submission.into_parts();
        let DeploymentRequest {
            config,
            source,
            storage,
            nvidia_profile,
            allow_unsafe_remote_tar,
        } = request;
        let name = config.name.clone();
        let (deployer, storage) = self.backends(source, storage, allow_unsafe_remote_tar);
        let _operation = self.exec_ctx.host_operations.begin();

        crate::nspawn::ops::provision::run_deployment(
            deployer,
            storage,
            name,
            config,
            nvidia_profile,
            Arc::clone(&self.exec_ctx),
            secrets,
            job,
        )
        .await
        .map_err(map_deployment_error)
    }
}

impl NspawnProvisioningAdapter {
    fn backends(
        &self,
        source: DeploymentSource,
        storage: DeploymentStorage,
        allow_unsafe_remote_tar: bool,
    ) -> (Box<dyn Deployer>, Box<dyn StorageBackend>) {
        let managed_storage = self.exec_ctx.managed_storage.clone();
        let storage: Box<dyn StorageBackend> = match storage {
            DeploymentStorage::Directory => {
                Box::new(DirectoryBackend::new(managed_storage.clone()))
            }
            DeploymentStorage::Subvolume => {
                Box::new(SubvolumeBackend::new(managed_storage.clone()))
            }
            DeploymentStorage::DiskImage(config) => {
                Box::new(DiskImageBackend::new(config, managed_storage))
            }
        };

        let deployer: Box<dyn Deployer> = match source {
            DeploymentSource::Copy { source_name } => Box::new(clone::CloneDeployer {
                source_name,
                system_operations: self.exec_ctx.system_operations.clone(),
                nspawn: self.exec_ctx.nspawn.clone(),
                systemd_unit: self.exec_ctx.systemd_unit.clone(),
            }),
            DeploymentSource::Oci {
                reference,
                read_only,
                network,
            } => Box::new(oci::OciDeployer {
                reference,
                read_only,
                network,
                oci_pull: self.exec_ctx.oci_pull.clone(),
                nspawn: self.exec_ctx.nspawn.clone(),
            }),
            DeploymentSource::Artifact(artifact) => Box::new(image::ImageDeployer {
                source: image::ImageSource::Local(artifact.path.clone()),
                format: image::ImageFormat::from_artifact(&artifact),
                image_import: self.exec_ctx.image_import.clone(),
                allow_unsafe_remote_tar: false,
            }),
            DeploymentSource::Pull { url, is_raw } => Box::new(image::ImageDeployer {
                source: image::ImageSource::Remote(url),
                format: if is_raw {
                    image::ImageFormat::Raw
                } else {
                    image::ImageFormat::Tar
                },
                image_import: self.exec_ctx.image_import.clone(),
                allow_unsafe_remote_tar,
            }),
            DeploymentSource::Bootstrap(spec) => Box::new(bootstrap::BootstrapDeployer {
                spec,
                bootstrap: self.exec_ctx.bootstrap.clone(),
            }),
        };

        (deployer, storage)
    }
}

fn map_deployment_error(error: NspawnError) -> DeploymentError {
    if crate::nspawn::ops::provision::is_cancelled_outcome(&error) {
        DeploymentError::cancelled(error.to_string())
    } else {
        DeploymentError::failed(error.to_string())
    }
}
