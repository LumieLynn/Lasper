//! Typed debootstrap, pacstrap, and DNF5 deployment implementations.

use async_trait::async_trait;
use tokio::io::AsyncBufReadExt;

use crate::nspawn::adapters::rootfs::RootfsTarget;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{
    BootstrapSpec, ContainerConfig, DebootstrapReleaseSignaturePolicy, Dnf5PackageSignaturePolicy,
};
use crate::nspawn::ops::provision::bootstrap_operation::BootstrapRequest;
use crate::nspawn::ops::provision::{
    send_deploy_log, send_deploy_stream_log, BootstrapStore, DeployLogEvent, Deployer,
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
        let mut spawned = self.bootstrap.spawn(request).await?;
        {
            let reader = &mut spawned.stdout;
            let mut lines = tokio::io::BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                send_deploy_stream_log(&logs, line).await;
            }
        }

        let status = spawned
            .wait()
            .await
            .map_err(|error| NspawnError::Io(std::path::PathBuf::from(label), error))?;
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
