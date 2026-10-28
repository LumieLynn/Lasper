//! OCI source deployment and native importd transfer.

mod transfer;

pub(crate) use transfer::{ensure_pull_oci_available, OciPullStore};

use crate::adapters::provisioning::engine::command::wait_for_deployment_command;
use crate::adapters::provisioning::engine::diagnostics::DeploymentDiagnostics;
use crate::adapters::provisioning::engine::execution::check_deployment_cancellation;
use crate::adapters::provisioning::engine::failure::{OperationFailure, OperationResult};
use crate::adapters::provisioning::engine::host::HostResultExt;
use crate::adapters::provisioning::engine::resources::{AppliedResource, ApplyReport};
use crate::adapters::provisioning::engine::sources::Deployer;
use crate::application::provisioning::{DeploymentCancellation, DeploymentResource};
use crate::domain::machine::MachineName;
use crate::domain::provisioning::oci::{OciNetworkMode, OciReference};
use async_trait::async_trait;
use transfer::OciPullRequest;

pub struct OciDeployer {
    pub reference: String,
    pub read_only: bool,
    pub network: OciNetworkMode,
    pub oci_pull: OciPullStore,
    pub nspawn: crate::adapters::config::NspawnConfigStore,
}

#[async_trait]
impl Deployer for OciDeployer {
    fn source_stage_resources(&self, target: &MachineName) -> Vec<DeploymentResource> {
        vec![
            DeploymentResource::ExternalImage(target.clone()),
            DeploymentResource::NspawnConfig(target.clone()),
        ]
    }

    async fn deploy(
        &self,
        name: &str,
        _rootfs: &std::path::Path,
        logs: &DeploymentDiagnostics,
        cancellation: &DeploymentCancellation,
        report: &mut ApplyReport,
    ) -> OperationResult<()> {
        ensure_pull_oci_available()?;
        check_deployment_cancellation(cancellation)?;
        self.nspawn
            .prepare_oci_promotion(name)
            .await
            .host_context("prepare OCI startup configuration")?;
        check_deployment_cancellation(cancellation)?;
        let request = OciPullRequest {
            reference: OciReference::new(self.reference.trim())
                .map_err(|error| OperationFailure::InvalidInput(error.to_string()))?,
            machine: MachineName::new(name)
                .map_err(|error| OperationFailure::InvalidInput(error.to_string()))?,
            read_only: self.read_only,
        };

        logs.message("Pulling OCI application with systemd-importd (HTTPS authentication only)...");
        log::warn!(
            "[AUDIT] [Container: {}] [Step: OCI] Registry transport is authenticated by HTTPS; publisher signatures are not verified",
            name
        );

        let spawned = self.oci_pull.spawn(request).await?;
        let status = wait_for_deployment_command(
            spawned,
            logs,
            cancellation,
            "systemd-importd OCI transfer",
        )
        .await?;
        if !status.success() {
            return Err(OperationFailure::command_exit(
                "systemd OCI pull",
                "typed systemd-importd PullOci operation",
                status,
            ));
        }
        report.record_created(AppliedResource::ExternalImage);
        check_deployment_cancellation(cancellation)?;

        logs.message(format!(
            "Preserving OCI runtime configuration with PrivateUsers=no and network={}",
            self.network.as_str()
        ));
        log::warn!(
            "[AUDIT] [Container: {}] [Security] PrivateUsers=no, user namespacing disabled for system-scoped OCI application.",
            name
        );
        logs.message(
            "WARNING: PrivateUsers=no disables user namespace isolation for this OCI application.",
        );
        let apply = self
            .nspawn
            .promote_oci(name, self.network)
            .await
            .map_err(|error| {
                OperationFailure::InconsistentState(format!(
                    "Failed to configure OCI application after import: {error}"
                ))
            })?;
        report.record_apply(AppliedResource::NspawnConfig, apply)?;
        check_deployment_cancellation(cancellation)?;

        logs.message(format!(
            "OCI application installed as /var/lib/machines/{name}.mstack"
        ));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oci_source_declares_native_image_and_configuration_effects() {
        let deployer = OciDeployer {
            reference: "docker.io/library/nginx:latest".into(),
            read_only: false,
            network: OciNetworkMode::Host,
            oci_pull: OciPullStore::new(),
            nspawn: crate::adapters::config::NspawnConfigStore::direct(),
        };

        let target = MachineName::new("test").unwrap();
        assert_eq!(
            deployer.source_stage_resources(&target),
            vec![
                DeploymentResource::ExternalImage(target.clone()),
                DeploymentResource::NspawnConfig(target),
            ]
        );
    }
}
