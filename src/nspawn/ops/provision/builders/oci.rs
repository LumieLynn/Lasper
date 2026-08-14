//! systemd-native OCI application image deployment.

use async_trait::async_trait;
use tokio::io::AsyncBufReadExt;

use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ContainerConfig, MachineName, OciNetworkMode, OciReference};
use crate::nspawn::ops::provision::oci_operation::{ensure_pull_oci_available, OciPullRequest};
use crate::nspawn::ops::provision::{
    send_deploy_log, send_deploy_stream_log, DeployLogEvent, Deployer, DeploymentReceipt,
    OciPullStore,
};

pub struct OciDeployer {
    pub reference: String,
    pub read_only: bool,
    pub network: OciNetworkMode,
    pub oci_pull: OciPullStore,
    pub nspawn: crate::nspawn::adapters::config::NspawnConfigStore,
}

#[async_trait]
impl Deployer for OciDeployer {
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
        ensure_pull_oci_available()?;
        self.nspawn.prepare_oci_promotion(name).await?;
        let request = OciPullRequest {
            reference: OciReference::new(self.reference.trim())
                .map_err(|error| NspawnError::Validation(error.to_string()))?,
            machine: MachineName::new(name)
                .map_err(|error| NspawnError::Validation(error.to_string()))?,
            read_only: self.read_only,
        };

        send_deploy_log(
            &logs,
            "Pulling OCI application with systemd-importd (HTTPS authentication only)...",
        )
        .await;
        log::warn!(
            "[AUDIT] [Container: {}] [Step: OCI] Registry transport is authenticated by HTTPS; publisher signatures are not verified",
            name
        );

        let mut spawned = self.oci_pull.spawn(request).await?;
        {
            let mut lines = tokio::io::BufReader::new(&mut spawned.stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                send_deploy_stream_log(&logs, line).await;
            }
        }
        let status = spawned.wait().await.map_err(|error| {
            NspawnError::Io(std::path::PathBuf::from("importctl pull-oci"), error)
        })?;
        if !status.success() {
            return Err(NspawnError::CommandFailed(
                "systemd OCI pull".into(),
                "typed importctl pull-oci operation".into(),
                "Command failed. Check deployment logs for detailed output.".into(),
            ));
        }

        send_deploy_log(
            &logs,
            format!(
                "Preserving OCI runtime configuration with PrivateUsers=no and network={}",
                self.network.as_str()
            ),
        )
        .await;
        log::warn!(
            "[AUDIT] [Container: {}] [Security] PrivateUsers=no, user namespacing disabled for system-scoped OCI application.",
            name
        );
        send_deploy_log(
            &logs,
            "WARNING: PrivateUsers=no disables user namespace isolation for this OCI application.",
        )
        .await;
        if let Err(error) = self.nspawn.promote_oci(name, self.network).await {
            return Err(NspawnError::Runtime(format!(
                "Failed to configure OCI application after import: {error}. The new systemd image was retained because removing it could also remove administrator-owned .nspawn settings; inspect and clean it up manually."
            )));
        }

        send_deploy_log(
            &logs,
            format!("OCI application installed as /var/lib/machines/{name}.mstack"),
        )
        .await;
        Ok(DeploymentReceipt::external_image())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::sys::command::DefaultCommandRunner;
    use std::sync::Arc;

    #[test]
    fn oci_deployer_uses_external_storage_and_skips_rootfs_post_config() {
        let deployer = OciDeployer {
            reference: "docker.io/library/nginx:latest".into(),
            read_only: false,
            network: OciNetworkMode::Host,
            oci_pull: OciPullStore::new(Arc::new(DefaultCommandRunner), None),
            nspawn: crate::nspawn::adapters::config::NspawnConfigStore::new(None),
        };

        assert!(deployer.is_external_storage_managed());
        assert!(!deployer.requires_post_config());
    }
}
