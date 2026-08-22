pub(crate) mod engine;
mod preparation;

use crate::adapters::provisioning::engine::builders::{bootstrap, clone, image, oci};
use crate::adapters::provisioning::engine::{
    BootstrapStore, Deployer, DeploymentHost, ImageImportStore, OciPullStore,
};
use crate::adapters::storage::{
    DirectoryBackend, DiskImageBackend, StorageBackend, SubvolumeBackend,
};
use crate::application::provisioning::{
    DeploymentError, DeploymentExecutor, DeploymentJobContext, DeploymentRequest, DeploymentSource,
    DeploymentStorage, DeploymentSubmission, ProvisioningService, RemoteTarSafety, SourcePreflight,
};
use crate::application::HostOperationTracker;
use crate::nspawn::errors::NspawnError;
use async_trait::async_trait;
use std::sync::Arc;

pub(crate) fn compose_provisioning_preparation_service(
) -> Arc<crate::application::provisioning::ProvisioningPreparationService> {
    Arc::new(
        crate::application::provisioning::ProvisioningPreparationService::new(Arc::new(
            preparation::NspawnProvisioningPreparation,
        )),
    )
}

pub(crate) fn compose_provisioning_service(
    exec_ctx: &Arc<crate::composition::ExecutionContext>,
) -> Arc<ProvisioningService> {
    Arc::new(ProvisioningService::new(
        Arc::new(NspawnSourcePreflight {
            image_import: exec_ctx.image_import.clone(),
        }),
        Arc::new(NspawnProvisioningAdapter {
            host: DeploymentHost::new(
                exec_ctx.system_operations.clone(),
                exec_ctx.nspawn.clone(),
                exec_ctx.systemd_unit.clone(),
                exec_ctx.rootfs.clone(),
                exec_ctx.nvidia_state.clone(),
            ),
            host_operations: exec_ctx.host_operations.clone(),
            managed_storage: exec_ctx.managed_storage.clone(),
            bootstrap: exec_ctx.bootstrap.clone(),
            image_import: exec_ctx.image_import.clone(),
            oci_pull: exec_ctx.oci_pull.clone(),
        }),
    ))
}

struct NspawnSourcePreflight {
    image_import: ImageImportStore,
}

#[async_trait]
impl SourcePreflight for NspawnSourcePreflight {
    async fn inspect_remote_tar(&self) -> Result<RemoteTarSafety, DeploymentError> {
        let assessment = self
            .image_import
            .assess_tar_runtime()
            .await
            .map_err(|error| {
                DeploymentError::failed(format!("Could not inspect the host tar runtime: {error}"))
            })?;
        Ok(match assessment.risk {
            Some(risk) => RemoteTarSafety::Risk(risk),
            None => RemoteTarSafety::Compatible,
        })
    }
}

struct NspawnProvisioningAdapter {
    host: DeploymentHost,
    host_operations: HostOperationTracker,
    managed_storage: crate::adapters::storage::ManagedStorageStore,
    bootstrap: BootstrapStore,
    image_import: ImageImportStore,
    oci_pull: OciPullStore,
}

#[async_trait]
impl DeploymentExecutor for NspawnProvisioningAdapter {
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
            wayland,
            allow_unsafe_remote_tar,
        } = request;
        let name = config.name.clone();
        let (deployer, storage) = self.backends(source, storage, allow_unsafe_remote_tar);
        let _operation = self.host_operations.begin();

        crate::adapters::provisioning::engine::run_deployment(
            deployer,
            storage,
            name,
            config,
            nvidia_profile,
            wayland,
            self.host.clone(),
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
        let managed_storage = self.managed_storage.clone();
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
                system_operations: self.host.system_operations.clone(),
                nspawn: self.host.nspawn.clone(),
                systemd_unit: self.host.systemd_unit.clone(),
            }),
            DeploymentSource::Oci {
                reference,
                read_only,
                network,
            } => Box::new(oci::OciDeployer {
                reference,
                read_only,
                network,
                oci_pull: self.oci_pull.clone(),
                nspawn: self.host.nspawn.clone(),
            }),
            DeploymentSource::Artifact(artifact) => Box::new(image::ImageDeployer {
                source: image::ImageSource::Local(artifact.path.clone()),
                format: image::ImageFormat::from_artifact(&artifact),
                image_import: self.image_import.clone(),
                allow_unsafe_remote_tar: false,
            }),
            DeploymentSource::Pull { url, is_raw } => Box::new(image::ImageDeployer {
                source: image::ImageSource::Remote(url),
                format: if is_raw {
                    image::ImageFormat::Raw
                } else {
                    image::ImageFormat::Tar
                },
                image_import: self.image_import.clone(),
                allow_unsafe_remote_tar,
            }),
            DeploymentSource::Bootstrap(spec) => Box::new(bootstrap::BootstrapDeployer {
                spec,
                bootstrap: self.bootstrap.clone(),
            }),
        };

        (deployer, storage)
    }
}

fn map_deployment_error(error: NspawnError) -> DeploymentError {
    if crate::adapters::provisioning::engine::is_cancelled_outcome(&error) {
        DeploymentError::cancelled(error.to_string())
    } else {
        DeploymentError::failed(error.to_string())
    }
}
