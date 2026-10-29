//! Direct provisioning runner used by root/direct mode and inside the daemon.

use super::engine::sources::{
    BootstrapDeployer, BootstrapStore, CloneDeployer, Deployer, ImageAcquisitionStore,
    ImageDeployer, ImageFormat, ImageImportStore, ImageSource, OciDeployer, OciPullStore,
};
use super::engine::{
    DeploymentDiagnostics, DeploymentExecution, DirectProvisioningCapabilities, StoragePreparation,
    SystemConfiguration,
};
use super::state::FilesystemDeploymentState;
use crate::adapters::process::{CommandRunner, DefaultCommandRunner};
use crate::adapters::storage::{
    DirectoryBackend, DiskImageBackend, StorageBackend, SubvolumeBackend,
};
use crate::adapters::trusted_state::TrustedStateRoot;
use crate::application::provisioning::{
    DeploymentError, DeploymentExecutor, DeploymentJobContext, DeploymentPlan, DeploymentResult,
    DeploymentSecrets, DeploymentStage, DeploymentStatePort, DeploymentStateSession,
};
use crate::application::HostOperationTracker;
use crate::domain::machine::MachineName;
use crate::domain::provisioning::{DeploymentSpec, DeploymentStorage, ImageImport, ImportSource};
use async_trait::async_trait;
use std::sync::Arc;

pub(crate) struct DirectProvisioningExecutor {
    host: DirectProvisioningCapabilities,
    host_operations: HostOperationTracker,
    managed_storage: crate::adapters::storage::ManagedStorageStore,
    bootstrap: BootstrapStore,
    image_acquisition: ImageAcquisitionStore,
    image_import: ImageImportStore,
    oci_pull: OciPullStore,
    deployment_state: Arc<dyn DeploymentStatePort>,
    artifact_source: Option<Arc<std::fs::File>>,
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
            bootstrap: BootstrapStore::new(command_runner.clone()),
            image_acquisition: ImageAcquisitionStore::new(),
            image_import: ImageImportStore::new(command_runner),
            oci_pull: OciPullStore::new(),
            deployment_state,
            artifact_source: artifact_source.map(Arc::new),
        }
    }

    fn storage(&self, storage: DeploymentStorage) -> Box<dyn StorageBackend> {
        match storage {
            DeploymentStorage::Directory => {
                Box::new(DirectoryBackend::new(self.managed_storage.clone()))
            }
            DeploymentStorage::Subvolume => {
                Box::new(SubvolumeBackend::new(self.managed_storage.clone()))
            }
            DeploymentStorage::DiskImage(config) => {
                Box::new(DiskImageBackend::new(config, self.managed_storage.clone()))
            }
        }
    }

    fn image_source(&self, source: ImportSource) -> ImageSource {
        match source {
            ImportSource::Local { path } => match &self.artifact_source {
                Some(source) => ImageSource::Opened(Arc::clone(source)),
                None => ImageSource::Local(path),
            },
            ImportSource::Remote { url } => ImageSource::Remote(url),
        }
    }

    fn execution(
        &self,
        target: MachineName,
        spec: DeploymentSpec,
    ) -> Result<DeploymentExecution, DeploymentError> {
        if self.artifact_source.is_some() && spec.local_artifact_path().is_none() {
            return Err(DeploymentError::rejected(
                "an artifact source fd was supplied for a deployment without a local artifact",
            ));
        }
        let (source, storage, configuration): (Box<dyn Deployer>, _, _) = match spec {
            DeploymentSpec::Bootstrap(deployment) => {
                let source = Box::new(BootstrapDeployer {
                    include_sudo: deployment.users.iter().any(|user| user.sudoer),
                    spec: deployment.provider,
                    bootstrap: self.bootstrap.clone(),
                });
                (
                    source,
                    StoragePreparation::Allocate(self.storage(deployment.storage)),
                    Some(SystemConfiguration {
                        machine: deployment.machine,
                        users: deployment.users,
                        wayland: deployment.wayland,
                        integration: deployment.integration,
                    }),
                )
            }
            DeploymentSpec::Import(deployment) => {
                let (source, format, storage, allow_unsafe_remote_tar) = match deployment.image {
                    ImageImport::Archive {
                        source,
                        storage,
                        allow_unsafe_remote_tar,
                    } => (
                        source,
                        ImageFormat::Tar,
                        StoragePreparation::Allocate(self.storage(storage)),
                        allow_unsafe_remote_tar,
                    ),
                    ImageImport::DiskImage { source } => (
                        source,
                        ImageFormat::Raw,
                        StoragePreparation::ProviderImage,
                        false,
                    ),
                };
                (
                    Box::new(ImageDeployer {
                        source: self.image_source(source),
                        format,
                        acquisition: self.image_acquisition.clone(),
                        image_import: self.image_import.clone(),
                        allow_unsafe_remote_tar,
                    }),
                    storage,
                    Some(SystemConfiguration {
                        machine: deployment.machine,
                        users: Vec::new(),
                        wayland: Vec::new(),
                        integration: deployment.integration,
                    }),
                )
            }
            DeploymentSpec::Clone(deployment) => (
                Box::new(CloneDeployer {
                    source_name: deployment.source_name,
                    system_operations: self.host.system_operations().clone(),
                    nspawn: self.host.nspawn().clone(),
                    systemd_unit: self.host.systemd_unit().clone(),
                }),
                StoragePreparation::ProviderImage,
                None,
            ),
            DeploymentSpec::Oci(deployment) => (
                Box::new(OciDeployer {
                    reference: deployment.reference,
                    read_only: deployment.read_only,
                    network: deployment.network,
                    oci_pull: self.oci_pull.clone(),
                    nspawn: self.host.nspawn().clone(),
                }),
                StoragePreparation::ProviderImage,
                None,
            ),
        };
        Ok(DeploymentExecution {
            target,
            source,
            storage,
            configuration,
        })
    }
}

