//! Command-based runtime backend.
//!
//! This entry owns catalog reads and local/elevated [`query`] routing. [`query`]
//! inspects systemd properties; [`session`] builds native machinectl commands.

use crate::adapters::elevated::ElevatedDaemon;
use crate::adapters::error::{NspawnError, Result};
use crate::adapters::process::CommandRunner;
use crate::adapters::runtime::source::RuntimeSource;
use crate::application::operations::ExecutionRoute;
use crate::domain::inspection::MachineProperties;
use crate::domain::machine::MachineName;
use crate::domain::runtime::{ImageEntry, MachineEntry};
use serde::Deserialize;
use std::time::Duration;

pub(crate) mod query;
pub(crate) mod readiness;
pub(crate) mod session;

use query::{get_properties_with_runner, get_unit_properties_with_runner};

const HOST_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
struct MachinectlImageRow {
    name: String,
    #[serde(rename = "type")]
    image_type: String,
    ro: bool,
    usage: Option<u64>,
}

fn parse_list_images_json(output: &[u8]) -> Result<Vec<ImageEntry>> {
    let rows: Vec<MachinectlImageRow> = serde_json::from_slice(output).map_err(|error| {
        NspawnError::Runtime(format!(
            "failed to parse machinectl list-images JSON output: {error}"
        ))
    })?;
    let mut images = rows
        .into_iter()
        .map(|row| ImageEntry {
            name: row.name,
            image_type: row.image_type,
            readonly: row.ro,
            usage: row.usage.map(ImageEntry::format_usage),
            dbus_object_path: None,
        })
        .collect::<Vec<_>>();
    images.sort();
    Ok(images)
}

#[derive(Clone)]
pub struct SystemdToolsBackend {
    cmd_runner: std::sync::Arc<dyn CommandRunner>,
    property_query_route: SystemdToolsPropertyQueryRoute,
    runtime_machines_dir: std::path::PathBuf,
}

#[derive(Clone)]
enum SystemdToolsPropertyQueryRoute {
    Local(std::sync::Arc<dyn CommandRunner>),
    Elevated(std::sync::Arc<ElevatedDaemon>),
}

impl SystemdToolsPropertyQueryRoute {
    fn route(&self) -> ExecutionRoute {
        match self {
            Self::Local(_) => ExecutionRoute::LocalSystemdTools,
            Self::Elevated(_) => ExecutionRoute::ElevatedSystemdTools,
        }
    }
}

impl SystemdToolsBackend {
    pub fn new(runner: std::sync::Arc<dyn CommandRunner>) -> Self {
        Self {
            cmd_runner: std::sync::Arc::clone(&runner),
            property_query_route: SystemdToolsPropertyQueryRoute::Local(runner),
            runtime_machines_dir: crate::paths::runtime_machines_dir(),
        }
    }

    pub(crate) fn elevated(
        runner: std::sync::Arc<dyn CommandRunner>,
        daemon: std::sync::Arc<ElevatedDaemon>,
    ) -> Self {
        Self {
            cmd_runner: runner,
            property_query_route: SystemdToolsPropertyQueryRoute::Elevated(daemon),
            runtime_machines_dir: crate::paths::runtime_machines_dir(),
        }
    }

    pub(crate) fn property_query_route(&self) -> ExecutionRoute {
        self.property_query_route.route()
    }

    #[cfg(test)]
    fn with_runner(runner: std::sync::Arc<dyn CommandRunner>) -> Self {
        Self::new(runner)
    }

    #[cfg(test)]
    fn with_runtime_machines_dir(mut self, path: std::path::PathBuf) -> Self {
        self.runtime_machines_dir = path;
        self
    }
}

#[async_trait::async_trait]
impl RuntimeSource for SystemdToolsBackend {
    async fn get_unit_properties(&self, machine: &MachineName) -> Result<MachineProperties> {
        match &self.property_query_route {
            SystemdToolsPropertyQueryRoute::Local(runner) => {
                get_unit_properties_with_runner(machine, runner.as_ref()).await
            }
            SystemdToolsPropertyQueryRoute::Elevated(daemon) => daemon
                .systemd_tools_inspect_unit(machine)
                .await
                .map_err(|error| {
                    NspawnError::Io(std::path::PathBuf::from("elevated unit inspection"), error)
                }),
        }
    }

    async fn is_available(&self) -> bool {
        which::which("machinectl").is_ok()
    }

    async fn list_machines(&self) -> Result<Vec<MachineEntry>> {
        crate::adapters::runtime::registration::list_machines_at(self.runtime_machines_dir.clone())
            .await
    }

    async fn list_images(&self) -> Result<Vec<ImageEntry>> {
        let out = self
            .cmd_runner
            .run_bounded(
                "machinectl",
                vec![
                    "--no-ask-password".to_string(),
                    "--output=json".to_string(),
                    "--no-pager".to_string(),
                    "--all".to_string(),
                    "--".to_string(),
                    "list-images".to_string(),
                ],
                HOST_QUERY_TIMEOUT,
            )
            .await
            .map_err(|e| NspawnError::Io(std::path::PathBuf::from("machinectl"), e))?;

        if !out.status.success() {
            return Err(NspawnError::cmd_failed(
                "machinectl list-images",
                "machinectl --no-ask-password --output=json --no-pager --all -- list-images",
                &out,
            ));
        }

        parse_list_images_json(&out.stdout)
    }

