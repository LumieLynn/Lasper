//! Container cloning deployment implementation.

use async_trait::async_trait;

use crate::nspawn::errors::Result;
use crate::nspawn::models::ContainerConfig;
use crate::nspawn::ops::provision::backend::ProvisionBackend;
use crate::nspawn::ops::provision::Deployer;

pub struct CloneDeployer {
    pub source_name: String,
    pub provision: std::sync::Arc<dyn ProvisionBackend>,
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
        logs: tokio::sync::mpsc::Sender<String>,
    ) -> Result<()> {
        let _ = logs
            .send(format!(
                "Cloning container {} to {}...",
                self.source_name, name
            ))
            .await;

        self.provision.clone_image(&self.source_name, name).await?;

        // Clone configs
        if let Err(e) = crate::nspawn::adapters::config::nspawn_file::clone_nspawn_config(
            &self.source_name,
            name,
        )
        .await
        {
            let _ = logs
                .send(format!("WARNING: Failed to clone .nspawn config: {}", e))
                .await;
        }
        if let Err(e) = crate::nspawn::adapters::config::systemd_unit::clone_systemd_override(
            &self.source_name,
            name,
        )
        .await
        {
            let _ = logs
                .send(format!("WARNING: Failed to clone systemd override: {}", e))
                .await;
        }

        let _ = self.provision.reload_daemon().await;

        Ok(())
    }
}
