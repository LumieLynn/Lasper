//! Typed debootstrap, pacstrap, and DNF5 deployment implementations.

use async_trait::async_trait;

use crate::adapters::provisioning::engine::bootstrap_operation::BootstrapRequest;
use crate::adapters::provisioning::engine::{
    send_deploy_log, stream_deploy_command, ApplyReport, BootstrapStore, DeployLogEvent, Deployer,
    DeploymentCancellation,
};
use crate::adapters::rootfs::RootfsTarget;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{
    BootstrapSpec, ContainerConfig, DebootstrapReleaseSignaturePolicy, Dnf5PackageSignaturePolicy,
};

pub struct BootstrapDeployer {
    pub spec: BootstrapSpec,
    pub bootstrap: BootstrapStore,
}

#[async_trait]
impl Deployer for BootstrapDeployer {
    async fn deploy(
        &self,
        name: &str,
        cfg: &ContainerConfig,
        rootfs: &std::path::Path,
        logs: tokio::sync::mpsc::Sender<DeployLogEvent>,
        cancellation: &DeploymentCancellation,
        _report: &mut ApplyReport,
    ) -> Result<()> {
        self.spec.validate()?;
        if signature_verification_disabled(&self.spec) {
            send_deploy_log(
                &logs,
                "WARNING: Bootstrap signature verification is explicitly disabled by policy.",
            )
            .await;
            log::warn!(
                "[AUDIT] [Container: {}] [Step: Bootstrap] Signature verification disabled",
                name
            );
        }
        if !self.spec.inherits_default_packages() {
            send_deploy_log(
                &logs,
                "WARNING: Default packages are disabled; the configured package set must provide the services required by the selected container setup.",
            )
            .await;
            log::warn!(
                "[AUDIT] [Container: {}] [Step: Bootstrap] Provider default packages disabled",
                name
            );
        }
        match &self.spec {
            BootstrapSpec::Pacstrap(spec)
                if matches!(spec.cache, crate::nspawn::models::PacstrapCacheMode::Host)
                    || matches!(
                        spec.policy.keyring,
                        crate::nspawn::models::PacmanKeyringMode::CopyHost
                    )
                    || matches!(
                        spec.policy.mirrorlist,
                        crate::nspawn::models::PacmanMirrorlistMode::CopyHost
                    ) =>
            {
                log::info!(
                            "[AUDIT] [Container: {}] [Step: Bootstrap] pacstrap uses provider-default host cache/keyring/mirrorlist behavior",
                            name
                        );
            }
            BootstrapSpec::Dnf5(spec)
                if spec.repository == crate::nspawn::models::Dnf5RepositorySource::Host =>
            {
                send_deploy_log(
                    &logs,
                    "DNF5 is using the host repository configuration for this bootstrap.",
                )
                .await;
                log::info!(
                    "[AUDIT] [Container: {}] [Step: Bootstrap] DNF5 host repository configuration enabled",
                    name
                );
            }
            _ => {}
        }

        let label = provider_label(&self.spec);
        let request = BootstrapRequest {
            target: RootfsTarget::from_provisioned_path(name, rootfs)?,
            spec: self.spec.clone(),
            include_sudo: cfg.users.iter().any(|user| user.sudoer),
        };
        let spawned = self.bootstrap.spawn(request).await?;
        let status = stream_deploy_command(spawned, &logs, cancellation, label).await?;
        if !status.success() {
            return Err(NspawnError::CommandFailed(
                format!("Bootstrap tool ({label})"),
                format!("typed {label} operation"),
                "Command failed. Check deployment logs for detailed output.".to_string(),
            ));
        }
        Ok(())
    }
}

fn provider_label(spec: &BootstrapSpec) -> &'static str {
    match spec {
        BootstrapSpec::Debootstrap(_) => "debootstrap",
        BootstrapSpec::Pacstrap(_) => "pacstrap",
        BootstrapSpec::Dnf5(_) => "dnf5",
    }
}

fn signature_verification_disabled(spec: &BootstrapSpec) -> bool {
    match spec {
        BootstrapSpec::Debootstrap(spec) => {
            spec.policy.release_signatures == DebootstrapReleaseSignaturePolicy::Disabled
        }
        BootstrapSpec::Pacstrap(_) => false,
        BootstrapSpec::Dnf5(spec) => {
            spec.policy.package_signatures == Dnf5PackageSignaturePolicy::Disabled
        }
    }
}
