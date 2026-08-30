//! Typed bootstrap execution shared by direct and elevated modes.

use super::bootstrap_args::{
    debootstrap_args_with_signature_style, dnf5_args, pacstrap_args,
    DebootstrapSignatureOptionStyle,
};
use crate::adapters::process::{CommandRunner, SpawnedProcess};
use crate::adapters::rootfs::RootfsTarget;
use crate::domain::bootstrap::{BootstrapSpec, DebootstrapReleaseSignaturePolicy};
use crate::nspawn::errors::{NspawnError, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BootstrapRequest {
    pub(crate) target: RootfsTarget,
    pub(crate) spec: BootstrapSpec,
    pub(crate) include_sudo: bool,
}

#[derive(Clone)]
pub struct BootstrapStore {
    cmd_runner: Arc<dyn CommandRunner>,
}

impl BootstrapStore {
    pub fn new(cmd_runner: Arc<dyn CommandRunner>) -> Self {
        Self { cmd_runner }
    }

    pub(crate) async fn spawn(&self, request: BootstrapRequest) -> Result<SpawnedProcess> {
        validate_target(&request.target).await?;
        let signature_style = self.probe_debootstrap_signature_style(&request).await?;
        let (program, args) = build_command(&request, signature_style)?;
        self.cmd_runner
            .spawn(&program, args)
            .await
            .map_err(|error| NspawnError::Io(PathBuf::from(program), error))
    }

    async fn probe_debootstrap_signature_style(
        &self,
        request: &BootstrapRequest,
    ) -> Result<DebootstrapSignatureOptionStyle> {
        let policy = debootstrap_signature_policy(request);
        if policy == DebootstrapReleaseSignaturePolicy::ProviderDefault {
            return Ok(DebootstrapSignatureOptionStyle::Sig);
        }
        let output = self
            .cmd_runner
            .run("debootstrap", vec!["--help".into()])
            .await
            .map_err(|error| NspawnError::Io(PathBuf::from("debootstrap --help"), error))?;
        signature_style_from_output(policy, &output.stdout, &output.stderr)
    }
}

pub(crate) fn build_command(
    request: &BootstrapRequest,
    signature_style: DebootstrapSignatureOptionStyle,
) -> Result<(String, Vec<String>)> {
    let target = request.target.path()?;
    let args = match &request.spec {
        BootstrapSpec::Debootstrap(spec) => debootstrap_args_with_signature_style(
            spec,
            &target,
            request.include_sudo,
            signature_style,
        )?,
        BootstrapSpec::Pacstrap(spec) => pacstrap_args(spec, &target, request.include_sudo)?,
        BootstrapSpec::Dnf5(spec) => dnf5_args(spec, &target, request.include_sudo)?,
    };
    let program = match request.spec {
        BootstrapSpec::Debootstrap(_) => "debootstrap",
        BootstrapSpec::Pacstrap(_) => "pacstrap",
        BootstrapSpec::Dnf5(_) => "dnf5",
    };
    Ok((program.into(), args))
}

fn debootstrap_signature_policy(request: &BootstrapRequest) -> DebootstrapReleaseSignaturePolicy {
    match &request.spec {
        BootstrapSpec::Debootstrap(spec) => spec.policy.release_signatures,
        BootstrapSpec::Pacstrap(_) | BootstrapSpec::Dnf5(_) => {
            DebootstrapReleaseSignaturePolicy::ProviderDefault
        }
    }
}

fn signature_style_from_output(
    policy: DebootstrapReleaseSignaturePolicy,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<DebootstrapSignatureOptionStyle> {
    let help = format!(
        "{}\n{}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    );
    let (modern, legacy) = match policy {
        DebootstrapReleaseSignaturePolicy::ProviderDefault => {
            return Ok(DebootstrapSignatureOptionStyle::Sig)
        }
        DebootstrapReleaseSignaturePolicy::Required => ("--force-check-sig", "--force-check-gpg"),
        DebootstrapReleaseSignaturePolicy::Disabled => ("--no-check-sig", "--no-check-gpg"),
    };
    if help.contains(modern) {
        Ok(DebootstrapSignatureOptionStyle::Sig)
    } else if help.contains(legacy) {
        Ok(DebootstrapSignatureOptionStyle::Gpg)
    } else {
        Err(NspawnError::Validation(format!(
            "Installed debootstrap supports neither {modern} nor {legacy}"
        )))
    }
}

pub(crate) async fn validate_target(target: &RootfsTarget) -> Result<()> {
    let path = target.path()?;
    let metadata = tokio::fs::symlink_metadata(&path)
        .await
        .map_err(|error| NspawnError::Io(path.clone(), error))?;
    if !metadata.file_type().is_dir() {
        return Err(NspawnError::Validation(format!(
            "Bootstrap target is not a directory: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn missing_machine_target() -> RootfsTarget {
        RootfsTarget::Machine {
            machine: crate::domain::machine::MachineName::new("lasper-bootstrap-missing-target")
                .unwrap(),
        }
    }

    #[tokio::test]
    async fn bootstrap_validates_its_target_in_the_owning_process() {
        assert!(validate_target(&missing_machine_target()).await.is_err());
    }

    #[test]
    fn signature_probe_prefers_modern_spelling() {
        assert_eq!(
            signature_style_from_output(
                DebootstrapReleaseSignaturePolicy::Required,
                b"--force-check-gpg --force-check-sig",
                b"",
            )
            .unwrap(),
            DebootstrapSignatureOptionStyle::Sig
        );
    }

    #[test]
    fn signature_probe_falls_back_to_legacy_spelling() {
        assert_eq!(
            signature_style_from_output(
                DebootstrapReleaseSignaturePolicy::Disabled,
                b"",
                b"--no-check-gpg",
            )
            .unwrap(),
            DebootstrapSignatureOptionStyle::Gpg
        );
    }

    #[test]
    fn signature_probe_fails_when_policy_cannot_be_enforced() {
        assert!(signature_style_from_output(
            DebootstrapReleaseSignaturePolicy::Required,
            b"usage: debootstrap",
            b"",
        )
        .is_err());
    }
}
