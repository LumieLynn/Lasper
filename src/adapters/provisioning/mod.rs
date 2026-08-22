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
    operations: Arc<crate::application::OperationRegistry>,
    runtime: Arc<crate::application::RuntimeCatalog>,
) -> Arc<ProvisioningService> {
    let deployment_state: Arc<dyn DeploymentStatePort> = match exec_ctx.daemon_ref() {
        Some(daemon) => Arc::new(state::ElevatedDeploymentState::new(Arc::clone(daemon))),
        None => Arc::new(state::FilesystemDeploymentState::new(
            exec_ctx.trusted_state_root.clone(),
        )),
    };
    let (source_preflight, executor): (
        Arc<dyn SourcePreflight>,
        Arc<dyn crate::application::provisioning::DeploymentExecutor>,
    ) = match exec_ctx.daemon_ref() {
        Some(daemon) => (
            Arc::new(ElevatedSourcePreflight {
                daemon: Arc::clone(daemon),
            }),
            Arc::new(elevated::ElevatedProvisioningExecutor::new(Arc::clone(
                daemon,
            ))),
        ),
        None => (
            Arc::new(DirectSourcePreflight),
            Arc::new(direct::DirectProvisioningExecutor::new(
                Arc::clone(&exec_ctx.local_cmd),
                exec_ctx.host_operations.clone(),
                exec_ctx.trusted_state_root.clone(),
                Arc::clone(&deployment_state),
            )),
        ),
    };
    let recovery: Arc<dyn DeploymentRecoveryProbe> = match exec_ctx.daemon_ref() {
        Some(daemon) => Arc::new(recovery::ElevatedDeploymentRecoveryProbe::new(Arc::clone(
            daemon,
        ))),
        None => Arc::new(recovery::DirectDeploymentRecoveryProbe::new(
            runtime,
            exec_ctx.trusted_state_root.clone(),
        )),
    };
    let claim_control: Arc<dyn DeploymentClaimControl> = match exec_ctx.daemon_ref() {
        Some(daemon) => Arc::new(claim::ElevatedDeploymentClaimControl::new(Arc::clone(
            daemon,
        ))),
        None => Arc::new(claim::DirectDeploymentClaimControl),
    };
    Arc::new(ProvisioningService::new(
        source_preflight,
        executor,
        deployment_state,
        recovery,
        claim_control,
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
