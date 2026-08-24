mod claim;
pub(crate) mod direct;
mod elevated;
pub(crate) mod engine;
mod preparation;
pub(crate) mod recovery;
pub(crate) mod state;

use crate::application::provisioning::{
    DeploymentClaimControl, DeploymentError, DeploymentRecoveryProbe, DeploymentStatePort,
    ProvisioningService, RemoteTarSafety, SourcePreflight,
};
use async_trait::async_trait;
use std::sync::Arc;

pub(crate) enum ProvisioningRoute {
    Direct {
        local_cmd: Arc<dyn crate::adapters::process::CommandRunner>,
        host_operations: crate::application::HostOperationTracker,
        trusted_state_root: crate::adapters::trusted_state::TrustedStateRoot,
    },
    Elevated(Arc<crate::adapters::elevated::ElevatedDaemon>),
}

struct ProvisioningPorts {
    source_preflight: Arc<dyn SourcePreflight>,
    executor: Arc<dyn crate::application::provisioning::DeploymentExecutor>,
    deployment_state: Arc<dyn DeploymentStatePort>,
    recovery: Arc<dyn DeploymentRecoveryProbe>,
    claim_control: Arc<dyn DeploymentClaimControl>,
}

pub(crate) fn compose_provisioning_preparation_service(
) -> Arc<crate::application::provisioning::ProvisioningPreparationService> {
    Arc::new(
        crate::application::provisioning::ProvisioningPreparationService::new(Arc::new(
            preparation::NspawnProvisioningPreparation,
        )),
    )
}

pub(crate) fn compose_provisioning_service(
    route: ProvisioningRoute,
    operations: Arc<crate::application::OperationRegistry>,
    runtime: Arc<crate::application::RuntimeCatalog>,
) -> Arc<ProvisioningService> {
    let ports = match route {
        ProvisioningRoute::Elevated(daemon) => {
            let deployment_state: Arc<dyn DeploymentStatePort> =
                Arc::new(state::ElevatedDeploymentState::new(Arc::clone(&daemon)));
            ProvisioningPorts {
                source_preflight: Arc::new(ElevatedSourcePreflight {
                    daemon: Arc::clone(&daemon),
                }),
                executor: Arc::new(elevated::ElevatedProvisioningExecutor::new(Arc::clone(
                    &daemon,
                ))),
                deployment_state,
                recovery: Arc::new(recovery::ElevatedDeploymentRecoveryProbe::new(Arc::clone(
                    &daemon,
                ))),
                claim_control: Arc::new(claim::ElevatedDeploymentClaimControl::new(daemon)),
            }
        }
        ProvisioningRoute::Direct {
            local_cmd,
            host_operations,
            trusted_state_root,
        } => {
            let deployment_state: Arc<dyn DeploymentStatePort> = Arc::new(
                state::FilesystemDeploymentState::new(trusted_state_root.clone()),
            );
            ProvisioningPorts {
                source_preflight: Arc::new(DirectSourcePreflight),
                executor: Arc::new(direct::DirectProvisioningExecutor::new(
                    local_cmd,
                    host_operations,
                    trusted_state_root.clone(),
                    Arc::clone(&deployment_state),
                )),
                deployment_state,
                recovery: Arc::new(recovery::DirectDeploymentRecoveryProbe::new(
                    runtime,
                    trusted_state_root,
                )),
                claim_control: Arc::new(claim::DirectDeploymentClaimControl),
            }
        }
    };
    Arc::new(ProvisioningService::new(
        ports.source_preflight,
        ports.executor,
        ports.deployment_state,
        ports.recovery,
        ports.claim_control,
        operations,
    ))
}

struct DirectSourcePreflight;

#[async_trait]
impl SourcePreflight for DirectSourcePreflight {
    async fn inspect_remote_tar(&self) -> Result<RemoteTarSafety, DeploymentError> {
        let assessment = tokio::task::spawn_blocking(
            crate::adapters::provisioning::engine::image_operation::inspect_tar_runtime,
        )
        .await
        .map_err(|error| {
            DeploymentError::failed(format!("Tar runtime inspection task failed: {error}"))
        })?
        .map_err(|error| {
            DeploymentError::failed(format!("Could not inspect the host tar runtime: {error}"))
        })?;
        Ok(tar_safety(assessment))
    }
}

struct ElevatedSourcePreflight {
    daemon: Arc<crate::adapters::elevated::ElevatedDaemon>,
}

#[async_trait]
impl SourcePreflight for ElevatedSourcePreflight {
    async fn inspect_remote_tar(&self) -> Result<RemoteTarSafety, DeploymentError> {
        let assessment = self.daemon.assess_tar_runtime().await.map_err(|error| {
            DeploymentError::failed(format!("Could not inspect the host tar runtime: {error}"))
        })?;
        Ok(tar_safety(assessment))
    }
}

fn tar_safety(
    assessment: crate::adapters::provisioning::engine::image_operation::TarRuntimeAssessment,
) -> RemoteTarSafety {
    match assessment.risk {
        Some(risk) => RemoteTarSafety::Risk(risk),
        None => RemoteTarSafety::Compatible,
    }
}
