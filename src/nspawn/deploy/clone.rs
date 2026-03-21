//! Container cloning deployment implementation.

use std::sync::{Arc, Mutex};
use async_trait::async_trait;
use tokio::process::Command;

use crate::nspawn::models::ContainerConfig;
use crate::nspawn::deploy::Deployer;
use crate::nspawn::create;
use crate::nspawn::errors::{NspawnError, Result};

pub struct CloneDeployer {
    pub source_name: String,
}

#[async_trait]
impl Deployer for CloneDeployer {
    fn is_external_storage_managed(&self) -> bool {
        true
    }

    async fn deploy(
        &self,
        name: &str,
        _cfg: &ContainerConfig,
        _rootfs: &std::path::Path,
        logs: Arc<Mutex<Vec<String>>>,
    ) -> Result<()> {
        
        {
            let mut l = logs.lock().unwrap();
            l.push(format!("Cloning container {} to {}...", self.source_name, name));
        }

        let out = Command::new("machinectl")
            .args(["clone", &self.source_name, name])
            .output()
            .await
            .map_err(|e| NspawnError::Io(std::path::PathBuf::from("machinectl"), e))?;
        
        if !out.status.success() {
            return Err(NspawnError::CommandFailed("machinectl clone".into(), String::from_utf8_lossy(&out.stderr).trim().to_string()));
        }

        // machinectl clone creates the container in /var/lib/machines/NAME automatically.

        // Clone configs
        if let Err(e) = create::clone_nspawn_config(&self.source_name, name) {
             logs.lock().unwrap().push(format!("WARNING: Failed to clone .nspawn config: {}", e));
        }
        if let Err(e) = create::clone_systemd_override(&self.source_name, name) {
             logs.lock().unwrap().push(format!("WARNING: Failed to clone systemd override: {}", e));
        }

        Ok(())
    }
}
