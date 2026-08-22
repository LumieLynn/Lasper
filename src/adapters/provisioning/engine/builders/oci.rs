//! systemd-native OCI application image deployment.

use async_trait::async_trait;

use crate::adapters::provisioning::engine::oci_operation::{
    ensure_pull_oci_available, OciPullRequest,
};
use crate::adapters::provisioning::engine::{
    send_deploy_log, stream_deploy_command, AppliedResource, ApplyReport, DeployLogEvent, Deployer,
    DeploymentCancellation, OciPullStore,
};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ContainerConfig, MachineName, OciNetworkMode, OciReference};

pub struct OciDeployer {
    pub reference: String,
    pub read_only: bool,
    pub network: OciNetworkMode,
    pub oci_pull: OciPullStore,
    pub nspawn: crate::adapters::config::NspawnConfigStore,
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
        cancellation: &DeploymentCancellation,
        report: &mut ApplyReport,
    ) -> Result<()> {
        ensure_pull_oci_available()?;
        cancellation.checkpoint()?;
        self.nspawn.prepare_oci_promotion(name).await?;
        cancellation.checkpoint()?;
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

        let spawned = self.oci_pull.spawn(request).await?;
        let status =
            stream_deploy_command(spawned, &logs, cancellation, "systemd-importd OCI transfer")
                .await?;
        if !status.success() {
            return Err(NspawnError::CommandFailed(
                "systemd OCI pull".into(),
                "typed systemd-importd PullOci operation".into(),
                "Command failed. Check deployment logs for detailed output.".into(),
            ));
        }
        report.record_created(AppliedResource::ExternalImage);
        cancellation.checkpoint()?;

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
        let apply = self
            .nspawn
            .promote_oci(name, self.network)
            .await
            .map_err(|error| {
                NspawnError::Runtime(format!(
                    "Failed to configure OCI application after import: {error}"
                ))
            })?;
        report.record_apply(AppliedResource::NspawnConfig, apply)?;
        cancellation.checkpoint()?;

        send_deploy_log(
            &logs,
            format!("OCI application installed as /var/lib/machines/{name}.mstack"),
        )
        .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oci_deployer_uses_external_storage_and_skips_rootfs_post_config() {
        let deployer = OciDeployer {
            reference: "docker.io/library/nginx:latest".into(),
            read_only: false,
            network: OciNetworkMode::Host,
            oci_pull: OciPullStore::new(None),
            nspawn: crate::adapters::config::NspawnConfigStore::new(None),
        };

        assert!(deployer.is_external_storage_managed());
        assert!(!deployer.requires_post_config());
    }
}
