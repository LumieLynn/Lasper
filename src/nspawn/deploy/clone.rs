//! Container cloning deployment implementation.

use async_trait::async_trait;
#[allow(unused_imports)]
use std::sync::{Arc, Mutex};
use crate::nspawn::utils::{new_command, CommandLogged};

use crate::nspawn::deploy::Deployer;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::ContainerConfig;

pub struct CloneDeployer {
    pub source_name: String,
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

        let out = new_command("machinectl")
            .args(["clone", &self.source_name, name])
            .logged_output("machinectl")
            .await
            .map_err(|e| NspawnError::Io(std::path::PathBuf::from("machinectl"), e))?;

        if !out.status.success() {
            return Err(NspawnError::cmd_failed(
                "machinectl clone",
                format!("machinectl clone {} {}", self.source_name, name),
                &out,
            ));
        }

        // machinectl clone creates the container in /var/lib/machines/NAME automatically.

        // Clone configs
        if let Err(e) = crate::nspawn::config::nspawn_file::clone_nspawn_config(&self.source_name, name).await {
            let _ = logs
                .send(format!("WARNING: Failed to clone .nspawn config: {}", e))
                .await;
        }
        if let Err(e) = crate::nspawn::config::systemd_unit::clone_systemd_override(&self.source_name, name).await {
            let _ = logs
                .send(format!("WARNING: Failed to clone systemd override: {}", e))
                .await;
        }

        Ok(())
    }
}
