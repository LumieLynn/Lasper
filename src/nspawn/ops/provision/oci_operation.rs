//! Typed systemd-native OCI acquisition shared by direct and elevated modes.

use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{MachineName, OciReference};
use crate::nspawn::sys::command::{CommandRunner, SpawnedProcess};
use crate::nspawn::sys::daemon::ElevatedDaemon;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OciPullRequest {
    pub(crate) reference: OciReference,
    pub(crate) machine: MachineName,
    pub(crate) read_only: bool,
}

#[derive(Clone)]
pub struct OciPullStore {
    cmd_runner: Arc<dyn CommandRunner>,
    daemon: Option<Arc<ElevatedDaemon>>,
}

impl OciPullStore {
    pub fn new(cmd_runner: Arc<dyn CommandRunner>, daemon: Option<Arc<ElevatedDaemon>>) -> Self {
        Self { cmd_runner, daemon }
    }

    pub(crate) async fn spawn(&self, request: OciPullRequest) -> Result<SpawnedProcess> {
        if let Some(daemon) = &self.daemon {
            let cmd_id = daemon.reserve_spawn_id();
            let stdout_fd = daemon
                .spawn_oci_pull(cmd_id, request)
                .await
                .map_err(|error| NspawnError::Io(PathBuf::from("importctl pull-oci"), error))?;
            let receiver = crate::nspawn::sys::daemon::pipe_reader(stdout_fd)
                .map_err(|error| NspawnError::Io(PathBuf::from("importctl pull-oci"), error))?;
            let daemon = daemon.clone();
            Ok(SpawnedProcess::new(Box::new(receiver), async move {
                let code = daemon
                    .wait_command(cmd_id)
                    .await
                    .map_err(|error| std::io::Error::other(error.to_string()))?;
                Ok(std::os::unix::process::ExitStatusExt::from_raw(code))
            }))
        } else {
            let (program, args) = build_command(&request);
            self.cmd_runner
                .spawn(program, args)
                .await
                .map_err(|error| NspawnError::Io(PathBuf::from(program), error))
        }
    }
}

impl std::fmt::Debug for OciPullStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OciPullStore")
            .field("daemon", &self.daemon)
            .finish_non_exhaustive()
    }
}

pub(crate) fn build_command(request: &OciPullRequest) -> (&'static str, Vec<String>) {
    let mut args = vec![
        "--system".into(),
        "--class=machine".into(),
        "--no-pager".into(),
    ];
    if request.read_only {
        args.push("--read-only".into());
    }
    args.extend([
        "--".into(),
        "pull-oci".into(),
        request.reference.as_str().into(),
        request.machine.as_str().into(),
    ]);
    ("importctl", args)
}

/// Probe the actual importctl verb instead of trusting a parsed version number.
pub fn ensure_pull_oci_available() -> Result<()> {
    let output = crate::nspawn::sys::new_sync_command("importctl")
        .arg("--help")
        .output()
        .map_err(|error| NspawnError::Io(PathBuf::from("importctl --help"), error))?;
    let help = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if output.status.success()
        && help
            .lines()
            .any(|line| line.trim_start().starts_with("pull-oci "))
    {
        Ok(())
    } else {
        Err(NspawnError::Validation(
            "OCI applications require systemd 260 or newer with importctl pull-oci".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(read_only: bool) -> OciPullRequest {
        OciPullRequest {
            reference: OciReference::new("docker.io/library/nginx:latest").unwrap(),
            machine: MachineName::new("web-app").unwrap(),
            read_only,
        }
    }

    #[test]
    fn command_is_fixed_to_system_machine_oci_pull() {
        let (program, args) = build_command(&request(false));
        assert_eq!(program, "importctl");
        assert_eq!(
            args,
            [
                "--system",
                "--class=machine",
                "--no-pager",
                "--",
                "pull-oci",
                "docker.io/library/nginx:latest",
                "web-app",
            ]
        );
    }

    #[test]
    fn read_only_maps_to_importctl_flag() {
        let (_, args) = build_command(&request(true));
        assert!(args.iter().any(|argument| argument == "--read-only"));
        assert!(!args.iter().any(|argument| argument == "--force"));
    }

    #[test]
    fn request_deserialization_rejects_arbitrary_fields() {
        let json = r#"{
            "reference":"docker.io/library/nginx:latest",
            "machine":"web-app",
            "read_only":false,
            "program":"sh"
        }"#;
        assert!(serde_json::from_str::<OciPullRequest>(json).is_err());
    }
}