    async fn get_properties(
        &self,
        name: &str,
        include_nspawn_unit: bool,
    ) -> Result<MachineProperties> {
        match &self.property_query_route {
            SystemdToolsPropertyQueryRoute::Local(runner) => {
                get_properties_with_runner(name, include_nspawn_unit, runner.as_ref()).await
            }
            SystemdToolsPropertyQueryRoute::Elevated(daemon) => daemon
                .systemd_tools_inspect_machine(name, include_nspawn_unit)
                .await
                .map_err(|error| {
                    NspawnError::Io(
                        std::path::PathBuf::from("elevated systemd tools inspection"),
                        error,
                    )
                }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::process::MockCommandRunner;
    use crate::domain::runtime::MachineState;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Output;

    fn mock_output(status: bool, stdout: &str, stderr: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(if status { 0 } else { 256 }),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[tokio::test]
    async fn list_machines_uses_runtime_registrations_without_commands() {
        let runner: std::sync::Arc<dyn CommandRunner> = std::sync::Arc::new({
            let mut r = MockCommandRunner::new();
            r.expect_run().never();
            r
        });
        let runtime = tempfile::tempdir().unwrap();
        std::fs::write(
            runtime.path().join("active"),
            "NAME=active\nCLASS=container\nSERVICE=systemd-nspawn\n",
        )
        .unwrap();
        let provider = SystemdToolsBackend::with_runner(runner)
            .with_runtime_machines_dir(runtime.path().to_path_buf());

        let machines = RuntimeSource::list_machines(&provider).await.unwrap();

        assert_eq!(machines.len(), 1);
        assert_eq!(machines[0].name, "active");
        assert_eq!(machines[0].state, MachineState::Running);
        assert!(matches!(
            machines[0].addresses,
            crate::domain::runtime::MachineAddressObservation::Unsupported(_)
        ));
    }

    #[tokio::test]
    async fn list_images_preserves_systemd_image_names() {
        let runner = std::sync::Arc::new({
            let mut r = MockCommandRunner::new();
            r.expect_run_bounded()
                .withf(|program, args, timeout| {
                    program == "machinectl"
                        && args.first().is_some_and(|arg| arg == "--no-ask-password")
                        && args.get(1).is_some_and(|arg| arg == "--output=json")
                        && args.get(4).is_some_and(|arg| arg == "--")
                        && args.get(5).is_some_and(|arg| arg == "list-images")
                        && *timeout == HOST_QUERY_TIMEOUT
                })
                .returning(|_, _, _| {
                    Ok(mock_output(
                        true,
                        r#"[
                            {"name":".host","type":"subvolume","ro":true,"usage":null,"created":null,"modified":null},
                            {"name":".oci-sha256:abc","type":"subvolume","ro":true,"usage":10485760,"created":0,"modified":0},
                            {"name":"Ubuntu Resolute 镜像","type":"directory","ro":false,"usage":20971520,"created":0,"modified":0},
                            {"name":" edge spaced ","type":"raw","ro":false,"usage":0,"created":0,"modified":0}
                        ]"#,
                        "",
                    ))
                });
            r
        });
        let provider = SystemdToolsBackend::with_runner(runner);

        let images = RuntimeSource::list_images(&provider).await.unwrap();

        assert_eq!(images.len(), 4);
        assert!(
            images
                .iter()
                .all(|image| image.name == ".host"
                    || image.is_hidden() == image.name.starts_with('.'))
        );
        assert!(images.iter().any(|image| image.name == ".host"));
        let ubuntu = images
            .iter()
            .find(|image| image.name == "Ubuntu Resolute 镜像")
            .unwrap();
        assert!(!ubuntu.is_hidden());
        assert_eq!(ubuntu.image_type, "directory");
        assert_eq!(ubuntu.usage.as_deref(), Some("20.0M"));
        assert!(images.iter().any(|image| image.name == " edge spaced "));
        assert_eq!(
            images
                .iter()
                .find(|image| image.name == ".host")
                .and_then(|image| image.usage.as_deref()),
            None
        );
    }

    #[test]
    fn list_images_rejects_non_json_output() {
        let error = parse_list_images_json(b"ubuntu directory no 20M\n").unwrap_err();

        assert!(error
            .to_string()
            .contains("failed to parse machinectl list-images JSON output"));
    }

    #[tokio::test]
    async fn automatic_snapshot_runs_only_the_noninteractive_image_query() {
        let runner: std::sync::Arc<dyn CommandRunner> = std::sync::Arc::new({
            let mut runner = MockCommandRunner::new();
            runner
                .expect_run_bounded()
                .times(1)
                .withf(|program, args, timeout| {
                    program == "machinectl"
                        && args
                            == &[
                                "--no-ask-password".to_string(),
                                "--output=json".to_string(),
                                "--no-pager".to_string(),
                                "--all".to_string(),
                                "--".to_string(),
                                "list-images".to_string(),
                            ]
                        && *timeout == HOST_QUERY_TIMEOUT
                })
                .returning(|_, _, _| {
                    Ok(mock_output(
                        true,
                        r#"[{"name":"active","type":"directory","ro":false,"usage":20971520}]"#,
                        "",
                    ))
                });
            runner
        });
        let runtime = tempfile::tempdir().unwrap();
        std::fs::write(
            runtime.path().join("active"),
            "NAME=active\nCLASS=container\nSERVICE=systemd-nspawn\n",
        )
        .unwrap();
        let provider = SystemdToolsBackend::with_runner(runner)
            .with_runtime_machines_dir(runtime.path().to_path_buf());

        let snapshot = RuntimeSource::snapshot(&provider).await.unwrap();

        assert_eq!(snapshot.machines.len(), 1);
        assert_eq!(snapshot.machines[0].name, "active");
        assert_eq!(snapshot.images.len(), 1);
        assert_eq!(snapshot.images[0].name, "active");
    }
}
