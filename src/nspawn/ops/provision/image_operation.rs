//! Typed image import operations shared by direct and elevated modes.

use crate::nspawn::adapters::rootfs::RootfsTarget;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::MachineName;
use crate::nspawn::sys::daemon::ElevatedDaemon;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImportTarRequest {
    pub(crate) target: RootfsTarget,
    pub(crate) source_origin: TarSourceOrigin,
    pub(crate) allow_unsafe_remote: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TarSourceOrigin {
    Local,
    Remote,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TarRuntimeAssessment {
    pub(crate) risk: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ImageImportReport {
    pub(crate) warnings: Vec<String>,
}

#[derive(Clone)]
pub struct ImageImportStore {
    daemon: Option<Arc<ElevatedDaemon>>,
}

impl ImageImportStore {
    pub fn new(daemon: Option<Arc<ElevatedDaemon>>) -> Self {
        Self { daemon }
    }

    pub(crate) async fn import_raw(
        &self,
        machine: MachineName,
        source: std::fs::File,
    ) -> Result<()> {
        validate_source(&source)?;
        if let Some(daemon) = &self.daemon {
            daemon
                .import_raw_image(machine, source)
                .await
                .map_err(|error| NspawnError::Runtime(error.to_string()))
        } else {
            import_raw_system_image(machine, source).await
        }
    }

    pub(crate) async fn import_tar(
        &self,
        target: RootfsTarget,
        source: std::fs::File,
        source_origin: TarSourceOrigin,
        allow_unsafe_remote: bool,
    ) -> Result<ImageImportReport> {
        validate_source(&source)?;
        let request = ImportTarRequest {
            target,
            source_origin,
            allow_unsafe_remote,
        };
        if let Some(daemon) = &self.daemon {
            daemon
                .import_tar_image(request, source)
                .await
                .map_err(|error| NspawnError::Runtime(error.to_string()))
        } else {
            import_tar_image(request, source).await
        }
    }

    pub(crate) async fn assess_tar_runtime(&self) -> Result<TarRuntimeAssessment> {
        if let Some(daemon) = &self.daemon {
            daemon
                .assess_tar_runtime()
                .await
                .map_err(|error| NspawnError::Runtime(error.to_string()))
        } else {
            tokio::task::spawn_blocking(inspect_tar_runtime)
                .await
                .map_err(|error| {
                    NspawnError::Runtime(format!("tar runtime inspection task failed: {error}"))
                })?
        }
    }
}

pub(crate) async fn import_raw_system_image(
    machine: MachineName,
    source: std::fs::File,
) -> Result<()> {
    validate_source(&source)?;
    let path =
        crate::nspawn::adapters::storage::image_ops::import_raw_image(&machine, source).await?;
    let output = match crate::nspawn::sys::new_command("systemd-dissect")
        .args(["--validate"])
        .arg(&path)
        .output()
        .await
    {
        Ok(output) => output,
        Err(error) => {
            remove_failed_raw_import(&path).await;
            return Err(NspawnError::Io(PathBuf::from("systemd-dissect"), error));
        }
    };
    crate::nspawn::sys::log_output("systemd-dissect --validate", &output);
    if !output.status.success() {
        remove_failed_raw_import(&path).await;
        return Err(NspawnError::cmd_failed(
            "validate imported raw system image",
            format!("systemd-dissect --validate {}", path.display()),
            &output,
        ));
    }
    Ok(())
}

async fn remove_failed_raw_import(path: &std::path::Path) {
    if let Err(error) = tokio::fs::remove_file(path).await {
        log::warn!(
            "Failed to remove rejected raw image {}: {}",
            path.display(),
            error
        );
    }
}

impl std::fmt::Debug for ImageImportStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageImportStore")
            .field("daemon", &self.daemon)
            .finish()
    }
}

fn validate_source(source: &std::fs::File) -> Result<()> {
    crate::nspawn::adapters::storage::image_ops::validate_import_source(source)
}

pub(crate) async fn import_tar_image(
    request: ImportTarRequest,
    source: std::fs::File,
) -> Result<ImageImportReport> {
    validate_source(&source)?;
    validate_tar_target(&request.target).await?;
    let target = request.target.path()?;
    let operation_target = target.clone();
    let source_origin = request.source_origin;
    let allow_unsafe_remote = request.allow_unsafe_remote;

    tokio::task::spawn_blocking(move || {
        extract_tar_at(
            &operation_target,
            source,
            source_origin,
            allow_unsafe_remote,
        )
    })
    .await
    .map_err(|error| NspawnError::Runtime(format!("tar import task failed: {error}")))?
}

fn extract_tar_at(
    target: &std::path::Path,
    source: std::fs::File,
    source_origin: TarSourceOrigin,
    allow_unsafe_remote: bool,
) -> Result<ImageImportReport> {
    let assessment = inspect_tar_runtime()?;
    let report = tar_runtime_policy(source_origin, &assessment, allow_unsafe_remote)?;
    for warning in &report.warnings {
        log::warn!("[AUDIT] [Tar import] {warning}");
    }

    let output = tar_command()
        .args(["--numeric-owner", "-pxf", "-", "-C"])
        .arg(target)
        .stdin(Stdio::from(source))
        .output()
        .map_err(|error| NspawnError::Io(PathBuf::from("tar"), error))?;
    crate::nspawn::sys::log_output("typed tar import", &output);
    if output.status.success() {
        Ok(report)
    } else {
        Err(NspawnError::cmd_failed(
            "extract rootfs archive",
            format!("tar --numeric-owner -pxf - -C {}", target.display()),
            &output,
        ))
    }
}

fn tar_command() -> std::process::Command {
    let mut command = crate::nspawn::sys::new_sync_command("tar");
    // GNU tar reads additional extraction flags from this environment variable.
    // Ignore it so callers cannot silently enable unsafe link-following behavior.
    command.env_remove("TAR_OPTIONS");
    command
}

pub(crate) fn inspect_tar_runtime() -> Result<TarRuntimeAssessment> {
    let output = tar_command()
        .arg("--version")
        .output()
        .map_err(|error| NspawnError::Io(PathBuf::from("tar"), error))?;
    crate::nspawn::sys::log_output("tar --version", &output);

    let version = output
        .status
        .success()
        .then(|| parse_gnu_tar_version(&String::from_utf8_lossy(&output.stdout)))
        .flatten();
    Ok(TarRuntimeAssessment {
        risk: tar_runtime_risk(version),
    })
}

fn parse_gnu_tar_version(output: &str) -> Option<(u32, u32)> {
    let line = output.lines().next()?.trim();
    if !line.contains("(GNU tar)") {
        return None;
    }

    line.split_ascii_whitespace().rev().find_map(|word| {
        let version =
            word.trim_matches(|character: char| !character.is_ascii_digit() && character != '.');
        let mut components = version.split('.');
        let major = components.next()?.parse().ok()?;
        let minor = components.next()?.parse().ok()?;
        Some((major, minor))
    })
}

fn tar_runtime_risk(version: Option<(u32, u32)>) -> Option<String> {
    match version {
        Some(version) if version < (1, 34) => Some(format!(
            "GNU tar {}.{} predates 1.34 and may follow archive-created symbolic links outside the target.",
            version.0, version.1
        )),
        Some(version) if version < (1, 35) => Some(format!(
            "GNU tar {}.{} lacks the hard-link confinement added in 1.35.",
            version.0, version.1
        )),
        Some(_) => None,
        None => Some(
            "Could not verify GNU tar 1.35+ extraction protections for the host tar implementation."
                .into(),
        ),
    }
}

fn tar_runtime_policy(
    source_origin: TarSourceOrigin,
    assessment: &TarRuntimeAssessment,
    allow_unsafe_remote: bool,
) -> Result<ImageImportReport> {
    let Some(risk) = assessment.risk.as_deref() else {
        return Ok(ImageImportReport::default());
    };

    match source_origin {
        TarSourceOrigin::Local => Ok(ImageImportReport {
            warnings: vec![format!(
                "{risk} Continuing because the archive was explicitly selected from a local path; import only trusted archives or upgrade to GNU tar 1.35+."
            )],
        }),
        TarSourceOrigin::Remote if allow_unsafe_remote => Ok(ImageImportReport {
            warnings: vec![format!(
                "{risk} Continuing with the remote archive after explicit confirmation; import only trusted archives or upgrade to GNU tar 1.35+."
            )],
        }),
        TarSourceOrigin::Remote => Err(NspawnError::Validation(format!(
            "Remote tar import requires explicit risk confirmation: {risk}"
        ))),
    }
}

async fn validate_tar_target(target: &RootfsTarget) -> Result<()> {
    let path = target.path()?;
    let metadata = tokio::fs::symlink_metadata(&path)
        .await
        .map_err(|error| NspawnError::Io(path.clone(), error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(NspawnError::Validation(format!(
            "Tar import target is not a managed directory: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom};

    #[test]
    fn tar_request_rejects_untyped_target_fields() {
        let request = r#"{
            "target": {
                "kind": "machine",
                "machine": "../escape"
            }
        }"#;
        assert!(serde_json::from_str::<ImportTarRequest>(request).is_err());

        let missing_origin = r#"{
            "target": {
                "kind": "machine",
                "machine": "valid-machine"
            }
        }"#;
        assert!(serde_json::from_str::<ImportTarRequest>(missing_origin).is_err());

        let valid = r#"{
            "target": {
                "kind": "machine",
                "machine": "valid-machine"
            },
            "source_origin": "remote",
            "allow_unsafe_remote": true
        }"#;
        let request = serde_json::from_str::<ImportTarRequest>(valid).unwrap();
        assert_eq!(request.source_origin, TarSourceOrigin::Remote);
        assert!(request.allow_unsafe_remote);

        let request = r#"{
            "target": {
                "kind": "machine",
                "machine": "valid-machine",
                "path": "/tmp/escape"
            }
        }"#;
        assert!(serde_json::from_str::<ImportTarRequest>(request).is_err());
    }

    #[tokio::test]
    async fn tar_target_validation_rejects_missing_managed_target() {
        let target = RootfsTarget::Machine {
            machine: MachineName::new("lasper-missing-tar-target").unwrap(),
        };
        assert!(validate_tar_target(&target).await.is_err());
    }

    #[test]
    fn source_validation_rejects_empty_files() {
        let source = tempfile::tempfile().unwrap();
        assert!(validate_source(&source).is_err());
    }

    #[test]
    fn gnu_tar_versions_are_parsed_and_risk_classified() {
        assert_eq!(parse_gnu_tar_version("tar (GNU tar) 1.35\n"), Some((1, 35)));
        assert_eq!(
            parse_gnu_tar_version("tar (GNU tar) 1.35.90\n"),
            Some((1, 35))
        );
        assert_eq!(parse_gnu_tar_version("bsdtar 3.7.7\n"), None);

        assert!(tar_runtime_risk(Some((1, 33)))
            .unwrap()
            .contains("symbolic links"));
        assert!(tar_runtime_risk(Some((1, 34)))
            .unwrap()
            .contains("hard-link"));
        assert!(tar_runtime_risk(Some((1, 35))).is_none());
        assert!(tar_runtime_risk(None).unwrap().contains("Could not verify"));
    }

    #[test]
    fn risky_tar_runtime_requires_remote_confirmation() {
        let assessment = TarRuntimeAssessment {
            risk: tar_runtime_risk(Some((1, 34))),
        };
        let local = tar_runtime_policy(TarSourceOrigin::Local, &assessment, false).unwrap();
        assert_eq!(local.warnings.len(), 1);
        assert!(local.warnings[0].contains("local path"));

        let error = tar_runtime_policy(TarSourceOrigin::Remote, &assessment, false).unwrap_err();
        assert!(error.to_string().contains("explicit risk confirmation"));

        let accepted = tar_runtime_policy(TarSourceOrigin::Remote, &assessment, true).unwrap();
        assert_eq!(accepted.warnings.len(), 1);
        assert!(accepted.warnings[0].contains("explicit confirmation"));

        let safe = TarRuntimeAssessment::default();
        assert_eq!(
            tar_runtime_policy(TarSourceOrigin::Remote, &safe, false).unwrap(),
            ImageImportReport::default()
        );
    }

    #[test]
    fn tar_commands_ignore_environment_options() {
        let command = tar_command();
        assert!(command
            .get_envs()
            .any(|(name, value)| name == "TAR_OPTIONS" && value.is_none()));
    }

    #[test]
    fn typed_tar_import_reads_archive_from_fd() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::fs::create_dir(source.path().join("etc")).unwrap();
        std::fs::write(source.path().join("etc/os-release"), b"NAME=Lasper test\n").unwrap();

        let mut archive = tempfile::tempfile().unwrap();
        let archive_output = archive.try_clone().unwrap();
        let output = tar_command()
            .args(["-cf", "-", "-C"])
            .arg(source.path())
            .arg(".")
            .stdout(Stdio::from(archive_output))
            .output()
            .unwrap();
        assert!(output.status.success());
        archive.seek(SeekFrom::Start(0)).unwrap();

        extract_tar_at(target.path(), archive, TarSourceOrigin::Local, false).unwrap();
        assert_eq!(
            std::fs::read(target.path().join("etc/os-release")).unwrap(),
            b"NAME=Lasper test\n"
        );
    }
}
