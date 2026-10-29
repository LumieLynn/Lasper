//! Bootstrap provider policy and source deployment.

mod arguments;
mod command;

pub(crate) use command::BootstrapCommand;

use crate::adapters::provisioning::engine::command::wait_for_deployment_command;
use crate::adapters::provisioning::engine::diagnostics::DeploymentDiagnostics;
use crate::adapters::provisioning::engine::failure::{OperationFailure, OperationResult};
use crate::adapters::storage::rootfs::RootfsTarget;
use crate::application::provisioning::DeploymentCancellation;
use crate::domain::provisioning::bootstrap::{
    BootstrapSpec, DebootstrapReleaseSignaturePolicy, Dnf5PackageSignaturePolicy,
    Dnf5RepositorySource, PacmanKeyringMode, PacmanMirrorlistMode, PacstrapCacheMode,
};
use command::BootstrapRequest;

pub struct BootstrapDeployer {
    pub include_sudo: bool,
    pub spec: BootstrapSpec,
    pub bootstrap: BootstrapCommand,
}

impl BootstrapDeployer {
    pub(crate) async fn deploy(
        &self,
        target: &RootfsTarget,
        logs: &DeploymentDiagnostics,
        cancellation: &DeploymentCancellation,
    ) -> OperationResult<()> {
        self.spec
            .validate()
            .map_err(|error| OperationFailure::InvalidInput(error.to_string()))?;
        if signature_verification_disabled(&self.spec) {
            logs.warning(
                "WARNING: Bootstrap signature verification is explicitly disabled by policy.",
            );
        }
        if !self.spec.inherits_default_packages() {
            logs.warning(
                "WARNING: Default packages are disabled; the configured package set must provide the services required by the selected container setup.",
            );
        }
        match &self.spec {
            BootstrapSpec::Pacstrap(spec)
                if matches!(spec.cache, PacstrapCacheMode::Host)
                    || matches!(spec.policy.keyring, PacmanKeyringMode::CopyHost)
                    || matches!(spec.policy.mirrorlist, PacmanMirrorlistMode::CopyHost) =>
            {
                logs.message(
                    "pacstrap uses provider-default host cache/keyring/mirrorlist behavior.",
                );
            }
            BootstrapSpec::Dnf5(spec) if spec.repository == Dnf5RepositorySource::Host => {
                logs.message("DNF5 is using the host repository configuration for this bootstrap.");
            }
            _ => {}
        }

        let label = provider_label(&self.spec);
        let request = BootstrapRequest {
            target: target.clone(),
            spec: self.spec.clone(),
            include_sudo: self.include_sudo,
        };
        let spawned = self.bootstrap.spawn(request).await?;
        let status = wait_for_deployment_command(spawned, logs, cancellation, label).await?;
        if !status.success() {
            return Err(OperationFailure::command_exit(
                format!("Bootstrap tool ({label})"),
                format!("typed {label} operation"),
                status,
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