#[async_trait]
impl DeploymentExecutor for DirectProvisioningExecutor {
    async fn run(
        &self,
        plan: DeploymentPlan,
        secrets: DeploymentSecrets,
        job: DeploymentJobContext,
    ) -> Result<DeploymentResult, DeploymentError> {
        let mut diagnostics =
            DeploymentDiagnostics::new(job.id(), plan.target().clone(), job.event_sender());
        let result = async {
            diagnostics.enter_stage(DeploymentStage::Validation);
            let planned_target = plan.target().clone();
            let deployment_state =
                DeploymentStateSession::new(Arc::clone(&self.deployment_state), job.id(), &plan);
            let spec = plan.into_spec();
            if let Some(machine) = spec.machine_config() {
                super::validate_nspawn_config(machine)?;
            }
            let execution = self.execution(planned_target, spec)?;

            deployment_state
                .prepare()
                .await
                .map_err(|error| DeploymentError::failed(error.to_string()))?;
            let job = job.with_state_session(deployment_state);
            let _operation = self.host_operations.begin();
            super::engine::run_deployment(
                execution,
                self.host.clone(),
                secrets,
                job,
                &mut diagnostics,
            )
            .await
            .map_err(super::engine::DeploymentFailure::into_application_error)
        }
        .await;
        diagnostics.finish(&result);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::storage::StorageType;
    use crate::application::provisioning::MemoryDeploymentStatePort;
    use crate::domain::provisioning::storage::DiskImageConfig;

    fn executor(state_root: &std::path::Path) -> DirectProvisioningExecutor {
        DirectProvisioningExecutor::new(
            Arc::new(DefaultCommandRunner),
            HostOperationTracker::default(),
            TrustedStateRoot::for_test(state_root.to_path_buf()),
            Arc::new(MemoryDeploymentStatePort::default()),
        )
    }

    #[test]
    fn remote_tar_preserves_the_selected_storage_backend() {
        let temporary = tempfile::tempdir().unwrap();
        let executor = executor(&temporary.path().join("state"));
        let machine = "tar-backend-matrix";
        let cases = [
            (DeploymentStorage::Directory, StorageType::Directory),
            (DeploymentStorage::Subvolume, StorageType::Subvolume),
            (
                DeploymentStorage::DiskImage(DiskImageConfig::default()),
                StorageType::DiskImage,
            ),
        ];

        for (storage, expected_type) in cases {
            let execution = executor
                .execution(
                    MachineName::new(machine).unwrap(),
                    DeploymentSpec::Import(Box::new(
                        crate::domain::provisioning::ImportDeployment {
                            machine: crate::domain::machine::MachineConfig {
                                name: machine.into(),
                                ..Default::default()
                            },
                            image: ImageImport::Archive {
                                source: ImportSource::Remote {
                                    url: "https://example.test/rootfs.tar.xz".into(),
                                },
                                storage,
                                allow_unsafe_remote_tar: true,
                            },
                            integration: Default::default(),
                        },
                    )),
                )
                .unwrap();
            let storage = execution.storage.backend().unwrap();
            assert_eq!(storage.get_type(), expected_type);
            let expected_path = match expected_type {
                StorageType::Directory | StorageType::Subvolume => {
                    crate::paths::machine_root(machine)
                }
                StorageType::DiskImage => crate::paths::machine_raw_image(machine),
            };
            assert_eq!(storage.get_path(machine), expected_path);
            let configuration = execution.configuration.unwrap();
            assert!(configuration.users.is_empty());
            assert!(configuration.wayland.is_empty());
        }
    }

    #[test]
    fn native_images_do_not_allocate_another_storage_backend() {
        use crate::domain::provisioning::{CloneDeployment, ImportDeployment, OciDeployment};
        let temporary = tempfile::tempdir().unwrap();
        let executor = executor(&temporary.path().join("state"));
        let machine = MachineName::new("native-image").unwrap();
        let import = DeploymentSpec::Import(Box::new(ImportDeployment {
            machine: crate::domain::machine::MachineConfig {
                name: machine.to_string(),
                ..Default::default()
            },
            image: ImageImport::DiskImage {
                source: ImportSource::Remote {
                    url: "https://example.test/rootfs.raw".into(),
                },
            },
            integration: Default::default(),
        }));
        let execution = executor.execution(machine.clone(), import).unwrap();
        assert!(execution.storage.backend().is_none());
        assert!(execution.configuration.unwrap().users.is_empty());
        for spec in [
            DeploymentSpec::Clone(CloneDeployment {
                name: machine.to_string(),
                source_name: "base".into(),
            }),
            DeploymentSpec::Oci(OciDeployment {
                name: machine.to_string(),
                reference: "docker.io/library/nginx:latest".into(),
                read_only: false,
                network: crate::domain::provisioning::oci::OciNetworkMode::Host,
            }),
        ] {
            let execution = executor.execution(machine.clone(), spec).unwrap();
            assert!(execution.storage.backend().is_none());
            assert!(execution.configuration.is_none());
        }
    }
}
