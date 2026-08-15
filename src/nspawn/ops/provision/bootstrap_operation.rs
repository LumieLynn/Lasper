//! Typed bootstrap execution shared by direct and elevated modes.

use crate::nspawn::adapters::rootfs::RootfsTarget;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{
    BootstrapSpec, DebootstrapReleaseSignaturePolicy, DebootstrapSignatureOptionStyle,
};
use crate::nspawn::sys::command::{CommandRunner, SpawnedProcess};
use crate::nspawn::sys::daemon::ElevatedDaemon;
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
    daemon: Option<Arc<ElevatedDaemon>>,
}

impl BootstrapStore {
    pub fn new(cmd_runner: Arc<dyn CommandRunner>, daemon: Option<Arc<ElevatedDaemon>>) -> Self {
        Self { cmd_runner, daemon }
    }

    pub(crate) async fn spawn(&self, request: BootstrapRequest) -> Result<SpawnedProcess> {
        validate_client_target(&request.target, self.daemon.is_some()).await?;

        if let Some(daemon) = &self.daemon {
            let cmd_id = daemon.reserve_spawn_id();
            let stdout_fd = daemon
                .spawn_bootstrap(cmd_id, request)
                .await
                .map_err(|error| NspawnError::Io(PathBuf::from("bootstrap"), error))?;
            let receiver = crate::nspawn::sys::daemon::pipe_reader(stdout_fd)
                .map_err(|error| NspawnError::Io(PathBuf::from("bootstrap"), error))?;
            let wait_daemon = daemon.clone();
            let signal_daemon = daemon.clone();
            Ok(SpawnedProcess::new_cancellable(
                Box::new(receiver),
                async move {
                    let code = wait_daemon
                        .wait_command(cmd_id)
                        .await
                        .map_err(|error| std::io::Error::other(error.to_string()))?;
                    Ok(std::os::unix::process::ExitStatusExt::from_raw(code))
                },
                move |signal| {
                    let daemon = signal_daemon.clone();
                    Box::pin(async move { daemon.signal_command(cmd_id, signal).await })
                },
            ))
        } else {
            let signature_style = self.probe_debootstrap_signature_style(&request).await?;
            let (program, args) = build_command(&request, signature_style)?;
            self.cmd_runner
                .spawn(&program, args)
                .await
                .map_err(|error| NspawnError::Io(PathBuf::from(program), error))
        }
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

async fn validate_client_target(target: &RootfsTarget, delegated_to_daemon: bool) -> Result<()> {
    if delegated_to_daemon {
        // Elevated targets are root-owned; the daemon validates them before spawning.
        Ok(())
    } else {
        validate_target(target).await
    }
}

pub(crate) fn build_command(
    request: &BootstrapRequest,
    signature_style: DebootstrapSignatureOptionStyle,
) -> Result<(String, Vec<String>)> {
    let target = request.target.path()?;
    let args = match &request.spec {
        BootstrapSpec::Debootstrap(spec) => {
            spec.args_with_signature_style(&target, request.include_sudo, signature_style)?
        }
        BootstrapSpec::Pacstrap(spec) => spec.args(&target, request.include_sudo)?,
        BootstrapSpec::Dnf5(spec) => spec.args(&target, request.include_sudo)?,
    };
    let program = match request.spec {
        BootstrapSpec::Debootstrap(_) => "debootstrap",
        BootstrapSpec::Pacstrap(_) => "pacstrap",
        BootstrapSpec::Dnf5(_) => "dnf5",
    };
    Ok((program.into(), args))
}

pub(crate) fn probe_debootstrap_signature_style_sync(
    request: &BootstrapRequest,
) -> Result<DebootstrapSignatureOptionStyle> {
    let policy = debootstrap_signature_policy(request);
    if policy == DebootstrapReleaseSignaturePolicy::ProviderDefault {
        return Ok(DebootstrapSignatureOptionStyle::Sig);
    }
    let output = crate::nspawn::sys::new_sync_command("debootstrap")
        .arg("--help")
        .output()
        .map_err(|error| NspawnError::Io(PathBuf::from("debootstrap --help"), error))?;
    signature_style_from_output(policy, &output.stdout, &output.stderr)
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
            machine: crate::nspawn::models::MachineName::new("lasper-bootstrap-missing-target")
                .unwrap(),
        }
    }

    #[tokio::test]
    async fn delegated_bootstrap_does_not_probe_target_in_unprivileged_client() {
        assert!(validate_client_target(&missing_machine_target(), true)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn direct_bootstrap_still_validates_target_locally() {
        assert!(validate_client_target(&missing_machine_target(), false)
            .await
            .is_err());
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
