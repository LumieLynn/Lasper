//! Direct provisioning runner used by root/direct mode and inside the daemon.

use super::engine::builders::{bootstrap, clone, image, oci};
use super::engine::{
    BootstrapStore, Deployer, DirectProvisioningCapabilities, ImageImportStore, OciPullStore,
};
use super::state::FilesystemDeploymentState;
use crate::adapters::error::NspawnError;
use crate::adapters::process::{CommandRunner, DefaultCommandRunner};
use crate::adapters::storage::{
    DirectoryBackend, DiskImageBackend, StorageBackend, SubvolumeBackend,
};
use crate::adapters::trusted_state::TrustedStateRoot;
use crate::application::provisioning::{
    DeploymentError, DeploymentExecutor, DeploymentJobContext, DeploymentPlan, DeploymentRequest,
    DeploymentSecrets, DeploymentSource, DeploymentStatePort, DeploymentStateSession,
    DeploymentStorage,
};
use crate::application::HostOperationTracker;
use async_trait::async_trait;
use std::sync::Arc;

pub(crate) struct DirectProvisioningExecutor {
    host: DirectProvisioningCapabilities,
    host_operations: HostOperationTracker,
    managed_storage: crate::adapters::storage::ManagedStorageStore,
    bootstrap: BootstrapStore,
    image_import: ImageImportStore,
    oci_pull: OciPullStore,
    deployment_state: Arc<dyn DeploymentStatePort>,
    artifact_source: Option<Arc<std::fs::File>>,
}

struct DeploymentBackends {
    deployer: Box<dyn Deployer>,
    storage: Box<dyn StorageBackend>,
}

impl DirectProvisioningExecutor {
    pub(crate) fn new(
        command_runner: Arc<dyn CommandRunner>,
        host_operations: HostOperationTracker,
        state_root: TrustedStateRoot,
        deployment_state: Arc<dyn DeploymentStatePort>,
    ) -> Self {
        Self::assemble(
            command_runner,
            host_operations,
            state_root,
            deployment_state,
            None,
        )
    }

    pub(crate) fn for_daemon(
        state_root: TrustedStateRoot,
        artifact_source: Option<std::fs::File>,
    ) -> Self {
        let command_runner: Arc<dyn CommandRunner> = Arc::new(DefaultCommandRunner);
        let deployment_state: Arc<dyn DeploymentStatePort> =
            Arc::new(FilesystemDeploymentState::new(state_root.clone()));
        Self::assemble(
            command_runner,
            HostOperationTracker::default(),
            state_root,
            deployment_state,
            artifact_source,
        )
    }

    fn assemble(
        command_runner: Arc<dyn CommandRunner>,
        host_operations: HostOperationTracker,
        state_root: TrustedStateRoot,
        deployment_state: Arc<dyn DeploymentStatePort>,
        artifact_source: Option<std::fs::File>,
    ) -> Self {
        Self {
            host: DirectProvisioningCapabilities::from_direct(
                Arc::clone(&command_runner),
                state_root,
            ),
            host_operations,
            managed_storage: crate::adapters::storage::ManagedStorageStore::new(),
            bootstrap: BootstrapStore::new(command_runner),
            image_import: ImageImportStore::new(),
            oci_pull: OciPullStore::new(),
            deployment_state,
            artifact_source: artifact_source.map(Arc::new),
        }
    }

    fn backends(
        &self,
        source: DeploymentSource,
        storage: DeploymentStorage,
        allow_unsafe_remote_tar: bool,
    ) -> Result<DeploymentBackends, DeploymentError> {
        if self.artifact_source.is_some() && !matches!(&source, DeploymentSource::Artifact(_)) {
            return Err(DeploymentError::rejected(
                "an artifact source fd was supplied for a non-artifact deployment",
            ));
        }

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
                system_operations: self.host.system_operations().clone(),
                nspawn: self.host.nspawn().clone(),
                systemd_unit: self.host.systemd_unit().clone(),
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
                nspawn: self.host.nspawn().clone(),
            }),
            DeploymentSource::Artifact(artifact) => {
                let source = match &self.artifact_source {
                    Some(source) => image::ImageSource::Opened(Arc::clone(source)),
                    None => image::ImageSource::Local(artifact.path.clone()),
                };
                Box::new(image::ImageDeployer {
                    source,
                    format: image::ImageFormat::from_artifact(&artifact),
                    image_import: self.image_import.clone(),
                    allow_unsafe_remote_tar: false,
                })
            }
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

        Ok(DeploymentBackends { deployer, storage })
    }
}

#[async_trait]
impl DeploymentExecutor for DirectProvisioningExecutor {
    async fn run(
        &self,
        plan: DeploymentPlan,
        secrets: DeploymentSecrets,
        job: DeploymentJobContext,
    ) -> Result<(), DeploymentError> {
        let planned_target = plan.target().clone();
        let deployment_state =
            DeploymentStateSession::new(Arc::clone(&self.deployment_state), job.id(), &plan);
        let request = plan.into_request();
        let DeploymentRequest {
            config,
            source,
            storage,
            nvidia_profile,
            wayland,
            allow_unsafe_remote_tar,
        } = request;
        if planned_target.as_str() != config.name {
            return Err(DeploymentError::rejected(
                "deployment plan target no longer matches its configuration",
            ));
        }
        super::validate_nspawn_config(&config)?;
        let name = config.name.clone();
        let DeploymentBackends { deployer, storage } =
            self.backends(source, storage, allow_unsafe_remote_tar)?;

        deployment_state
            .prepare()
            .await
            .map_err(|error| DeploymentError::failed(error.to_string()))?;
        let job = job.with_state_session(deployment_state.clone());
        let _operation = self.host_operations.begin();
        let result = super::engine::run_deployment(
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
        .map_err(map_deployment_error);
        if result.is_ok() {
            deployment_state
                .finish()
                .await
                .map_err(|error| DeploymentError::reconciliation_required(error.to_string()))?;
        }
        result
    }
}

fn map_deployment_error(error: NspawnError) -> DeploymentError {
    if matches!(
        error,
        NspawnError::DeploymentProcessStateUnknown(_)
            | NspawnError::DeploymentCancellationRollbackIncomplete(_)
            | NspawnError::DeploymentRollbackIncomplete(_)
    ) {
        DeploymentError::reconciliation_required(error.to_string())
    } else if super::engine::is_cancelled_outcome(&error) {
        DeploymentError::cancelled(error.to_string())
    } else {
        DeploymentError::failed(error.to_string())
    }
}
