//! Container cloning deployment implementation.

use async_trait::async_trait;

use crate::nspawn::errors::Result;
use crate::nspawn::models::ContainerConfig;
use crate::nspawn::ops::provision::{send_deploy_log, DeployLogEvent, Deployer, DeploymentReceipt};

pub struct CloneDeployer {
    pub source_name: String,
    pub system_operations: crate::nspawn::ops::SystemOperationStore,
    pub nspawn: crate::nspawn::adapters::config::NspawnConfigStore,
    pub systemd_unit: crate::nspawn::adapters::config::SystemdUnitStore,
}

#[async_trait]
impl Deployer for CloneDeployer {
    fn is_external_storage_managed(&self) -> bool {
        true
    }

    fn requires_post_config(&self) -> bool {
        false
    }

    async fn deploy(
        &self,
        name: &str,
        _cfg: &ContainerConfig,
        _rootfs: &std::path::Path,
        logs: tokio::sync::mpsc::Sender<DeployLogEvent>,
    ) -> Result<DeploymentReceipt> {
        send_deploy_log(
            &logs,
            format!("Cloning container {} to {}...", self.source_name, name),
        )
        .await;

        self.system_operations
            .clone_image(&self.source_name, name)
            .await?;

        // Clone configs
        if let Err(e) = self.nspawn.clone_config(&self.source_name, name).await {
            send_deploy_log(
                &logs,
                format!("WARNING: Failed to clone .nspawn config: {}", e),
            )
            .await;
        }
        if let Err(e) = self
            .systemd_unit
            .clone_override(&self.source_name, name)
            .await
        {
            send_deploy_log(
                &logs,
                format!("WARNING: Failed to clone systemd override: {}", e),
            )
            .await;
        }

        let _ = self.system_operations.reload_daemon().await;

        Ok(DeploymentReceipt::external_image())
    }
}
